//! TypeScript / JavaScript front end: oxc AST → `merak_ir` structural IR.

use merak_ir as ir;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::operator::{BinaryOperator, LogicalOperator, UnaryOperator};
use std::collections::BTreeMap;

pub fn is_supported(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    (p.ends_with(".ts") || p.ends_with(".tsx") || p.ends_with(".js") || p.ends_with(".jsx") || p.ends_with(".mjs") || p.ends_with(".cjs"))
        && !p.ends_with(".d.ts")
}

/// Lower a set of source files (`path → text`) into a structural program.
pub fn lower_program(files: &BTreeMap<String, String>) -> ir::Program {
    let mut program = ir::Program::default();
    for (path, text) in files {
        if is_supported(path) {
            program.modules.insert(path.clone(), lower_module(path, text));
        } else if is_project_config(path) {
            program.aliases.extend(path_aliases(path, text));
        }
    }
    program
}

/// `tsconfig.json` / `jsconfig.json` at any depth.
pub fn is_project_config(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "tsconfig.json" || name == "jsconfig.json"
}

/// Import aliases declared by a tsconfig's `compilerOptions.paths` and `baseUrl`.
/// `extends` is not followed.
pub fn path_aliases(config_path: &str, text: &str) -> Vec<ir::PathAlias> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(text)) else { return vec![] };
    let Some(opts) = json.get("compilerOptions") else { return vec![] };
    let scope = config_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    let base = join_path(&scope, opts.get("baseUrl").and_then(|b| b.as_str()).unwrap_or("."));
    let mut out = vec![];
    if let Some(paths) = opts.get("paths").and_then(|p| p.as_object()) {
        for (pattern, targets) in paths {
            let targets = targets.as_array().into_iter().flatten().filter_map(|t| t.as_str()).map(|t| join_path(&base, t)).collect();
            out.push(ir::PathAlias { scope: scope.clone(), pattern: pattern.clone(), targets });
        }
    }
    if opts.get("baseUrl").is_some() {
        // Bare specifiers resolve against baseUrl (after `paths`).
        out.push(ir::PathAlias { scope: scope.clone(), pattern: "*".into(), targets: vec![join_path(&base, "*")] });
    }
    out
}

/// Join a repository-relative directory and a relative path, normalising `.` / `..`.
fn join_path(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Remove `//` and `/* */` comments and trailing commas, so JSONC parses as JSON.
fn strip_jsonc(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
            out.push(b'"');
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            while i < b.len() && !b[i..].starts_with(b"*/") {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == b',' {
            let rest = text[i + 1..].trim_start();
            if !(rest.starts_with('}') || rest.starts_with(']')) {
                out.push(b',');
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn lower_module(path: &str, text: &str) -> ir::Module {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let ret = Parser::new(&allocator, text, source_type).parse();
    let mut lw = Lowerer::new(path, text);
    for d in ret.diagnostics.iter() {
        lw.module.parse_errors.push(d.to_string());
    }
    for stmt in &ret.program.body {
        lw.top_level(stmt, false);
    }
    lw.module
}

struct Lowerer<'s> {
    path: String,
    src: &'s str,
    line_starts: Vec<usize>,
    module: ir::Module,
    /// Qualified-name stack of enclosing functions, for naming closures.
    scope: Vec<String>,
    closure_counter: BTreeMap<String, u32>,
    /// Module constant whose initializer is being lowered: names its closures
    /// (`SharedLinkCreateSchema.superRefine`) without being their parent function.
    const_scope: Option<String>,
}

impl<'s> Lowerer<'s> {
    fn new(path: &str, src: &'s str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Lowerer {
            path: path.to_string(),
            src,
            line_starts,
            module: ir::Module { path: path.to_string(), ..Default::default() },
            scope: vec![],
            closure_counter: BTreeMap::new(),
            const_scope: None,
        }
    }

    fn line(&self, offset: u32) -> u32 {
        match self.line_starts.binary_search(&(offset as usize)) {
            Ok(i) => i as u32 + 1,
            Err(i) => i as u32,
        }
    }

    fn loc(&self, span: Span) -> ir::Loc {
        ir::Loc { file: self.path.clone(), line: self.line(span.start) }
    }

    fn text(&self, span: Span) -> String {
        let s = &self.src[span.start as usize..span.end as usize];
        let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if one_line.len() > 120 {
            format!("{}…", &one_line[..one_line.char_indices().nth(117).map(|(i, _)| i).unwrap_or(one_line.len())])
        } else {
            one_line
        }
    }

    fn entity_id(&self, qualified: &str) -> String {
        format!("{}::{}", self.path, qualified)
    }

    // ---------------------------------------------------------------- top level

    fn top_level(&mut self, stmt: &Statement, exported: bool) {
        match stmt {
            Statement::ImportDeclaration(decl) => self.import(decl),
            Statement::ExportNamedDeclaration(_) => {}
            Statement::ExportDeclaration(decl) => self.declaration(&decl.declaration, true),
            Statement::ExportDefaultDeclaration(decl) => match &decl.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => self.function_decl(f, true),
                ExportDefaultDeclarationKind::ClassDeclaration(c) => self.class(c, true),
                _ => {}
            },
            Statement::VariableDeclaration(_)
            | Statement::FunctionDeclaration(_)
            | Statement::ClassDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_) => {
                if let Some(decl) = stmt.as_declaration() {
                    self.declaration(decl, exported)
                }
            }
            _ => {}
        }
    }

    fn import(&mut self, decl: &ImportDeclaration) {
        let source = decl.source.value.as_str().to_string();
        let Some(specs) = &decl.specifiers else { return };
        for spec in specs {
            let (local, imported) = match spec {
                ImportDeclarationSpecifier::ImportSpecifier(s) => (s.local.name.as_str().to_string(), module_export_name(&s.imported)),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => (s.local.name.as_str().to_string(), "default".into()),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => (s.local.name.as_str().to_string(), "*".into()),
            };
            self.module.imports.push(ir::Import { local, imported, source: source.clone() });
        }
    }

    fn declaration(&mut self, decl: &Declaration, exported: bool) {
        match decl {
            Declaration::FunctionDeclaration(f) => self.function_decl(f, exported),
            Declaration::ClassDeclaration(c) => self.class(c, exported),
            Declaration::VariableDeclaration(v) => {
                for d in &v.declarations {
                    let BindingPattern::BindingIdentifier(id) = &d.id else { continue };
                    let name = id.name.as_str().to_string();
                    let ty = d.type_annotation.as_ref().map(|t| self.ts_type(&t.type_annotation));
                    // `const f = () => {}` at top level is a function entity.
                    match &d.init {
                        Some(Expression::ArrowFunctionExpression(a)) => {
                            self.arrow(a, &name, exported);
                        }
                        Some(Expression::FunctionExpression(f)) => {
                            self.function(f, &name, None, ir::FunctionKind::Function, exported, None);
                        }
                        init => {
                            self.const_scope = Some(name.clone());
                            let init = init.as_ref().map(|e| self.expr(e));
                            self.const_scope = None;
                            let loc = self.loc(d.span);
                            self.module.consts.push(ir::Const { name, ty, init, exported, loc });
                        }
                    }
                }
            }
            Declaration::TSEnumDeclaration(e) => {
                let mut members = BTreeMap::new();
                for m in &e.body.members {
                    let name = match &m.id {
                        TSEnumMemberName::Identifier(i) => i.name.as_str().to_string(),
                        TSEnumMemberName::String(s) | TSEnumMemberName::ComputedString(s) => s.value.as_str().to_string(),
                        TSEnumMemberName::ComputedTemplateString(_) => continue,
                    };
                    let value = match &m.initializer {
                        Some(Expression::StringLiteral(s)) => s.value.as_str().to_string(),
                        _ => name.clone(),
                    };
                    members.insert(name, value);
                }
                self.module.enums.push(ir::Enum { name: e.id.name.as_str().to_string(), members });
            }
            Declaration::TSInterfaceDeclaration(i) => {
                let fields = self.signatures(&i.body.body);
                self.module.interfaces.push(ir::Interface { name: i.id.name.as_str().to_string(), fields });
            }
            Declaration::TSTypeAliasDeclaration(t) => {
                let ty = self.ts_type(&t.type_annotation);
                self.module.type_aliases.insert(t.id.name.as_str().to_string(), ty);
            }
            _ => {}
        }
    }

    fn signatures(&self, sigs: &[TSSignature]) -> BTreeMap<String, ir::TypeRef> {
        let mut fields = BTreeMap::new();
        for sig in sigs {
            if let TSSignature::TSPropertySignature(p) = sig {
                if let Some(name) = property_key_name(&p.key) {
                    let ty = p.type_annotation.as_ref().map(|t| self.ts_type(&t.type_annotation)).unwrap_or(ir::TypeRef::Unknown);
                    fields.insert(name, ty);
                }
            }
        }
        fields
    }

    fn class(&mut self, class: &Class, exported: bool) {
        let Some(id) = &class.id else { return };
        let name = id.name.as_str().to_string();
        let extends = class.heritage.as_ref().and_then(|h| match &h.expression {
            Expression::Identifier(i) => Some(i.name.as_str().to_string()),
            Expression::StaticMemberExpression(m) => Some(m.property.name.as_str().to_string()),
            _ => None,
        });
        let mut fields = BTreeMap::new();
        let mut methods = vec![];
        for el in &class.body.body {
            match el {
                ClassElement::PropertyDefinition(p) => {
                    if let Some(key) = property_key_name(&p.key) {
                        let ty = p
                            .type_annotation
                            .as_ref()
                            .map(|t| self.ts_type(&t.type_annotation))
                            .or_else(|| match &p.value {
                                Some(Expression::NewExpression(n)) => callee_name(&n.callee).map(|c| ir::TypeRef::Named(c, vec![])),
                                _ => None,
                            })
                            .unwrap_or(ir::TypeRef::Unknown);
                        fields.insert(key, ty);
                    }
                }
                ClassElement::MethodDefinition(m) => {
                    let kind = match m.kind {
                        MethodDefinitionKind::Constructor => ir::FunctionKind::Constructor,
                        _ => ir::FunctionKind::Method,
                    };
                    if kind == ir::FunctionKind::Constructor {
                        for p in &m.value.params.items {
                            if p.accessibility.is_some() || p.readonly {
                                if let BindingPattern::BindingIdentifier(b) = &p.pattern {
                                    let ty = p.type_annotation.as_ref().map(|t| self.ts_type(&t.type_annotation)).unwrap_or(ir::TypeRef::Unknown);
                                    fields.insert(b.name.as_str().to_string(), ty);
                                }
                            }
                        }
                    }
                    let Some(mname) = property_key_name(&m.key) else { continue };
                    let mname = if kind == ir::FunctionKind::Constructor { "constructor".to_string() } else { mname };
                    let qualified = format!("{name}.{mname}");
                    let fid = self.function(&m.value, &qualified, Some(&name), kind, exported, None);
                    let decorators = self.decorators(&m.decorators);
                    if let Some(f) = self.module.functions.iter_mut().rev().find(|f| f.id == fid) {
                        f.decorators = decorators;
                    }
                    methods.push(fid);
                }
                _ => {}
            }
        }
        let loc = self.loc(class.span);
        let decorators = self.decorators(&class.decorators);
        self.module.classes.push(ir::Class { id: self.entity_id(&name), name, extends, fields, methods, exported, loc, decorators });
    }

    fn decorators(&mut self, decorators: &[Decorator]) -> Vec<ir::Call> {
        decorators
            .iter()
            .map(|d| match &d.expression {
                Expression::CallExpression(c) => {
                    let callee = self.expr(&c.callee);
                    let args = c.arguments.iter().filter_map(|a| a.as_expression()).map(|a| self.expr(a)).collect();
                    ir::Call { callee, args, loc: self.loc(d.span) }
                }
                other => ir::Call { callee: self.expr(other), args: vec![], loc: self.loc(d.span) },
            })
            .collect()
    }

    fn function_decl(&mut self, f: &Function, exported: bool) {
        let Some(id) = &f.id else { return };
        let name = id.name.as_str().to_string();
        self.function(f, &name, None, ir::FunctionKind::Function, exported, None);
    }

    fn params(&self, params: &FormalParameters) -> Vec<ir::Param> {
        params
            .items
            .iter()
            .map(|p| {
                let name = match &p.pattern {
                    BindingPattern::BindingIdentifier(b) => b.name.as_str().to_string(),
                    BindingPattern::AssignmentPattern(a) => match &a.left {
                        BindingPattern::BindingIdentifier(b) => b.name.as_str().to_string(),
                        _ => "_".into(),
                    },
                    _ => "_".into(),
                };
                let ty = p.type_annotation.as_ref().map(|t| self.ts_type(&t.type_annotation));
                ir::Param { name, ty }
            })
            .collect()
    }

    fn function(&mut self, f: &Function, qualified: &str, class: Option<&str>, kind: ir::FunctionKind, exported: bool, parent: Option<String>) -> String {
        let id = self.entity_id(qualified);
        let params = self.params(&f.params);
        let return_type = f.return_type.as_ref().map(|t| self.ts_type(&t.type_annotation));
        self.scope.push(qualified.to_string());
        let body = f.body.as_ref().map(|b| self.stmts(&b.statements)).unwrap_or_default();
        self.scope.pop();
        self.push_function(ir::Function {
            id: id.clone(),
            name: qualified.rsplit('.').next().unwrap_or(qualified).to_string(),
            kind,
            class: class.map(str::to_string),
            parent,
            params,
            return_type,
            body,
            is_async: f.r#async,
            exported,
            loc: self.loc(f.span),
            end_line: self.line(f.span.end),
            body_hash: String::new(),
            decorators: vec![],
        });
        id
    }

    fn arrow(&mut self, a: &ArrowFunctionExpression, qualified: &str, exported: bool) -> String {
        let id = self.entity_id(qualified);
        let params = self.params(&a.params);
        let return_type = a.return_type.as_ref().map(|t| self.ts_type(&t.type_annotation));
        let parent = self.scope.last().map(|q| self.entity_id(q));
        self.scope.push(qualified.to_string());
        let body = match &a.body {
            ArrowFunctionBody::FunctionBody(b) => self.stmts(&b.statements),
            // Expression-bodied arrows are stored as a single return.
            other => match other.as_expression() {
                Some(e) => vec![ir::Stmt::Return(Some(self.expr(e)), self.loc(e.span()))],
                None => vec![],
            },
        };
        self.scope.pop();
        let kind = if parent.is_some() { ir::FunctionKind::Closure } else { ir::FunctionKind::Function };
        self.push_function(ir::Function {
            id: id.clone(),
            name: qualified.rsplit('.').next().unwrap_or(qualified).to_string(),
            kind,
            class: None,
            parent,
            params,
            return_type,
            body,
            is_async: a.r#async,
            exported,
            loc: self.loc(a.span),
            end_line: self.line(a.span.end),
            body_hash: String::new(),
            decorators: vec![],
        });
        id
    }

    fn push_function(&mut self, mut f: ir::Function) {
        f.body_hash = body_hash(&f.body);
        self.module.functions.push(f);
    }

    /// Name for a closure found inside the current function.
    fn closure_name(&mut self, hint: Option<String>) -> String {
        let parent = self.scope.last().or(self.const_scope.as_ref()).cloned().unwrap_or_else(|| "<module>".into());
        let base = match hint {
            Some(h) => format!("{parent}.{h}"),
            None => format!("{parent}.<closure>"),
        };
        let n = self.closure_counter.entry(base.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            base
        } else {
            format!("{base}#{n}")
        }
    }

    // ---------------------------------------------------------------- statements

    fn stmts(&mut self, stmts: &[Statement]) -> Vec<ir::Stmt> {
        stmts.iter().filter_map(|s| self.stmt(s)).collect()
    }

    fn block(&mut self, s: &Statement) -> Vec<ir::Stmt> {
        match s {
            Statement::BlockStatement(b) => self.stmts(&b.body),
            other => self.stmt(other).into_iter().collect(),
        }
    }

    fn stmt(&mut self, s: &Statement) -> Option<ir::Stmt> {
        let loc = self.loc(s.span());
        Some(match s {
            Statement::ExpressionStatement(e) => ir::Stmt::Expr(self.expr(&e.expression), loc),
            Statement::VariableDeclaration(v) => {
                let mut out = vec![];
                for d in &v.declarations {
                    let dloc = self.loc(d.span);
                    match &d.id {
                        BindingPattern::BindingIdentifier(id) => {
                            let name = id.name.as_str().to_string();
                            let ty = d.type_annotation.as_ref().map(|t| self.ts_type(&t.type_annotation));
                            let init = match &d.init {
                                Some(Expression::ArrowFunctionExpression(a)) => {
                                    let q = self.closure_name(Some(name.clone()));
                                    Some(ir::Expr::Closure(self.arrow(a, &q, false)))
                                }
                                Some(e) => Some(self.expr(e)),
                                None => None,
                            };
                            out.push(ir::Stmt::Let { name, ty, init, loc: dloc });
                        }
                        _ => {
                            if let Some(e) = &d.init {
                                out.push(ir::Stmt::Expr(self.expr(e), dloc));
                            }
                        }
                    }
                }
                if out.len() == 1 {
                    out.pop().unwrap()
                } else {
                    ir::Stmt::Block(out)
                }
            }
            Statement::IfStatement(i) => ir::Stmt::If {
                test: self.expr(&i.test),
                then: self.block(&i.consequent),
                otherwise: i.alternate.as_ref().map(|a| self.block(a)).unwrap_or_default(),
                loc,
            },
            Statement::ReturnStatement(r) => ir::Stmt::Return(r.argument.as_ref().map(|e| self.expr(e)), loc),
            Statement::ThrowStatement(t) => ir::Stmt::Throw(self.expr(&t.argument), loc),
            Statement::BlockStatement(b) => ir::Stmt::Block(self.stmts(&b.body)),
            Statement::ForOfStatement(f) => {
                let binding = match &f.left {
                    ForStatementLeft::VariableDeclaration(v) => v.declarations.first().and_then(|d| match &d.id {
                        BindingPattern::BindingIdentifier(b) => Some(b.name.as_str().to_string()),
                        _ => None,
                    }),
                    _ => None,
                };
                ir::Stmt::Loop { binding, iter: Some(self.expr(&f.right)), body: self.block(&f.body), loc }
            }
            Statement::ForInStatement(f) => ir::Stmt::Loop { binding: None, iter: Some(self.expr(&f.right)), body: self.block(&f.body), loc },
            Statement::ForStatement(f) => ir::Stmt::Loop { binding: None, iter: None, body: self.block(&f.body), loc },
            Statement::WhileStatement(w) => ir::Stmt::Loop { binding: None, iter: Some(self.expr(&w.test)), body: self.block(&w.body), loc },
            Statement::DoWhileStatement(w) => ir::Stmt::Loop { binding: None, iter: Some(self.expr(&w.test)), body: self.block(&w.body), loc },
            Statement::TryStatement(t) => ir::Stmt::Try {
                body: self.stmts(&t.block.body),
                handler: t.handler.as_ref().map(|h| self.stmts(&h.body.body)).unwrap_or_default(),
                finalizer: t.finalizer.as_ref().map(|f| self.stmts(&f.body)).unwrap_or_default(),
                loc,
            },
            Statement::SwitchStatement(sw) => {
                // Lower `switch` into a chain of ifs on `discriminant === test`.
                let disc = self.expr(&sw.discriminant);
                let mut arms = vec![];
                for case in &sw.cases {
                    let body: Vec<ir::Stmt> = self.stmts(&case.consequent);
                    let test = case.test.as_ref().map(|t| ir::Expr::Binary { op: ir::BinOp::Eq, left: Box::new(disc.clone()), right: Box::new(self.expr(t)) });
                    arms.push((test, body, self.loc(case.span)));
                }
                let mut out = vec![];
                for (test, then, cloc) in arms {
                    match test {
                        Some(test) => out.push(ir::Stmt::If { test, then, otherwise: vec![], loc: cloc }),
                        None => out.push(ir::Stmt::Block(then)),
                    }
                }
                ir::Stmt::Block(out)
            }
            Statement::FunctionDeclaration(f) => {
                if let Some(id) = &f.id {
                    let q = self.closure_name(Some(id.name.as_str().to_string()));
                    let parent = self.scope.last().map(|p| self.entity_id(p));
                    self.function(f, &q, None, ir::FunctionKind::Closure, false, parent);
                }
                return None;
            }
            Statement::EmptyStatement(_) => return None,
            Statement::BreakStatement(_) | Statement::ContinueStatement(_) => ir::Stmt::Jump(loc),
            _ => ir::Stmt::Expr(ir::Expr::Opaque(self.text(s.span())), loc),
        })
    }

    // ---------------------------------------------------------------- expressions

    fn expr(&mut self, e: &Expression) -> ir::Expr {
        match e {
            Expression::StringLiteral(s) => ir::Expr::Str(s.value.as_str().to_string()),
            Expression::NumericLiteral(n) => ir::Expr::Num(n.value),
            Expression::BooleanLiteral(b) => ir::Expr::Bool(b.value),
            Expression::NullLiteral(_) => ir::Expr::Null,
            Expression::Identifier(i) if i.name.as_str() == "undefined" => ir::Expr::Undefined,
            Expression::Identifier(i) => ir::Expr::Ident(i.name.as_str().to_string()),
            Expression::ThisExpression(_) => ir::Expr::This,
            Expression::TemplateLiteral(t) => ir::Expr::Template {
                quasis: t.quasis.iter().map(|q| q.value.cooked.map(|c| c.as_str().to_string()).unwrap_or_default()).collect(),
                exprs: t.expressions.iter().map(|x| self.expr(x)).collect(),
            },
            Expression::StaticMemberExpression(m) => {
                ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: m.property.name.as_str().to_string() }
            }
            Expression::PrivateFieldExpression(m) => ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: m.field.name.as_str().to_string() },
            Expression::ComputedMemberExpression(m) => match &m.expression {
                Expression::StringLiteral(s) => ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: s.value.as_str().to_string() },
                idx => ir::Expr::Index { object: Box::new(self.expr(&m.object)), index: Box::new(self.expr(idx)) },
            },
            Expression::CallExpression(c) => self.call(c),
            Expression::ChainExpression(c) => match &c.expression {
                ChainElement::CallExpression(call) => self.call(call),
                ChainElement::StaticMemberExpression(m) => {
                    ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: m.property.name.as_str().to_string() }
                }
                ChainElement::TSNonNullExpression(n) => self.expr(&n.expression),
                _ => ir::Expr::Opaque(self.text(c.span)),
            },
            Expression::NewExpression(n) => {
                ir::Expr::New { callee: Box::new(self.expr(&n.callee)), args: n.arguments.iter().map(|a| self.arg(a, None)).collect(), loc: self.loc(n.span) }
            }
            Expression::BinaryExpression(b) => ir::Expr::Binary {
                op: match b.operator {
                    BinaryOperator::Equality | BinaryOperator::StrictEquality => ir::BinOp::Eq,
                    BinaryOperator::Inequality | BinaryOperator::StrictInequality => ir::BinOp::NotEq,
                    BinaryOperator::LessThan => ir::BinOp::Lt,
                    BinaryOperator::LessEqualThan => ir::BinOp::LtEq,
                    BinaryOperator::GreaterThan => ir::BinOp::Gt,
                    BinaryOperator::GreaterEqualThan => ir::BinOp::GtEq,
                    _ => ir::BinOp::Other,
                },
                left: Box::new(self.expr(&b.left)),
                right: Box::new(self.expr(&b.right)),
            },
            Expression::LogicalExpression(l) => ir::Expr::Logical {
                op: match l.operator {
                    LogicalOperator::And => ir::LogicOp::And,
                    LogicalOperator::Or => ir::LogicOp::Or,
                    LogicalOperator::Coalesce => ir::LogicOp::Coalesce,
                },
                left: Box::new(self.expr(&l.left)),
                right: Box::new(self.expr(&l.right)),
            },
            Expression::UnaryExpression(u) if u.operator == UnaryOperator::LogicalNot => ir::Expr::Not(Box::new(self.expr(&u.argument))),
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::TSAsExpression(a) => self.expr(&a.expression),
            Expression::TSSatisfiesExpression(a) => self.expr(&a.expression),
            Expression::TSNonNullExpression(a) => self.expr(&a.expression),
            Expression::TSTypeAssertion(a) => self.expr(&a.expression),
            Expression::AwaitExpression(a) => ir::Expr::Await(Box::new(self.expr(&a.argument))),
            Expression::ObjectExpression(o) => {
                let mut props = vec![];
                for p in &o.properties {
                    match p {
                        ObjectPropertyKind::ObjectProperty(p) => {
                            if let Some(k) = property_key_name(&p.key) {
                                props.push((k, self.expr(&p.value)));
                            }
                        }
                        ObjectPropertyKind::SpreadProperty(s) => props.push(("...".into(), self.expr(&s.argument))),
                    }
                }
                ir::Expr::Object(props)
            }
            Expression::ArrayExpression(a) => ir::Expr::Array(
                a.elements
                    .iter()
                    .filter_map(|el| match el {
                        ArrayExpressionElement::SpreadElement(s) => Some(self.expr(&s.argument)),
                        ArrayExpressionElement::Elision(_) => None,
                        other => other.as_expression().map(|x| self.expr(x)),
                    })
                    .collect(),
            ),
            Expression::ConditionalExpression(c) => ir::Expr::Conditional {
                test: Box::new(self.expr(&c.test)),
                then: Box::new(self.expr(&c.consequent)),
                otherwise: Box::new(self.expr(&c.alternate)),
            },
            Expression::AssignmentExpression(a) => {
                let target = match &a.left {
                    AssignmentTarget::AssignmentTargetIdentifier(i) => ir::Expr::Ident(i.name.as_str().to_string()),
                    AssignmentTarget::StaticMemberExpression(m) => {
                        ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: m.property.name.as_str().to_string() }
                    }
                    AssignmentTarget::PrivateFieldExpression(m) => {
                        ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: m.field.name.as_str().to_string() }
                    }
                    AssignmentTarget::ComputedMemberExpression(m) => match &m.expression {
                        Expression::StringLiteral(s) => ir::Expr::Member { object: Box::new(self.expr(&m.object)), property: s.value.as_str().to_string() },
                        idx => ir::Expr::Index { object: Box::new(self.expr(&m.object)), index: Box::new(self.expr(idx)) },
                    },
                    _ => ir::Expr::Opaque(self.text(a.left.span())),
                };
                ir::Expr::Assign { target: Box::new(target), value: Box::new(self.expr(&a.right)), loc: self.loc(a.span) }
            }
            Expression::ArrowFunctionExpression(a) => {
                let q = self.closure_name(None);
                ir::Expr::Closure(self.arrow(a, &q, false))
            }
            Expression::FunctionExpression(f) => {
                let q = self.closure_name(None);
                let parent = self.scope.last().map(|p| self.entity_id(p));
                ir::Expr::Closure(self.function(f, &q, None, ir::FunctionKind::Closure, false, parent))
            }
            Expression::SequenceExpression(s) => s.expressions.last().map(|x| self.expr(x)).unwrap_or(ir::Expr::Undefined),
            other => ir::Expr::Opaque(self.text(other.span())),
        }
    }

    fn call(&mut self, c: &CallExpression) -> ir::Expr {
        let callee = self.expr(&c.callee);
        // Closures passed as arguments are named after the call they're passed to:
        // `events.on("OrderCancelled", async (e) => ...)` → `<fn>.on("OrderCancelled")`.
        let method = match &callee {
            ir::Expr::Member { property, .. } => Some(property.clone()),
            ir::Expr::Ident(n) => Some(n.clone()),
            _ => None,
        };
        let first_str = c.arguments.first().and_then(|a| match a {
            Argument::StringLiteral(s) => Some(s.value.as_str().to_string()),
            _ => None,
        });
        let hint = method.map(|m| match &first_str {
            Some(s) => format!("{m}({s:?})"),
            None => m,
        });
        let args = c.arguments.iter().map(|a| self.arg(a, hint.clone())).collect();
        ir::Expr::Call(Box::new(ir::Call { callee, args, loc: self.loc(c.span) }))
    }

    fn arg(&mut self, a: &Argument, closure_hint: Option<String>) -> ir::Expr {
        match a {
            Argument::SpreadElement(s) => self.expr(&s.argument),
            Argument::ArrowFunctionExpression(f) => {
                let q = self.closure_name(closure_hint);
                ir::Expr::Closure(self.arrow(f, &q, false))
            }
            Argument::FunctionExpression(f) => {
                let q = self.closure_name(closure_hint);
                let parent = self.scope.last().map(|p| self.entity_id(p));
                ir::Expr::Closure(self.function(f, &q, None, ir::FunctionKind::Closure, false, parent))
            }
            other => match other.as_expression() {
                Some(e) => self.expr(e),
                None => ir::Expr::Opaque(self.text(other.span())),
            },
        }
    }

    // ---------------------------------------------------------------- types

    fn ts_type(&self, t: &TSType) -> ir::TypeRef {
        match t {
            TSType::TSTypeReference(r) => {
                let name = ts_type_name(&r.type_name);
                let args = r.type_arguments.as_ref().map(|a| a.params.iter().map(|p| self.ts_type(p)).collect()).unwrap_or_default();
                ir::TypeRef::Named(name, args)
            }
            TSType::TSArrayType(a) => ir::TypeRef::Array(Box::new(self.ts_type(&a.element_type))),
            TSType::TSUnionType(u) => {
                let parts: Vec<ir::TypeRef> = u.types.iter().map(|x| self.ts_type(x)).collect();
                let lits: Vec<String> = parts
                    .iter()
                    .filter_map(|p| match p {
                        ir::TypeRef::StringLiterals(v) if v.len() == 1 => Some(v[0].clone()),
                        _ => None,
                    })
                    .collect();
                if lits.len() == parts.len() {
                    ir::TypeRef::StringLiterals(lits)
                } else {
                    ir::TypeRef::Union(parts)
                }
            }
            TSType::TSLiteralType(l) => match &l.literal {
                TSLiteral::StringLiteral(s) => ir::TypeRef::StringLiterals(vec![s.value.as_str().to_string()]),
                _ => ir::TypeRef::Primitive("literal".into()),
            },
            TSType::TSParenthesizedType(p) => self.ts_type(&p.type_annotation),
            TSType::TSTypeLiteral(l) => ir::TypeRef::Object(self.signatures(&l.members).into_iter().collect()),
            TSType::TSStringKeyword(_) => ir::TypeRef::Primitive("string".into()),
            TSType::TSNumberKeyword(_) => ir::TypeRef::Primitive("number".into()),
            TSType::TSBooleanKeyword(_) => ir::TypeRef::Primitive("boolean".into()),
            TSType::TSNullKeyword(_) => ir::TypeRef::Primitive("null".into()),
            TSType::TSUndefinedKeyword(_) => ir::TypeRef::Primitive("undefined".into()),
            TSType::TSVoidKeyword(_) => ir::TypeRef::Primitive("void".into()),
            _ => ir::TypeRef::Unknown,
        }
    }
}

fn ts_type_name(n: &TSTypeName) -> String {
    match n {
        TSTypeName::IdentifierReference(i) => i.name.as_str().to_string(),
        TSTypeName::QualifiedName(q) => format!("{}.{}", ts_type_name(&q.left), q.right.name.as_str()),
        TSTypeName::ThisExpression(_) => "this".into(),
    }
}

fn module_export_name(n: &ModuleExportName) -> String {
    match n {
        ModuleExportName::IdentifierName(i) => i.name.as_str().to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.as_str().to_string(),
        ModuleExportName::StringLiteral(s) => s.value.as_str().to_string(),
    }
}

fn property_key_name(k: &PropertyKey) -> Option<String> {
    match k {
        PropertyKey::StaticIdentifier(i) => Some(i.name.as_str().to_string()),
        PropertyKey::PrivateIdentifier(i) => Some(i.name.as_str().to_string()),
        PropertyKey::StringLiteral(s) => Some(s.value.as_str().to_string()),
        _ => None,
    }
}

fn callee_name(e: &Expression) -> Option<String> {
    match e {
        Expression::Identifier(i) => Some(i.name.as_str().to_string()),
        Expression::StaticMemberExpression(m) => Some(m.property.name.as_str().to_string()),
        _ => None,
    }
}

/// Location-independent hash of a lowered body, used to detect moved/renamed entities.
fn body_hash(body: &[ir::Stmt]) -> String {
    fn strip_locs(v: &mut serde_json::Value) {
        match v {
            // Locations appear both as `loc` fields and inside tuple variants.
            serde_json::Value::Object(map) if map.len() == 2 && map.contains_key("file") && map.contains_key("line") => {
                *v = serde_json::Value::Null;
            }
            serde_json::Value::Object(map) => {
                map.remove("loc");
                map.values_mut().for_each(strip_locs);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip_locs),
            _ => {}
        }
    }
    let mut v = serde_json::to_value(body).unwrap_or_default();
    strip_locs(&mut v);
    ir::stable_hash(&v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowers_class_with_parameter_properties() {
        let m = lower_module(
            "src/a.ts",
            "import { B } from './b';\nexport class A {\n  constructor(private readonly b: B) {}\n  async run(x: string): Promise<void> {\n    if (!x) { throw new Error('no'); }\n    await this.b.go(x);\n  }\n}\n",
        );
        assert!(m.parse_errors.is_empty(), "{:?}", m.parse_errors);
        let class = &m.classes[0];
        assert_eq!(class.fields.get("b"), Some(&ir::TypeRef::Named("B".into(), vec![])));
        let run = m.functions.iter().find(|f| f.id == "src/a.ts::A.run").unwrap();
        assert_eq!(run.body.len(), 2);
        assert!(matches!(run.body[0], ir::Stmt::If { .. }));
    }

    #[test]
    fn body_hash_ignores_line_shifts() {
        let a = lower_module("src/a.ts", "export function f(x: number) {\n  if (x) { throw new Error('x'); }\n  return g(x);\n}\n");
        let b = lower_module("src/a.ts", "\n\n// moved down\nexport function f(x: number) {\n  if (x) { throw new Error('x'); }\n  return g(x);\n}\n");
        assert_eq!(a.functions[0].body_hash, b.functions[0].body_hash);
    }

    #[test]
    fn names_callback_closures_after_their_call() {
        let m = lower_module("src/h.ts", "export function reg(events: Bus) {\n  events.on(\"Evt\", async (e) => { await x(e); });\n}\n");
        assert!(m.functions.iter().any(|f| f.id == "src/h.ts::reg.on(\"Evt\")"), "{:?}", m.functions.iter().map(|f| &f.id).collect::<Vec<_>>());
    }
}
