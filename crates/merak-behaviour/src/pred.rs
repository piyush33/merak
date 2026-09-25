//! Normalized predicates: the vocabulary for guards, authorization rules and
//! state preconditions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Pred {
    /// `field ∈ values`
    In {
        field: String,
        values: BTreeSet<String>,
    },
    /// `field ∉ values`
    NotIn {
        field: String,
        values: BTreeSet<String>,
    },
    Truthy(String),
    Falsy(String),
    /// Result of calling another entity (usually a policy/predicate function).
    Call {
        entity: String,
        negated: bool,
    },
    /// `left op right` over non-constant operands, canonicalised by [`Pred::compare`]:
    /// only `<`, `<=`, `==`, `!=`, with `==`/`!=` operands in sorted order.
    Compare {
        left: String,
        op: String,
        right: String,
    },
    And(Vec<Pred>),
    Or(Vec<Pred>),
    Opaque {
        text: String,
        negated: bool,
    },
    True,
    False,
}

impl Pred {
    /// Canonical comparison: `a > b` becomes `b < a`, `a >= b` becomes `b <= a`,
    /// and equality operands are ordered, so equivalent spellings compare equal.
    pub fn compare(left: String, op: &str, right: String) -> Pred {
        let (left, op, right) = match op {
            ">" => (right, "<", left),
            ">=" => (right, "<=", left),
            "==" | "!=" if right < left => (right, op, left),
            _ => (left, op, right),
        };
        Pred::Compare { left, op: op.to_string(), right }
    }

    pub fn negate(self) -> Pred {
        match self {
            Pred::In { field, values } => Pred::NotIn { field, values },
            Pred::NotIn { field, values } => Pred::In { field, values },
            Pred::Truthy(f) => Pred::Falsy(f),
            Pred::Falsy(f) => Pred::Truthy(f),
            Pred::Call { entity, negated } => Pred::Call { entity, negated: !negated },
            Pred::Compare { left, op, right } => match op.as_str() {
                "<" => Pred::compare(left, ">=", right),
                "<=" => Pred::compare(left, ">", right),
                "==" => Pred::compare(left, "!=", right),
                _ => Pred::compare(left, "==", right),
            },
            Pred::Opaque { text, negated } => Pred::Opaque { text, negated: !negated },
            Pred::And(ps) => Pred::Or(ps.into_iter().map(Pred::negate).collect()).simplify(),
            Pred::Or(ps) => Pred::And(ps.into_iter().map(Pred::negate).collect()).simplify(),
            Pred::True => Pred::False,
            Pred::False => Pred::True,
        }
    }

    /// Merge same-field set constraints: `a=X || a=Y` → `a ∈ {X,Y}`,
    /// `a≠X && a≠Y` → `a ∉ {X,Y}`; flatten nested and/or.
    pub fn simplify(self) -> Pred {
        match self {
            Pred::Or(ps) => merge(ps, true),
            Pred::And(ps) => merge(ps, false),
            other => other,
        }
    }

    /// The field this predicate constrains, if it constrains exactly one.
    pub fn field(&self) -> Option<&str> {
        match self {
            Pred::In { field, .. } | Pred::NotIn { field, .. } | Pred::Truthy(field) | Pred::Falsy(field) => Some(field),
            _ => None,
        }
    }

    /// Rewrite `f ∉ S` as `f ∈ (domain − S)` when the field has a known finite domain.
    pub fn with_domain(self, domain: &dyn Fn(&str) -> Option<BTreeSet<String>>) -> Pred {
        match self {
            Pred::NotIn { field, values } => match domain(&field) {
                Some(all) => Pred::In { values: all.difference(&values).cloned().collect(), field },
                None => Pred::NotIn { field, values },
            },
            Pred::And(ps) => Pred::And(ps.into_iter().map(|p| p.with_domain(domain)).collect()),
            Pred::Or(ps) => Pred::Or(ps.into_iter().map(|p| p.with_domain(domain)).collect()),
            other => other,
        }
    }

    pub fn render(&self) -> String {
        match self {
            Pred::In { field, values } => format!("{field} ∈ {}", render_set(values)),
            Pred::NotIn { field, values } => format!("{field} ∉ {}", render_set(values)),
            Pred::Truthy(f) => format!("{f} is set"),
            Pred::Falsy(f) => format!("{f} is not set"),
            Pred::Call { entity, negated } => {
                let short = entity.rsplit("::").next().unwrap_or(entity);
                if *negated {
                    format!("!{short}()")
                } else {
                    format!("{short}()")
                }
            }
            Pred::Compare { left, op, right } => format!("{left} {op} {right}"),
            Pred::And(ps) => ps.iter().map(|p| p.render()).collect::<Vec<_>>().join(" ∧ "),
            Pred::Or(ps) => ps.iter().map(|p| p.render()).collect::<Vec<_>>().join(" ∨ "),
            Pred::Opaque { text, negated } => {
                if *negated {
                    format!("!({text})")
                } else {
                    text.clone()
                }
            }
            Pred::True => "true".into(),
            Pred::False => "false".into(),
        }
    }
}

pub fn render_set(values: &BTreeSet<String>) -> String {
    format!("{{{}}}", values.iter().cloned().collect::<Vec<_>>().join(", "))
}

fn merge(ps: Vec<Pred>, is_or: bool) -> Pred {
    let mut flat = vec![];
    for p in ps {
        match p.simplify() {
            Pred::Or(inner) if is_or => flat.extend(inner),
            Pred::And(inner) if !is_or => flat.extend(inner),
            other => flat.push(other),
        }
    }
    // Under OR, `In` sets on the same field union; under AND, `NotIn` sets union.
    let mut out: Vec<Pred> = vec![];
    for p in flat {
        let merged = out.iter_mut().any(|q| match (&p, q) {
            (Pred::In { field: f1, values: v1 }, Pred::In { field: f2, values: v2 }) if is_or && f1 == f2 => {
                v2.extend(v1.iter().cloned());
                true
            }
            (Pred::NotIn { field: f1, values: v1 }, Pred::NotIn { field: f2, values: v2 }) if !is_or && f1 == f2 => {
                v2.extend(v1.iter().cloned());
                true
            }
            (Pred::In { field: f1, values: v1 }, Pred::In { field: f2, values: v2 }) if !is_or && f1 == f2 => {
                let inter: BTreeSet<String> = v1.intersection(v2).cloned().collect();
                *v2 = inter;
                true
            }
            _ => false,
        });
        if !merged && !out.contains(&p) {
            out.push(p);
        }
    }
    if out.len() == 1 {
        out.pop().unwrap()
    } else if is_or {
        Pred::Or(out)
    } else {
        Pred::And(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn de_morgan_merges_state_guards() {
        // !(s != A && s != B)  ==  s ∈ {A, B}
        let p = Pred::And(vec![
            Pred::NotIn { field: "Order.status".into(), values: set(&["A"]) },
            Pred::NotIn { field: "Order.status".into(), values: set(&["B"]) },
        ])
        .simplify()
        .negate();
        assert_eq!(p, Pred::In { field: "Order.status".into(), values: set(&["A", "B"]) });
    }

    #[test]
    fn comparisons_are_canonical() {
        let c = |l: &str, op: &str, r: &str| Pred::compare(l.into(), op, r.into());
        assert_eq!(c("a", ">", "b"), c("b", "<", "a"));
        assert_eq!(c("a", "<=", "b").negate(), c("a", ">", "b"));
        assert_eq!(c("a", "==", "b").negate(), c("b", "!=", "a"));
    }

    #[test]
    fn or_of_equalities_is_membership() {
        let p =
            Pred::Or(vec![Pred::In { field: "User.role".into(), values: set(&["ADMIN"]) }, Pred::In { field: "User.role".into(), values: set(&["MANAGER"]) }])
                .simplify();
        assert_eq!(p.render(), "User.role ∈ {ADMIN, MANAGER}");
    }
}
