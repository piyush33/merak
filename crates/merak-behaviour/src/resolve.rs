//! Program index: module/symbol resolution, lightweight type inference,
//! call resolution and constant evaluation over the structural IR.

use merak_ir::{self as ir, Expr, TypeRef};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A resolved type, as far as behaviour extraction needs it.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// Instance of a class in the program (class entity id).
    Class(String),
    /// A named record type (interface / type alias / class used as data), by simple name.
    Record {
        name: String,
        module: String,
    },
    /// A value from an external module, by canonical name (see `catalog.toml`).
    External(String),
    Array(Box<Ty>),
    Object(Vec<(String, Ty)>),
    /// Reference to an enum object (e.g. `OrderStatus`).
    EnumObject {
        module: String,
        name: String,
    },
    /// A function value (entity id).
    Function(String),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Callee {
    Internal(String),
    External(String),
    Unresolved(String),
}

#[derive(Debug, Clone)]
enum Symbol {
    Class(String),
    Function(String),
    Const { module: String, name: String },
    Enum { module: String, name: String },
    Record { module: String, name: String },
    External(String),
}

pub struct Index<'p> {
    pub program: &'p ir::Program,
    functions: HashMap<String, &'p ir::Function>,
    classes: HashMap<String, &'p ir::Class>,
}

impl<'p> Index<'p> {
    pub fn new(program: &'p ir::Program) -> Self {
        let mut functions = HashMap::new();
        let mut classes = HashMap::new();
        for m in program.modules.values() {
            for f in &m.functions {
                functions.insert(f.id.clone(), f);
            }
            for c in &m.classes {
                classes.insert(c.id.clone(), c);
            }
        }
        Index { program, functions, classes }
    }

    pub fn function(&self, id: &str) -> Option<&'p ir::Function> {
        self.functions.get(id).copied()
    }

    pub fn class(&self, id: &str) -> Option<&'p ir::Class> {
        self.classes.get(id).copied()
    }

    fn module(&self, path: &str) -> Option<&'p ir::Module> {
        self.program.modules.get(path)
    }

    /// Resolve an import specifier to a module path in the program: relative
    /// specifiers first, then the aliases in scope (tsconfig `paths` / `baseUrl`).
    pub fn resolve_module(&self, from: &str, spec: &str) -> Option<String> {
        if spec.starts_with('.') {
            let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            return self.probe(&join(dir, spec));
        }
        // Innermost scope wins; within a scope, the longest matching prefix.
        let mut aliases: Vec<_> = self.program.aliases.iter().filter(|a| a.scope.is_empty() || from.starts_with(&format!("{}/", a.scope))).collect();
        aliases.sort_by_key(|a| std::cmp::Reverse((a.scope.len(), a.pattern.split('*').next().unwrap_or("").len())));
        for a in aliases {
            let Some(star) = match_alias(&a.pattern, spec) else { continue };
            for t in &a.targets {
                if let Some(m) = self.probe(&t.replacen('*', star, 1)) {
                    return Some(m);
                }
            }
        }
        None
    }

    /// The program module a (normalised, extension-optional) path refers to.
    fn probe(&self, base: &str) -> Option<String> {
        let stem = base.trim_end_matches(".js").trim_end_matches(".jsx");
        let candidates = [
            base.to_string(),
            format!("{stem}.ts"),
            format!("{stem}.tsx"),
            format!("{stem}.js"),
            format!("{stem}.jsx"),
            format!("{base}/index.ts"),
            format!("{base}/index.js"),
        ];
        candidates.into_iter().find(|c| self.program.modules.contains_key(c))
    }

    fn resolve_symbol(&self, module: &str, name: &str) -> Option<Symbol> {
        self.resolve_symbol_depth(module, name, 0)
    }

    fn resolve_symbol_depth(&self, module: &str, name: &str, depth: u32) -> Option<Symbol> {
        if depth > 8 {
            return None;
        }
        let m = self.module(module)?;
        if let Some(c) = m.classes.iter().find(|c| c.name == name) {
            return Some(Symbol::Class(c.id.clone()));
        }
        if let Some(f) = m.functions.iter().find(|f| f.parent.is_none() && f.class.is_none() && f.id.rsplit("::").next() == Some(name)) {
            return Some(Symbol::Function(f.id.clone()));
        }
        if m.enums.iter().any(|e| e.name == name) {
            return Some(Symbol::Enum { module: module.into(), name: name.into() });
        }
        if m.consts.iter().any(|c| c.name == name) {
            return Some(Symbol::Const { module: module.into(), name: name.into() });
        }
        if m.interfaces.iter().any(|i| i.name == name) || m.type_aliases.contains_key(name) {
            return Some(Symbol::Record { module: module.into(), name: name.into() });
        }
        let imp = m.imports.iter().find(|i| i.local == name)?;
        match self.resolve_module(module, &imp.source) {
            Some(target) => {
                if imp.imported == "*" || imp.imported == "default" {
                    // Namespace/default imports of program modules: not modelled yet.
                    None
                } else {
                    self.resolve_symbol_depth(&target, &imp.imported, depth + 1)
                }
            }
            None => Some(Symbol::External(match imp.imported.as_str() {
                "default" | "*" => package_alias(&imp.source).to_string(),
                n => format!("{}.{}", package_alias(&imp.source), n),
            })),
        }
    }

    /// Resolve a type annotation written in `module`.
    pub fn resolve_type(&self, module: &str, t: &TypeRef) -> Ty {
        self.resolve_type_depth(module, t, 0)
    }

    fn resolve_type_depth(&self, module: &str, t: &TypeRef, depth: u32) -> Ty {
        let t = t.unwrap_async();
        if let Some(el) = t.element() {
            return Ty::Array(Box::new(self.resolve_type_depth(module, el, depth)));
        }
        match t {
            TypeRef::Named(name, _) => match self.resolve_symbol(module, name) {
                Some(Symbol::Class(id)) => Ty::Class(id),
                // `type AssetExpressionBuilder = ExpressionBuilder<DB, 'asset'>` is the aliased type.
                Some(Symbol::Record { module, name }) => match self.module(&module).and_then(|m| m.type_aliases.get(&name)) {
                    Some(target @ TypeRef::Named(..)) if depth < 8 => match self.resolve_type_depth(&module, target, depth + 1) {
                        Ty::Unknown => Ty::Record { name, module },
                        ty => ty,
                    },
                    _ => Ty::Record { name, module },
                },
                Some(Symbol::External(canon)) => Ty::External(format!("{canon}#")),
                _ => Ty::Unknown,
            },
            TypeRef::Object(fields) => Ty::Object(fields.iter().map(|(k, v)| (k.clone(), self.resolve_type(module, v))).collect()),
            _ => Ty::Unknown,
        }
    }

    /// Declared type of `field` on a record/class type, resolved in its own module.
    pub fn field_type(&self, ty: &Ty, field: &str) -> Ty {
        match ty {
            Ty::Class(id) => {
                // Own fields first, then (program-internal) superclasses.
                let mut cur = id.clone();
                for _ in 0..16 {
                    let Some(c) = self.class(&cur) else { break };
                    let module = cur.split("::").next().unwrap_or("").to_string();
                    if let Some(t) = c.fields.get(field) {
                        return self.resolve_type(&module, t);
                    }
                    match c.extends.as_ref().and_then(|b| self.resolve_symbol(&module, b)) {
                        Some(Symbol::Class(base)) => cur = base,
                        _ => break,
                    }
                }
                Ty::Unknown
            }
            Ty::Record { name, module } => match self.record_fields(module, name).and_then(|f| f.get(field).cloned()) {
                Some(t) => self.resolve_type(module, &t),
                None => Ty::Unknown,
            },
            Ty::Object(fields) => fields.iter().find(|(k, _)| k == field).map(|(_, t)| t.clone()).unwrap_or(Ty::Unknown),
            Ty::External(canon) => Ty::External(format!("{canon}.{field}")),
            _ => Ty::Unknown,
        }
    }

    fn record_fields(&self, module: &str, name: &str) -> Option<BTreeMap<String, TypeRef>> {
        let m = self.module(module)?;
        if let Some(i) = m.interfaces.iter().find(|i| i.name == name) {
            return Some(i.fields.clone());
        }
        match m.type_aliases.get(name) {
            Some(TypeRef::Object(fields)) => Some(fields.iter().cloned().collect()),
            _ => None,
        }
    }

    /// Finite value domain of a record field typed as an enum or string-literal union.
    pub fn field_domain(&self, record: &str, field: &str) -> Option<BTreeSet<String>> {
        for m in self.program.modules.values() {
            let Some(fields) = self.record_fields(&m.path, record) else { continue };
            let t = fields.get(field)?;
            return match t {
                TypeRef::StringLiterals(v) => Some(v.iter().cloned().collect()),
                TypeRef::Named(n, _) => match self.resolve_symbol(&m.path, n) {
                    Some(Symbol::Enum { module, name }) => {
                        let e = self.module(&module)?.enums.iter().find(|e| e.name == name)?;
                        Some(e.members.values().cloned().collect())
                    }
                    Some(Symbol::Record { module, name }) => match self.module(&module)?.type_aliases.get(&name) {
                        Some(TypeRef::StringLiterals(v)) => Some(v.iter().cloned().collect()),
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            };
        }
        None
    }

    /// Find a method on a class or its (program-internal) superclasses.
    /// Returns `Err(external_base)` if the lookup reaches an external base class.
    pub fn lookup_method(&self, class_id: &str, method: &str) -> Result<Option<String>, String> {
        let mut cur = class_id.to_string();
        for _ in 0..16 {
            let Some(c) = self.class(&cur) else { return Ok(None) };
            let candidate = format!("{}.{}", c.id, method);
            if self.functions.contains_key(&candidate) {
                return Ok(Some(candidate));
            }
            let Some(base) = &c.extends else { return Ok(None) };
            let module = cur.split("::").next().unwrap_or("").to_string();
            match self.resolve_symbol(&module, base) {
                Some(Symbol::Class(id)) => cur = id,
                Some(Symbol::External(canon)) => return Err(format!("{canon}#")),
                _ => return Ok(None),
            }
        }
        Ok(None)
    }
}

/// Per-function typing environment (locals, parameters, captured variables).
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub module: String,
    pub this_class: Option<String>,
    pub vars: HashMap<String, Ty>,
}

impl<'p> Index<'p> {
    pub fn type_of(&self, env: &Env, e: &Expr) -> Ty {
        match e {
            Expr::This => env.this_class.clone().map(Ty::Class).unwrap_or(Ty::Unknown),
            Expr::Ident(n) => {
                if let Some(t) = env.vars.get(n) {
                    return t.clone();
                }
                match self.resolve_symbol(&env.module, n) {
                    Some(Symbol::Const { module, name }) => self.const_type(&module, &name),
                    Some(Symbol::Enum { module, name }) => Ty::EnumObject { module, name },
                    Some(Symbol::External(canon)) => Ty::External(canon),
                    Some(Symbol::Function(id)) => Ty::Function(id),
                    _ => Ty::Unknown,
                }
            }
            Expr::Member { object, property } => {
                let ot = self.type_of(env, object);
                match &ot {
                    Ty::Class(id) => match self.lookup_method(id, property) {
                        Ok(Some(_)) => Ty::Unknown,
                        Err(ext) => {
                            let ft = self.field_type(&ot, property);
                            if ft == Ty::Unknown {
                                Ty::External(format!("{ext}.{property}"))
                            } else {
                                ft
                            }
                        }
                        Ok(None) => self.field_type(&ot, property),
                    },
                    Ty::Array(_) if property == "length" => Ty::Unknown,
                    _ => self.field_type(&ot, property),
                }
            }
            Expr::Index { object, .. } => match self.type_of(env, object) {
                Ty::Array(el) => *el,
                _ => Ty::Unknown,
            },
            Expr::Await(inner) => self.type_of(env, inner),
            Expr::New { callee, .. } => match callee.as_ref() {
                Expr::Ident(n) => match self.resolve_symbol(&env.module, n) {
                    Some(Symbol::Class(id)) => Ty::Class(id),
                    Some(Symbol::External(canon)) => Ty::External(format!("{canon}#")),
                    _ => Ty::Unknown,
                },
                _ => Ty::Unknown,
            },
            Expr::Call(call) => match self.resolve_call(env, &call.callee) {
                Callee::Internal(id) => match self.function(&id).and_then(|f| f.return_type.as_ref()) {
                    Some(rt) => self.resolve_type(id.split("::").next().unwrap_or(""), rt),
                    None => Ty::Unknown,
                },
                Callee::External(canon) => Ty::External(canon),
                Callee::Unresolved(_) => Ty::Unknown,
            },
            Expr::Closure(id) => Ty::Function(id.clone()),
            Expr::Object(props) => Ty::Object(props.iter().map(|(k, v)| (k.clone(), self.type_of(env, v))).collect()),
            _ => Ty::Unknown,
        }
    }

    fn const_type(&self, module: &str, name: &str) -> Ty {
        let Some(c) = self.module(module).and_then(|m| m.consts.iter().find(|c| c.name == name)) else { return Ty::Unknown };
        if let Some(t) = &c.ty {
            let ty = self.resolve_type(module, t);
            if ty != Ty::Unknown {
                return ty;
            }
        }
        match &c.init {
            Some(init) => {
                let env = Env { module: module.into(), ..Default::default() };
                self.type_of(&env, init)
            }
            None => Ty::Unknown,
        }
    }

    pub fn resolve_call(&self, env: &Env, callee: &Expr) -> Callee {
        match callee {
            Expr::Member { object, property } => {
                let ot = self.type_of(env, object);
                match &ot {
                    Ty::Class(id) => match self.lookup_method(id, property) {
                        Ok(Some(m)) => Callee::Internal(m),
                        Ok(None) => match self.field_type(&ot, property) {
                            Ty::Function(f) => Callee::Internal(f),
                            _ => Callee::Unresolved(render(callee)),
                        },
                        Err(ext) => Callee::External(format!("{ext}.{property}()")),
                    },
                    Ty::External(canon) => Callee::External(format!("{canon}.{property}()")),
                    Ty::Array(_) => Callee::External(format!("Array#.{property}()")),
                    Ty::EnumObject { .. } | Ty::Record { .. } | Ty::Object(_) | Ty::Function(_) | Ty::Unknown => {
                        // Well-known globals: `JSON.stringify`, `Math.max`, `console.log` …
                        match object.as_ref() {
                            Expr::Ident(n) if matches!(ot, Ty::Unknown) => Callee::External(format!("{n}.{property}()")),
                            _ => Callee::Unresolved(render(callee)),
                        }
                    }
                }
            }
            Expr::Ident(n) => match self.type_of(env, &Expr::Ident(n.clone())) {
                Ty::Function(id) => Callee::Internal(id),
                Ty::External(canon) => Callee::External(format!("{canon}()")),
                _ if env.vars.contains_key(n) => Callee::Unresolved(n.clone()),
                // Unbound identifier: a global such as `fetch`.
                _ => Callee::External(format!("{n}()")),
            },
            other => Callee::Unresolved(render(other)),
        }
    }

    /// Evaluate an expression to a constant string, following consts and enum members.
    pub fn const_str(&self, env: &Env, e: &Expr) -> Option<String> {
        self.const_str_depth(env, e, 0)
    }

    fn const_str_depth(&self, env: &Env, e: &Expr, depth: u32) -> Option<String> {
        if depth > 8 {
            return None;
        }
        match e {
            Expr::Str(s) => Some(s.clone()),
            Expr::Num(n) => Some(n.to_string()),
            Expr::Bool(b) => Some(b.to_string()),
            Expr::Null => Some("null".into()),
            Expr::Template { quasis, exprs } => {
                let mut out = String::new();
                for (i, q) in quasis.iter().enumerate() {
                    out.push_str(q);
                    if let Some(x) = exprs.get(i) {
                        out.push_str(&self.const_str_depth(env, x, depth + 1)?);
                    }
                }
                Some(out)
            }
            Expr::Ident(n) if !env.vars.contains_key(n) => match self.resolve_symbol(&env.module, n)? {
                Symbol::Const { module, name } => {
                    let c = self.module(&module)?.consts.iter().find(|c| c.name == name)?;
                    let cenv = Env { module: module.clone(), ..Default::default() };
                    self.const_str_depth(&cenv, c.init.as_ref()?, depth + 1)
                }
                _ => None,
            },
            Expr::Member { object, property } => match self.type_of(env, object) {
                Ty::EnumObject { module, name } => {
                    let en = self.module(&module)?.enums.iter().find(|x| x.name == name)?;
                    en.members.get(property).cloned()
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// `Type.field` name for a member access on a typed record, if any.
    pub fn field_path(&self, env: &Env, object: &Expr, property: &str) -> Option<String> {
        match self.type_of(env, object) {
            Ty::Record { name, .. } => Some(format!("{name}.{property}")),
            Ty::Class(id) => {
                let c = self.class(&id)?;
                // Only data fields count, not injected collaborators or methods.
                let is_data =
                    c.fields.get(property).map(|t| !matches!(self.resolve_type(id.split("::").next().unwrap_or(""), t), Ty::Class(_) | Ty::External(_)));
                if is_data == Some(true) {
                    Some(format!("{}.{property}", c.name))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Compact source-like rendering of an IR expression (for evidence and opaque predicates).
pub fn render(e: &Expr) -> String {
    match e {
        Expr::Str(s) => format!("{s:?}"),
        Expr::Num(n) => n.to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Null => "null".into(),
        Expr::Undefined => "undefined".into(),
        Expr::Ident(n) => n.clone(),
        Expr::This => "this".into(),
        Expr::Member { object, property } => format!("{}.{property}", render(object)),
        Expr::Index { object, index } => format!("{}[{}]", render(object), render(index)),
        Expr::Call(c) => format!("{}({})", render(&c.callee), c.args.iter().map(render).collect::<Vec<_>>().join(", ")),
        Expr::New { callee, args, .. } => format!("new {}({})", render(callee), args.iter().map(render).collect::<Vec<_>>().join(", ")),
        Expr::Binary { op, left, right } => {
            let o = match op {
                ir::BinOp::Eq => "===",
                ir::BinOp::NotEq => "!==",
                ir::BinOp::Lt => "<",
                ir::BinOp::LtEq => "<=",
                ir::BinOp::Gt => ">",
                ir::BinOp::GtEq => ">=",
                ir::BinOp::Add => "+",
                ir::BinOp::Sub => "-",
                ir::BinOp::Mul => "*",
                ir::BinOp::Div => "/",
                ir::BinOp::Rem => "%",
                ir::BinOp::Other => "?",
            };
            format!("{} {o} {}", render(left), render(right))
        }
        Expr::Logical { op, left, right } => {
            let o = match op {
                ir::LogicOp::And => "&&",
                ir::LogicOp::Or => "||",
                ir::LogicOp::Coalesce => "??",
            };
            format!("{} {o} {}", render(left), render(right))
        }
        Expr::Not(x) => format!("!{}", render(x)),
        Expr::Template { .. } => "`…`".into(),
        Expr::Object(_) => "{…}".into(),
        Expr::Array(xs) => format!("[{}]", xs.iter().map(render).collect::<Vec<_>>().join(", ")),
        Expr::Closure(id) => format!("<fn {}>", id.rsplit("::").next().unwrap_or(id)),
        Expr::Await(x) => format!("await {}", render(x)),
        Expr::Conditional { test, then, otherwise } => format!("{} ? {} : {}", render(test), render(then), render(otherwise)),
        Expr::Assign { target, value, .. } => format!("{} = {}", render(target), render(value)),
        Expr::Jsx(j) => render_jsx(j),
        Expr::Opaque(t) => t.clone(),
    }
}

/// `<span.font-semibold.text-gray-900 onClick>{phrase}</span>`: classes as selectors,
/// handlers by name, other attributes by value.
fn render_jsx(j: &ir::Jsx) -> String {
    let classes: String = j.class().map(|c| c.split_whitespace().map(|x| format!(".{x}")).collect()).unwrap_or_default();
    let attrs: Vec<String> = j
        .attrs
        .iter()
        .filter(|(k, _)| k != "key" && !(matches!(k.as_str(), "className" | "class") && j.class().is_some()))
        .map(|(k, v)| match v {
            _ if k.starts_with("on") => k.clone(),
            Expr::Str(s) => format!("{k}={s:?}"),
            Expr::Bool(true) => k.clone(),
            other => format!("{k}={{{}}}", render(other)),
        })
        .collect();
    let open = format!("{}{classes}{}", j.tag, if attrs.is_empty() { String::new() } else { format!(" {}", attrs.join(" ")) });
    let children: String = j
        .children
        .iter()
        .map(|c| match c {
            Expr::Str(s) => s.clone(),
            Expr::Jsx(inner) => render_jsx(inner),
            other => format!("{{{}}}", render(other)),
        })
        .collect();
    if children.is_empty() && !j.tag.is_empty() {
        format!("<{open}/>")
    } else {
        format!("<{open}>{children}</{}>", j.tag)
    }
}

fn join(dir: &str, rel: &str) -> String {
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

/// Match `spec` against a tsconfig-style pattern; returns what `*` captured
/// (empty for an exact, star-less match).
fn match_alias<'s>(pattern: &str, spec: &'s str) -> Option<&'s str> {
    match pattern.split_once('*') {
        None => (pattern == spec).then_some(""),
        Some((pre, post)) => spec.strip_prefix(pre)?.strip_suffix(post).filter(|_| spec.len() >= pre.len() + post.len()),
    }
}

/// Packages that are drop-in builds of another package share its canonical name.
fn package_alias(spec: &str) -> &str {
    match spec {
        "lodash-es" => "lodash",
        other => other,
    }
}
