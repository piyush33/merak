//! Invariant inference: mines likely design invariants from how state fields are
//! written across the program. Everything here is `Origin::Inferred`.

use crate::model::*;
use crate::pred::Pred;
use std::collections::BTreeSet;

/// Writers of `field := value` (entity id, write location), creation included or not.
pub fn writers<'m>(model: &'m Model, field: &str, value: &str, include_creation: bool) -> Vec<(&'m Entity, &'m WriteFact)> {
    let mut out = vec![];
    for e in model.entities.values() {
        for w in &e.writes {
            if w.field == field && w.value.as_deref() == Some(value) && (include_creation || !w.creation) {
                out.push((e, w));
            }
        }
    }
    out
}

pub fn writes_non_null(e: &Entity, field: &str) -> bool {
    e.writes.iter().any(|w| w.field == field && w.value.as_deref() != Some("null"))
}

/// Guard keys an entity requires, excluding guards on `state_field` itself
/// (those are captured by the state machine).
pub fn guard_keys(model: &Model, e: &Entity, state_field: &str) -> Vec<(String, Pred)> {
    model.summaries[&e.id].requires.iter().filter(|(_, p, _)| p.field() != Some(state_field)).map(|(k, p, _)| (k.clone(), p.clone())).collect()
}

fn confidence(support: usize) -> f64 {
    // Laplace-style: one supporting writer is weak evidence, many is strong.
    let c = support as f64 / (support as f64 + 1.0);
    (c * 100.0).round() / 100.0
}

pub fn mine(model: &Model) -> Vec<Invariant> {
    let mut out = vec![];
    for sm in model.state_machines.values() {
        for value in sm.targets() {
            // (a) co-write: every writer of F := V also sets G.
            let ws = writers(model, &sm.field, &value, true);
            if ws.len() >= 2 {
                let mut common: Option<BTreeSet<String>> = None;
                for (e, _) in &ws {
                    let set: BTreeSet<String> =
                        e.writes.iter().filter(|w| w.field != sm.field && w.value.as_deref() != Some("null")).map(|w| w.field.clone()).collect();
                    common = Some(match common {
                        None => set,
                        Some(c) => c.intersection(&set).cloned().collect(),
                    });
                }
                for g in common.unwrap_or_default() {
                    out.push(Invariant {
                        key: format!("cowrite:{}={}=>{}", sm.field, value, g),
                        statement: format!("{} := {} ⇒ {} is set", sm.field, value, g),
                        field: sm.field.clone(),
                        value: value.clone(),
                        requirement: Requirement::CoWrite(g),
                        support: ws.len(),
                        confidence: confidence(ws.len()),
                        evidence: ws.iter().map(|(_, w)| w.loc.clone()).collect(),
                    });
                }
            }
            // (b) guard: every (non-creation) writer of F := V checks P first.
            let ws = writers(model, &sm.field, &value, false);
            if ws.is_empty() {
                continue;
            }
            let mut common: Option<Vec<(String, Pred)>> = None;
            for (e, _) in &ws {
                let keys = guard_keys(model, e, &sm.field);
                common = Some(match common {
                    None => keys,
                    Some(c) => c.into_iter().filter(|(k, _)| keys.iter().any(|(k2, _)| k2 == k)).collect(),
                });
            }
            for (k, p) in common.unwrap_or_default() {
                out.push(Invariant {
                    key: format!("guard:{}={}=>{}", sm.field, value, k),
                    statement: format!("{} := {} requires {}", sm.field, value, p.render()),
                    field: sm.field.clone(),
                    value: value.clone(),
                    requirement: Requirement::Guard(k),
                    support: ws.len(),
                    confidence: confidence(ws.len()),
                    evidence: ws.iter().map(|(_, w)| w.loc.clone()).collect(),
                });
            }
        }
    }
    out
}

/// Does `writer` (in `model`) satisfy `inv`? `rename` maps entity ids of the
/// model the invariant was mined in to ids in `model`.
pub fn satisfies(model: &Model, inv: &Invariant, writer: &Entity, rename: &dyn Fn(&str) -> String) -> bool {
    match &inv.requirement {
        Requirement::CoWrite(g) => writes_non_null(writer, g),
        Requirement::Guard(key) => {
            let wanted = match key.split_once("call:") {
                Some((neg, id)) => format!("{neg}call:{}", rename(id)),
                None => key.clone(),
            };
            guard_keys(model, writer, &inv.field).iter().any(|(k, _)| *k == wanted)
        }
    }
}
