//! Validation schemas declared as module constants (`export const XSchema = z.object({…})`).
//! Each field is rendered as its constraints, with documentation-only calls
//! (`describe`, `meta`, …) left out, so a transition sees what input is accepted.

use crate::catalog::Catalog;
use crate::model::SchemaFact;
use crate::resolve::{render, Env, Index, Ty};
use merak_ir::{self as ir, Expr};
use std::collections::BTreeMap;

pub fn extract(ix: &Index, cat: &Catalog, module: &ir::Module) -> Vec<(String, SchemaFact)> {
    let env = Env { module: module.path.clone(), ..Default::default() };
    let r = Renderer { ix, cat, env: &env };
    module
        .consts
        .iter()
        .filter_map(|c| {
            let init = c.init.as_ref()?;
            if !r.is_schema(init) {
                return None;
            }
            let mut fields = BTreeMap::new();
            let whole = r.text(init, &mut Some(&mut fields));
            // Anything beyond the plain object (refinements, `.strict()`, a non-object schema).
            if !(whole == "object({…})" && !fields.is_empty()) {
                fields.insert("*".to_string(), whole);
            }
            Some((format!("{}::{}", module.path, c.name), SchemaFact { fields, loc: c.loc.clone() }))
        })
        .collect()
}

struct Renderer<'a, 'p> {
    ix: &'a Index<'p>,
    cat: &'a Catalog,
    env: &'a Env,
}

impl Renderer<'_, '_> {
    /// The expression is a chain rooted at a schema library (directly or through another schema const).
    fn is_schema(&self, e: &Expr) -> bool {
        let mut cur = e;
        loop {
            cur = match cur {
                Expr::Call(c) => &c.callee,
                Expr::Member { object, .. } => object,
                Expr::Ident(_) => break,
                _ => return false,
            };
        }
        matches!(&self.ix.type_of(self.env, cur), Ty::External(canon) if self.is_root(canon))
    }

    fn is_root(&self, canon: &str) -> bool {
        self.cat.schema.roots.iter().any(|r| canon == r || canon.starts_with(&format!("{r}.")))
    }

    /// Canonical constraint text. `fields`, when given, receives the members of the chain's
    /// object shape (`object({…})`, `.extend({…})`, and those of a schema constant it is built
    /// from), which are then rendered as `{…}`.
    fn text(&self, e: &Expr, fields: &mut Option<&mut BTreeMap<String, String>>) -> String {
        match e {
            Expr::Call(c) => {
                let Expr::Member { object, property } = &c.callee else { return render(e) };
                if self.cat.schema.docs.contains(property) {
                    return self.text(object, fields);
                }
                let head = self.text(object, fields);
                // `strictObject(shape)` with `const shape = { … }`: the shape's fields.
                let shape = match c.args.as_slice() {
                    [Expr::Ident(n)] => self.const_object(n),
                    [Expr::Object(props)] => Some(props),
                    _ => None,
                };
                let args = match (property.as_str(), shape, fields.as_deref_mut()) {
                    ("object" | "strictObject" | "looseObject" | "extend", Some(props), Some(out)) => {
                        for (k, v) in props {
                            out.insert(k.clone(), self.text(v, &mut None));
                        }
                        "{…}".to_string()
                    }
                    _ => c.args.iter().map(|a| self.arg(a)).collect::<Vec<_>>().join(", "),
                };
                if head.is_empty() {
                    format!("{property}({args})")
                } else {
                    format!("{head}.{property}({args})")
                }
            }
            Expr::Member { object, property } => match self.text(object, fields) {
                h if h.is_empty() => property.clone(),
                h => format!("{h}.{property}"),
            },
            // The library root (`z`, a plain import) is left out: `z.string()` is `string()`.
            // Schema constants built from it (`stringToBool`) keep their name.
            Expr::Ident(n) => match self.ix.type_of(self.env, e) {
                Ty::External(canon) if self.is_root(&canon) && !canon.contains('(') => String::new(),
                _ => {
                    // Built from another schema in this module: its fields are this one's too.
                    if let (Some(out), Some(init)) = (fields.as_deref_mut(), self.const_init(n)) {
                        if self.is_schema(init) {
                            self.text(init, &mut Some(out));
                        }
                    }
                    n.clone()
                }
            },
            other => self.arg(other),
        }
    }

    fn const_init(&self, name: &str) -> Option<&Expr> {
        let m = self.ix.program.modules.get(&self.env.module)?;
        m.consts.iter().find(|c| c.name == name)?.init.as_ref()
    }

    /// The object literal a module constant is initialised with.
    fn const_object(&self, name: &str) -> Option<&Vec<(String, Expr)>> {
        match self.const_init(name)? {
            Expr::Object(props) => Some(props),
            _ => None,
        }
    }

    fn arg(&self, a: &Expr) -> String {
        if let Some(v) = self.ix.const_str(self.env, a) {
            // Strings quoted, numbers and booleans as written.
            return if matches!(a, Expr::Num(_) | Expr::Bool(_) | Expr::Null) { v } else { format!("{v:?}") };
        }
        match a {
            Expr::Object(props) => {
                let mut kv: Vec<String> = props.iter().map(|(k, v)| format!("{k}: {}", self.text(v, &mut None))).collect();
                kv.sort();
                format!("{{{}}}", kv.join(", "))
            }
            Expr::Array(xs) => format!("[{}]", xs.iter().map(|x| self.arg(x)).collect::<Vec<_>>().join(", ")),
            Expr::Call(_) | Expr::Member { .. } => self.text(a, &mut None),
            Expr::Closure(_) => "<fn>".into(),
            other => render(other),
        }
    }
}
