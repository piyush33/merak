//! Rendering a transition for people.
//!
//! [`markdown`] is the default view: a one-line summary, then plain sentences grouped by
//! what a reviewer cares about (access, data, effects, checks, new code), with the routes
//! each change reaches and one place to look. [`detailed`] lists every operation as Merak
//! derived it; JSON output is for machines.

use crate::{Layer, Op, Transition};
use std::collections::BTreeSet;
use std::fmt::Write;

fn section(layer: Layer) -> &'static str {
    match layer {
        Layer::Behaviour => "Behaviour",
        Layer::Design => "Design",
        Layer::Structural => "Structure",
    }
}

fn short(s: &str) -> String {
    // `src/services/order.service.ts::OrderService.cancelOrder` → `OrderService.cancelOrder`
    s.split(" → ").map(|p| p.rsplit("::").next().unwrap_or(p)).collect::<Vec<_>>().join(" → ")
}

fn title(kind: &str) -> String {
    kind.replace('_', " ").to_lowercase()
}

/// Every operation, as derived (`merak diff --detail`).
pub fn detailed(t: &Transition, header: Option<&str>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## Semantic transition{}", header.map(|h| format!(" — {h}")).unwrap_or_default());
    let _ = writeln!(
        s,
        "\n{} behavioural/design change(s) across {} changed entit{}.\n",
        t.behavioural().count(),
        t.stats.entities_changed,
        if t.stats.entities_changed == 1 { "y" } else { "ies" }
    );
    if t.ops.is_empty() {
        s.push_str("_No semantic change detected._\n");
        return s;
    }
    for layer in [Layer::Behaviour, Layer::Design, Layer::Structural] {
        let ops: Vec<&Op> = t.ops.iter().filter(|o| o.layer == layer).collect();
        if ops.is_empty() {
            continue;
        }
        let _ = writeln!(s, "### {}\n", section(layer));
        for op in ops {
            let _ = write!(s, "- **{}** `{}`", title(&op.kind), short(&op.subject));
            match (&op.before, &op.after) {
                (Some(b), Some(a)) => {
                    let _ = write!(s, "\n  - before: {b}\n  - after: {a}");
                }
                (Some(b), None) => {
                    let _ = write!(s, "\n  - was: {b}");
                }
                (None, Some(a)) => {
                    let _ = write!(s, "\n  - now: {a}");
                }
                (None, None) => {}
            }
            if let Some(n) = &op.note {
                let _ = write!(s, "\n  - {n}");
            }
            if !op.affects.is_empty() {
                let _ = write!(s, "\n  - affects: {}", op.affects.iter().map(|a| format!("`{a}`")).collect::<Vec<_>>().join(", "));
            }
            let mut ev: Vec<String> = op.evidence_after.iter().map(|l| format!("`{l}`")).collect();
            if ev.is_empty() {
                ev = op.evidence_before.iter().map(|l| format!("`{l}` (before)")).collect();
            }
            if !ev.is_empty() {
                ev.truncate(4);
                let _ = write!(s, "\n  - evidence: {}", ev.join(", "));
            }
            match op.origin {
                merak_behaviour::Origin::Static => {}
                merak_behaviour::Origin::Declared => s.push_str("\n  - origin: declared in merak.toml"),
                merak_behaviour::Origin::Inferred => {
                    let _ = write!(s, "\n  - origin: inferred, confidence {:.2}", op.confidence);
                }
            }
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

// ------------------------------------------------------------------ plain view

/// Sections of the plain view, in the order a reviewer should read them.
const SECTIONS: &[(&str, &[&str])] = &[
    ("Who can do what", &["AUTH_WIDENED", "AUTH_NARROWED", "AUTH_CHANGED"]),
    (
        "Which data it reads and changes",
        &[
            "QUERY_FILTER_REMOVED",
            "QUERY_FILTER_CHANGED",
            "QUERY_FILTER_ADDED",
            "DATAFLOW_EXPANDED",
            "DATAFLOW_RESTRICTED",
            "STATE_TRANSITION_SOURCE_WIDENED",
            "STATE_TRANSITION_ADDED",
            "STATE_TRANSITION_CHANGED",
            "STATE_TRANSITION_SOURCE_NARROWED",
            "STATE_TRANSITION_REMOVED",
        ],
    ),
    (
        "What it does",
        &[
            "ENTRYPOINT_ADDED",
            "ENTRYPOINT_REMOVED",
            "EFFECT_ADDED",
            "EFFECT_REMOVED",
            "EFFECT_MADE_ASYNC",
            "EFFECT_MADE_SYNC",
            "EVENT_HANDLER_ADDED",
            "EVENT_HANDLER_REMOVED",
        ],
    ),
    (
        "What it checks",
        &[
            "GUARD_REMOVED",
            "GUARD_CHANGED",
            "GUARD_ADDED",
            "VALIDATION_REMOVED",
            "VALIDATION_ADDED",
            "SCHEMA_FIELD_REMOVED",
            "SCHEMA_FIELD_CHANGED",
            "SCHEMA_FIELD_ADDED",
            "SCHEMA_CHANGED",
        ],
    ),
    ("New code", &["BEHAVIOUR_ADDED"]),
    (
        "Design rules",
        &["INVARIANT_VIOLATED", "LAYER_VIOLATION_INTRODUCED", "OWNERSHIP_VIOLATION_INTRODUCED", "LAYER_VIOLATION_RESOLVED", "OWNERSHIP_VIOLATION_RESOLVED"],
    ),
    ("Needs a human look", &["UNCLASSIFIED_CHANGE"]),
];

/// The default view: what the change does, in plain language.
pub fn markdown(t: &Transition, header: Option<&str>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## What this change does{}\n", header.map(|h| format!(" ({h})")).unwrap_or_default());
    let plan = NewCode::plan(t);
    let _ = writeln!(s, "{}\n", headline(t, &plan));
    for (title, kinds) in SECTIONS {
        let ops: Vec<&Op> = kinds.iter().flat_map(|k| t.ops.iter().filter(move |o| o.kind == *k)).filter(|o| !plan.folded.contains(&o.id)).collect();
        if ops.is_empty() {
            continue;
        }
        let _ = writeln!(s, "### {title}\n");
        for op in ops {
            let _ = writeln!(s, "- {}", plan.sentence(op));
            if let Some(w) = whereabouts(op) {
                let _ = writeln!(s, "  {w}");
            }
        }
        s.push('\n');
    }
    if let Some(also) = also(t, &plan) {
        let _ = writeln!(s, "{also}");
    }
    s
}

/// How new code is presented: a new endpoint is described by its handler, and other new
/// functions behind new endpoints show only what they add to that description.
struct NewCode {
    /// Ops shown as part of another op's sentence.
    folded: BTreeSet<String>,
    /// Endpoint op id → the handler's description.
    endpoint_desc: std::collections::BTreeMap<String, String>,
    /// New-code op id → (routes it is behind, what it adds).
    extra: std::collections::BTreeMap<String, (Vec<String>, String)>,
}

impl NewCode {
    fn plan(t: &Transition) -> NewCode {
        let mut plan = NewCode { folded: BTreeSet::new(), endpoint_desc: Default::default(), extra: Default::default() };
        let new_routes: Vec<&Op> = t.ops.iter().filter(|o| o.kind == "ENTRYPOINT_ADDED").collect();
        let file = |o: &Op| o.evidence_after.first().map(|l| l.file.clone());
        let mut shown: std::collections::BTreeMap<&str, Vec<(String, String)>> = Default::default();
        for route in &new_routes {
            // The handler: new code reached only by this route, in the same file when there is one
            // (NestJS decorators), else the only such code (an Express route in `app.ts`).
            let only_here: Vec<&Op> = t.ops.iter().filter(|o| o.kind == "BEHAVIOUR_ADDED" && o.affects == [route.subject.clone()]).collect();
            let handler = only_here.iter().find(|o| file(o) == file(route)).or(if only_here.len() == 1 { only_here.first() } else { None }).copied();
            if let (Some(h), Some(desc)) = (handler, handler.and_then(|h| h.after.as_deref())) {
                plan.folded.insert(h.id.clone());
                plan.endpoint_desc.insert(route.id.clone(), desc.to_string());
                shown.insert(route.subject.as_str(), clauses(desc));
            }
        }
        let handlers = plan.folded.clone();
        for o in t.ops.iter().filter(|o| o.kind == "BEHAVIOUR_ADDED" && !handlers.contains(&o.id)) {
            if o.affects.is_empty() || !o.affects.iter().all(|r| shown.contains_key(r.as_str())) {
                continue;
            }
            let seen: BTreeSet<&(String, String)> = o.affects.iter().flat_map(|r| &shown[r.as_str()]).collect();
            let rest: Vec<(String, String)> = clauses(o.after.as_deref().unwrap_or("")).into_iter().filter(|c| !seen.contains(c)).collect();
            if rest.is_empty() {
                plan.folded.insert(o.id.clone());
            } else {
                plan.extra.insert(o.id.clone(), (o.affects.clone(), unclauses(&rest)));
            }
        }
        plan
    }

    fn sentence(&self, op: &Op) -> String {
        if let Some(desc) = self.endpoint_desc.get(&op.id) {
            // Middleware and global guards are invisible to Merak: say what it found, not more.
            let checked = ["requires", "guards", "validates"].iter().any(|l| desc.contains(&format!("{l}: ")));
            let note = if checked { "" } else { " **Merak found no access check on it.**" };
            return format!("New endpoint `{}`: it {}.{note}", op.subject, describe(desc));
        }
        if let Some((routes, rest)) = self.extra.get(&op.id) {
            let behind = routes.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", ");
            return format!("New `{}` behind {behind} also {}.", name(&op.subject), describe(rest));
        }
        sentence(op)
    }
}

/// `effects: a, b; requires: c` → [(effects, a), (effects, b), (requires, c)].
fn clauses(summary: &str) -> Vec<(String, String)> {
    summary
        .split("; ")
        .filter_map(|part| part.split_once(": "))
        .flat_map(|(label, items)| items.split(", ").map(move |i| (label.to_string(), i.to_string())))
        .collect()
}

fn unclauses(cs: &[(String, String)]) -> String {
    let mut out: Vec<(String, Vec<String>)> = vec![];
    for (label, item) in cs {
        match out.iter_mut().find(|(l, _)| l == label) {
            Some((_, items)) => items.push(item.clone()),
            None => out.push((label.clone(), vec![item.clone()])),
        }
    }
    out.iter().map(|(l, items)| format!("{l}: {}", items.join(", "))).collect::<Vec<_>>().join("; ")
}

/// The behaviour and design changes as one-line sentences, most important first, grouped
/// the same way as [`markdown`] (for short reports such as the Claude Code hook).
pub fn bullets(t: &Transition) -> Vec<String> {
    let plan = NewCode::plan(t);
    SECTIONS
        .iter()
        .flat_map(|(_, kinds)| kinds.iter().flat_map(|k| t.ops.iter().filter(move |o| o.kind == *k)))
        .filter(|o| !plan.folded.contains(&o.id))
        .map(|op| match whereabouts(op) {
            Some(w) => format!("{} ({w})", plan.sentence(op)),
            None => plan.sentence(op),
        })
        .collect()
}

fn headline(t: &Transition, plan: &NewCode) -> String {
    let unclassified = t.ops.iter().filter(|o| o.kind == "UNCLASSIFIED_CHANGE").count();
    // Count what is shown: a handler folded into its endpoint's line is one change.
    let named = t.ops.iter().filter(|o| o.layer != Layer::Structural && o.kind != "UNCLASSIFIED_CHANGE" && !plan.folded.contains(&o.id)).count();
    let restructured = t.ops.iter().any(|o| o.kind == "PURE_REFACTOR");
    let logging = t.ops.iter().any(|o| o.kind == "LOGGING_CHANGED");
    if t.ops.is_empty() {
        return "No change to the code Merak analyses.".into();
    }
    if restructured {
        return format!(
            "**No behaviour change.** {} restructured (moved, renamed or extracted), and each does what it did before.",
            plural(t.stats.entities_changed.max(1), "function was", "functions were")
        );
    }
    let review = |n: usize| format!("{} Merak can't classify: review {} by hand.", plural(n, "change", "changes"), if n == 1 { "it" } else { "them" });
    match (named, unclassified) {
        (0, 0) if logging => "**No behaviour change.** Only logging changed.".into(),
        (0, 0) => "**No behaviour change.** Only the code's structure changed.".into(),
        (0, u) => format!("**No behaviour change Merak can name.** {}", review(u)),
        (n, 0) => format!("**{}.**", plural(n, "behaviour change", "behaviour changes")),
        (n, u) => format!("**{}.** Also {}", plural(n, "behaviour change", "behaviour changes"), review(u)),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `Affects POST /x · src/a.ts:4`: the routes a change reaches and one place to look.
fn whereabouts(op: &Op) -> Option<String> {
    let mut parts = vec![];
    if !op.affects.is_empty() {
        let shown: Vec<String> = op.affects.iter().take(3).map(|a| format!("`{a}`")).collect();
        let more = op.affects.len().saturating_sub(3);
        parts.push(format!("Affects {}{}", shown.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() }));
    }
    if let Some(l) = op.evidence_after.first().or(op.evidence_before.first()) {
        parts.push(format!("`{l}`"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `src/a.ts::OrderService.cancel.$if#2` → `OrderService.cancel` (closures by their function).
fn name(subject: &str) -> String {
    let local = subject.rsplit("::").next().unwrap_or(subject);
    let kept: Vec<&str> =
        local.split('.').take_while(|seg| !seg.starts_with('$') && !seg.starts_with('<') && !seg.contains('(') && !seg.contains('#')).collect();
    if kept.is_empty() {
        local.to_string()
    } else {
        kept.join(".")
    }
}

fn sentence(op: &Op) -> String {
    let n = format!("`{}`", name(&op.subject));
    let (b, a) = (op.before.as_deref().unwrap_or(""), op.after.as_deref().unwrap_or(""));
    let (b, a) = (&blanks(b), &blanks(a));
    match op.kind.as_str() {
        "AUTH_WIDENED" => match (parse_set(b), parse_set(a)) {
            (Some((f, bv, false)), Some((f2, av, false))) if f == f2 => {
                let added: Vec<&String> = av.iter().filter(|v| !bv.contains(v)).collect();
                format!("**More access:** {n} now also allows {}{} (before: only {}).", who(&f), list(&added), list(&bv.iter().collect::<Vec<_>>()))
            }
            _ => access_change("More access", &n, b, a),
        },
        "AUTH_NARROWED" => match (parse_set(b), parse_set(a)) {
            (Some((f, bv, false)), Some((f2, av, false))) if f == f2 => {
                let gone: Vec<&String> = bv.iter().filter(|v| !av.contains(v)).collect();
                format!("**Less access:** {n} no longer allows {}{} (now: only {}).", who(&f), list(&gone), list(&av.iter().collect::<Vec<_>>()))
            }
            _ => access_change("Less access", &n, b, a),
        },
        "AUTH_CHANGED" => access_change("Access changed", &n, b, a),
        "QUERY_FILTER_ADDED" => format!("{n} now only reaches rows where `{a}`."),
        "QUERY_FILTER_REMOVED" => format!("{n} no longer filters on `{b}`, so it can reach more rows."),
        "QUERY_FILTER_CHANGED" => format!("{n} filters rows differently: `{b}` → `{a}`."),
        "DATAFLOW_EXPANDED" => format!("{n} now also {}.", code_lists(a)),
        "DATAFLOW_RESTRICTED" => format!("{n} no longer {}.", code_lists(b)),
        k if k.starts_with("STATE_TRANSITION") => state(op),
        "EFFECT_ADDED" => format!("{n} now {}.", effect(a)),
        "EFFECT_REMOVED" => format!("{n} no longer {}.", effect(b)),
        "EFFECT_MADE_ASYNC" => format!("{n} now {} later, via {}, instead of right away.", effect(strip_mode(a)), via(a)),
        "EFFECT_MADE_SYNC" => format!("{n} now {} right away instead of later via {}.", effect(strip_mode(a)), via(b)),
        "ENTRYPOINT_ADDED" => match effects_list(a) {
            Some(e) => format!("New endpoint `{}`: it {e}.", op.subject),
            None => format!("New endpoint `{}`.", op.subject),
        },
        "ENTRYPOINT_REMOVED" => format!("Endpoint `{}` was removed.", op.subject),
        "EVENT_HANDLER_ADDED" => {
            let by = op.note.as_deref().and_then(|x| x.strip_prefix("handled by ")).map(|h| format!(" (`{h}`)")).unwrap_or_default();
            match effects_list(a) {
                Some(e) => format!("New handler for the `{}` event{by}: it {e}.", op.subject),
                None => format!("New handler for the `{}` event{by}.", op.subject),
            }
        }
        "EVENT_HANDLER_REMOVED" => format!("The `{}` event is no longer handled here.", op.subject),
        "GUARD_ADDED" => format!("{n} now only continues if `{a}`."),
        "GUARD_REMOVED" => format!("{n} no longer checks that `{b}`."),
        "GUARD_CHANGED" => format!("{n} checks something different: `{b}` → `{a}`."),
        "VALIDATION_ADDED" => format!("{n} now runs the `{}` check.", name(a)),
        "VALIDATION_REMOVED" => format!("{n} no longer runs the `{}` check.", name(b)),
        "SCHEMA_FIELD_ADDED" => {
            let (schema, field) = schema_field(&op.subject);
            format!("`{schema}` has a new field `{field}`: `{a}`.")
        }
        "SCHEMA_FIELD_REMOVED" => {
            let (schema, field) = schema_field(&op.subject);
            format!("`{schema}` no longer has the field `{field}` (was `{b}`).")
        }
        "SCHEMA_FIELD_CHANGED" => {
            let (schema, field) = schema_field(&op.subject);
            format!("`{schema}` validates `{field}` differently: `{b}` → `{a}`.")
        }
        "SCHEMA_CHANGED" if b.is_empty() => format!("`{}` as a whole is now `{a}`.", name(&op.subject)),
        "SCHEMA_CHANGED" => format!("`{}` as a whole changed: `{b}` → `{a}`.", name(&op.subject)),
        "BEHAVIOUR_ADDED" => format!("New {n}: {}.", describe(a)),
        "INVARIANT_VIOLATED" => invariant(op),
        "LAYER_VIOLATION_INTRODUCED" => {
            let files: Vec<String> = a
                .split("; ")
                .map(|d| d.split(" → ").map(|f| format!("`{}`", f.rsplit('/').next().unwrap_or(f))).collect::<Vec<_>>().join(" now uses "))
                .collect();
            format!("**Breaks a layering rule** (merak.toml says {} may not use {}): {}.", layer_of(&op.subject, 0), layer_of(&op.subject, 1), files.join("; "))
        }
        "OWNERSHIP_VIOLATION_INTRODUCED" => {
            let writers: Vec<String> = a.split("; ").map(|w| format!("`{}`", name(w))).collect();
            format!("**Breaks an ownership rule** (merak.toml): `{}` is now also written by {}, outside the code that owns it.", op.subject, join_and(&writers))
        }
        "LAYER_VIOLATION_RESOLVED" | "OWNERSHIP_VIOLATION_RESOLVED" => format!("A declared rule is no longer broken in `{}`.", op.subject),
        "UNCLASSIFIED_CHANGE" => format!("{n} changed in a way Merak can't classify{}.", calls_summary(b, a)),
        other => format!("{} {n}", other.replace('_', " ").to_lowercase()),
    }
}

/// Merak writes a value it cannot know as `_`; people read `…`.
fn blanks(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let word = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '=');
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| if *c == '_' && !word(i.checked_sub(1).and_then(|j| chars.get(j))) && !word(chars.get(i + 1)) { '…' } else { *c })
        .collect()
}

/// How to name the field an access rule tests: `User.role` → "role ", else "`field` = ".
fn who(field: &str) -> String {
    match field.rsplit('.').next().unwrap_or(field).to_lowercase().as_str() {
        "role" | "roles" => "role ".into(),
        "permission" | "permissions" => "permission ".into(),
        "scope" | "scopes" => "scope ".into(),
        _ => format!("`{field}` = "),
    }
}

/// `controller → repository`: part `i`, in words.
fn layer_of(rule: &str, i: usize) -> String {
    format!("the {} layer", rule.split(" → ").nth(i).unwrap_or("?"))
}

/// Inferred invariants in words:
/// `Order.status := CANCELLED requires User.role ∈ {ADMIN}` and `… := PAID ⇒ Order.paymentId is set`.
fn invariant(op: &Op) -> String {
    let who = op.after.as_deref().map(|a| a.replace(" does not satisfy it", "")).unwrap_or_default();
    let rule = &op.subject;
    let pattern = if let Some((write, cond)) = rule.split_once(" requires ") {
        let (field, value) = write.split_once(" := ").unwrap_or((write, "?"));
        format!("everywhere else, `{field}` becomes `{value}` only after checking `{cond}`; `{who}` doesn't check it")
    } else if let Some((write, then)) = rule.split_once(" ⇒ ") {
        let (field, value) = write.split_once(" := ").unwrap_or((write, "?"));
        let other = then.trim_end_matches(" is set");
        format!("everywhere else, setting `{field}` to `{value}` also sets `{other}`; `{who}` doesn't")
    } else {
        format!("{rule}; `{who}` doesn't follow it")
    };
    format!("**Breaks a pattern the rest of the code follows** ({:.0}% sure): {pattern}.", op.confidence * 100.0)
}

fn access_change(label: &str, n: &str, b: &str, a: &str) -> String {
    let dynamic = |s: &str| !s.is_empty() && s.split(", ").all(|r| r.ends_with("=_"));
    match (b.is_empty(), a.is_empty()) {
        (true, _) => format!("**{label}:** {n} now requires {}.", requirements(a)),
        (_, true) => format!("**{label}:** {n} no longer requires {}.", requirements(b)),
        _ if dynamic(a) => format!("**{label}:** {n} no longer requires {} itself; its callers now choose the {}.", requirements(b), key_of(a)),
        _ => format!("**{label}:** {n} now requires {} instead of {}.", requirements(a), requirements(b)),
    }
}

fn key_of(req: &str) -> String {
    req.split('=').next().unwrap_or(req).trim_end_matches('s').to_string()
}

/// `permission=asset.share, public=true` → "the `asset.share` permission and public access (no login)".
fn requirements(list: &str) -> String {
    let one = |r: &str| {
        let (k, v) = r.split_once('=').unwrap_or((r, ""));
        match (k, v) {
            (_, "_") => format!("a {} chosen by its callers", key_of(r)),
            ("permission" | "permissions", v) => format!("the `{v}` permission"),
            ("role" | "roles", v) => format!("role `{v}`"),
            ("scope" | "scopes", v) => format!("the `{v}` scope"),
            ("public", "true") => "public access (no login)".into(),
            ("admin" | "isAdmin", "true") => "an admin".into(),
            _ => format!("`{r}`"),
        }
    };
    join_and(&list.split(", ").map(one).collect::<Vec<_>>())
}

/// `User.role ∈ {ADMIN, MANAGER}` → (`User.role`, [ADMIN, MANAGER], negated).
fn parse_set(p: &str) -> Option<(String, Vec<String>, bool)> {
    let (field, rest, negated) = match p.split_once(" ∈ ") {
        Some((f, r)) => (f, r, false),
        None => p.split_once(" ∉ ").map(|(f, r)| (f, r, true))?,
    };
    Some((field.to_string(), set_values(rest)?, negated))
}

/// `{A, B}` → [A, B].
fn set_values(s: &str) -> Option<Vec<String>> {
    let inner = s.trim().strip_prefix('{')?.strip_suffix('}')?;
    Some(inner.split(", ").filter(|v| !v.is_empty()).map(str::to_string).collect())
}

fn list(values: &[&String]) -> String {
    let shown: Vec<String> = values.iter().map(|v| format!("`{v}`")).collect();
    match shown.as_slice() {
        [] => "nothing".into(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn state(op: &Op) -> String {
    let (field, to) = op.subject.split_once(" → ").unwrap_or((&op.subject, "?"));
    let from = |s: &str| {
        let vs = set_values(s).unwrap_or_default();
        let names: Vec<String> = vs
            .iter()
            .map(|v| match v.as_str() {
                "∅" => "creation".to_string(),
                "*" => "any state".to_string(),
                v => format!("`{v}`"),
            })
            .collect();
        names.join(", ")
    };
    let b = op.before.as_deref().unwrap_or("{}");
    let a = op.after.as_deref().unwrap_or("{}");
    let via = op.note.as_deref().and_then(|x| x.strip_prefix("new path via ")).map(|w| format!(" (through `{w}`)")).unwrap_or_default();
    match op.kind.as_str() {
        "STATE_TRANSITION_SOURCE_WIDENED" => {
            let (bv, av) = (set_values(b).unwrap_or_default(), set_values(a).unwrap_or_default());
            let new: Vec<String> = av.into_iter().filter(|v| !bv.contains(v)).collect();
            format!("`{field}` can now become `{to}` from {} too{via}; before, only from {}.", from(&format!("{{{}}}", new.join(", "))), from(b))
        }
        "STATE_TRANSITION_SOURCE_NARROWED" => format!("`{field}` can become `{to}` only from {} now (before: {}).", from(a), from(b)),
        "STATE_TRANSITION_ADDED" => format!("`{field}` can now become `{to}`, from {}{via}.", from(a)),
        "STATE_TRANSITION_REMOVED" => format!("`{field}` can no longer become `{to}`."),
        _ => format!("`{field}` becomes `{to}` from {} instead of {}{via}.", from(a), from(b)),
    }
}

/// `db_write order (sync)` → "writes the `order` table".
fn effect(e: &str) -> String {
    let e = strip_mode(e);
    let mut words = e.splitn(2, ' ');
    let kind = words.next().unwrap_or("");
    let rest = words.next().unwrap_or("").trim();
    let dynamic = rest.is_empty() || rest.contains("<dynamic>");
    let thing = |label: &str, what: &str| if dynamic { format!("{label} chosen at runtime") } else { format!("`{rest}` {what}") };
    match kind {
        "db_write" => format!("writes {}", thing("a table", "table")).replace("writes `", "writes the `"),
        "db_read" => format!("reads {}", thing("a table", "table")).replace("reads `", "reads the `"),
        "db_query" => "runs database queries".into(),
        "http" => {
            if dynamic || rest.ends_with(' ') || rest.split(' ').nth(1).is_none_or(|h| h.is_empty()) {
                format!(
                    "makes {} HTTP request",
                    rest.split(' ').next().filter(|m| !m.is_empty() && !m.contains('<')).map(|m| format!("a {m}")).unwrap_or_else(|| "an".into())
                )
            } else {
                format!("calls `{rest}`")
            }
        }
        "event_emit" => format!("emits {}", thing("an event", "event").replace("emits `", "emits the `")),
        "queue" => {
            if dynamic {
                "queues background jobs".into()
            } else {
                format!("queues `{rest}` jobs")
            }
        }
        "fs_write" => "writes files".into(),
        "fs_read" => "reads files".into(),
        "cache_write" => "writes to the cache".into(),
        "cache_read" => "reads from the cache".into(),
        _ => format!("`{e}`"),
    }
}

fn strip_mode(e: &str) -> &str {
    match e.rfind(" (") {
        Some(i) if e.ends_with(')') => &e[..i],
        _ => e,
    }
}

/// `… (async via OrderCancelled)` → "the `OrderCancelled` event".
fn via(e: &str) -> String {
    match e.split("via ").nth(1).map(|x| x.trim_end_matches(')')) {
        Some(ev) => format!("the `{ev}` event"),
        None => "an event".into(),
    }
}

/// `db_read order, db_write order` → "reads and writes the `order` table".
fn effects_list(s: &str) -> Option<String> {
    let items: Vec<&str> = s.split(", ").filter(|x| !x.is_empty()).collect();
    (!items.is_empty()).then(|| join_and(&effects(&items)))
}

/// Effect phrases, with a table that is both read and written said once.
fn effects(items: &[&str]) -> Vec<String> {
    let table = |kind: &str, e: &str| strip_mode(e).strip_prefix(kind).map(|t| t.trim().to_string()).filter(|t| !t.contains('<'));
    let written: BTreeSet<String> = items.iter().filter_map(|e| table("db_write ", e)).collect();
    let read: BTreeSet<String> = items.iter().filter_map(|e| table("db_read ", e)).collect();
    let mut out = vec![];
    for e in items {
        if e.starts_with('+') {
            out.push(e.to_string());
        } else if let Some(t) = table("db_write ", e).filter(|t| read.contains(t)) {
            out.push(format!("reads and writes the `{t}` table"));
        } else if table("db_read ", e).is_some_and(|t| written.contains(&t)) {
            continue;
        } else {
            out.push(effect(e));
        }
    }
    out
}

fn join_and(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// `writes Order.status; reads Order.id` with the names in code font.
fn code_lists(s: &str) -> String {
    s.split("; ")
        .map(|part| match part.split_once(' ') {
            Some((verb, items)) => format!("{verb} {}", items.split(", ").map(|i| format!("`{i}`")).collect::<Vec<_>>().join(", ")),
            None => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

/// `src/dtos/tag.dto.ts::TagCreateSchema.name` → (`TagCreateSchema`, `name`).
fn schema_field(subject: &str) -> (String, String) {
    let local = subject.rsplit("::").next().unwrap_or(subject);
    match local.split_once('.') {
        Some((s, f)) => (s.to_string(), f.to_string()),
        None => (local.to_string(), String::new()),
    }
}

/// A `BEHAVIOUR_ADDED` summary (`effects: …; requires: …; guards: …`) in words.
fn describe(summary: &str) -> String {
    let mut out = vec![];
    let summary = blanks(summary);
    for part in summary.split("; ") {
        let Some((label, items)) = part.split_once(": ") else { continue };
        let items: Vec<&str> = items.split(", ").collect();
        let code = |xs: &[&str]| xs.iter().map(|x| if x.starts_with('+') { x.to_string() } else { format!("`{x}`") }).collect::<Vec<_>>();
        match label {
            "effects" => out.push(join_and(&effects(&items))),
            "requires" => out.push(format!("requires {}", requirements(&items.join(", ")))),
            "filters" => out.push(format!("only reaches rows where {}", join_and(&code(&items)))),
            "guards" => out.push(format!("only continues if {}", join_and(&code(&items)))),
            "validates" => out.push(format!("runs the {} check{}", join_and(&code(&items)), if items.len() == 1 { "" } else { "s" })),
            "writes" => out.push(format!("sets {}", join_and(&code(&items)))),
            _ => out.push(part.to_string()),
        }
    }
    out.join("; ")
}

/// What an unclassified change did to calls, by function name: ": it now also calls `trim`, …".
fn calls_summary(before: &str, after: &str) -> String {
    let names = |s: &str| -> Vec<String> {
        let mut seen = BTreeSet::new();
        s.split("; ").filter(|x| !x.is_empty()).map(call_name).filter(|n| seen.insert(n.clone())).collect()
    };
    let (b, a) = (names(before), names(after));
    let added: Vec<&String> = a.iter().filter(|n| !b.contains(n)).collect();
    let removed: Vec<&String> = b.iter().filter(|n| !a.contains(n)).collect();
    let show = |xs: &[&String]| {
        let shown: Vec<&String> = xs.iter().take(6).copied().collect();
        let more = xs.len().saturating_sub(6);
        format!("{}{}", list(&shown), if more > 0 { format!(" and {more} more") } else { String::new() })
    };
    let mut parts = vec![];
    if !added.is_empty() {
        parts.push(format!("it now also calls {}", show(&added)));
    }
    if !removed.is_empty() {
        parts.push(format!("it no longer calls {}", show(&removed)));
    }
    if parts.is_empty() {
        // Same functions: say what differs about the calls, starting with how many arguments.
        let arity = |s: &str, name: &str| -> Option<usize> {
            s.split("; ").find(|x| call_name(x) == name).map(|x| {
                let args = x.split(" when ").next().unwrap_or(x);
                let inner = args.find('(').map(|i| &args[i + 1..args.len().saturating_sub(1)]).unwrap_or("");
                if inner.trim().is_empty() {
                    0
                } else {
                    inner.split(", ").count()
                }
            })
        };
        let arities: Vec<String> = a
            .iter()
            .filter_map(|n| match (arity(before, n), arity(after, n)) {
                (Some(x), Some(y)) if x != y => Some(format!("calls `{n}` with {} instead of {x}", plural(y, "argument", "arguments"))),
                _ => None,
            })
            .collect();
        if !arities.is_empty() {
            return format!(": it now {}", join_and(&arities));
        }
        let same: Vec<&String> = a.iter().chain(b.iter()).collect::<BTreeSet<_>>().into_iter().collect();
        if same.is_empty() {
            return String::new();
        }
        return format!(": the arguments or conditions of its calls to {} changed", show(&same));
    }
    format!(": {}", parts.join(", and "))
}

/// `typed.trim()…toLowerCase() when x` → `toLowerCase`; `UserRepository.update(_)` → `UserRepository.update`.
fn call_name(shape: &str) -> String {
    let target = shape.split(" when ").next().unwrap_or(shape).trim();
    // Drop only the final argument list: `a.toLowerCase().indexOf(_)` → `a.toLowerCase().indexOf`.
    let target = match target.strip_suffix(')') {
        Some(inner) => {
            let mut depth = 1;
            let open = inner.char_indices().rev().find(|(_, c)| {
                match c {
                    ')' => depth += 1,
                    '(' => depth -= 1,
                    _ => {}
                }
                depth == 0
            });
            open.map_or(target, |(i, _)| &inner[..i])
        }
        None => target,
    };
    let target = target.replace("().", ".");
    let target = target.as_str();
    let target = target.rsplit('…').next().unwrap_or(target);
    let target = target.trim_start_matches("new ");
    let segs: Vec<&str> = target.split('.').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        [.., class, method] if class.chars().next().is_some_and(char::is_uppercase) && !class.contains('#') => format!("{class}.{method}"),
        [.., last] => last.trim_start_matches('<').trim_end_matches('>').to_string(),
        [] => target.to_string(),
    }
}

/// The structural remainder in one line: "Also: added `findTypedPrefix`; 2 functions moved."
fn also(t: &Transition, _plan: &NewCode) -> Option<String> {
    // Closures and code already described above are not listed again.
    let described: BTreeSet<&str> = t.ops.iter().filter(|o| o.kind == "BEHAVIOUR_ADDED").map(|o| o.subject.as_str()).collect();
    // `f` or `Class.method`; a third segment is a callback inside one (`Repo.get.where`).
    let top_level =
        |o: &&Op| o.subject.rsplit("::").next().is_some_and(|l| name(&o.subject) == l && l.split('.').count() <= 2) && !described.contains(o.subject.as_str());
    let names = |kind: &str| -> Vec<String> {
        let mut seen = BTreeSet::new();
        t.ops.iter().filter(|o| o.kind == kind).filter(top_level).map(|o| format!("`{}`", name(&o.subject))).filter(|n| seen.insert(n.clone())).collect()
    };
    let count = |kinds: &[&str]| t.ops.iter().filter(|o| kinds.contains(&o.kind.as_str())).count();
    let few = |xs: Vec<String>| {
        let more = xs.len().saturating_sub(5);
        let shown = xs.into_iter().take(5).collect::<Vec<_>>().join(", ");
        if more > 0 {
            format!("{shown} and {more} more")
        } else {
            shown
        }
    };
    let mut parts = vec![];
    let added = names("ENTITY_ADDED");
    if !added.is_empty() {
        parts.push(format!("added {}", few(added)));
    }
    let removed = names("ENTITY_REMOVED");
    if !removed.is_empty() {
        parts.push(format!("removed {}", few(removed)));
    }
    let moved = count(&["ENTITY_MOVED", "ENTITY_RENAMED"]);
    if moved > 0 {
        parts.push(format!("{} moved or renamed", plural(moved, "function or class", "functions or classes")));
    }
    let logging = names("LOGGING_CHANGED");
    if !logging.is_empty() {
        parts.push(format!("logging changed in {}", few(logging)));
    }
    let deps = count(&["DEPENDENCY_ADDED", "DEPENDENCY_REMOVED"]);
    if deps > 0 {
        parts.push(format!("{} between files changed", plural(deps, "dependency", "dependencies")));
    }
    (!parts.is_empty()).then(|| format!("Also: {}.", parts.join("; ")))
}

// ------------------------------------------------------------------ contract view

/// Source text of one line, before (`true`) or after the change, for code under changed clauses.
pub type SourceLine<'a> = &'a dyn Fn(&merak_ir::Loc, bool) -> Option<String>;

const WIDTH: usize = 112;

/// The contract diff: per changed or new function, what it promises before and after.
pub fn contracts(t: &Transition, header: Option<&str>, source: SourceLine) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## Contract diff{}\n", header.map(|h| format!(" · {h}")).unwrap_or_default());
    let _ = writeln!(s, "{}\n", contract_headline(t));
    for c in &t.contracts {
        let kind = match c.status.as_str() {
            "new" => "new",
            "schema" => "validation schema",
            _ if c.untyped_only() => "changed, untyped",
            _ => "changed",
        };
        let _ = writeln!(s, "### `{}` · {kind}\n", c.name);
        let mut meta = vec![];
        if !c.routes.is_empty() || !c.new_routes.is_empty() {
            let mut shown: Vec<String> = c
                .new_routes
                .iter()
                .map(|r| match r.strip_prefix("event ") {
                    Some(ev) => format!("event `{ev}` (new handler)"),
                    None => format!("`{r}` (new route)"),
                })
                .collect();
            shown.extend(c.routes.iter().filter(|r| !c.new_routes.contains(r)).map(|r| format!("`{r}`")));
            let more = shown.len().saturating_sub(3);
            shown.truncate(3);
            meta.push(format!("reached from {}{}", shown.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() }));
        }
        if !c.loc.file.is_empty() {
            meta.push(format!("`{}`", c.loc));
        }
        if !meta.is_empty() {
            let _ = writeln!(s, "{}\n", meta.join(" · "));
        }
        for line in block(c, Some(source), &c.loc.file) {
            let _ = writeln!(s, "    {line}");
        }
        s.push('\n');
    }
    if let Some(also) = also(t, &NewCode::plan(t)) {
        let _ = writeln!(s, "{also}");
    }
    s
}

/// A restyled piece of content: `{phrase}: span.font-semibold  →  span.text-gray-400` (before → after).
fn restyled(x: &crate::contract::Clause) -> Option<String> {
    let split = |v: &Option<String>| v.as_deref().and_then(|v| v.rsplit_once(": ")).map(|(c, s)| (c.to_string(), s.to_string()));
    match (split(&x.before), split(&x.after)) {
        (Some((cb, sb)), Some((ca, sa))) if cb == ca => Some(format!("{ca}: {sb}  →  {sa}")),
        _ => None,
    }
}

/// Each contract's changed clauses as one-liners (for the Claude Code hook).
pub fn contract_lines(t: &Transition) -> Vec<String> {
    let mut out = vec![];
    for c in &t.contracts {
        let changed: Vec<&crate::contract::Clause> = c.clauses.iter().filter(|x| x.mark != ' ' && x.mark != '?').collect();
        if changed.is_empty() {
            continue;
        }
        let routes = if c.new_routes.is_empty() { String::new() } else { format!(" (new route {})", c.new_routes.join(", ")) };
        out.push(format!("{} · {}{routes}", c.name, if c.status == "new" { "new" } else { "changed" }));
        for x in changed {
            let text = match (&x.before, &x.after) {
                _ if x.row == "renders" && restyled(x).is_some() => restyled(x).unwrap_or_default(),
                (Some(b), Some(a)) => format!("{} → {}", item(&x.row, b), item(&x.row, a)),
                (None, Some(a)) => item(&x.row, a),
                (Some(b), None) => item(&x.row, b),
                (None, None) => String::new(),
            };
            let tag = x.tag.as_deref().map(|t| format!("  ({t})")).unwrap_or_default();
            let at = x.evidence_after.as_ref().or(x.evidence_before.as_ref()).map(|l| format!("  {l}")).unwrap_or_default();
            out.push(format!("  {} {:<9} {text}{tag}{at}", x.mark, x.row));
        }
    }
    out
}

fn contract_headline(t: &Transition) -> String {
    if t.contracts.is_empty() {
        return headline(t, &NewCode::plan(t));
    }
    let count = |f: &dyn Fn(&crate::contract::ContractDiff) -> bool| t.contracts.iter().filter(|c| f(c)).count();
    let changed = count(&|c| c.status == "changed" && c.name != "Program" && !c.untyped_only());
    let new = count(&|c| c.status == "new");
    let schemas = count(&|c| c.status == "schema");
    let untyped = count(&|c| c.status == "changed" && c.untyped_only());
    let rules: usize = t.contracts.iter().map(|c| c.clauses.iter().filter(|x| x.mark == '!').count()).sum();
    let mut parts = vec![];
    if changed > 0 {
        parts.push(plural(changed, "contract changed", "contracts changed"));
    }
    if new > 0 {
        parts.push(plural(new, "new contract", "new contracts"));
    }
    if schemas > 0 {
        parts.push(plural(schemas, "validation schema changed", "validation schemas changed"));
    }
    let mut line = if parts.is_empty() { "**No behaviour change Merak can name.**".to_string() } else { format!("**{}.**", join_and(&parts)) };
    if rules > 0 {
        line.push_str(&format!(" {} broken.", plural(rules, "rule", "rules")));
    }
    if untyped > 0 {
        line.push_str(&format!(" {} Merak can't type: review by hand.", plural(untyped, "change", "changes")));
    }
    line.push_str("\n\n`+` added · `-` removed · `~` changed · `!` rule broken · `?` untyped · unmarked: unchanged · `a → b`: before → after");
    line
}

/// The clause block of one contract, aligned for a monospace code block.
fn block(c: &crate::contract::ContractDiff, source: Option<SourceLine>, file: &str) -> Vec<String> {
    let mut out = vec![];
    let rows: Vec<&str> = {
        let mut r: Vec<&str> = c.clauses.iter().map(|x| x.row.as_str()).collect();
        r.dedup();
        r
    };
    for row in rows {
        let items: Vec<&crate::contract::Clause> = c.clauses.iter().filter(|x| x.row == row).collect();
        // Unchanged items of a row share one line: context, not news. In a new contract
        // every item is new, so its rows read the same way, marked `+`.
        let compact = |x: &&&crate::contract::Clause| x.mark == ' ' || (c.status == "new" && x.mark == '+');
        let same: Vec<String> = items.iter().filter(compact).filter_map(|x| x.after.as_deref()).map(|a| item(row, a)).collect();
        // Unchanged markup is context nobody reads: count it instead.
        let (short, long): (Vec<String>, Vec<String>) = same.into_iter().partition(|t| row != "renders" && t.chars().count() <= 90);
        let mark = if c.status == "new" { '+' } else { ' ' };
        if !short.is_empty() {
            out.extend(wrap_items(&format!("{mark} {row:<9} "), &short));
        }
        if !long.is_empty() && c.status != "new" {
            let what = if row == "renders" { "piece of content" } else { "case" };
            out.push(format!("{mark} {row:<9} {} unchanged", plural(long.len(), what, &format!("{what}s").replace("piece of contents", "pieces of content"))));
        }
        for x in items.iter().filter(|x| !compact(x)) {
            let tag = x.tag.as_deref().map(|t| format!("   ← {t}")).unwrap_or_default();
            let head = format!("{} {row:<9} ", x.mark);
            // A changed output that is markup: `renders` below says how; say which case here.
            let markup = |v: &Option<String>| v.as_deref().is_some_and(|v| v.split_once(": ").is_some_and(|(_, val)| val.starts_with('<')));
            if row == "returns" && x.mark == '~' && markup(&x.before) && markup(&x.after) {
                let case = x.after.as_deref().and_then(|v| v.split_once(": ")).map(|(c, _)| c).unwrap_or("");
                out.push(format!("{head}{case}: renders differently (see renders)"));
                if let Some(src) = source {
                    out.extend(code(x, src, file));
                }
                continue;
            }
            // `content: before-style  →  after-style` on one line.
            if row == "renders" && x.mark == '~' {
                if let Some(text) = restyled(x) {
                    // Too long for one line: break before the arrow, never inside a style.
                    match text.split_once("  →  ") {
                        Some((before, after)) if head.chars().count() + text.chars().count() > WIDTH => {
                            out.push(format!("{head}{before}"));
                            out.push(format!("{:<12}→ {after}", ""));
                        }
                        _ => out.extend(tagged(wrap(&head, &text), &tag)),
                    }
                    continue;
                }
            }
            match (&x.before, &x.after) {
                _ if x.mark == '?' => {
                    let summary = calls_summary(x.before.as_deref().unwrap_or(""), x.after.as_deref().unwrap_or(""));
                    let text = summary.trim_start_matches(": ").to_string();
                    let text = if text.is_empty() { "calls changed".to_string() } else { text };
                    out.extend(tagged(wrap(&head, &text), &tag));
                }
                (Some(b), Some(a)) => {
                    let (b, a) = (item(row, b), item(row, a));
                    if head.chars().count() + b.chars().count() + a.chars().count() + 5 + tag.chars().count() <= WIDTH {
                        out.push(format!("{head}{b}  →  {a}{tag}"));
                    } else {
                        out.extend(wrap(&head, &b));
                        out.extend(tagged(wrap(&format!("{:<12}→ ", ""), &a), &tag));
                    }
                }
                (None, Some(a)) => out.extend(tagged(wrap(&head, &item(row, a)), &tag)),
                (Some(b), None) => out.extend(tagged(wrap(&head, &item(row, b)), &tag)),
                (None, None) => {}
            }
            if let (Some(src), false) = (source, row == "renders") {
                out.extend(code(x, src, file));
            }
        }
    }
    out
}

/// A tag goes at the end of the last line, or on its own line when it does not fit: never split.
fn tagged(mut lines: Vec<String>, tag: &str) -> Vec<String> {
    if tag.is_empty() {
        return lines;
    }
    match lines.last_mut() {
        Some(last) if last.chars().count() + tag.chars().count() <= WIDTH => last.push_str(tag),
        _ => lines.push(format!("{:<12}{}", "", tag.trim_start())),
    }
    lines
}

/// The source lines behind a changed clause: `59 - if (…)` / `59 + if (…)`; lines from
/// another file than the contract's are prefixed with that file's name.
fn code(x: &crate::contract::Clause, source: SourceLine, file: &str) -> Vec<String> {
    let at = |l: &merak_ir::Loc| {
        if l.file == file || file.is_empty() {
            l.line.to_string()
        } else {
            format!("{}:{}", l.file.rsplit('/').next().unwrap_or(&l.file), l.line)
        }
    };
    let fetch = |l: &Option<merak_ir::Loc>, before: bool| l.as_ref().and_then(|l| source(l, before).map(|t| (at(l), t.trim().to_string())));
    let (b, a) = match x.mark {
        '~' | '!' => (fetch(&x.evidence_before, true), fetch(&x.evidence_after, false)),
        '-' => (fetch(&x.evidence_before, true), None),
        '+' | '?' => (None, fetch(&x.evidence_after, false)),
        _ => (None, None),
    };
    if let (Some(b), Some(a)) = (&b, &a) {
        if b.1 == a.1 {
            return vec![format!("{:<12}{:>4}   {}", "", a.0, clip(&a.1))];
        }
    }
    let mut out = vec![];
    if let Some((n, t)) = b {
        out.push(format!("{:<12}{n:>4} - {}", "", clip(&t)));
    }
    if let Some((n, t)) = a {
        out.push(format!("{:<12}{n:>4} + {}", "", clip(&t)));
    }
    out
}

fn clip(s: &str) -> String {
    const MAX: usize = WIDTH - 19;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX - 1).collect::<String>())
    }
}

/// Items joined with ` · `, wrapped between items (never inside one).
fn wrap_items(head: &str, items: &[String]) -> Vec<String> {
    let indent = " ".repeat(head.chars().count());
    let room = WIDTH.saturating_sub(indent.len()).max(30);
    let mut lines: Vec<String> = vec![];
    let mut cur = String::new();
    for it in items {
        if !cur.is_empty() && cur.chars().count() + 3 + it.chars().count() > room {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str(" · ");
        }
        cur.push_str(it);
    }
    lines.push(cur);
    lines.into_iter().enumerate().map(|(i, l)| if i == 0 { format!("{head}{l}") } else { format!("{indent}{l}") }).collect()
}

/// Wrap `text` after `head`, continuing under the text column.
fn wrap(head: &str, text: &str) -> Vec<String> {
    let indent = " ".repeat(head.chars().count());
    let room = WIDTH.saturating_sub(indent.len()).max(30);
    let mut lines = vec![];
    let mut cur = String::new();
    for word in text.split(' ') {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > room {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    lines.push(cur);
    lines.into_iter().enumerate().map(|(i, l)| if i == 0 { format!("{head}{l}") } else { format!("{indent}{l}") }).collect()
}

/// One clause item, with effects in short verbs (`write order`, `POST payments.example.com`)
/// and values Merak cannot know as `…`.
fn item(row: &str, text: &str) -> String {
    if row != "effects" {
        return blanks(text);
    }
    let (core, later) = match text.find(" (") {
        Some(i) if text.ends_with(')') => (&text[..i], Some(text[i + 2..text.len() - 1].to_string())),
        _ => (text, None),
    };
    let mut words = core.splitn(2, ' ');
    let (kind, rest) = (words.next().unwrap_or(""), words.next().unwrap_or("").replace("<dynamic>", "(dynamic)"));
    let verb = match kind {
        "db_write" => format!("write {rest}"),
        "db_read" => format!("read {rest}"),
        "db_query" => format!("query {rest}"),
        "http" => rest.to_string(),
        "event_emit" => format!("emit {rest}"),
        "queue" => format!("queue {rest}"),
        "fs_write" => format!("write file {rest}"),
        "fs_read" => format!("read file {rest}"),
        "cache_write" => format!("write cache {rest}"),
        "cache_read" => format!("read cache {rest}"),
        _ => core.to_string(),
    };
    match later {
        Some(l) => format!("{verb} ({})", l.replace("async via ", "later, via ")),
        None => verb,
    }
}
