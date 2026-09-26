//! Semantic transitions: what changed in a program's behaviour and design
//! between two Program Behaviour Models.
//!
//! The snapshot models are only the substrate; the output of this crate — a
//! list of typed, evidence-backed operations — is the object Merak is about.

pub mod render;

use merak_behaviour::infer;
use merak_behaviour::pred::{render_set, Pred};
use merak_behaviour::{AccessFact, CallShape, EffectInfo, EffectKey, Model, Origin, QueryFilter};
use merak_ir::Loc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Structural,
    Behaviour,
    Design,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Op {
    pub id: String,
    pub kind: String,
    pub layer: Layer,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence_before: Vec<Loc>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evidence_after: Vec<Loc>,
    pub origin: Origin,
    pub confidence: f64,
    /// HTTP entry points / callers whose behaviour this op reaches.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub affects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub ops: Vec<Op>,
    pub stats: Stats,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub entities_before: usize,
    pub entities_after: usize,
    pub entities_changed: usize,
}

impl Transition {
    pub fn behavioural(&self) -> impl Iterator<Item = &Op> {
        self.ops.iter().filter(|o| o.layer != Layer::Structural)
    }
}

struct Builder {
    ops: Vec<Op>,
}

impl Builder {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: &str,
        layer: Layer,
        subject: impl Into<String>,
        before: Option<String>,
        after: Option<String>,
        eb: Vec<Loc>,
        ea: Vec<Loc>,
    ) -> &mut Op {
        let subject = subject.into();
        let id = format!("{}:{}", kind.to_lowercase(), merak_ir::stable_hash(&format!("{kind}|{subject}|{before:?}|{after:?}")));
        self.ops.push(Op {
            id,
            kind: kind.into(),
            layer,
            subject,
            before,
            after,
            evidence_before: eb,
            evidence_after: ea,
            origin: Origin::Static,
            confidence: 1.0,
            affects: vec![],
            note: None,
        });
        self.ops.last_mut().unwrap()
    }
}

/// Correspondence between entity ids of the two versions.
#[derive(Debug, Default)]
pub struct Matching {
    /// before id → after id, for every entity present in both (possibly moved).
    pub forward: BTreeMap<String, String>,
    pub backward: BTreeMap<String, String>,
    /// before module → after module, for modules that moved as a whole.
    pub modules: BTreeMap<String, String>,
    /// Classes that moved: before class id → after class id.
    pub moved_classes: BTreeMap<String, String>,
    /// Free functions that moved or were renamed.
    pub moved_functions: BTreeMap<String, String>,
}

impl Matching {
    pub fn to_after(&self, id: &str) -> String {
        self.forward.get(id).cloned().unwrap_or_else(|| id.to_string())
    }

    fn module_to_after(&self, m: &str) -> String {
        self.modules.get(m).cloned().unwrap_or_else(|| m.to_string())
    }
}

pub fn match_entities(a: &Model, b: &Model) -> Matching {
    let mut m = Matching::default();
    for id in a.entities.keys() {
        if b.entities.contains_key(id) {
            m.forward.insert(id.clone(), id.clone());
        }
    }
    // Classes that disappeared and reappeared elsewhere with the same name and methods.
    let gone: Vec<_> = a.classes.values().filter(|c| !b.classes.contains_key(&c.id)).collect();
    let new: Vec<_> = b.classes.values().filter(|c| !a.classes.contains_key(&c.id)).collect();
    for ca in &gone {
        let names = |ms: &[String]| -> BTreeSet<String> { ms.iter().filter_map(|x| x.rsplit('.').next().map(str::to_string)).collect() };
        if let Some(cb) = new.iter().find(|cb| cb.name == ca.name && names(&cb.methods) == names(&ca.methods)) {
            m.moved_classes.insert(ca.id.clone(), cb.id.clone());
            for ma in &ca.methods {
                let suffix = ma.rsplit_once("::").map(|(_, q)| q).unwrap_or(ma);
                if let Some(mb) = cb.methods.iter().find(|mb| mb.ends_with(&format!("::{suffix}"))) {
                    m.forward.insert(ma.clone(), mb.clone());
                }
            }
        }
    }
    // Remaining free functions: same body hash and same name (moved) or same module (renamed).
    let unmatched_b: BTreeSet<&String> = b.entities.keys().filter(|id| !m.forward.values().any(|v| v == *id)).collect();
    for (ida, ea) in &a.entities {
        if m.forward.contains_key(ida) || ea.class.is_some() || ea.parent.is_some() {
            continue;
        }
        let candidate = unmatched_b.iter().find(|idb| {
            let eb = &b.entities[**idb];
            eb.class.is_none() && eb.parent.is_none() && eb.body_hash == ea.body_hash && (eb.name == ea.name || eb.module == ea.module)
        });
        if let Some(idb) = candidate {
            m.forward.insert(ida.clone(), (*idb).clone());
            m.moved_functions.insert(ida.clone(), (*idb).clone());
        }
    }
    // Closures follow their parents: `a::f.on("X")` ↔ `b::g.on("X")` when f ↔ g.
    let parents: Vec<(String, String)> = m.forward.iter().filter(|(x, y)| x != y).map(|(x, y)| (x.clone(), y.clone())).collect();
    for (pa, pb) in parents {
        for ida in a.entities.keys().filter(|k| k.starts_with(&format!("{pa}."))) {
            let idb = format!("{pb}{}", &ida[pa.len()..]);
            if b.entities.contains_key(&idb) {
                m.forward.insert(ida.clone(), idb);
            }
        }
    }
    // Whole-module moves: every entity of module A now lives in module B.
    let mut module_votes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (x, y) in &m.forward {
        let (ma, mb) = (&a.entities[x].module, &b.entities[y].module);
        module_votes.entry(ma.clone()).or_default().insert(mb.clone());
    }
    let b_modules: BTreeSet<&String> = b.entities.values().map(|e| &e.module).collect();
    for (ma, targets) in module_votes {
        if targets.len() == 1 && !b_modules.contains(&ma) {
            m.modules.insert(ma, targets.into_iter().next().unwrap());
        }
    }
    m.backward = m.forward.iter().map(|(x, y)| (y.clone(), x.clone())).collect();
    m
}

fn short(id: &str) -> &str {
    id.rsplit("::").next().unwrap_or(id)
}

fn is_auth_field(field: &str) -> bool {
    let f = field.rsplit('.').next().unwrap_or(field).to_lowercase();
    ["role", "roles", "permission", "permissions", "scope", "scopes", "isadmin", "admin", "group", "groups", "claims"].contains(&f.as_str())
}

fn effect_render(k: &EffectKey, info: &EffectInfo) -> String {
    format!("{} ({})", k.render(), info.modes.iter().cloned().collect::<Vec<_>>().join(", "))
}

fn is_sync_only(info: &EffectInfo) -> bool {
    info.modes.iter().all(|m| m == "sync")
}

fn is_async_only(info: &EffectInfo) -> bool {
    info.modes.iter().all(|m| m.starts_with("async"))
}

pub fn diff(a: &Model, b: &Model) -> Transition {
    let m = match_entities(a, b);
    let mut out = Builder { ops: vec![] };
    let mut changed = 0;

    // ------------------------------------------------------------ structural
    let moved_class_methods: BTreeSet<&String> = m.moved_classes.keys().filter_map(|c| a.classes.get(c)).flat_map(|c| c.methods.iter()).collect();
    for (ca, cb) in &m.moved_classes {
        out.push("ENTITY_MOVED", Layer::Structural, ca.clone(), None, Some(cb.clone()), vec![a.classes[ca].loc.clone()], vec![b.classes[cb].loc.clone()]);
    }
    for (fa, fb) in &m.moved_functions {
        let kind = if a.entities[fa].name == b.entities[fb].name { "ENTITY_MOVED" } else { "ENTITY_RENAMED" };
        out.push(kind, Layer::Structural, fa.clone(), None, Some(fb.clone()), vec![a.entities[fa].loc.clone()], vec![b.entities[fb].loc.clone()]);
    }
    let new_classes: BTreeSet<&String> = b.classes.keys().filter(|c| !a.classes.contains_key(*c) && !m.moved_classes.values().any(|v| v == *c)).collect();
    for c in &new_classes {
        out.push("ENTITY_ADDED", Layer::Structural, (*c).clone(), None, None, vec![], vec![b.classes[*c].loc.clone()]);
    }
    let new_class_methods: BTreeSet<&String> = new_classes.iter().filter_map(|c| b.classes.get(*c)).flat_map(|c| c.methods.iter()).collect();
    for (id, e) in &b.entities {
        if m.backward.contains_key(id) || new_class_methods.contains(id) || e.kind == merak_behaviour::EntityKind::Constructor {
            continue;
        }
        out.push("ENTITY_ADDED", Layer::Structural, id.clone(), None, None, vec![], vec![e.loc.clone()]);
    }
    let removed_classes: BTreeSet<&String> = a.classes.keys().filter(|c| !b.classes.contains_key(*c) && !m.moved_classes.contains_key(*c)).collect();
    let removed_class_methods: BTreeSet<&String> = removed_classes.iter().filter_map(|c| a.classes.get(*c)).flat_map(|c| c.methods.iter()).collect();
    for (id, e) in &a.entities {
        if m.forward.contains_key(id)
            || moved_class_methods.contains(id)
            || removed_class_methods.contains(id)
            || e.kind == merak_behaviour::EntityKind::Constructor
        {
            continue;
        }
        out.push("ENTITY_REMOVED", Layer::Structural, id.clone(), None, None, vec![e.loc.clone()], vec![]);
    }
    for c in a.classes.keys().filter(|c| !b.classes.contains_key(*c) && !m.moved_classes.contains_key(*c)) {
        out.push("ENTITY_REMOVED", Layer::Structural, c.clone(), None, None, vec![a.classes[c].loc.clone()], vec![]);
    }

    let deps_a: BTreeMap<(String, String), &Vec<Loc>> = a.module_deps.iter().map(|((x, y), l)| ((m.module_to_after(x), m.module_to_after(y)), l)).collect();
    for ((x, y), locs) in &b.module_deps {
        if !deps_a.contains_key(&(x.clone(), y.clone())) {
            out.push("DEPENDENCY_ADDED", Layer::Structural, format!("{x} → {y}"), None, None, vec![], locs.clone());
        }
    }
    for ((x, y), locs) in &deps_a {
        if !b.module_deps.contains_key(&(x.clone(), y.clone())) {
            out.push("DEPENDENCY_REMOVED", Layer::Structural, format!("{x} → {y}"), None, None, (*locs).clone(), vec![]);
        }
    }

    // ------------------------------------------------------------ behaviour: surface
    let routes_a: BTreeMap<String, &merak_behaviour::Route> = a.routes.iter().map(|r| (r.key(), r)).collect();
    let routes_b: BTreeMap<String, &merak_behaviour::Route> = b.routes.iter().map(|r| (r.key(), r)).collect();
    for (k, r) in &routes_b {
        if !routes_a.contains_key(k) {
            let effects = b.summaries.get(&r.handler).map(|s| s.effects.keys().map(|e| e.render()).collect::<Vec<_>>().join(", "));
            out.push("ENTRYPOINT_ADDED", Layer::Behaviour, k.clone(), None, effects, vec![], vec![r.loc.clone()]);
        }
    }
    for (k, r) in &routes_a {
        if !routes_b.contains_key(k) {
            out.push("ENTRYPOINT_REMOVED", Layer::Behaviour, k.clone(), None, None, vec![r.loc.clone()], vec![]);
        }
    }
    let subs_a: BTreeSet<(String, String)> = a.subscriptions.iter().map(|s| (s.event.clone(), m.to_after(&s.handler))).collect();
    for s in &b.subscriptions {
        if !subs_a.contains(&(s.event.clone(), s.handler.clone())) {
            let effects = b.summaries.get(&s.handler).map(|x| x.effects.keys().map(|e| e.render()).collect::<Vec<_>>().join(", "));
            out.push("EVENT_HANDLER_ADDED", Layer::Behaviour, s.event.clone(), None, effects, vec![], vec![s.loc.clone()]).note =
                Some(format!("handled by {}", short(&s.handler)));
        }
    }
    let subs_b: BTreeSet<(String, String)> = b.subscriptions.iter().map(|s| (s.event.clone(), s.handler.clone())).collect();
    for s in &a.subscriptions {
        if !subs_b.contains(&(s.event.clone(), m.to_after(&s.handler))) {
            out.push("EVENT_HANDLER_REMOVED", Layer::Behaviour, s.event.clone(), None, None, vec![s.loc.clone()], vec![]);
        }
    }

    // ------------------------------------------------------------ behaviour: per changed entity
    let (kids_a, kids_b) = (closures_by_parent(a), closures_by_parent(b));
    for (ida, idb) in &m.forward {
        let (ea, eb) = (&a.entities[ida], &b.entities[idb]);
        // Query filters belong to the top-level entity, closures included: a new
        // `$if(c, (qb) => qb.where(…))` narrows the method's query, and closure ids
        // shift when a sibling closure is inserted.
        let (fa, fb) = match ea.parent {
            None => (scoped_filters(a, &kids_a, ida), scoped_filters(b, &kids_b, idb)),
            Some(_) => (vec![], vec![]),
        };
        // Decorators are not in the body hash: `@Authenticated({ public: true })` alone is a change.
        if ea.body_hash == eb.body_hash && same_filters(&fa, &fb) && own_access(ea) == own_access(eb) {
            continue;
        }
        changed += 1;
        let (sa, sb) = (&a.summaries[ida], &b.summaries[idb]);
        let affects = b.entry_points_reaching(idb);
        let start = out.ops.len();

        // Authorization rules defined by predicate functions.
        if let (Some(pa), Some(pb)) = (&ea.returns, &eb.returns) {
            if pa != pb {
                auth_change(&mut out, idb, pa, pb, &ea.loc, &eb.loc);
            }
        }

        // Guards (excluding those on state fields this entity writes — see state machines).
        let state_fields: BTreeSet<&str> = ea
            .writes
            .iter()
            .chain(&eb.writes)
            .map(|w| w.field.as_str())
            .filter(|f| a.field_domains.contains_key(*f) || b.field_domains.contains_key(*f))
            .collect();
        let relevant = |p: &Pred| p.field().is_none_or(|f| !state_fields.contains(f));
        let ga: Vec<_> = sa.requires.iter().filter(|(_, p, _)| relevant(p)).map(|(k, p, l)| (rename_key(&m, k), p, l)).collect();
        let gb: Vec<_> = sb.requires.iter().filter(|(_, p, _)| relevant(p)).collect();
        let mut consumed_b = BTreeSet::new();
        for (ka, pa, la) in &ga {
            if let Some((i, (_, pb, lb))) = gb.iter().enumerate().find(|(_, (kb, _, _))| kb == ka) {
                consumed_b.insert(i);
                if pa != &pb && is_auth_pred(pa) {
                    auth_change(&mut out, idb, pa, pb, la, lb);
                }
                continue;
            }
            // Same auth field, different set: an inline authorization rule changed.
            if let Some((i, (_, pb, lb))) =
                gb.iter().enumerate().find(|(i, (_, pb, _))| !consumed_b.contains(i) && is_auth_pred(pa) && pa.field() == pb.field())
            {
                consumed_b.insert(i);
                auth_change(&mut out, idb, pa, pb, la, lb);
                continue;
            }
            out.push("GUARD_REMOVED", Layer::Behaviour, idb.clone(), Some(pa.render()), None, vec![(*la).clone()], vec![]);
        }
        for (i, (_, pb, lb)) in gb.iter().enumerate() {
            if !consumed_b.contains(&i) {
                out.push("GUARD_ADDED", Layer::Behaviour, idb.clone(), None, Some(pb.render()), vec![], vec![lb.clone()]);
            }
        }

        // Validations reached.
        let va: BTreeMap<String, &Loc> = sa.validations.iter().map(|(v, l)| (m.to_after(v), l)).collect();
        let removed_validators: BTreeSet<&String> = va.keys().filter(|v| !sb.validations.contains_key(*v)).collect();
        for v in &removed_validators {
            out.push("VALIDATION_REMOVED", Layer::Behaviour, idb.clone(), Some((*v).clone()), None, vec![va[*v].clone()], vec![]);
        }
        for (v, l) in &sb.validations {
            if !va.contains_key(v) {
                out.push("VALIDATION_ADDED", Layer::Behaviour, idb.clone(), None, Some(v.clone()), vec![], vec![l.clone()]);
            }
        }

        // Effects reached (transitively, including through event handlers).
        for (k, ib) in &sb.effects {
            match sa.effects.get(k) {
                None => {
                    out.push("EFFECT_ADDED", Layer::Behaviour, idb.clone(), None, Some(effect_render(k, ib)), vec![], ib.evidence.iter().cloned().collect());
                }
                Some(ia) if is_sync_only(ia) && is_async_only(ib) => {
                    out.push(
                        "EFFECT_MADE_ASYNC",
                        Layer::Behaviour,
                        idb.clone(),
                        Some(effect_render(k, ia)),
                        Some(effect_render(k, ib)),
                        ia.evidence.iter().cloned().collect(),
                        ib.evidence.iter().cloned().collect(),
                    );
                }
                Some(ia) if is_async_only(ia) && is_sync_only(ib) => {
                    out.push(
                        "EFFECT_MADE_SYNC",
                        Layer::Behaviour,
                        idb.clone(),
                        Some(effect_render(k, ia)),
                        Some(effect_render(k, ib)),
                        ia.evidence.iter().cloned().collect(),
                        ib.evidence.iter().cloned().collect(),
                    );
                }
                Some(_) => {}
            }
        }
        for (k, ia) in &sa.effects {
            if sb.effects.contains_key(k) {
                continue;
            }
            // Effects that only came from a removed validation are part of that op.
            if !removed_validators.is_empty() && ia.via.iter().all(|v| removed_validators.contains(&m.to_after(v))) {
                continue;
            }
            out.push("EFFECT_REMOVED", Layer::Behaviour, idb.clone(), Some(effect_render(k, ia)), None, ia.evidence.iter().cloned().collect(), vec![]);
        }

        // Data touched (transitively).
        let new_writes: Vec<&String> = sb.writes.difference(&sa.writes).collect();
        let new_reads: Vec<&String> = sb.reads.difference(&sa.reads).filter(|r| !sb.writes.contains(*r)).collect();
        if !new_writes.is_empty() || !new_reads.is_empty() {
            let mut parts = vec![];
            if !new_writes.is_empty() {
                parts.push(format!("writes {}", new_writes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            }
            if !new_reads.is_empty() {
                parts.push(format!("reads {}", new_reads.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            }
            out.push("DATAFLOW_EXPANDED", Layer::Behaviour, idb.clone(), None, Some(parts.join("; ")), vec![], vec![eb.loc.clone()]);
        }
        let lost_writes: Vec<&String> = sa.writes.difference(&sb.writes).collect();
        if !lost_writes.is_empty() {
            out.push(
                "DATAFLOW_RESTRICTED",
                Layer::Behaviour,
                idb.clone(),
                Some(format!("writes {}", lost_writes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))),
                None,
                vec![ea.loc.clone()],
                vec![],
            );
        }

        query_filters(&mut out, idb, &fa, &fb);
        access_change(&mut out, idb, &sa.access, &sb.access);

        for op in &mut out.ops[start..] {
            op.affects = affects.clone();
        }
    }

    // ------------------------------------------------------------ behaviour: new code
    // An added entity is described by what it does, not only that it exists, unless it
    // was extracted: every caller existed before and behaves as it did.
    let changed_behaviour: BTreeSet<&str> = out.ops.iter().filter(|o| o.layer != Layer::Structural).map(|o| o.subject.as_str()).collect();
    let mut callers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in b.entities.values() {
        for c in &e.calls {
            callers.entry(c.target.as_str()).or_default().push(e.id.as_str());
        }
    }
    let mut described = vec![];
    for (id, e) in &b.entities {
        if m.backward.contains_key(id) || e.parent.is_some() || e.kind == merak_behaviour::EntityKind::Constructor {
            continue;
        }
        let cs = callers.get(id.as_str()).map(Vec::as_slice).unwrap_or_default();
        if !cs.is_empty() && cs.iter().all(|c| m.backward.contains_key(*c) && !changed_behaviour.contains(c)) {
            continue;
        }
        if let Some(summary) = describe(b, &kids_b, id) {
            described.push((id.clone(), summary, e.loc.clone(), b.entry_points_reaching(id)));
        }
    }
    for (id, summary, loc, affects) in described {
        out.push("BEHAVIOUR_ADDED", Layer::Behaviour, id, None, Some(summary), vec![], vec![loc]).affects = affects;
    }

    // ------------------------------------------------------------ behaviour: validation schemas
    for (key, sb) in &b.schemas {
        let (module, name) = key.split_once("::").unwrap_or(("", key));
        let old = a.schemas.iter().find(|(k, _)| k.split_once("::").is_some_and(|(ma, na)| na == name && m.module_to_after(ma) == module));
        let Some((_, sa)) = old else { continue };
        // `*` is the schema as a whole (refinements, strictness): SCHEMA_CHANGED.
        let subject = |f: &str| if f == "*" { key.clone() } else { format!("{key}.{f}") };
        let kind = |f: &str, field_kind: &'static str| if f == "*" { "SCHEMA_CHANGED" } else { field_kind };
        for (f, cb) in &sb.fields {
            match sa.fields.get(f) {
                None => {
                    out.push(kind(f, "SCHEMA_FIELD_ADDED"), Layer::Behaviour, subject(f), None, Some(cb.clone()), vec![], vec![sb.loc.clone()]);
                }
                Some(ca) if ca != cb => {
                    out.push(
                        kind(f, "SCHEMA_FIELD_CHANGED"),
                        Layer::Behaviour,
                        subject(f),
                        Some(ca.clone()),
                        Some(cb.clone()),
                        vec![sa.loc.clone()],
                        vec![sb.loc.clone()],
                    );
                }
                Some(_) => {}
            }
        }
        for (f, ca) in &sa.fields {
            if !sb.fields.contains_key(f) {
                out.push(kind(f, "SCHEMA_FIELD_REMOVED"), Layer::Behaviour, subject(f), Some(ca.clone()), None, vec![sa.loc.clone()], vec![]);
            }
        }
    }

    // ------------------------------------------------------------ behaviour: state machines
    let fields: BTreeSet<&String> = a.state_machines.keys().chain(b.state_machines.keys()).collect();
    for f in fields {
        let (sma, smb) = (a.state_machines.get(f), b.state_machines.get(f));
        let targets: BTreeSet<String> = sma.map(|s| s.targets()).unwrap_or_default().union(&smb.map(|s| s.targets()).unwrap_or_default()).cloned().collect();
        for to in targets {
            let src_a = sma.map(|s| s.sources_of(&to)).unwrap_or_default();
            let src_b = smb.map(|s| s.sources_of(&to)).unwrap_or_default();
            if src_a == src_b {
                continue;
            }
            let ev = |sm: Option<&merak_behaviour::StateMachine>| -> Vec<Loc> {
                sm.map(|s| s.edges.iter().filter(|e| e.to == to).map(|e| e.loc.clone()).collect()).unwrap_or_default()
            };
            let subject = format!("{f} → {to}");
            let (kind, before, after) = if src_a.is_empty() {
                ("STATE_TRANSITION_ADDED", None, Some(render_set(&src_b)))
            } else if src_b.is_empty() {
                ("STATE_TRANSITION_REMOVED", Some(render_set(&src_a)), None)
            } else if src_b.is_superset(&src_a) {
                ("STATE_TRANSITION_SOURCE_WIDENED", Some(render_set(&src_a)), Some(render_set(&src_b)))
            } else if src_b.is_subset(&src_a) {
                ("STATE_TRANSITION_SOURCE_NARROWED", Some(render_set(&src_a)), Some(render_set(&src_b)))
            } else {
                ("STATE_TRANSITION_CHANGED", Some(render_set(&src_a)), Some(render_set(&src_b)))
            };
            let writers: Vec<String> =
                smb.map(|s| s.edges.iter().filter(|e| e.to == to && !src_a.is_superset(&e.from)).map(|e| e.writer.clone()).collect()).unwrap_or_default();
            let affects: Vec<String> = writers.iter().flat_map(|w| b.entry_points_reaching(w)).collect::<BTreeSet<_>>().into_iter().collect();
            let op = out.push(kind, Layer::Behaviour, subject, before, after, ev(sma), ev(smb));
            op.affects = affects;
            if !writers.is_empty() {
                op.note = Some(format!("new path via {}", writers.iter().map(|w| short(w)).collect::<Vec<_>>().join(", ")));
            }
        }
    }

    // ------------------------------------------------------------ design: inferred invariants
    for inv in &a.invariants {
        for (w, wf) in infer::writers(b, &inv.field, &inv.value, matches!(inv.requirement, merak_behaviour::Requirement::CoWrite(_))) {
            let rename = |id: &str| m.to_after(id);
            if infer::satisfies(b, inv, w, &rename) {
                continue;
            }
            // Only report violations this change introduced.
            let previously = m.backward.get(&w.id).and_then(|old| a.entities.get(old)).is_some_and(|old| {
                !infer::satisfies(a, inv, old, &|x: &str| x.to_string())
                    && old.writes.iter().any(|x| x.field == inv.field && x.value.as_deref() == Some(&inv.value))
            });
            if previously {
                continue;
            }
            let op = out.push(
                "INVARIANT_VIOLATED",
                Layer::Design,
                inv.statement.clone(),
                Some(format!("holds for {} writer(s)", inv.support)),
                Some(format!("{} does not satisfy it", short(&w.id))),
                inv.evidence.clone(),
                vec![wf.loc.clone()],
            );
            op.origin = Origin::Inferred;
            op.confidence = inv.confidence;
            op.affects = b.entry_points_reaching(&w.id);
        }
    }

    // ------------------------------------------------------------ design: declared rules
    let key = |kind: &str, subject: &str| (kind.to_string(), subject.to_string());
    let va: BTreeMap<(String, String), BTreeSet<String>> = a.violations.iter().fold(BTreeMap::new(), |mut acc, v| {
        acc.entry(key(&v.kind, &v.subject)).or_default().insert(map_detail(&m, &v.detail));
        acc
    });
    let mut vb: BTreeMap<(String, String), Vec<&merak_behaviour::Violation>> = BTreeMap::new();
    for v in &b.violations {
        vb.entry(key(&v.kind, &v.subject)).or_default().push(v);
    }
    for ((kind, subject), vs) in &vb {
        let fresh: Vec<&&merak_behaviour::Violation> =
            vs.iter().filter(|v| !va.get(&(kind.clone(), subject.clone())).is_some_and(|d| d.contains(&v.detail))).collect();
        if fresh.is_empty() {
            continue;
        }
        let op_kind = if kind == "layer" { "LAYER_VIOLATION_INTRODUCED" } else { "OWNERSHIP_VIOLATION_INTRODUCED" };
        let op = out.push(
            op_kind,
            Layer::Design,
            subject.clone(),
            None,
            Some(fresh.iter().map(|v| v.detail.clone()).collect::<Vec<_>>().join("; ")),
            vec![],
            fresh.iter().flat_map(|v| v.evidence.clone()).collect(),
        );
        op.origin = Origin::Declared;
    }
    for ((kind, subject), details) in &va {
        let still: BTreeSet<String> = vb.get(&(kind.clone(), subject.clone())).map(|vs| vs.iter().map(|v| v.detail.clone()).collect()).unwrap_or_default();
        if details.iter().any(|d| !still.contains(d)) && still.is_empty() {
            let op_kind = if kind == "layer" { "LAYER_VIOLATION_RESOLVED" } else { "OWNERSHIP_VIOLATION_RESOLVED" };
            out.push(op_kind, Layer::Design, subject.clone(), None, None, vec![], vec![]).origin = Origin::Declared;
        }
    }

    // ------------------------------------------------------------ unclassified
    // A changed entity whose calls changed, but which no typed op explains: say so
    // rather than let it count towards a pure refactor.
    let explained: BTreeSet<&str> = out.ops.iter().filter(|o| o.layer != Layer::Structural).map(|o| o.subject.as_str()).collect();
    let mut unclassified = vec![];
    for (ida, idb) in &m.forward {
        let (ea, eb) = (&a.entities[ida], &b.entities[idb]);
        if ea.body_hash == eb.body_hash || explained.contains(idb.as_str()) {
            continue;
        }
        // Calls into helpers that exist on one side only (extracted or inlined) count as their bodies.
        let sa = inline_shapes(a, ida, &|t| m.forward.contains_key(t));
        let sb = inline_shapes(b, idb, &|t| m.backward.contains_key(t));
        let (sa, sb): (Vec<&CallShape>, Vec<&CallShape>) = (sa.iter().collect(), sb.iter().collect());
        let (gone, new) = multiset_diff(&sa, &sb);
        if gone.is_empty() && new.is_empty() {
            continue;
        }
        let show = |xs: &[&CallShape]| (!xs.is_empty()).then(|| xs.iter().map(|c| c.key()).collect::<Vec<_>>().join("; "));
        let evidence = |xs: &[&CallShape]| {
            let mut locs: Vec<Loc> = xs.iter().map(|c| c.loc.clone()).collect();
            locs.dedup();
            locs
        };
        unclassified.push((idb.clone(), show(&gone), show(&new), evidence(&gone), evidence(&new), b.entry_points_reaching(idb)));
    }
    for (subject, before, after, eb, ea, affects) in unclassified {
        let op = out.push("UNCLASSIFIED_CHANGE", Layer::Behaviour, subject, before, after, eb, ea);
        op.confidence = 0.5;
        op.affects = affects;
        op.note = Some("calls changed in a way the behaviour model does not classify; review manually".into());
    }

    // ------------------------------------------------------------ logging
    for (ida, idb) in &m.forward {
        let (ea, eb) = (&a.entities[ida], &b.entities[idb]);
        if ea.body_hash == eb.body_hash {
            continue;
        }
        let (la, lb): (Vec<&CallShape>, Vec<&CallShape>) = (ea.logs.iter().collect(), eb.logs.iter().collect());
        let (gone, new) = multiset_diff(&la, &lb);
        if gone.is_empty() && new.is_empty() {
            continue;
        }
        let show = |xs: &[&CallShape]| (!xs.is_empty()).then(|| xs.iter().map(|c| c.key()).collect::<Vec<_>>().join("; "));
        let locs = |xs: &[&CallShape]| xs.iter().map(|c| c.loc.clone()).collect();
        out.push("LOGGING_CHANGED", Layer::Structural, idb.clone(), show(&gone), show(&new), locs(&gone), locs(&new));
    }

    // ------------------------------------------------------------ pure refactor
    // Changed logging is not behaviour, but it is not a refactor either.
    let structural_change = !out.ops.is_empty() || changed > 0;
    if structural_change && out.ops.iter().all(|o| o.layer == Layer::Structural && o.kind != "LOGGING_CHANGED") {
        out.push("PURE_REFACTOR", Layer::Structural, "*", None, Some(format!("{changed} entities changed; behaviour summaries unchanged")), vec![], vec![]);
    }

    Transition { ops: out.ops, stats: Stats { entities_before: a.entities.len(), entities_after: b.entities.len(), entities_changed: changed } }
}

fn map_detail(m: &Matching, detail: &str) -> String {
    // Details are `mod → mod` (layer) or an entity id (ownership).
    match detail.split_once(" → ") {
        Some((x, y)) => format!("{} → {}", m.module_to_after(x), m.module_to_after(y)),
        None => m.to_after(detail),
    }
}

fn rename_key(m: &Matching, key: &str) -> String {
    match key.split_once("call:") {
        Some((neg, id)) => format!("{neg}call:{}", m.to_after(id)),
        None => key.to_string(),
    }
}

fn is_auth_pred(p: &Pred) -> bool {
    match p {
        Pred::In { field, .. } | Pred::NotIn { field, .. } => is_auth_field(field),
        Pred::And(ps) | Pred::Or(ps) => ps.iter().any(is_auth_pred),
        _ => false,
    }
}

fn auth_change(out: &mut Builder, subject: &str, pa: &Pred, pb: &Pred, la: &Loc, lb: &Loc) {
    if !is_auth_pred(pa) && !is_auth_pred(pb) {
        out.push("GUARD_CHANGED", Layer::Behaviour, subject, Some(pa.render()), Some(pb.render()), vec![la.clone()], vec![lb.clone()]);
        return;
    }
    let kind = match (pa, pb) {
        (Pred::In { field: fa, values: va }, Pred::In { field: fb, values: vb }) if fa == fb => {
            if vb.is_superset(va) {
                "AUTH_WIDENED"
            } else if vb.is_subset(va) {
                "AUTH_NARROWED"
            } else {
                "AUTH_CHANGED"
            }
        }
        _ => "AUTH_CHANGED",
    };
    out.push(kind, Layer::Behaviour, subject, Some(pa.render()), Some(pb.render()), vec![la.clone()], vec![lb.clone()]);
}

/// What an entity does, in one line: effects reached, access required, query filters
/// (closures included), guards, validations and fields written. `None` when it does none of these.
fn describe(model: &Model, kids: &BTreeMap<&str, Vec<&str>>, id: &str) -> Option<String> {
    const MAX: usize = 8;
    let s = model.summaries.get(id)?;
    let list = |label: &str, items: Vec<String>| -> Option<String> {
        let mut items = items;
        items.dedup();
        if items.is_empty() {
            return None;
        }
        let more = items.len().saturating_sub(MAX);
        items.truncate(MAX);
        Some(format!("{label}: {}{}", items.join(", "), if more > 0 { format!(", +{more} more") } else { String::new() }))
    };
    let mut filters: Vec<String> = scoped_filters(model, kids, id).iter().map(|f| f.expr.clone()).collect();
    filters.sort();
    let effects: Vec<&EffectKey> = model.effects_outside_checks(id).into_iter().map(|(k, _)| k).collect();
    let parts: Vec<String> = [
        list("effects", effects.iter().map(|k| k.render()).collect()),
        list("requires", s.access.keys().cloned().collect()),
        list("filters", filters),
        list("guards", s.requires.iter().map(|(_, p, _)| p.render()).collect()),
        list("validates", s.validations.keys().map(|v| short(v).to_string()).collect()),
        list("writes", s.writes.iter().cloned().collect()),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn own_access(e: &merak_behaviour::Entity) -> BTreeSet<&str> {
    e.access.iter().map(|x| x.requirement.as_str()).collect()
}

fn closures_by_parent(model: &Model) -> BTreeMap<&str, Vec<&str>> {
    let mut out: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in model.entities.values() {
        if let Some(p) = &e.parent {
            out.entry(p.as_str()).or_default().push(e.id.as_str());
        }
    }
    out
}

/// Filters of `id` and of every closure nested in it.
fn scoped_filters<'x>(model: &'x Model, kids: &BTreeMap<&str, Vec<&str>>, id: &str) -> Vec<&'x QueryFilter> {
    let mut out = vec![];
    let mut stack = vec![id];
    while let Some(cur) = stack.pop() {
        out.extend(model.entities.get(cur).into_iter().flat_map(|e| &e.filters));
        stack.extend(kids.get(cur).into_iter().flatten());
    }
    out
}

fn same_filters(a: &[&QueryFilter], b: &[&QueryFilter]) -> bool {
    fn sorted<'x>(xs: &[&'x QueryFilter]) -> Vec<&'x str> {
        let mut v: Vec<&str> = xs.iter().map(|f| f.expr.as_str()).collect();
        v.sort_unstable();
        v
    }
    sorted(a) == sorted(b)
}

/// Access requirements reached before and after. Dropping a requirement or adding
/// a grant widens access; the reverse narrows it.
fn access_change(out: &mut Builder, subject: &str, a: &BTreeMap<String, AccessFact>, b: &BTreeMap<String, AccessFact>) {
    if a.keys().eq(b.keys()) {
        return;
    }
    let gone: Vec<&AccessFact> = a.iter().filter(|(k, _)| !b.contains_key(*k)).map(|(_, f)| f).collect();
    let new: Vec<&AccessFact> = b.iter().filter(|(k, _)| !a.contains_key(*k)).map(|(_, f)| f).collect();
    let widens = gone.iter().any(|f| !f.grant) || new.iter().any(|f| f.grant);
    let narrows = new.iter().any(|f| !f.grant) || gone.iter().any(|f| f.grant);
    let kind = match (widens, narrows) {
        (true, false) => "AUTH_WIDENED",
        (false, true) => "AUTH_NARROWED",
        _ => "AUTH_CHANGED",
    };
    let show = |fs: &[&AccessFact]| (!fs.is_empty()).then(|| fs.iter().map(|f| f.requirement.as_str()).collect::<Vec<_>>().join(", "));
    let locs = |fs: &[&AccessFact]| fs.iter().map(|f| f.loc.clone()).collect();
    out.push(kind, Layer::Behaviour, subject, show(&gone), show(&new), locs(&gone), locs(&new));
}

/// Query predicates that appeared or disappeared in one entity. A removed filter
/// widens the rows a query reaches; one replaced on the same column changed it.
fn query_filters(out: &mut Builder, subject: &str, fa: &[&QueryFilter], fb: &[&QueryFilter]) {
    let mut gone: Vec<&QueryFilter> = fa.to_vec();
    let mut new = vec![];
    for &f in fb {
        match gone.iter().position(|g| g.expr == f.expr) {
            Some(i) => {
                gone.remove(i);
            }
            None => new.push(f),
        }
    }
    // The column a comparison constrains: `asset.ownerId = _` → `asset.ownerId`.
    let column = |f: &QueryFilter| f.expr.split(' ').next().unwrap_or("").to_string();
    let mut pairs = vec![];
    gone.retain(|g| match new.iter().position(|n| column(n) == column(g)) {
        Some(i) => {
            pairs.push((*g, new.remove(i)));
            false
        }
        None => true,
    });
    if let ([g], [n]) = (gone.as_slice(), new.as_slice()) {
        pairs.push((*g, *n));
        (gone, new) = (vec![], vec![]);
    }
    for (g, n) in pairs {
        out.push("QUERY_FILTER_CHANGED", Layer::Behaviour, subject, Some(g.expr.clone()), Some(n.expr.clone()), vec![g.loc.clone()], vec![n.loc.clone()]);
    }
    for g in gone {
        out.push("QUERY_FILTER_REMOVED", Layer::Behaviour, subject, Some(g.expr.clone()), None, vec![g.loc.clone()], vec![]).note =
            Some("the query no longer applies this predicate: it can reach more rows".into());
    }
    for n in new {
        out.push("QUERY_FILTER_ADDED", Layer::Behaviour, subject, None, Some(n.expr.clone()), vec![], vec![n.loc.clone()]).note =
            Some("the query now applies this predicate: it reaches fewer rows".into());
    }
}

/// Call shapes of `id`, with calls to entities absent from the other version
/// (`matched` is false) replaced by those entities' own shapes, under the
/// conditions of the call site as well as their own.
fn inline_shapes(model: &Model, id: &str, matched: &dyn Fn(&str) -> bool) -> Vec<CallShape> {
    /// `inlined`: `id` is a helper seen from its caller, so its own guards are conditions too.
    fn walk(model: &Model, id: &str, outer: &[String], inlined: bool, matched: &dyn Fn(&str) -> bool, seen: &mut BTreeSet<String>, out: &mut Vec<CallShape>) {
        let Some(e) = model.entities.get(id) else { return };
        for c in &e.call_shapes {
            let mut when: Vec<String> = c.when.iter().chain(outer).chain(c.guards.iter().filter(|_| inlined)).cloned().collect();
            when.sort();
            when.dedup();
            match &c.target {
                Some(t) if !matched(t) && model.entities.contains_key(t) && seen.insert(t.clone()) => walk(model, t, &when, true, matched, seen, out),
                _ => out.push(CallShape { when, ..c.clone() }),
            }
        }
    }
    let mut out = vec![];
    walk(model, id, &[], false, matched, &mut BTreeSet::from([id.to_string()]), &mut out);
    out
}

/// Items only in `a`, and only in `b`, comparing shapes as multisets (locations ignored).
fn multiset_diff<'x>(a: &[&'x CallShape], b: &[&'x CallShape]) -> (Vec<&'x CallShape>, Vec<&'x CallShape>) {
    let mut left: BTreeMap<String, Vec<&CallShape>> = BTreeMap::new();
    for x in a {
        left.entry(x.key()).or_default().push(x);
    }
    let mut only_b = vec![];
    for y in b {
        if left.get_mut(&y.key()).and_then(|v| v.pop()).is_none() {
            only_b.push(*y);
        }
    }
    (left.into_values().flatten().collect(), only_b)
}
