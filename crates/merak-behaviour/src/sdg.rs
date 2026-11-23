//! The control half of a system dependence graph (Horwitz–Reps–Binkley): when a function
//! reaches each effect, as a condition on its own inputs. A callee's condition is read at
//! each call in the caller's names (its parameters replaced by the arguments) and joined to
//! the call's own condition; the ways to reach an effect are or-ed. So a condition inside a
//! helper is a condition of every caller, in that caller's terms.

use crate::extract::{all, any};
use crate::model::*;
use crate::names::{call_names, destructured, is_ident, rename, Names};
use crate::pred::Pred;
use std::collections::{BTreeMap, BTreeSet};

/// Conditions longer than this are not kept: past it, they say little a reader can use.
const MAX_ATOMS: usize = 24;

/// Each entity's reach conditions, by effect, with the same keys as its summary's effects.
pub fn compute(model: &Model, sums: &BTreeMap<String, Summary>) -> BTreeMap<String, BTreeMap<EffectKey, Reach>> {
    // Functions something calls for real: a reference to one (`<fn goToLink>`, a nested
    // function its parent only defines or hands on) is not another way to reach it.
    let called: BTreeSet<&str> =
        model.entities.values().flat_map(|e| e.call_shapes.iter()).filter(|s| !s.shape.starts_with("<fn ")).filter_map(|s| s.target.as_deref()).collect();
    let g = Graph { model, sums, called };
    let mut memo = BTreeMap::new();
    for id in model.entities.keys() {
        let mut stack = BTreeSet::new();
        reach_of(&g, id, &mut memo, &mut stack);
    }
    memo
}

struct Graph<'m> {
    model: &'m Model,
    sums: &'m BTreeMap<String, Summary>,
    called: BTreeSet<&'m str>,
}

impl Graph<'_> {
    /// The call is only a reference to a function that is called for real elsewhere.
    fn reference_only(&self, e: &Entity, c: &CallFact) -> bool {
        self.called.contains(c.target.as_str())
            && e.call_shapes.iter().any(|s| s.target.as_deref() == Some(c.target.as_str()) && s.loc == c.loc && s.shape.starts_with("<fn "))
    }
}

fn reach_of(g: &Graph, id: &str, memo: &mut BTreeMap<String, BTreeMap<EffectKey, Reach>>, stack: &mut BTreeSet<String>) -> BTreeMap<EffectKey, Reach> {
    let (model, sums) = (g.model, g.sums);
    if let Some(r) = memo.get(id) {
        return r.clone();
    }
    let Some(e) = model.entities.get(id) else { return BTreeMap::new() };
    // Back to a function on the way: no way of reaching that is not counted already.
    if !stack.insert(id.to_string()) {
        return BTreeMap::new();
    }
    let mut ways: BTreeMap<EffectKey, (Vec<Pred>, Vec<Pred>)> = BTreeMap::new();
    let mut add = |k: EffectKey, when: Pred, path: Pred| {
        let w = ways.entry(k).or_default();
        w.0.push(when);
        w.1.push(path);
    };
    let handlers = |event: &str| model.subscriptions.iter().filter(move |s| s.event == event).map(|s| s.handler.clone()).collect::<Vec<_>>();
    for f in &e.effects {
        add(f.key.clone(), f.when.clone(), f.path.clone());
        // What an event's handlers do happens when it is emitted (their own conditions are
        // about the event's data, which has no name here).
        if f.key.kind == "event_emit" {
            for h in handlers(&f.key.target) {
                for k in reach_of(g, &h, memo, stack).into_keys() {
                    add(k, f.when.clone(), f.path.clone());
                }
            }
        }
    }
    for c in e.calls.iter().filter(|c| !g.reference_only(e, c)) {
        let (Some(t), Some(cs)) = (model.entities.get(&c.target), sums.get(&c.target)) else { continue };
        let mut names: Names = call_names(t, c, &Names::new());
        // A parameter the call gives no usable value for is unknown here, not a name of ours.
        for p in &t.params {
            let name = p.split(": ").next().unwrap_or(p).trim_end_matches('?').trim_start_matches("...");
            let bound = if is_ident(name) { vec![name.to_string()] } else { destructured(p).into_iter().map(|(_, l)| l).collect() };
            for b in bound {
                names.entry(b).or_insert(None);
            }
        }
        let inner = |s: &str| rename(s, &names);
        for (k, r) in reach_of(g, &c.target, memo, stack) {
            // A target the callee takes as a parameter: what this call passes there.
            let param = cs.by_param.iter().find(|(_, ks)| ks.contains(&k)).map(|(i, _)| *i);
            let k = match param.and_then(|i| c.arg_skeletons.get(i).cloned().flatten()) {
                Some(target) => EffectKey { target, ..k },
                None => k,
            };
            // A condition on something the caller cannot name (a closure, `{…}`) is unknown
            // here, so it is left out: the effect may happen.
            let when = r.when.map_operands(&inner).unwrap_or(Pred::True);
            let path = r.path.map_operands(&inner).unwrap_or(Pred::True);
            add(k, all(vec![c.when.clone(), when]), all(vec![c.path.clone(), path]));
        }
    }
    let out: BTreeMap<EffectKey, Reach> = ways.into_iter().map(|(k, (w, p))| (k, Reach { when: bounded(any(w)), path: bounded(any(p)) })).collect();
    stack.remove(id);
    memo.insert(id.to_string(), out.clone());
    out
}

/// `p`, or `True` when it has grown past [`MAX_ATOMS`].
fn bounded(p: Pred) -> Pred {
    if p.atom_keys().len() > MAX_ATOMS {
        Pred::True
    } else {
        p
    }
}

/// A decision that reads a field: a check (`check`) or a branch some work runs under, with
/// the condition as written there.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldUse {
    pub entity: String,
    pub check: bool,
    pub cond: String,
    pub loc: merak_ir::Loc,
}

/// The data half's first edge: for each typed field (`Order.Status`), the decisions anywhere
/// in the program that test it, so a changed write leads to what depends on the value.
pub fn decisions(model: &Model) -> BTreeMap<String, Vec<FieldUse>> {
    fn leaves<'p>(p: &'p Pred, out: &mut Vec<&'p Pred>) {
        match p {
            Pred::And(ps) | Pred::Or(ps) => ps.iter().for_each(|q| leaves(q, out)),
            Pred::True | Pred::False => {}
            q => out.push(q),
        }
    }
    let typed = |f: &str| {
        f.split_once('.').is_some_and(|(t, rest)| {
            t.starts_with(|c: char| c.is_ascii_uppercase()) && !rest.is_empty() && rest.chars().all(|c| c.is_alphanumeric() || c == '_')
        })
    };
    let mut out: BTreeMap<String, Vec<FieldUse>> = BTreeMap::new();
    for e in model.entities.values() {
        let mut found: Vec<(&Pred, bool, &merak_ir::Loc)> = e.guards.iter().map(|g| (&g.requires, true, &g.loc)).collect();
        found.extend(e.calls.iter().map(|c| (&c.path, false, &c.loc)));
        found.extend(e.effects.iter().map(|f| (&f.path, false, &f.loc)));
        let mut seen = BTreeSet::new();
        for (p, check, loc) in found {
            let mut ls = vec![];
            leaves(p, &mut ls);
            for leaf in ls {
                for (_, ops) in leaf.atom_operands() {
                    for f in ops.iter().filter(|f| typed(f)) {
                        let cond = leaf.render();
                        if seen.insert((f.clone(), cond.clone())) {
                            out.entry(f.clone()).or_default().push(FieldUse { entity: e.id.clone(), check, cond, loc: loc.clone() });
                        }
                    }
                }
            }
        }
    }
    out
}

/// Whether `text` reads the name `x` (not as a field `.x`, or part of a word).
pub fn mentions(text: &str, x: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(x).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + x.len()..].chars().next();
        !before.is_some_and(|c| ident(c) || c == '.' || c == '-') && !after.is_some_and(ident)
    })
}

/// What reads a component's state value `x`, in the component and the functions inside it:
/// computed attributes, shown content, conditions, props passed down, handlers.
pub fn state_readers(model: &Model, component: &str, x: &str) -> Vec<String> {
    let inside = |id: &str| {
        let mut cur = Some(id.to_string());
        while let Some(c) = cur {
            if c == component {
                return true;
            }
            cur = model.entities.get(&c).and_then(|e| e.parent.clone());
        }
        false
    };
    let mut out: Vec<String> = vec![];
    let mut add = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for e in model.entities.values().filter(|e| inside(&e.id)) {
        // A component's props are read below, from its render; markup's own attributes here.
        for b in e.binds.iter().filter(|b| mentions(&b.does, x) && !b.element.trim_start_matches('<').starts_with(|c: char| c.is_ascii_uppercase())) {
            let v = holes(&b.does, x).unwrap_or_else(|| if b.does.starts_with('`') { b.does.clone() } else { format!("`{}`", b.does) });
            add(format!("{} {} {v}", b.element, b.event));
        }
        for o in &e.outputs {
            for r in o.renders.iter().filter(|r| mentions(&r.content, x) || mentions(&r.when, x)) {
                let when = if r.when.is_empty() { String::new() } else { format!(" when `{}`", r.when) };
                add(format!("{} shows `{}`{when}", r.style.split('.').next().unwrap_or(&r.style), r.content));
            }
            if o.when.iter().any(|w| mentions(w, x)) {
                add(format!("what it shows when `{}`", o.when.join(" ∧ ")));
            }
        }
        for c in e.calls.iter().filter(|c| !c.props.is_empty()) {
            for (k, v) in c.props.iter().filter(|(_, v)| mentions(v, x)) {
                let comp = c.target.rsplit("::").next().unwrap_or(&c.target);
                add(format!("<{comp}> {k}={{{v}}}"));
            }
        }
        for s in e.call_shapes.iter().filter(|s| s.when.iter().any(|w| mentions(w, x))) {
            add(format!("`{}` when `{}`", s.shape, s.when.join(" ∧ ")));
        }
        for h in e.handlers.iter().filter(|h| mentions(&h.does, x)) {
            add(format!("{} on {} `{}`", h.element, h.event, h.does));
        }
    }
    out
}

/// The parts of a template that read `x`: `` `flex ${on ? "a" : "b"}` `` → `` `${on ? "a" : "b"}` ``.
fn holes(text: &str, x: &str) -> Option<String> {
    let t = text.strip_prefix('`')?.strip_suffix('`')?;
    let mut out = vec![];
    let mut rest = t;
    while let Some(i) = rest.find("${") {
        let mut depth = 0;
        let mut end = None;
        for (j, ch) in rest[i..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end?;
        let hole = &rest[i..=end];
        if mentions(hole, x) {
            out.push(hole.to_string());
        }
        rest = &rest[end + 1..];
    }
    (!out.is_empty()).then(|| format!("`{}`", out.join(" … ")))
}

/// Whether anything in the component (or inside it) still calls `setter`.
pub fn sets_state(model: &Model, component: &str, setter: &str) -> bool {
    let call = format!("{setter}(");
    model
        .entities
        .values()
        .filter(|e| {
            e.id == component
                || e.parent.is_some() && {
                    let mut cur = e.parent.clone();
                    let mut hit = false;
                    while let Some(c) = cur {
                        if c == component {
                            hit = true;
                            break;
                        }
                        cur = model.entities.get(&c).and_then(|x| x.parent.clone());
                    }
                    hit
                }
        })
        .any(|e| e.call_shapes.iter().any(|s| s.shape.starts_with(&call)) || e.handlers.iter().any(|h| h.does.contains(&call)))
}
