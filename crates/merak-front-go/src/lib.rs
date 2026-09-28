//! Go front end: tree-sitter-go → `merak_ir` structural IR.
//!
//! A Go package (the non-test `.go` files of one directory) is one IR module named by its
//! directory, so package members see each other without imports, as in Go. Structs (and
//! other named types with methods) are classes; the receiver is `this`. Error results are
//! Go's exceptions: returning a new error lowers to a throw, while handing on a callee's
//! error (`if err != nil { return err }`) is propagation, which the IR leaves implicit as
//! the TypeScript front end does. `go f()` is a spawned call.

use merak_ir as ir;
use std::collections::{BTreeMap, HashMap, HashSet};
use tree_sitter::{Node, Parser, Tree};

pub fn is_supported(path: &str) -> bool {
    path.ends_with(".go") && !path.ends_with("_test.go")
}

/// `go.mod` at any depth.
pub fn is_project_config(path: &str) -> bool {
    path.rsplit('/').next() == Some("go.mod")
}

/// The module (package directory) a Go file belongs to; `.` for the repository root.
pub fn package_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => ".".into(),
    }
}

/// Canonical name of an external Go import path: the host (`github.com`, `gorm.io`) and
/// major-version elements (`/v5`) dropped, other dots made `_`, since canonical names split
/// on dots. `github.com/go-chi/chi/v5` → `go-chi/chi`, `gorm.io/gorm` → `gorm`.
pub fn canonical_import(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let skip = usize::from(parts.len() > 1 && parts[0].contains('.'));
    let kept: Vec<&str> = parts[skip..].iter().filter(|p| !is_major_version(p)).copied().collect();
    let kept = if kept.is_empty() { vec![parts[0]] } else { kept };
    kept.join("/").replace('.', "_")
}

/// Imports of this repository's own packages are written `go:<directory>`.
const LOCAL: &str = "go:";

fn is_major_version(p: &str) -> bool {
    p.strip_prefix('v').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The package name an import is referred to by when it has no explicit name:
/// `github.com/redis/go-redis/v9` → `redis`, `gopkg.in/yaml.v3` → `yaml`.
fn default_import_name(path: &str) -> String {
    let last = path.split('/').rfind(|p| !is_major_version(p)).unwrap_or(path);
    let last = last.split('.').next().unwrap_or(last);
    let last = last.strip_prefix("go-").unwrap_or(last);
    let last = last.strip_suffix("-go").unwrap_or(last);
    last.replace('-', "_")
}

/// `module github.com/acme/shop` in a go.mod.
fn module_path(go_mod: &str) -> Option<String> {
    go_mod.lines().find_map(|l| l.trim().strip_prefix("module ")).map(|m| m.trim().trim_matches('"').to_string())
}

/// Lower the Go files of a source set (`path → text`) into a structural program.
pub fn lower_program(files: &BTreeMap<String, String>) -> ir::Program {
    let mut program = ir::Program::default();
    // go.mod module paths: imports under them are packages of this repository.
    let mut roots: Vec<(String, String)> = vec![];
    for (path, text) in files.iter().filter(|(p, _)| is_project_config(p)) {
        let Some(m) = module_path(text) else { continue };
        let dir = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        roots.push((m, dir));
    }
    roots.sort_by_key(|(c, _)| std::cmp::Reverse(c.len()));
    if !roots.is_empty() {
        program.aliases.push(ir::PathAlias { scope: String::new(), pattern: format!("{LOCAL}*"), targets: vec!["*".into()] });
    }

    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into()).expect("tree-sitter-go grammar loads");
    let mut packages: BTreeMap<String, Vec<(String, &str, Tree)>> = BTreeMap::new();
    for (path, text) in files.iter().filter(|(p, _)| is_supported(p)) {
        if let Some(tree) = parser.parse(text, None) {
            packages.entry(package_of(path)).or_default().push((path.clone(), text.as_str(), tree));
        }
    }
    let names: HashMap<String, String> = packages
        .iter()
        .filter_map(|(dir, fs)| {
            fs.iter().find_map(|(_, src, tree)| {
                let clause = children(tree.root_node()).into_iter().find(|n| n.kind() == "package_clause")?;
                let name = children(clause).into_iter().next()?;
                Some((dir.clone(), text(name, src).to_string()))
            })
        })
        .collect();
    let ctx = Ctx { roots, names };
    for (dir, fs) in &packages {
        program.modules.insert(dir.clone(), lower_package(dir, fs, &ctx));
    }
    program
}

struct Ctx {
    /// go.mod module path → its directory, longest first.
    roots: Vec<(String, String)>,
    /// Package directory → package name.
    names: HashMap<String, String>,
}

impl Ctx {
    /// The package directory an import path names, if it is in this repository.
    fn local_dir(&self, path: &str) -> Option<String> {
        self.roots.iter().find_map(|(root, dir)| {
            let rest = if path == root { "" } else { path.strip_prefix(&format!("{root}/"))? };
            Some(match (dir.is_empty(), rest.is_empty()) {
                (true, true) => ".".to_string(),
                (true, false) => rest.to_string(),
                (false, true) => dir.clone(),
                (false, false) => format!("{dir}/{rest}"),
            })
        })
    }

    /// An import's module specifier and the name it is referred to by when unnamed.
    fn import(&self, path: &str) -> (String, String) {
        match self.local_dir(path) {
            Some(dir) => {
                let name = self.names.get(&dir).cloned().unwrap_or_else(|| default_import_name(path));
                (format!("{LOCAL}{dir}"), name)
            }
            None => (canonical_import(path), default_import_name(path)),
        }
    }
}

fn lower_package(dir: &str, files: &[(String, &str, Tree)], ctx: &Ctx) -> ir::Module {
    let mut lw = Lowerer::new(dir);
    // Pass 1: the package's named types, so conversions (`OrderStatus(s)`) and typed
    // constant groups (enums) are recognised in any file.
    for (path, src, tree) in files {
        lw.set_file(path, src);
        lw.collect_types(tree.root_node());
    }
    for (path, src, tree) in files {
        lw.set_file(path, src);
        lw.file_imports.clear();
        let root = tree.root_node();
        if root.has_error() {
            lw.module.parse_errors.push(format!("{path}: syntax error"));
        }
        for n in children(root) {
            match n.kind() {
                "import_declaration" => lw.imports(n, ctx),
                "function_declaration" => lw.function_decl(n),
                "method_declaration" => lw.method_decl(n),
                "type_declaration" => lw.type_decl(n),
                "const_declaration" => lw.const_decl(n),
                "var_declaration" => lw.var_decl(n),
                _ => {}
            }
        }
    }
    lw.finish()
}

// ------------------------------------------------------------------ tree helpers

fn children(n: Node) -> Vec<Node> {
    let mut c = n.walk();
    n.named_children(&mut c).filter(|x| x.kind() != "comment").collect()
}

fn field<'t>(n: Node<'t>, name: &str) -> Option<Node<'t>> {
    n.child_by_field_name(name)
}

fn fields<'t>(n: Node<'t>, name: &str) -> Vec<Node<'t>> {
    let mut c = n.walk();
    n.children_by_field_name(name, &mut c).collect()
}

fn text<'s>(n: Node, src: &'s str) -> &'s str {
    &src[n.byte_range()]
}

fn exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Contents of a Go string literal (interpreted or raw).
fn unquote(lit: &str) -> String {
    if let Some(raw) = lit.strip_prefix('`') {
        return raw.strip_suffix('`').unwrap_or(raw).to_string();
    }
    let inner = lit.strip_prefix(['"', '\'']).and_then(|s| s.strip_suffix(['"', '\''])).unwrap_or(lit);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

const BUILTIN_TYPES: &[&str] = &[
    "string",
    "bool",
    "byte",
    "rune",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "error",
    "any",
];

/// Error values made on the spot: `errors.New`, `fmt.Errorf` (without wrapping a local error).
const ERROR_CONSTRUCTORS: &[(&str, &str)] =
    &[("errors", "New"), ("errors", "Errorf"), ("fmt", "Errorf"), ("pkg/errors", "New"), ("pkg/errors", "Errorf"), ("cockroachdb/errors", "New")];

enum ErrorRole {
    Nil,
    New(ir::Expr),
    HandOn,
    Call,
}

/// What a function returns, as far as lowering its `return`s needs.
#[derive(Clone, Default)]
struct Results {
    count: usize,
    /// The last result is an `error`.
    error: bool,
}

struct Lowerer<'s> {
    dir: String,
    file: String,
    src: &'s str,
    module: ir::Module,
    /// Import local name → canonical import, for the current file.
    file_imports: HashMap<String, String>,
    /// Named types declared in the package, with their underlying type node kind.
    types: HashMap<String, &'static str>,
    /// Typed constants, per named basic type: `OrderStatus` → [(StatusPending, "PENDING")].
    enum_members: BTreeMap<String, BTreeMap<String, String>>,
    /// Qualified-name stack of enclosing functions, for naming closures.
    scope: Vec<String>,
    closure_counter: BTreeMap<String, u32>,
    /// Variables of the function being lowered (parameters, locals, captured ones):
    /// returning one of them hands on an error rather than making a new one.
    locals: HashSet<String>,
    receiver: Option<String>,
    results: Results,
    /// Error variables and the call that last set them: `err` ← `h.svc.Cancel`.
    err_sources: HashMap<String, String>,
    /// String variables' current values in straight-line code (`query := "SELECT …"`, then
    /// `query = "UPDATE …"`): a call argument naming one is that string.
    strings: HashMap<String, ir::Expr>,
    /// Every variable assigned, in order: names assigned inside a branch lose their value after it.
    assigned: Vec<String>,
}

impl<'s> Lowerer<'s> {
    fn new(dir: &str) -> Self {
        Lowerer {
            dir: dir.to_string(),
            file: String::new(),
            src: "",
            module: ir::Module { path: dir.to_string(), ..Default::default() },
            file_imports: HashMap::new(),
            types: HashMap::new(),
            enum_members: BTreeMap::new(),
            scope: vec![],
            closure_counter: BTreeMap::new(),
            locals: HashSet::new(),
            receiver: None,
            results: Results::default(),
            err_sources: HashMap::new(),
            strings: HashMap::new(),
            assigned: vec![],
        }
    }

    fn set_file(&mut self, path: &str, src: &'s str) {
        self.file = path.to_string();
        self.src = src;
    }

    fn t(&self, n: Node) -> &'s str {
        text(n, self.src)
    }

    fn loc(&self, n: Node) -> ir::Loc {
        ir::Loc { file: self.file.clone(), line: n.start_position().row as u32 + 1 }
    }

    fn opaque(&self, n: Node) -> ir::Expr {
        let one_line: String = self.t(n).split_whitespace().collect::<Vec<_>>().join(" ");
        let short = match one_line.char_indices().nth(117) {
            Some((i, _)) if one_line.len() > 120 => format!("{}…", &one_line[..i]),
            _ => one_line,
        };
        ir::Expr::Opaque(short)
    }

    fn entity_id(&self, qualified: &str) -> String {
        format!("{}::{}", self.dir, qualified)
    }

    fn finish(mut self) -> ir::Module {
        for (name, members) in std::mem::take(&mut self.enum_members) {
            self.module.enums.push(ir::Enum { name, members });
        }
        // Methods declared in another file than their type.
        let mut methods: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for f in &self.module.functions {
            if let (Some(c), ir::FunctionKind::Method) = (&f.class, f.kind) {
                methods.entry(c.clone()).or_default().push(f.id.clone());
            }
        }
        for c in &mut self.module.classes {
            c.methods = methods.remove(&c.name).unwrap_or_default();
        }
        self.module
    }

    // ---------------------------------------------------------------- declarations

    fn collect_types(&mut self, root: Node) {
        for decl in children(root).into_iter().filter(|n| n.kind() == "type_declaration") {
            for spec in children(decl) {
                let (Some(name), Some(ty)) = (field(spec, "name"), field(spec, "type")) else { continue };
                let kind = match ty.kind() {
                    "struct_type" => "struct",
                    "interface_type" => "interface",
                    "type_identifier" if BUILTIN_TYPES.contains(&self.t(ty)) => "basic",
                    _ => "other",
                };
                self.types.insert(self.t(name).to_string(), kind);
            }
        }
    }

    fn imports(&mut self, decl: Node, ctx: &Ctx) {
        let mut specs = vec![];
        for n in children(decl) {
            match n.kind() {
                "import_spec" => specs.push(n),
                "import_spec_list" => specs.extend(children(n).into_iter().filter(|s| s.kind() == "import_spec")),
                _ => {}
            }
        }
        for spec in specs {
            let Some(path) = field(spec, "path") else { continue };
            let path = unquote(self.t(path));
            let (source, name) = ctx.import(&path);
            let local = match field(spec, "name").map(|n| self.t(n)) {
                Some("_" | ".") => continue,
                Some(n) => n.to_string(),
                None => name,
            };
            self.file_imports.insert(local.clone(), source.clone());
            if !self.module.imports.iter().any(|i| i.local == local) {
                self.module.imports.push(ir::Import { local, imported: "*".into(), source });
            }
        }
    }

    fn type_decl(&mut self, decl: Node) {
        for spec in children(decl) {
            let (Some(name), Some(ty)) = (field(spec, "name"), field(spec, "type")) else { continue };
            let name = self.t(name).to_string();
            match ty.kind() {
                "struct_type" => {
                    let mut fields_map = BTreeMap::new();
                    let mut tags = BTreeMap::new();
                    let mut extends = None;
                    for list in children(ty) {
                        for fd in children(list).into_iter().filter(|f| f.kind() == "field_declaration") {
                            let Some(t) = field(fd, "type") else { continue };
                            let tr = self.type_ref(t);
                            let names = fields(fd, "name");
                            if names.is_empty() {
                                // Embedded: its fields and methods are promoted.
                                if extends.is_none() {
                                    extends = tr.name().map(str::to_string);
                                }
                                if let Some(n) = tr.name() {
                                    fields_map.insert(n.rsplit('.').next().unwrap_or(n).to_string(), tr.clone());
                                }
                            }
                            let tag = field(fd, "tag").map(|t| unquote(self.t(t)));
                            for n in names {
                                fields_map.insert(self.t(n).to_string(), tr.clone());
                                if let Some(tag) = &tag {
                                    tags.insert(self.t(n).to_string(), tag.clone());
                                }
                            }
                        }
                    }
                    self.ensure_class(&name, spec);
                    let c = self.module.classes.iter_mut().find(|c| c.name == name).expect("class just ensured");
                    c.fields = fields_map;
                    c.field_tags = tags;
                    c.extends = extends;
                }
                "interface_type" => {
                    let mut methods = BTreeMap::new();
                    for m in children(ty) {
                        if matches!(m.kind(), "method_elem" | "method_spec") {
                            let Some(n) = field(m, "name") else { continue };
                            let result = field(m, "result").map(|r| self.first_result(r)).unwrap_or(ir::TypeRef::Unknown);
                            methods.insert(self.t(n).to_string(), ir::TypeRef::Named("func".into(), vec![result]));
                        }
                    }
                    self.module.interfaces.push(ir::Interface { name, fields: methods });
                }
                _ => {
                    let tr = self.type_ref(ty);
                    self.module.type_aliases.insert(name, tr);
                }
            }
        }
    }

    fn ensure_class(&mut self, name: &str, at: Node) {
        if self.module.classes.iter().any(|c| c.name == name) {
            return;
        }
        self.module.classes.push(ir::Class {
            id: self.entity_id(name),
            name: name.to_string(),
            extends: None,
            fields: BTreeMap::new(),
            methods: vec![],
            field_tags: BTreeMap::new(),
            exported: exported(name),
            loc: self.loc(at),
            decorators: vec![],
        });
    }

    /// `const ( StatusPending OrderStatus = "PENDING"; … )`, with Go's implicit repetition
    /// of the previous type and value, and `iota` values named after their constant.
    fn const_decl(&mut self, decl: Node) {
        let mut last_ty: Option<Node> = None;
        let mut last_values: Vec<Node> = vec![];
        for spec in children(decl).into_iter().filter(|s| s.kind() == "const_spec") {
            let names = fields(spec, "name");
            let ty = field(spec, "type");
            let values: Vec<Node> = field(spec, "value").map(children).unwrap_or_default();
            if ty.is_some() || !values.is_empty() {
                last_ty = ty;
                last_values = values.clone();
            }
            for (i, n) in names.into_iter().enumerate() {
                let name = self.t(n).to_string();
                if name == "_" {
                    continue;
                }
                let value = last_values.get(i).copied();
                let init = match value {
                    Some(v) if matches!(v.kind(), "interpreted_string_literal" | "raw_string_literal" | "int_literal" | "float_literal") => self.expr(v, None),
                    Some(v) if v.kind() == "identifier" && self.t(v) != "iota" => self.expr(v, None),
                    // `iota` and expressions of it: the constant stands for itself.
                    _ => ir::Expr::Str(name.clone()),
                };
                let tr = last_ty.map(|t| self.type_ref(t));
                if let Some(ir::TypeRef::Named(tn, _)) = &tr {
                    if self.types.get(tn) == Some(&"basic") {
                        if let ir::Expr::Str(v) | ir::Expr::Opaque(v) = &init {
                            self.enum_members.entry(tn.clone()).or_default().insert(name.clone(), v.clone());
                        } else if let ir::Expr::Num(v) = &init {
                            self.enum_members.entry(tn.clone()).or_default().insert(name.clone(), v.to_string());
                        }
                    }
                }
                self.module.consts.push(ir::Const { name: name.clone(), ty: tr, init: Some(init), exported: exported(&name), loc: self.loc(n) });
            }
        }
    }

    fn var_decl(&mut self, decl: Node) {
        for spec in var_specs(decl) {
            let ty = field(spec, "type").map(|t| self.type_ref(t));
            let values: Vec<Node> = field(spec, "value").map(children).unwrap_or_default();
            for (i, n) in fields(spec, "name").into_iter().enumerate() {
                let name = self.t(n).to_string();
                let init = values.get(i).map(|v| self.expr(*v, Some(name.clone())));
                self.module.consts.push(ir::Const { name: name.clone(), ty: ty.clone(), init, exported: exported(&name), loc: self.loc(n) });
            }
        }
    }

    fn function_decl(&mut self, n: Node) {
        let Some(name) = field(n, "name") else { return };
        let name = self.t(name).to_string();
        // Several `init` functions may share a package.
        let qualified = if name == "init" { self.closure_name_at(None, "init") } else { name.clone() };
        self.function(n, &qualified, None, ir::FunctionKind::Function, None, None, exported(&name));
    }

    fn method_decl(&mut self, n: Node) {
        let (Some(name), Some(recv)) = (field(n, "name"), field(n, "receiver")) else { return };
        let Some(param) = children(recv).into_iter().find(|p| p.kind() == "parameter_declaration") else { return };
        let Some(ty) = field(param, "type") else { return };
        let Some(class) = self.type_ref(ty).name().map(str::to_string) else { return };
        let receiver = field(param, "name").map(|r| self.t(r).to_string()).filter(|r| r != "_");
        self.ensure_class(&class, n);
        let name = self.t(name).to_string();
        let qualified = format!("{class}.{name}");
        self.function(n, &qualified, Some(class), ir::FunctionKind::Method, None, receiver, exported(&name));
    }

    /// Lower a function, method or function literal; returns its entity id.
    #[allow(clippy::too_many_arguments)]
    fn function(
        &mut self,
        n: Node,
        qualified: &str,
        class: Option<String>,
        kind: ir::FunctionKind,
        parent: Option<String>,
        receiver: Option<String>,
        is_exported: bool,
    ) -> String {
        let id = self.entity_id(qualified);
        let params = field(n, "parameters").map(|p| self.params(p)).unwrap_or_default();
        let (return_type, results, named_results) = match field(n, "result") {
            Some(r) => self.results_of(r),
            None => (None, Results::default(), vec![]),
        };
        let saved = (std::mem::take(&mut self.locals), std::mem::replace(&mut self.receiver, receiver.clone()), std::mem::replace(&mut self.results, results));
        let saved_strings = std::mem::take(&mut self.strings);
        // A closure sees its enclosing function's variables.
        if parent.is_some() {
            self.locals = saved.0.clone();
            if self.receiver.is_none() {
                self.receiver = saved.1.clone();
            }
        }
        self.locals.extend(params.iter().map(|p| p.name.clone()));
        self.locals.extend(receiver);
        self.locals.extend(named_results);
        self.scope.push(qualified.to_string());
        let body = field(n, "body").map(|b| self.block(b)).unwrap_or_default();
        self.scope.pop();
        (self.locals, self.receiver, self.results) = saved;
        self.strings = saved_strings;
        let body_hash = ir::body_hash(&body);
        self.module.functions.push(ir::Function {
            id: id.clone(),
            name: qualified.rsplit('.').next().unwrap_or(qualified).to_string(),
            kind,
            class,
            parent,
            params,
            return_type,
            body,
            is_async: false,
            exported: is_exported,
            loc: self.loc(n),
            end_line: n.end_position().row as u32 + 1,
            body_hash,
            decorators: vec![],
        });
        id
    }

    fn params(&self, list: Node) -> Vec<ir::Param> {
        let mut out = vec![];
        for p in children(list) {
            if !matches!(p.kind(), "parameter_declaration" | "variadic_parameter_declaration") {
                continue;
            }
            let ty = field(p, "type").map(|t| self.type_ref(t));
            let ty = if p.kind() == "variadic_parameter_declaration" { ty.map(|t| ir::TypeRef::Array(Box::new(t))) } else { ty };
            let names = fields(p, "name");
            if names.is_empty() {
                out.push(ir::Param { name: "_".into(), ty: ty.clone() });
            }
            for n in names {
                out.push(ir::Param { name: self.t(n).to_string(), ty: ty.clone() });
            }
        }
        out
    }

    /// The declared result: the interesting type (the first, when the last is an `error`),
    /// how `return`s are shaped, and the names of named results.
    fn results_of(&self, r: Node) -> (Option<ir::TypeRef>, Results, Vec<String>) {
        if r.kind() != "parameter_list" {
            let t = self.type_ref(r);
            let error = matches!(&t, ir::TypeRef::Primitive(p) if p == "error");
            return (Some(t), Results { count: 1, error }, vec![]);
        }
        let mut types = vec![];
        let mut names = vec![];
        for p in children(r).into_iter().filter(|p| p.kind() == "parameter_declaration") {
            let t = field(p, "type").map(|t| self.type_ref(t)).unwrap_or(ir::TypeRef::Unknown);
            let ns = fields(p, "name");
            for n in &ns {
                names.push(self.t(*n).to_string());
            }
            for _ in 0..ns.len().max(1) {
                types.push(t.clone());
            }
        }
        let error = matches!(types.last(), Some(ir::TypeRef::Primitive(p)) if p == "error");
        (types.first().cloned(), Results { count: types.len(), error }, names)
    }

    fn first_result(&self, r: Node) -> ir::TypeRef {
        self.results_of(r).0.unwrap_or(ir::TypeRef::Unknown)
    }

    fn type_ref(&self, t: Node) -> ir::TypeRef {
        match t.kind() {
            "pointer_type" | "parenthesized_type" => children(t).first().map(|x| self.type_ref(*x)).unwrap_or(ir::TypeRef::Unknown),
            "slice_type" | "array_type" => field(t, "element").map(|e| ir::TypeRef::Array(Box::new(self.type_ref(e)))).unwrap_or(ir::TypeRef::Unknown),
            "map_type" => ir::TypeRef::Named("map".into(), [field(t, "key"), field(t, "value")].into_iter().flatten().map(|x| self.type_ref(x)).collect()),
            "type_identifier" => {
                let n = self.t(t);
                match n {
                    "bool" => ir::TypeRef::Primitive("boolean".into()),
                    _ if BUILTIN_TYPES.contains(&n) => ir::TypeRef::Primitive(n.into()),
                    _ => ir::TypeRef::Named(n.into(), vec![]),
                }
            }
            "qualified_type" => ir::TypeRef::Named(self.t(t).split_whitespace().collect(), vec![]),
            "generic_type" => {
                let base = field(t, "type").map(|b| self.type_ref(b)).unwrap_or(ir::TypeRef::Unknown);
                let args = field(t, "type_arguments").map(|a| children(a).into_iter().map(|x| self.type_ref(x)).collect()).unwrap_or_default();
                match base {
                    ir::TypeRef::Named(n, _) => ir::TypeRef::Named(n, args),
                    other => other,
                }
            }
            "type_elem" => children(t).first().map(|x| self.type_ref(*x)).unwrap_or(ir::TypeRef::Unknown),
            "interface_type" if children(t).is_empty() => ir::TypeRef::Primitive("any".into()),
            "function_type" => ir::TypeRef::Named("func".into(), vec![]),
            "channel_type" => ir::TypeRef::Named("chan".into(), vec![]),
            _ => ir::TypeRef::Unknown,
        }
    }

    // ---------------------------------------------------------------- closures

    fn closure_name_at(&mut self, hint: Option<String>, parent: &str) -> String {
        let base = match hint {
            Some(h) if parent.is_empty() => h,
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

    fn func_literal(&mut self, n: Node, hint: Option<String>) -> ir::Expr {
        let parent_q = self.scope.last().cloned().unwrap_or_else(|| "<package>".into());
        let q = self.closure_name_at(hint, &parent_q);
        let parent = self.scope.last().map(|p| self.entity_id(p));
        let kind = if parent.is_some() { ir::FunctionKind::Closure } else { ir::FunctionKind::Function };
        ir::Expr::Closure(self.function(n, &q, None, kind, parent, None, false))
    }

    // ---------------------------------------------------------------- statements

    fn block(&mut self, n: Node) -> Vec<ir::Stmt> {
        let mut out = vec![];
        for c in children(n) {
            if c.kind() == "statement_list" {
                out.extend(self.block(c));
            } else {
                out.extend(self.stmt(c));
            }
        }
        out
    }

    fn track(&mut self, name: &str, value: Option<ir::Expr>) {
        self.assigned.push(name.to_string());
        match value {
            Some(v @ (ir::Expr::Str(_) | ir::Expr::Template { .. })) => self.strings.insert(name.to_string(), v),
            _ => self.strings.remove(name),
        };
    }

    fn stmt(&mut self, n: Node) -> Vec<ir::Stmt> {
        let mark = self.assigned.len();
        let out = self.stmt_inner(n);
        let branches =
            ["if_statement", "for_statement", "expression_switch_statement", "type_switch_statement", "select_statement", "block", "labeled_statement"];
        if branches.contains(&n.kind()) {
            for name in &self.assigned[mark..] {
                self.strings.remove(name);
            }
        }
        out
    }

    fn stmt_inner(&mut self, n: Node) -> Vec<ir::Stmt> {
        let loc = self.loc(n);
        match n.kind() {
            "expression_statement" => {
                let Some(e) = children(n).into_iter().next() else { return vec![] };
                // `panic(x)` is Go's throw.
                if e.kind() == "call_expression" && field(e, "function").is_some_and(|f| self.t(f) == "panic") {
                    let arg = field(e, "arguments").and_then(|a| children(a).into_iter().next());
                    let v = arg.map(|a| self.expr(a, None)).unwrap_or(ir::Expr::Null);
                    return vec![ir::Stmt::Throw(v, loc)];
                }
                vec![ir::Stmt::Expr(self.expr(e, None), loc)]
            }
            "short_var_declaration" => {
                let left = field(n, "left").map(children).unwrap_or_default();
                let right = field(n, "right").map(children).unwrap_or_default();
                self.declare(&left, &right, None, loc)
            }
            "var_declaration" => {
                let mut out = vec![];
                for spec in var_specs(n) {
                    let names = fields(spec, "name");
                    let values: Vec<Node> = field(spec, "value").map(children).unwrap_or_default();
                    let ty = field(spec, "type").map(|t| self.type_ref(t));
                    out.extend(self.declare(&names, &values, ty, self.loc(spec)));
                }
                out
            }
            "const_declaration" => {
                let mut out = vec![];
                for spec in children(n).into_iter().filter(|s| s.kind() == "const_spec") {
                    let names = fields(spec, "name");
                    let values: Vec<Node> = field(spec, "value").map(children).unwrap_or_default();
                    let ty = field(spec, "type").map(|t| self.type_ref(t));
                    out.extend(self.declare(&names, &values, ty, self.loc(spec)));
                }
                out
            }
            "assignment_statement" => {
                let left = field(n, "left").map(children).unwrap_or_default();
                let right = field(n, "right").map(children).unwrap_or_default();
                let op = field(n, "operator").map(|o| self.t(o)).unwrap_or("=");
                self.assign(&left, &right, op, loc)
            }
            "inc_statement" | "dec_statement" => {
                let Some(target) = children(n).into_iter().next() else { return vec![] };
                let t = self.expr(target, None);
                let op = if n.kind() == "inc_statement" { ir::BinOp::Add } else { ir::BinOp::Sub };
                let value = ir::Expr::Binary { op, left: Box::new(t.clone()), right: Box::new(ir::Expr::Num(1.0)) };
                vec![ir::Stmt::Expr(ir::Expr::Assign { target: Box::new(t), value: Box::new(value), loc: loc.clone() }, loc)]
            }
            "send_statement" => {
                let ch = field(n, "channel").map(|c| self.expr(c, None)).unwrap_or(ir::Expr::Undefined);
                let v = field(n, "value").map(|c| self.expr(c, None)).unwrap_or(ir::Expr::Undefined);
                let call = ir::Call { callee: ir::Expr::Ident("chan<-".into()), args: vec![ch, v], loc: loc.clone() };
                vec![ir::Stmt::Expr(ir::Expr::Call(Box::new(call)), loc)]
            }
            "return_statement" => self.ret(n, loc),
            "if_statement" => self.if_stmt(n),
            "for_statement" => self.for_stmt(n, loc),
            "expression_switch_statement" | "type_switch_statement" | "select_statement" => self.switch(n, loc),
            "go_statement" | "defer_statement" => {
                let Some(e) = children(n).into_iter().next() else { return vec![] };
                let hint = if n.kind() == "go_statement" { "go" } else { "defer" };
                let call = self.expr(e, Some(hint.into()));
                let e = if n.kind() == "go_statement" { ir::Expr::Spawn(Box::new(call)) } else { call };
                vec![ir::Stmt::Expr(e, loc)]
            }
            "labeled_statement" => children(n).into_iter().skip(1).flat_map(|s| self.stmt(s)).collect(),
            "break_statement" | "continue_statement" | "goto_statement" => vec![ir::Stmt::Jump(loc)],
            "block" => vec![ir::Stmt::Block(self.block(n))],
            "fallthrough_statement" | "empty_statement" | "type_declaration" => vec![],
            _ => vec![ir::Stmt::Expr(self.opaque(n), loc)],
        }
    }

    /// `a, b := f()` / `var x T = v`: one binding per name; a tuple result binds the first.
    /// Remember the call that sets each name, for naming error checks on them.
    fn note_sources(&mut self, names: &[Node], values: &[Node]) {
        let source = |lw: &Self, v: &Node| -> Option<String> {
            let f = field(*v, "function").filter(|_| v.kind() == "call_expression")?;
            let text: String = lw.t(f).split_whitespace().collect();
            Some(match lw.receiver.as_deref().and_then(|r| text.strip_prefix(&format!("{r}."))) {
                Some(rest) => rest.to_string(),
                None => text,
            })
        };
        for (i, n) in names.iter().enumerate() {
            let v = if values.len() == names.len() { values.get(i) } else { values.first() };
            let name = self.t(*n).to_string();
            match v.and_then(|v| source(self, v)) {
                Some(src) => self.err_sources.insert(name, src),
                None => self.err_sources.remove(&name),
            };
        }
    }

    /// `err != nil` on an error set by `h.svc.Cancel(…)` reads `err(svc.Cancel) != nil`, so two
    /// checks of different calls are different conditions.
    fn name_errors(&self, e: ir::Expr) -> ir::Expr {
        match e {
            ir::Expr::Binary { op: op @ (ir::BinOp::Eq | ir::BinOp::NotEq), left, right } if *right == ir::Expr::Null => {
                let left = match *left {
                    ir::Expr::Ident(x) if self.err_sources.contains_key(&x) => ir::Expr::Ident(format!("{x}({})", self.err_sources[&x])),
                    other => other,
                };
                ir::Expr::Binary { op, left: Box::new(left), right }
            }
            // `if !exists`: a result of `store.FeedExists(…)`.
            ir::Expr::Ident(x) if self.err_sources.contains_key(&x) => ir::Expr::Ident(format!("{x}({})", self.err_sources[&x])),
            ir::Expr::Logical { op, left, right } => {
                ir::Expr::Logical { op, left: Box::new(self.name_errors(*left)), right: Box::new(self.name_errors(*right)) }
            }
            ir::Expr::Not(x) => ir::Expr::Not(Box::new(self.name_errors(*x))),
            other => other,
        }
    }

    fn declare(&mut self, names: &[Node], values: &[Node], ty: Option<ir::TypeRef>, loc: ir::Loc) -> Vec<ir::Stmt> {
        self.note_sources(names, values);
        let names: Vec<String> = names.iter().map(|n| self.t(*n).to_string()).collect();
        let mut out = vec![];
        if values.len() == names.len() {
            for (name, v) in names.iter().zip(values) {
                let init = self.expr(*v, Some(name.clone()));
                out.push(self.bind(name, ty.clone(), Some(init), &loc));
            }
        } else {
            let init = values.first().map(|v| self.expr(*v, names.first().cloned()));
            for (i, name) in names.iter().enumerate() {
                let t = if i == 0 { ty.clone() } else { None };
                out.push(self.bind(name, t, if i == 0 { init.clone() } else { None }, &loc));
            }
        }
        out.retain(|s| !matches!(s, ir::Stmt::Block(b) if b.is_empty()));
        out
    }

    fn bind(&mut self, name: &str, ty: Option<ir::TypeRef>, init: Option<ir::Expr>, loc: &ir::Loc) -> ir::Stmt {
        if name == "_" {
            return match init {
                Some(e) => ir::Stmt::Expr(e, loc.clone()),
                None => ir::Stmt::Block(vec![]),
            };
        }
        self.locals.insert(name.to_string());
        self.track(name, init.clone());
        ir::Stmt::Let { name: name.to_string(), ty, init, loc: loc.clone() }
    }

    fn assign(&mut self, left: &[Node], right: &[Node], op: &str, loc: ir::Loc) -> Vec<ir::Stmt> {
        if op == "=" {
            self.note_sources(left, right);
        }
        let binop = match op.trim_end_matches('=') {
            "+" => Some(ir::BinOp::Add),
            "-" => Some(ir::BinOp::Sub),
            "*" => Some(ir::BinOp::Mul),
            "/" => Some(ir::BinOp::Div),
            "%" => Some(ir::BinOp::Rem),
            "" => None,
            _ => Some(ir::BinOp::Other),
        };
        let pairs: Vec<(Node, Option<Node>)> = if left.len() == right.len() {
            left.iter().copied().zip(right.iter().copied().map(Some)).collect()
        } else {
            // `a, err = f()`: the first target takes the call, the rest nothing to model.
            left.iter().enumerate().map(|(i, l)| (*l, if i == 0 { right.first().copied() } else { None })).collect()
        };
        let mut out = vec![];
        for (l, r) in pairs {
            let Some(r) = r else { continue };
            let hint = (l.kind() == "identifier").then(|| self.t(l).to_string());
            let value = self.expr(r, hint);
            if self.t(l) == "_" {
                out.push(ir::Stmt::Expr(value, loc.clone()));
                continue;
            }
            let target = self.expr(l, None);
            if let ir::Expr::Ident(name) = &target {
                let now = match (binop, self.strings.get(name)) {
                    (None, _) => Some(value.clone()),
                    // `query += " AND hidden = false"`
                    (Some(ir::BinOp::Add), Some(prev)) if is_stringish(&value) => Some(concat(prev.clone(), value.clone())),
                    _ => None,
                };
                self.track(&name.clone(), now);
            }
            let value = match binop {
                Some(op) => ir::Expr::Binary { op, left: Box::new(target.clone()), right: Box::new(value) },
                None => value,
            };
            out.push(ir::Stmt::Expr(ir::Expr::Assign { target: Box::new(target), value: Box::new(value), loc: loc.clone() }, loc.clone()));
        }
        out
    }

    /// Error results are Go's exceptions: returning a new error throws it; returning a
    /// callee's error (`return err`, `return nil, fmt.Errorf("…: %w", err)`) hands it on,
    /// which the IR leaves implicit, as in languages where exceptions propagate.
    fn ret(&mut self, n: Node, loc: ir::Loc) -> Vec<ir::Stmt> {
        let values: Vec<Node> = children(n).into_iter().flat_map(|c| if c.kind() == "expression_list" { children(c) } else { vec![c] }).collect();
        let mut exprs: Vec<ir::Expr> = values.iter().map(|v| self.expr(*v, None)).collect();
        let pack = |mut xs: Vec<ir::Expr>| match xs.len() {
            0 => None,
            1 => xs.pop(),
            _ => Some(ir::Expr::Array(xs)),
        };
        if !self.results.error || exprs.is_empty() {
            return vec![ir::Stmt::Return(pack(exprs), loc)];
        }
        // `return f()` handing on all of a call's results.
        if exprs.len() == 1 && self.results.count > 1 {
            return vec![ir::Stmt::Return(pack(exprs), loc)];
        }
        let err = exprs.pop().expect("non-empty");
        match self.error_role(&err) {
            ErrorRole::Nil | ErrorRole::HandOn => vec![ir::Stmt::Return(pack(exprs), loc)],
            ErrorRole::New(thrown) => vec![ir::Stmt::Throw(thrown, loc)],
            // `return s.repo.Save(ctx, o)`: the call runs; its error, if any, is handed on.
            ErrorRole::Call => vec![ir::Stmt::Expr(err, loc.clone()), ir::Stmt::Return(pack(exprs), loc)],
        }
    }

    /// What an error-typed result is: none, a new error (a constructor such as `errors.New`
    /// or `fmt.Errorf` without `%w`, a sentinel `ErrNotFound`, an error struct, an `…Error(…)`
    /// helper), an error handed on (a local, or a wrap of one), or a call whose error it is.
    fn error_role(&self, e: &ir::Expr) -> ErrorRole {
        match e {
            ir::Expr::Null => ErrorRole::Nil,
            ir::Expr::Ident(n) if self.locals.contains(n) => ErrorRole::HandOn,
            ir::Expr::Ident(_) | ir::Expr::New { .. } => ErrorRole::New(e.clone()),
            ir::Expr::Member { object, .. } if matches!(object.as_ref(), ir::Expr::Ident(p) if self.file_imports.contains_key(p)) => ErrorRole::New(e.clone()),
            ir::Expr::Call(c) => {
                let (pkg, name) = match &c.callee {
                    ir::Expr::Member { object, property } => match object.as_ref() {
                        ir::Expr::Ident(p) => (self.file_imports.get(p).cloned(), property.as_str()),
                        _ => (None, property.as_str()),
                    },
                    ir::Expr::Ident(n) => (None, n.as_str()),
                    _ => return ErrorRole::Call,
                };
                // A wrap of a local error (`fmt.Errorf("…: %v", err)`, `errors.Wrap(err, …)`) hands it on.
                let mut wraps_error = false;
                c.args.iter().for_each(|a| walk_idents(a, &mut |n| wraps_error |= self.locals.contains(n) && n.to_ascii_lowercase().contains("err")));
                if wraps_error {
                    return ErrorRole::HandOn;
                }
                if pkg.as_deref().is_some_and(|p| ERROR_CONSTRUCTORS.iter().any(|(cp, cn)| *cp == p && *cn == name)) {
                    // Made on the spot: its arguments, like `throw new X(…)`.
                    return ErrorRole::New(ir::Expr::New { callee: Box::new(c.callee.clone()), args: c.args.clone(), loc: c.loc.clone() });
                }
                if name.contains("Err") || name.starts_with("err") {
                    return ErrorRole::New(e.clone());
                }
                ErrorRole::Call
            }
            _ => ErrorRole::HandOn,
        }
    }

    fn if_stmt(&mut self, n: Node) -> Vec<ir::Stmt> {
        let mut out = field(n, "initializer").map(|i| self.stmt(i)).unwrap_or_default();
        let loc = self.loc(n);
        let test = field(n, "condition").map(|c| self.expr(c, None)).unwrap_or(ir::Expr::Bool(true));
        let then = field(n, "consequence").map(|c| self.block(c)).unwrap_or_default();
        let otherwise = match field(n, "alternative") {
            Some(a) if a.kind() == "if_statement" => self.if_stmt(a),
            Some(a) => self.block(a),
            None => vec![],
        };
        // `if err != nil { …; return err }`: only the calls before the hand-on remain,
        // run when the error is set.
        if let (Some(var), true) = (self.error_check(&test), otherwise.is_empty()) {
            if let Some(ir::Stmt::Return(..) | ir::Stmt::Throw(..)) = then.last() {
                // Any return that mentions the error hands it on: `%w`, `%v`, a custom wrap.
                if self.propagated(n).is_some_and(|v| v == var) {
                    let rest: Vec<ir::Stmt> = then[..then.len() - 1].to_vec();
                    if !rest.is_empty() {
                        out.push(ir::Stmt::If { test: self.name_errors(test), then: rest, otherwise: vec![], loc });
                    }
                    return out;
                }
            }
        }
        out.push(ir::Stmt::If { test: self.name_errors(test), then, otherwise, loc });
        out
    }

    /// `x != nil` for a local `x`.
    fn error_check(&self, test: &ir::Expr) -> Option<String> {
        match test {
            ir::Expr::Binary { op: ir::BinOp::NotEq, left, right } => match (left.as_ref(), right.as_ref()) {
                (ir::Expr::Ident(x), ir::Expr::Null) if self.locals.contains(x) => Some(x.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The local error a trailing `return …, err` (or a wrap of it) hands on, from the
    /// source of the `if`'s consequence.
    fn propagated(&self, if_node: Node) -> Option<String> {
        let block = field(if_node, "consequence")?;
        let stmts: Vec<Node> = children(block).into_iter().flat_map(|c| if c.kind() == "statement_list" { children(c) } else { vec![c] }).collect();
        let cond = field(if_node, "condition")?;
        let var = field(cond, "left").map(|l| self.t(l).to_string())?;
        self.hands_on(&stmts, &var).then_some(var)
    }

    /// The statements end in a `return` whose error mentions `var`.
    fn hands_on(&self, stmts: &[Node], var: &str) -> bool {
        let Some(ret) = stmts.last().filter(|r| r.kind() == "return_statement") else { return false };
        let values: Vec<Node> = children(*ret).into_iter().flat_map(|c| if c.kind() == "expression_list" { children(c) } else { vec![c] }).collect();
        let Some(last) = values.last() else { return false };
        let mut idents = vec![];
        collect_identifiers(*last, self.src, &mut idents);
        idents.iter().any(|i| i == var)
    }

    fn for_stmt(&mut self, n: Node, loc: ir::Loc) -> Vec<ir::Stmt> {
        let mut out = vec![];
        let (mut binding, mut iter) = (None, None);
        for c in children(n) {
            match c.kind() {
                "range_clause" => {
                    let names: Vec<String> = field(c, "left").map(children).unwrap_or_default().iter().map(|x| self.t(*x).to_string()).collect();
                    self.locals.extend(names.iter().cloned());
                    // `for _, o := range orders` binds the element; `for i := range xs` an index.
                    binding = names.get(1).filter(|x| *x != "_").cloned();
                    iter = field(c, "right").map(|r| self.expr(r, None));
                }
                "for_clause" => {
                    if let Some(init) = field(c, "initializer") {
                        out.extend(self.stmt(init));
                    }
                }
                _ => {}
            }
        }
        let body = field(n, "body").map(|b| self.block(b)).unwrap_or_default();
        out.push(ir::Stmt::Loop { binding, iter, body, loc });
        out
    }

    /// `switch v { case a, b: … default: … }` as an `if` chain on `v == a || v == b`;
    /// type switches and `select` cases keep their case text as the condition.
    fn switch(&mut self, n: Node, loc: ir::Loc) -> Vec<ir::Stmt> {
        let mut out = field(n, "initializer").map(|i| self.stmt(i)).unwrap_or_default();
        let subject = if n.kind() == "expression_switch_statement" { field(n, "value").map(|v| self.expr(v, None)) } else { None };
        if n.kind() == "type_switch_statement" {
            if let Some(alias) = field(n, "alias") {
                for a in children(alias) {
                    self.locals.insert(self.t(a).to_string());
                }
                if alias.kind() == "identifier" {
                    self.locals.insert(self.t(alias).to_string());
                }
            }
        }
        let mut cases: Vec<(Option<ir::Expr>, Vec<ir::Stmt>, ir::Loc)> = vec![];
        for c in children(n) {
            let is_default = c.kind() == "default_case";
            if !matches!(c.kind(), "expression_case" | "type_case" | "communication_case" | "default_case") {
                continue;
            }
            let value_nodes = fields(c, "value");
            let values: Vec<Node> = value_nodes.iter().flat_map(|v| if v.kind() == "expression_list" { children(*v) } else { vec![*v] }).collect();
            let test = if is_default {
                None
            } else if c.kind() == "expression_case" {
                let tests: Vec<ir::Expr> = values
                    .iter()
                    .map(|v| {
                        let v = self.expr(*v, None);
                        match &subject {
                            Some(s) => ir::Expr::Binary { op: ir::BinOp::Eq, left: Box::new(s.clone()), right: Box::new(v) },
                            None => v,
                        }
                    })
                    .collect();
                tests.into_iter().reduce(|a, b| ir::Expr::Logical { op: ir::LogicOp::Or, left: Box::new(a), right: Box::new(b) })
            } else {
                let head = self.t(c).split(':').next().unwrap_or("").trim().to_string();
                Some(ir::Expr::Opaque(head))
            };
            // `case err != nil: return nil, err` hands the error on, like the `if` form.
            if let Some(var) = test.as_ref().and_then(|t| self.error_check(t)) {
                let stmts: Vec<Node> = children(c)
                    .into_iter()
                    .filter(|s| !value_nodes.iter().chain(&values).any(|v| v.id() == s.id()))
                    .flat_map(|s| if s.kind() == "statement_list" { children(s) } else { vec![s] })
                    .collect();
                if stmts.len() == 1 && self.hands_on(&stmts, &var) {
                    continue;
                }
            }
            let value_ids: HashSet<usize> = value_nodes.iter().chain(&values).map(|v| v.id()).collect();
            let mut body = vec![];
            for s in children(c) {
                if value_ids.contains(&s.id()) || (c.kind() == "communication_case" && field(c, "communication").is_some_and(|x| x.id() == s.id())) {
                    continue;
                }
                if c.kind() == "type_case"
                    && matches!(s.kind(), "type_identifier" | "pointer_type" | "qualified_type" | "slice_type" | "map_type" | "generic_type")
                {
                    continue;
                }
                if s.kind() == "statement_list" {
                    body.extend(self.block(s));
                } else {
                    body.extend(self.stmt(s));
                }
            }
            cases.push((test, body, self.loc(c)));
        }
        let default = cases.iter().position(|(t, _, _)| t.is_none()).map(|i| cases.remove(i).1).unwrap_or_default();
        let mut chain = default;
        for (test, body, cloc) in cases.into_iter().rev() {
            let test = test.unwrap_or(ir::Expr::Bool(true));
            chain = vec![ir::Stmt::If { test, then: body, otherwise: chain, loc: cloc }];
        }
        if chain.is_empty() {
            return out;
        }
        let _ = loc;
        out.extend(chain);
        out
    }

    // ---------------------------------------------------------------- expressions

    /// Lower an expression; `hint` names a function literal found directly in it.
    fn expr(&mut self, n: Node, hint: Option<String>) -> ir::Expr {
        match n.kind() {
            "identifier" | "field_identifier" | "package_identifier" | "type_identifier" => {
                let name = self.t(n);
                if self.receiver.as_deref() == Some(name) {
                    ir::Expr::This
                } else {
                    ir::Expr::Ident(name.to_string())
                }
            }
            "nil" => ir::Expr::Null,
            "true" => ir::Expr::Bool(true),
            "false" => ir::Expr::Bool(false),
            "iota" => ir::Expr::Opaque("iota".into()),
            "int_literal" | "float_literal" => {
                let t = self.t(n).replace('_', "");
                match t.parse::<f64>() {
                    Ok(v) => ir::Expr::Num(v),
                    Err(_) => i64::from_str_radix(t.trim_start_matches("0x").trim_start_matches("0X"), 16)
                        .map(|v| ir::Expr::Num(v as f64))
                        .unwrap_or_else(|_| ir::Expr::Opaque(t)),
                }
            }
            "interpreted_string_literal" | "raw_string_literal" | "rune_literal" => ir::Expr::Str(unquote(self.t(n))),
            "parenthesized_expression" => children(n).first().map(|x| self.expr(*x, hint)).unwrap_or(ir::Expr::Undefined),
            "selector_expression" => {
                let (Some(obj), Some(f)) = (field(n, "operand"), field(n, "field")) else { return self.opaque(n) };
                let property = self.t(f).to_string();
                // `http.MethodPost` is the string it names.
                if let (Some("net/http"), Some(verb)) = (self.import_of(obj), property.strip_prefix("Method")) {
                    return ir::Expr::Str(verb.to_uppercase());
                }
                ir::Expr::Member { object: Box::new(self.expr(obj, None)), property }
            }
            "call_expression" => self.call(n, hint),
            "index_expression" => {
                let (Some(obj), Some(idx)) = (field(n, "operand"), field(n, "index")) else { return self.opaque(n) };
                let object = Box::new(self.expr(obj, None));
                match self.expr(idx, None) {
                    ir::Expr::Str(k) => ir::Expr::Member { object, property: k },
                    index => ir::Expr::Index { object, index: Box::new(index) },
                }
            }
            "slice_expression" | "type_assertion_expression" | "type_conversion_expression" => {
                let inner = field(n, "operand").or_else(|| children(n).last().copied());
                inner.map(|x| self.expr(x, hint)).unwrap_or(ir::Expr::Undefined)
            }
            "unary_expression" => {
                let (Some(op), Some(x)) = (field(n, "operator"), field(n, "operand")) else { return self.opaque(n) };
                match self.t(op) {
                    "!" => ir::Expr::Not(Box::new(self.expr(x, None))),
                    "&" | "*" | "+" => self.expr(x, hint),
                    "-" => match self.expr(x, None) {
                        ir::Expr::Num(v) => ir::Expr::Num(-v),
                        other => ir::Expr::Binary { op: ir::BinOp::Sub, left: Box::new(ir::Expr::Num(0.0)), right: Box::new(other) },
                    },
                    "<-" => {
                        let ch = self.expr(x, None);
                        ir::Expr::Call(Box::new(ir::Call { callee: ir::Expr::Ident("<-chan".into()), args: vec![ch], loc: self.loc(n) }))
                    }
                    _ => self.opaque(n),
                }
            }
            "binary_expression" => self.binary(n),
            "composite_literal" => self.composite(n),
            "literal_value" => self.literal_value(n, None),
            "func_literal" => self.func_literal(n, hint),
            _ => self.opaque(n),
        }
    }

    /// Canonical import an identifier node names in this file, if it is a package.
    fn import_of(&self, n: Node) -> Option<&str> {
        (n.kind() == "identifier").then(|| self.file_imports.get(self.t(n)).map(String::as_str)).flatten()
    }

    fn binary(&mut self, n: Node) -> ir::Expr {
        let (Some(l), Some(op), Some(r)) = (field(n, "left"), field(n, "operator"), field(n, "right")) else { return self.opaque(n) };
        let (left, right) = (self.expr(l, None), self.expr(r, None));
        let logic = |op| ir::Expr::Logical { op, left: Box::new(left.clone()), right: Box::new(right.clone()) };
        let bin = |op| ir::Expr::Binary { op, left: Box::new(left.clone()), right: Box::new(right.clone()) };
        match self.t(op) {
            "&&" => logic(ir::LogicOp::And),
            "||" => logic(ir::LogicOp::Or),
            "==" => bin(ir::BinOp::Eq),
            "!=" => bin(ir::BinOp::NotEq),
            "<" => bin(ir::BinOp::Lt),
            "<=" => bin(ir::BinOp::LtEq),
            ">" => bin(ir::BinOp::Gt),
            ">=" => bin(ir::BinOp::GtEq),
            // String building reads like a template: `base + "/orders/" + id`.
            "+" if is_stringish(&left) || is_stringish(&right) => concat(left, right),
            "+" => bin(ir::BinOp::Add),
            "-" => bin(ir::BinOp::Sub),
            "*" => bin(ir::BinOp::Mul),
            "/" => bin(ir::BinOp::Div),
            "%" => bin(ir::BinOp::Rem),
            _ => bin(ir::BinOp::Other),
        }
    }

    fn call(&mut self, n: Node, hint: Option<String>) -> ir::Expr {
        let Some(func) = field(n, "function") else { return self.opaque(n) };
        let arg_nodes: Vec<Node> = field(n, "arguments").map(children).unwrap_or_default();
        // Conversions to a type (`OrderStatus(s)`, `string(b)`) are the value converted.
        if func.kind() == "identifier" || func.kind() == "type_identifier" {
            let name = self.t(func);
            if (BUILTIN_TYPES.contains(&name) || self.types.contains_key(name)) && arg_nodes.len() == 1 {
                return self.expr(arg_nodes[0], hint);
            }
        }
        if matches!(func.kind(), "parenthesized_type" | "slice_type" | "array_type" | "pointer_type" | "map_type") && arg_nodes.len() == 1 {
            return self.expr(arg_nodes[0], hint);
        }
        // `fmt.Sprintf("%s/orders/%d", base, id)` reads like a template.
        if let (Some(obj), Some(f)) = (field(func, "operand"), field(func, "field")) {
            if self.import_of(obj) == Some("fmt") && self.t(f) == "Sprintf" {
                if let Some(ir::Expr::Str(format)) = arg_nodes.first().map(|a| self.expr(*a, None)) {
                    let args: Vec<ir::Expr> = arg_nodes[1..].iter().map(|a| self.expr(*a, None)).collect();
                    return sprintf(&format, args);
                }
            }
        }
        let callee = self.expr(func, None);
        // Closures passed as arguments are named after the call they're passed to:
        // `r.Post("/orders", func(w, r) {…})` → `<fn>.Post("/orders")`.
        let method = match &callee {
            ir::Expr::Member { property, .. } => Some(property.clone()),
            ir::Expr::Ident(n) => Some(n.clone()),
            _ => None,
        };
        let first_str = arg_nodes.first().filter(|a| matches!(a.kind(), "interpreted_string_literal" | "raw_string_literal")).map(|a| unquote(self.t(*a)));
        let arg_hint = method.map(|m| match &first_str {
            Some(s) => format!("{m}({s:?})"),
            None => m,
        });
        let callee = match callee {
            // `func() { … }()` runs a closure named for where it is (`go`, `defer`, a variable).
            ir::Expr::Opaque(_) if func.kind() == "func_literal" => self.func_literal(func, hint),
            other => other,
        };
        let args = arg_nodes
            .iter()
            .map(|a| match self.expr(*a, arg_hint.clone()) {
                ir::Expr::Ident(v) if self.strings.contains_key(&v) => self.strings[&v].clone(),
                other => other,
            })
            .collect();
        // In a chain spread over lines, a call is where its method name is.
        let at = field(func, "field").unwrap_or(n);
        ir::Expr::Call(Box::new(ir::Call { callee, args, loc: self.loc(at) }))
    }

    /// `Order{ID: id, Status: StatusPending}` constructs an `Order`; slice and map
    /// literals are arrays and objects.
    fn composite(&mut self, n: Node) -> ir::Expr {
        let Some(body) = field(n, "body") else { return self.opaque(n) };
        let ty = field(n, "type");
        let named = ty.filter(|t| matches!(t.kind(), "type_identifier" | "qualified_type" | "generic_type"));
        match named {
            Some(t) if !matches!(self.types.get(self.t(t)), Some(&"other") | Some(&"basic")) => {
                let callee = match t.kind() {
                    "qualified_type" => {
                        let (Some(p), Some(nm)) = (field(t, "package"), field(t, "name")) else { return self.opaque(n) };
                        ir::Expr::Member { object: Box::new(ir::Expr::Ident(self.t(p).into())), property: self.t(nm).into() }
                    }
                    "generic_type" => match field(t, "type") {
                        Some(b) => ir::Expr::Ident(self.t(b).to_string()),
                        None => return self.opaque(n),
                    },
                    _ => ir::Expr::Ident(self.t(t).to_string()),
                };
                let value = self.literal_value(body, None);
                ir::Expr::New { callee: Box::new(callee), args: vec![value], loc: self.loc(n) }
            }
            _ => self.literal_value(body, None),
        }
    }

    fn literal_value(&mut self, body: Node, _hint: Option<String>) -> ir::Expr {
        let elems = children(body);
        let keyed = elems.iter().any(|e| e.kind() == "keyed_element");
        if !keyed {
            return ir::Expr::Array(elems.iter().map(|e| self.element(*e, None)).collect());
        }
        let mut props = vec![];
        for e in elems.iter().filter(|e| e.kind() == "keyed_element") {
            let parts = children(*e);
            let (Some(k), Some(v)) = (parts.first(), parts.get(1)) else { continue };
            let key_node = if k.kind() == "literal_element" { children(*k).first().copied().unwrap_or(*k) } else { *k };
            let key = match key_node.kind() {
                "interpreted_string_literal" | "raw_string_literal" => unquote(self.t(key_node)),
                _ => self.t(key_node).to_string(),
            };
            let value = self.element(*v, Some(key.clone()));
            props.push((key, value));
        }
        ir::Expr::Object(props)
    }

    fn element(&mut self, e: Node, hint: Option<String>) -> ir::Expr {
        match e.kind() {
            "literal_element" => children(e).first().map(|x| self.element(*x, hint)).unwrap_or(ir::Expr::Undefined),
            "literal_value" => self.literal_value(e, hint),
            _ => self.expr(e, hint),
        }
    }
}

fn var_specs(decl: Node) -> Vec<Node> {
    let mut out = vec![];
    for c in children(decl) {
        match c.kind() {
            "var_spec" => out.push(c),
            "var_spec_list" => out.extend(children(c).into_iter().filter(|s| s.kind() == "var_spec")),
            _ => {}
        }
    }
    out
}

fn collect_identifiers(n: Node, src: &str, out: &mut Vec<String>) {
    if n.kind() == "identifier" {
        out.push(text(n, src).to_string());
    }
    for c in children(n) {
        collect_identifiers(c, src, out);
    }
}

fn walk_idents(e: &ir::Expr, f: &mut dyn FnMut(&str)) {
    match e {
        ir::Expr::Ident(n) => f(n),
        ir::Expr::Member { object, .. } => walk_idents(object, f),
        ir::Expr::Call(c) => {
            walk_idents(&c.callee, f);
            c.args.iter().for_each(|a| walk_idents(a, f));
        }
        ir::Expr::New { args, .. } => args.iter().for_each(|a| walk_idents(a, f)),
        ir::Expr::Template { exprs, .. } | ir::Expr::Array(exprs) => exprs.iter().for_each(|a| walk_idents(a, f)),
        ir::Expr::Object(props) => props.iter().for_each(|(_, v)| walk_idents(v, f)),
        ir::Expr::Binary { left, right, .. } | ir::Expr::Logical { left, right, .. } => {
            walk_idents(left, f);
            walk_idents(right, f);
        }
        ir::Expr::Not(x) => walk_idents(x, f),
        _ => {}
    }
}

fn is_stringish(e: &ir::Expr) -> bool {
    matches!(e, ir::Expr::Str(_) | ir::Expr::Template { .. })
}

/// `a + b` as a template, merging adjacent literal parts.
fn concat(left: ir::Expr, right: ir::Expr) -> ir::Expr {
    let parts = |e: ir::Expr| -> (Vec<String>, Vec<ir::Expr>) {
        match e {
            ir::Expr::Template { quasis, exprs } => (quasis, exprs),
            ir::Expr::Str(s) => (vec![s], vec![]),
            other => (vec![String::new(), String::new()], vec![other]),
        }
    };
    let (mut q, mut x) = parts(left);
    let (q2, x2) = parts(right);
    let mut q2 = q2.into_iter();
    if let (Some(last), Some(first)) = (q.last_mut(), q2.next()) {
        last.push_str(&first);
    }
    q.extend(q2);
    x.extend(x2);
    ir::Expr::Template { quasis: q, exprs: x }
}

/// `fmt.Sprintf("%s/orders/%d", base, id)` as a template of its verbs' arguments.
fn sprintf(format: &str, args: Vec<ir::Expr>) -> ir::Expr {
    let mut quasis = vec![String::new()];
    let mut exprs = vec![];
    let mut args = args.into_iter();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            quasis.last_mut().expect("non-empty").push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            quasis.last_mut().expect("non-empty").push('%');
            continue;
        }
        // Flags, width and precision, then the verb letter.
        while chars.peek().is_some_and(|c| !c.is_ascii_alphabetic()) {
            chars.next();
        }
        chars.next();
        exprs.push(args.next().unwrap_or(ir::Expr::Undefined));
        quasis.push(String::new());
    }
    ir::Expr::Template { quasis, exprs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(files: &[(&str, &str)]) -> ir::Program {
        lower_program(&files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect())
    }

    #[test]
    fn canonical_imports() {
        assert_eq!(canonical_import("github.com/go-chi/chi/v5"), "go-chi/chi");
        assert_eq!(canonical_import("gorm.io/gorm"), "gorm");
        assert_eq!(canonical_import("net/http"), "net/http");
        assert_eq!(canonical_import("github.com/jackc/pgx/v5/pgxpool"), "jackc/pgx/pgxpool");
        assert_eq!(default_import_name("github.com/redis/go-redis/v9"), "redis");
        assert_eq!(default_import_name("gopkg.in/yaml.v3"), "yaml");
    }

    #[test]
    fn a_package_is_one_module_with_methods_on_its_structs() {
        let p = lower(&[
            ("go.mod", "module github.com/acme/shop\n"),
            ("internal/orders/model.go", "package orders\ntype OrderStatus string\nconst (\n StatusPending OrderStatus = \"PENDING\"\n StatusCancelled OrderStatus = \"CANCELLED\"\n)\ntype Order struct { ID string; Status OrderStatus }\n"),
            (
                "internal/orders/service.go",
                "package orders\nimport \"errors\"\nvar ErrNotPending = errors.New(\"not pending\")\ntype Service struct { repo Repository }\ntype Repository interface { Save(o *Order) error }\nfunc (s *Service) Cancel(o *Order) error {\n if o.Status != StatusPending { return ErrNotPending }\n o.Status = StatusCancelled\n if err := s.repo.Save(o); err != nil { return err }\n return nil\n}\n",
            ),
        ]);
        let m = &p.modules["internal/orders"];
        assert_eq!(m.enums[0].members["StatusPending"], "PENDING");
        let svc = m.classes.iter().find(|c| c.name == "Service").unwrap();
        assert_eq!(svc.methods, vec!["internal/orders::Service.Cancel".to_string()]);
        let f = m.functions.iter().find(|f| f.name == "Cancel").unwrap();
        // guard (throw), write, the save call; the error hand-on is implicit.
        assert!(matches!(&f.body[0], ir::Stmt::If { then, .. } if matches!(then[0], ir::Stmt::Throw(..))), "{:?}", f.body);
        assert!(
            matches!(&f.body[1], ir::Stmt::Expr(ir::Expr::Assign { target, .. }, _) if matches!(target.as_ref(), ir::Expr::Member { object, .. } if **object == ir::Expr::Ident("o".into())))
        );
        assert!(matches!(&f.body[2], ir::Stmt::Let { name, init: Some(ir::Expr::Call(_)), .. } if name == "err"), "{:?}", f.body[2]);
        assert!(matches!(&f.body[3], ir::Stmt::Return(None, _)), "{:?}", f.body);
        assert!(p.aliases.iter().any(|a| a.pattern == "go:*" && a.targets == vec!["*".to_string()]));
    }

    #[test]
    fn receivers_are_this_and_goroutines_spawn() {
        let p = lower(&[("svc.go", "package svc\ntype S struct{}\nfunc (s *S) Run() { go s.notify(); s.done = true }\nfunc (s *S) notify() {}\n")]);
        let f = p.modules["."].functions.iter().find(|f| f.name == "Run").unwrap();
        let ir::Stmt::Expr(ir::Expr::Spawn(call), _) = &f.body[0] else { panic!("{:?}", f.body) };
        let ir::Expr::Call(c) = call.as_ref() else { panic!() };
        assert!(matches!(&c.callee, ir::Expr::Member { object, .. } if **object == ir::Expr::This));
    }

    #[test]
    fn sprintf_is_a_template() {
        let p = lower(&[("a.go", "package a\nimport \"fmt\"\nfunc U(id string) string { return fmt.Sprintf(\"%s/orders/%d\", base, id) }\n")]);
        let f = &p.modules["."].functions[0];
        assert!(
            matches!(&f.body[0], ir::Stmt::Return(Some(ir::Expr::Template { quasis, .. }), _) if quasis == &vec!["".to_string(), "/orders/".into(), "".into()])
        );
    }
}
