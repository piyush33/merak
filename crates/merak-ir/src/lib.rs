//! Structural IR: a small, owned, language-neutral lowering of source code.
//!
//! Front ends (e.g. `merak-front-ts`) lower their ASTs into these types; every
//! later layer (behaviour extraction, transitions) works only on this IR, so a
//! new source language only needs a new front end.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A source location. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Loc {
    pub file: String,
    pub line: u32,
}

impl std::fmt::Display for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// A (possibly generic) type annotation, reduced to what resolution needs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeRef {
    /// `Foo`, `Promise<Foo>` is stored as `Named("Promise", [Named("Foo")])`.
    Named(String, Vec<TypeRef>),
    Array(Box<TypeRef>),
    /// Union of string literals, e.g. `"ADMIN" | "MANAGER"`.
    StringLiterals(Vec<String>),
    Union(Vec<TypeRef>),
    /// Inline object type `{ a: T; b: U }`.
    Object(Vec<(String, TypeRef)>),
    Primitive(String),
    Unknown,
}

impl TypeRef {
    /// Strip `Promise<T>` and nullable unions down to the interesting type.
    pub fn unwrap_async(&self) -> &TypeRef {
        match self {
            TypeRef::Named(n, args) if n == "Promise" && args.len() == 1 => args[0].unwrap_async(),
            TypeRef::Union(parts) => {
                let non_null: Vec<&TypeRef> = parts.iter().filter(|t| !matches!(t, TypeRef::Primitive(p) if p == "null" || p == "undefined")).collect();
                if non_null.len() == 1 {
                    non_null[0].unwrap_async()
                } else {
                    self
                }
            }
            other => other,
        }
    }

    pub fn element(&self) -> Option<&TypeRef> {
        match self.unwrap_async() {
            TypeRef::Array(t) => Some(t),
            TypeRef::Named(n, args) if (n == "Array" || n == "ReadonlyArray") && args.len() == 1 => Some(&args[0]),
            _ => None,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self.unwrap_async() {
            TypeRef::Named(n, _) => Some(n),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogicOp {
    And,
    Or,
    Coalesce,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    This,
    Member {
        object: Box<Expr>,
        property: String,
    },
    /// `a[b]` with a non-literal key.
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    Call(Box<Call>),
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
        loc: Loc,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Logical {
        op: LogicOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    Template {
        quasis: Vec<String>,
        exprs: Vec<Expr>,
    },
    Object(Vec<(String, Expr)>),
    Array(Vec<Expr>),
    /// A nested function (arrow / function expression), lowered separately.
    /// The string is the entity id of the lowered function.
    Closure(String),
    Await(Box<Expr>),
    Conditional {
        test: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    Assign {
        target: Box<Expr>,
        value: Box<Expr>,
        loc: Loc,
    },
    /// Anything we don't model; keeps the source text for evidence/debugging.
    Opaque(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub callee: Expr,
    pub args: Vec<Expr>,
    pub loc: Loc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Stmt {
    Expr(Expr, Loc),
    Let {
        name: String,
        ty: Option<TypeRef>,
        init: Option<Expr>,
        loc: Loc,
    },
    If {
        test: Expr,
        then: Vec<Stmt>,
        otherwise: Vec<Stmt>,
        loc: Loc,
    },
    Return(Option<Expr>, Loc),
    Throw(Expr, Loc),
    /// Any loop. `binding` / `iter` are set for `for (const x of xs)`.
    Loop {
        binding: Option<String>,
        iter: Option<Expr>,
        body: Vec<Stmt>,
        loc: Loc,
    },
    Try {
        body: Vec<Stmt>,
        handler: Vec<Stmt>,
        finalizer: Vec<Stmt>,
        loc: Loc,
    },
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionKind {
    Function,
    Method,
    Constructor,
    Closure,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Function {
    /// Stable entity id: `<file>::<qualified name>`.
    pub id: String,
    pub name: String,
    pub kind: FunctionKind,
    /// Class the function belongs to (for methods), by simple name.
    pub class: Option<String>,
    /// Entity id of the enclosing function for closures.
    pub parent: Option<String>,
    pub params: Vec<Param>,
    pub return_type: Option<TypeRef>,
    pub body: Vec<Stmt>,
    pub is_async: bool,
    pub exported: bool,
    pub loc: Loc,
    pub end_line: u32,
    /// Hash of the body with names/locations stripped (used for move detection).
    pub body_hash: String,
    /// Decorators, as calls (`@Get(':id')`; a bare `@Injectable` has no args).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<Call>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Class {
    pub id: String,
    pub name: String,
    /// `extends` target, by simple name.
    pub extends: Option<String>,
    /// Fields with declared types, including constructor parameter properties.
    pub fields: BTreeMap<String, TypeRef>,
    /// Method entity ids.
    pub methods: Vec<String>,
    pub exported: bool,
    pub loc: Loc,
    /// Decorators, as calls (`@Get(':id')`; a bare `@Injectable` has no args).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<Call>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Import {
    pub local: String,
    /// Imported name; `default` or `*` for default / namespace imports.
    pub imported: String,
    /// Module specifier as written (`../infra/db`, `axios`).
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Enum {
    pub name: String,
    /// member name → value (string value, or the member name if none).
    pub members: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Const {
    pub name: String,
    pub ty: Option<TypeRef>,
    pub init: Option<Expr>,
    pub exported: bool,
    pub loc: Loc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    pub name: String,
    pub fields: BTreeMap<String, TypeRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Module {
    /// Repository-relative path, `/`-separated.
    pub path: String,
    pub imports: Vec<Import>,
    pub classes: Vec<Class>,
    pub functions: Vec<Function>,
    pub enums: Vec<Enum>,
    pub consts: Vec<Const>,
    pub interfaces: Vec<Interface>,
    /// Aliases like `type Role = "A" | "B"`.
    pub type_aliases: BTreeMap<String, TypeRef>,
    pub parse_errors: Vec<String>,
}

/// The structural IR of a whole program (one version of a repository).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub modules: BTreeMap<String, Module>,
    /// Non-relative import aliases (e.g. tsconfig `paths` / `baseUrl`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<PathAlias>,
}

/// `pattern` (with at most one `*`) maps to repository-relative `targets`, for
/// modules under `scope` (a directory; `""` is the repository root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathAlias {
    pub scope: String,
    pub pattern: String,
    pub targets: Vec<String>,
}

impl Program {
    pub fn functions(&self) -> impl Iterator<Item = &Function> {
        self.modules.values().flat_map(|m| m.functions.iter())
    }
}

/// Stable, location-independent hashing helper used by front ends.
pub fn stable_hash(text: &str) -> String {
    // FNV-1a 64: tiny, deterministic, dependency-free.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
