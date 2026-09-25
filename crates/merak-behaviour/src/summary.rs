//! Transitive behaviour summaries (a monotone fixpoint over the call graph and
//! event subscriptions), validator detection and state machines.

use crate::model::*;
use crate::pred::Pred;
use std::collections::{BTreeMap, BTreeSet};

const WRITE_EFFECTS: &[&str] = &["db_write", "db_query", "http", "event_emit", "fs_write", "queue", "cache_write"];

pub fn compute(model: &mut Model) {
    let handlers = handlers_by_event(model);

    // 1. Effects, reads, writes — fixpoint over calls and emitted events.
    let mut sums: BTreeMap<String, Summary> = BTreeMap::new();
    for e in model.entities.values() {
        let mut s = Summary::default();
        for eff in &e.effects {
            let info = s.effects.entry(eff.key.clone()).or_default();
            info.modes.insert("sync".into());
            info.via.insert("direct".into());
            info.evidence.insert(eff.loc.clone());
        }
        s.reads = e.reads.keys().cloned().collect();
        s.writes = e.writes.iter().map(|w| w.field.clone()).collect();
        sums.insert(e.id.clone(), s);
    }
    loop {
        let mut changed = false;
        for e in model.entities.values() {
            let mut add: Vec<(EffectKey, EffectInfo)> = vec![];
            let mut reads = BTreeSet::new();
            let mut writes = BTreeSet::new();
            for c in &e.calls {
                let Some(cs) = sums.get(&c.target) else { continue };
                for (k, info) in &cs.effects {
                    add.push((k.clone(), EffectInfo { modes: info.modes.clone(), via: BTreeSet::from([c.target.clone()]), evidence: info.evidence.clone() }));
                }
                reads.extend(cs.reads.iter().cloned());
                writes.extend(cs.writes.iter().cloned());
            }
            for eff in e.effects.iter().filter(|x| x.key.kind == "event_emit") {
                for h in handlers.get(&eff.key.target).into_iter().flatten() {
                    let Some(hs) = sums.get(h) else { continue };
                    for (k, info) in &hs.effects {
                        add.push((
                            k.clone(),
                            EffectInfo {
                                modes: BTreeSet::from([format!("async via {}", eff.key.target)]),
                                via: BTreeSet::from([format!("event:{}", eff.key.target)]),
                                evidence: info.evidence.clone(),
                            },
                        ));
                    }
                }
            }
            let s = sums.get_mut(&e.id).unwrap();
            for (k, info) in add {
                let slot = s.effects.entry(k).or_default();
                let before = (slot.modes.len(), slot.via.len(), slot.evidence.len());
                slot.modes.extend(info.modes);
                slot.via.extend(info.via);
                slot.evidence.extend(info.evidence);
                changed |= before != (slot.modes.len(), slot.via.len(), slot.evidence.len());
            }
            let before = (s.reads.len(), s.writes.len());
            s.reads.extend(reads);
            s.writes.extend(writes);
            changed |= before != (s.reads.len(), s.writes.len());
        }
        if !changed {
            break;
        }
    }

    // 2. Validators: throw on bad input, change nothing.
    let validators: BTreeSet<String> = model
        .entities
        .values()
        .filter(|e| {
            let s = &sums[&e.id];
            e.throws
                && e.returns.is_none()
                && s.writes.is_empty()
                && !s.effects.keys().any(|k| WRITE_EFFECTS.contains(&k.kind.as_str()))
                && !model.routes.iter().any(|r| r.handler == e.id)
        })
        .map(|e| e.id.clone())
        .collect();

    // 3. Validations reached (through non-validator callees).
    loop {
        let mut changed = false;
        for e in model.entities.values() {
            let mut found: Vec<(String, merak_ir::Loc)> = vec![];
            for c in &e.calls {
                if validators.contains(&c.target) {
                    found.push((c.target.clone(), c.loc.clone()));
                } else if let Some(cs) = sums.get(&c.target) {
                    found.extend(cs.validations.keys().map(|v| (v.clone(), c.loc.clone())));
                }
            }
            let s = sums.get_mut(&e.id).unwrap();
            for (v, loc) in found {
                if let std::collections::btree_map::Entry::Vacant(e) = s.validations.entry(v) {
                    e.insert(loc);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // 4. Requirements: own guards, with predicate-function calls expanded.
    for e in model.entities.values() {
        let s = sums.get_mut(&e.id).unwrap();
        for g in &e.guards {
            let (key, expanded) = expand_guard(model, &g.requires);
            s.requires.push((key, expanded, g.loc.clone()));
        }
    }

    model.summaries = sums;
    model.validators = validators;
    model.state_machines = state_machines(model);
}

fn handlers_by_event(model: &Model) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in &model.subscriptions {
        out.entry(s.event.clone()).or_default().push(s.handler.clone());
    }
    out
}

/// Stable key for a guard (so it can be matched across versions even when the
/// predicate function's body changes) plus its expanded meaning.
pub fn expand_guard(model: &Model, p: &Pred) -> (String, Pred) {
    let key = match p {
        Pred::Call { entity, negated } => format!("{}call:{entity}", if *negated { "!" } else { "" }),
        other => other.render(),
    };
    (key, expand(model, p, 0))
}

fn expand(model: &Model, p: &Pred, depth: u32) -> Pred {
    if depth > 6 {
        return p.clone();
    }
    match p {
        Pred::Call { entity, negated } => match model.entities.get(entity).and_then(|e| e.returns.clone()) {
            Some(r) => {
                let r = expand(model, &r, depth + 1);
                if *negated {
                    r.negate()
                } else {
                    r
                }
            }
            None => p.clone(),
        },
        Pred::And(ps) => Pred::And(ps.iter().map(|x| expand(model, x, depth + 1)).collect()).simplify(),
        Pred::Or(ps) => Pred::Or(ps.iter().map(|x| expand(model, x, depth + 1)).collect()).simplify(),
        other => other.clone(),
    }
}

fn state_machines(model: &Model) -> BTreeMap<String, StateMachine> {
    let mut out: BTreeMap<String, StateMachine> = BTreeMap::new();
    for e in model.entities.values() {
        let s = &model.summaries[&e.id];
        for w in &e.writes {
            let Some(to) = &w.value else { continue };
            let Some(domain) = model.field_domains.get(&w.field) else { continue };
            if !domain.contains(to) {
                continue;
            }
            let from: BTreeSet<String> = if w.creation {
                BTreeSet::from(["∅".to_string()])
            } else {
                s.requires
                    .iter()
                    .find_map(|(_, p, _)| match p {
                        Pred::In { field, values } if field == &w.field => Some(values.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| BTreeSet::from(["*".to_string()]))
            };
            let sm = out.entry(w.field.clone()).or_insert_with(|| StateMachine { field: w.field.clone(), states: domain.clone(), edges: vec![] });
            sm.edges.push(StateEdge { from, to: to.clone(), writer: e.id.clone(), loc: w.loc.clone() });
        }
    }
    for sm in out.values_mut() {
        sm.edges.sort();
    }
    out
}
