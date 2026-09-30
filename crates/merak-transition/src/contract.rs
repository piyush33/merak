//! Behaviour contracts: what each changed or new function promises, before and after.
//!
//! A contract has five clauses, computed from the Program Behaviour Model:
//! `who` (access requirements), `pre` (guards and checks), `where` (query filters),
//! `post` (fields written) and `effects` (external effects). A [`ContractDiff`] compares
//! them item by item and keeps unchanged items as context. Operations that are not
//! contract clauses are attached to the contract they concern: `lifecycle` (state
//! transitions), `rule` (invariants, layering, ownership), `input` (validation schemas)
//! and `calls` (changes Merak cannot type).

use crate::{closures_by_parent, is_auth_pred, scoped_filters, short, Layer, Matching, Op};
use merak_behaviour::{EntityKind, Model};
use merak_ir::Loc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractDiff {
    /// Entity id (after the change; before it for removed code), or a schema / route name.
    pub entity: String,
    /// Display name: `OrderService.cancelOrder`, `TagCreateSchema`, `POST /orders`.
    pub name: String,
    /// `changed`, `new` or `schema`.
    pub status: String,
    pub loc: Loc,
    /// Routes that reach it; `new_routes` are the ones this change adds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_routes: Vec<String>,
    pub clauses: Vec<Clause>,
    /// Ids of the operations this contract accounts for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clause {
    /// `who`, `pre`, `where` (query filters), `post`, `effects`, `lifecycle`, `rule`, `input`, `calls`, `other`.
    pub row: String,
    /// ` ` unchanged, `+` added, `-` removed, `~` changed, `!` rule broken, `?` untyped.
    pub mark: char,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// What the change means for this clause: `widened`, `reaches more rows`, `new` …
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_before: Option<Loc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_after: Option<Loc>,
}

impl ContractDiff {
    /// True when only context and untyped call changes remain.
    pub fn untyped_only(&self) -> bool {
        self.clauses.iter().all(|c| c.mark == ' ' || c.mark == '?')
    }

    pub fn has(&self, mark: char) -> bool {
        self.clauses.iter().any(|c| c.mark == mark)
    }
}

pub const ROWS: &[&str] = &[
    "takes",
    "who",
    "returns",
    "computes",
    "pre",
    "skips",
    "where",
    "post",
    "reads",
    "effects",
    "consts",
    "renders",
    "lifecycle",
    "rule",
    "input",
    "calls",
    "other",
];

/// One item of a contract clause.
#[derive(Debug, Clone)]
struct Item {
    row: &'static str,
    /// What pairs an item with its counterpart on the other side (a field, a requirement key …).
    key: String,
    text: String,
    loc: Option<Loc>,
}

fn items(model: &Model, kids: &BTreeMap<&str, Vec<&str>>, id: &str) -> Vec<Item> {
    let (Some(e), Some(s)) = (model.entities.get(id), model.summaries.get(id)) else { return vec![] };
    let mut out = vec![];
    for p in &e.params {
        let key = p.split(':').next().unwrap_or(p).trim().to_string();
        out.push(Item { row: "takes", key, text: p.clone(), loc: Some(e.loc.clone()) });
    }
    for a in s.access.values() {
        let key = a.requirement.split('=').next().unwrap_or(&a.requirement).to_string();
        let text = if a.dynamic() { format!("{key} chosen by the caller") } else { a.requirement.replace('=', " = ") };
        out.push(Item { row: "who", key, text, loc: Some(a.loc.clone()) });
    }
    // A predicate function (a policy) is its rule: `canCancel` returns `User.role ∈ {ADMIN}`.
    if let Some(r) = &e.returns {
        let text = r.render();
        let row = if is_auth_pred(r) { "who" } else { "returns" };
        let loc = e.returns_at.clone().unwrap_or_else(|| e.loc.clone());
        out.push(Item { row, key: r.field().map(str::to_string).unwrap_or_else(|| "returns".into()), text, loc: Some(loc) });
    }
    // `if (end <= start) return <span/>` is an output case (see `returns`), not a precondition.
    let cases: BTreeSet<&Loc> = e.guards.iter().filter(|g| g.returns_value).map(|g| &g.loc).collect();
    for (_, p, l) in s.requires.iter().filter(|(_, _, l)| !cases.contains(l)) {
        let text = p.render();
        // A role or permission test is an access rule, whatever form it takes.
        let row = if is_auth_pred(p) { "who" } else { "pre" };
        out.push(Item { row, key: p.field().map(str::to_string).unwrap_or_else(|| text.clone()), text, loc: Some(l.clone()) });
    }
    for (v, l) in &s.validations {
        let name = short(v).to_string();
        out.push(Item { row: "pre", key: format!("check {name}"), text: format!("passes {name}"), loc: Some(l.clone()) });
    }
    // What its local functions compute (`score = 0.25 * math.Log1p(breadth) + …`), and the
    // constants it and they use.
    let mut stack: Vec<&str> = kids.get(id).cloned().unwrap_or_default();
    let mut scope = vec![id];
    while let Some(k) = stack.pop() {
        scope.push(k);
        stack.extend(kids.get(k).into_iter().flatten());
        let Some(ke) = model.entities.get(k) else { continue };
        for o in ke.outputs.iter().filter(|o| o.formula) {
            let when = if o.when.is_empty() { String::new() } else { format!(" when {}", o.when.join(" ∧ ")) };
            out.push(Item { row: "computes", key: format!("{}{when}", ke.name), text: format!("{}{when} = {}", ke.name, o.value), loc: Some(o.loc.clone()) });
        }
    }
    if let Some(me) = model.entities.get(id) {
        for s in &me.skips {
            let text = format!("{}: skipped when {}", s.over, s.when);
            out.push(Item { row: "skips", key: text.clone(), text, loc: Some(s.loc.clone()) });
        }
    }
    let mut consts: BTreeMap<&String, &String> = BTreeMap::new();
    for k in scope {
        if let Some(ke) = model.entities.get(k) {
            consts.extend(ke.consts.iter());
        }
    }
    for (k, v) in consts {
        out.push(Item { row: "consts", key: k.clone(), text: format!("{k} = {v}"), loc: None });
    }
    let mut seen = BTreeSet::new();
    for f in scoped_filters(model, kids, id) {
        if seen.insert(f.expr.clone()) {
            // By column: `status` in `status = ?`, `icons: status` for SQL; a subquery by its text.
            let (table, pred) = f.expr.split_once(": ").unwrap_or(("", &f.expr));
            let first = pred.trim_start_matches(['(', '¬']).split(' ').next().unwrap_or(pred);
            let joins = ["join", "left", "right", "full", "inner", "cross"];
            let col = if matches!(first, "exists" | "not") {
                pred
            } else if joins.contains(&first) {
                // A join by what it joins: `left join warehouses w`.
                pred.split(" on ").next().unwrap_or(pred)
            } else {
                first
            };
            let key = if table.is_empty() { col.to_string() } else { format!("{table}: {col}") };
            out.push(Item { row: "where", key, text: f.expr.clone(), loc: Some(f.loc.clone()) });
        }
    }
    let mut own = BTreeSet::new();
    for w in &e.writes {
        let text = match &w.value {
            Some(v) => format!("{} := {v}", w.field),
            None => format!("{} is written", w.field),
        };
        if own.insert(text.clone()) {
            out.push(Item { row: "post", key: w.field.clone(), text, loc: Some(w.loc.clone()) });
        }
    }
    let own_fields: BTreeSet<&str> = e.writes.iter().map(|w| w.field.as_str()).collect();
    for f in s.writes.iter().filter(|f| !own_fields.contains(f.as_str())) {
        out.push(Item { row: "post", key: f.clone(), text: format!("{f} is written (by a callee)"), loc: None });
    }
    for o in &e.outputs {
        let case = if o.when.is_empty() { "otherwise".to_string() } else { format!("when {}", o.when.join(" ∧ ")) };
        out.push(Item { row: "returns", key: case.clone(), text: format!("{case}: {}", o.value), loc: Some(o.loc.clone()) });
        for r in &o.renders {
            let shown = if r.when.is_empty() { r.content.clone() } else { format!("{} if {}", r.content, r.when) };
            out.push(Item { row: "renders", key: format!("{case}|{shown}"), text: format!("{shown}: {}", r.style), loc: Some(o.loc.clone()) });
        }
    }
    let pure = s.effects.is_empty() && s.writes.is_empty();
    if pure && !e.outputs.is_empty() {
        out.push(Item { row: "effects", key: "none".into(), text: "none found".into(), loc: None });
    }
    for (k, i) in model.effects_outside_checks(id) {
        let later: Vec<&str> = i.modes.iter().filter(|m| m.starts_with("async")).map(String::as_str).collect();
        // What a request sends is part of the effect: `POST …/search {query, attrs?}`,
        // then when it happens: `(async via OrderCancelled)`.
        let mut text = k.render();
        if !i.payloads.is_empty() {
            text = format!("{text} {}", i.payloads.iter().cloned().collect::<Vec<_>>().join(" | "));
        }
        if !later.is_empty() && !i.modes.contains("sync") {
            text = format!("{text} ({})", later.join(", "));
        }
        out.push(Item { row: "effects", key: format!("{} {}", k.kind, k.target), text, loc: i.evidence.iter().next().cloned() });
    }
    out
}

/// Item-by-item comparison of two contracts, in clause order.
fn compare(before: &[Item], after: &[Item]) -> Vec<Clause> {
    let mut out = vec![];
    for row in ["takes", "who", "returns", "computes", "pre", "skips", "where", "post", "effects", "consts", "renders"] {
        let mut left: Vec<&Item> = before.iter().filter(|i| i.row == row).collect();
        for b in after.iter().filter(|i| i.row == row) {
            // Same text first (unchanged), then same key (changed).
            let pos = left.iter().position(|a| a.text == b.text).or_else(|| left.iter().position(|a| a.key == b.key));
            match pos.map(|p| left.remove(p)) {
                Some(a) if a.text == b.text => out.push(clause(row, ' ', Some(a), Some(b))),
                Some(a) => out.push(clause(row, '~', Some(a), Some(b))),
                None => out.push(clause(row, '+', None, Some(b))),
            }
        }
        for a in left {
            out.push(clause(row, '-', Some(a), None));
        }
        // "none found" says nothing when effects appear or disappear: the real items do.
        out.retain(|c| !(c.row == "effects" && c.mark != ' ' && (c.before.as_deref() == Some("none found") || c.after.as_deref() == Some("none found"))));
    }
    out
}

fn clause(row: &str, mark: char, a: Option<&Item>, b: Option<&Item>) -> Clause {
    let before = a.map(|i| i.text.clone());
    let after = b.map(|i| i.text.clone());
    let tag = tag(row, mark, before.as_deref(), after.as_deref());
    Clause {
        row: row.into(),
        mark,
        before: if mark == ' ' { None } else { before },
        after,
        tag,
        evidence_before: a.and_then(|i| i.loc.clone()),
        evidence_after: b.and_then(|i| i.loc.clone()),
    }
}

/// What a change to one clause item means, in a few words.
fn tag(row: &str, mark: char, before: Option<&str>, after: Option<&str>) -> Option<String> {
    let t = match (row, mark) {
        (_, ' ') => return None,
        ("who", '+') => "new requirement",
        ("who", '-') => "requirement dropped",
        ("who", '~') => return Some(set_change(before.unwrap_or(""), after.unwrap_or(""))),
        ("returns", '~') => "returns something else",
        ("computes", '~') => "computes something else",
        ("computes", '+') => "new computation",
        ("computes", '-') => "computation removed",
        ("skips", '+') => "leaves more items out",
        ("skips", '-') => "no longer leaves these out",
        ("consts", '~') => "value changed",
        ("consts", '+') => "now used",
        ("consts", '-') => "no longer used",
        ("returns", '+') => "new output case",
        ("returns", '-') => "output case removed",
        ("takes", '+') => "new parameter",
        ("takes", '-') => "parameter removed",
        ("takes", '~') => "parameter type changed",
        // `span.text-gray-500 → span.font-semibold` says it; a tag would repeat it.
        ("renders", '~') => return None,
        ("renders", '+') => "now rendered",
        ("renders", '-') => "no longer rendered",
        ("pre", '+') => "new check",
        ("pre", '-') => "check dropped",
        ("pre", '~') => return Some(set_change(before.unwrap_or(""), after.unwrap_or(""))),
        // An outer join keeps every row: it brings data in rather than filtering.
        ("where", '+' | '-') if [before, after].iter().flatten().any(|t| [": left join", ": right join", ": full join"].iter().any(|j| t.contains(j))) => {
            if mark == '+' {
                "joins in more data"
            } else {
                "joins in less data"
            }
        }
        ("where", '+') => "reaches fewer rows",
        ("where", '-') => "reaches more rows",
        ("where", '~') => return Some(match reach(before.unwrap_or(""), after.unwrap_or("")) {
            Some(Reach::Narrower(extra)) => format!("narrower: rows must now also match {extra}"),
            Some(Reach::Wider(extra)) => format!("wider: rows can now also match {extra}"),
            Some(Reach::Looser(gone)) => format!("wider: rows no longer have to match {gone}"),
            Some(Reach::Tighter(gone)) => format!("narrower: rows matching only {gone} are no longer reached"),
            None => "reaches different rows".into(),
        }),
        ("post", '+') => "new write",
        ("post", '-') => "no longer written",
        ("post", '~') => "writes a different value",
        ("effects", '+') => "new effect",
        ("effects", '-') => "effect dropped",
        ("effects", '~') if body_delta(before, after).is_some() => return body_delta(before, after),
        ("effects", '~') if after.is_some_and(|a| a.contains("async")) => "now happens later",
        ("effects", '~') => "now happens right away",
        _ => return None,
    };
    Some(t.into())
}

/// How a changed row filter moved: terms ANDed in or dropped, alternatives ORed in or dropped.
#[derive(Debug, PartialEq)]
pub(crate) enum Reach {
    /// `a` → `a AND b`: rows must also match `b`.
    Narrower(String),
    /// `a` → `a OR b`.
    Wider(String),
    /// `a AND b` → `a`.
    Looser(String),
    /// `a OR b` → `a`.
    Tighter(String),
}

/// Compare two filters on the same scope term by term. Builders are matched by name, not by
/// what they are passed (`⟨anchorPredicate(a)⟩` is `⟨built by anchorPredicate⟩`), so a
/// builder that now ANDs in a second term reads as that term.
pub(crate) fn reach(before: &str, after: &str) -> Option<Reach> {
    use merak_behaviour::clause::{split_top, strip_outer};
    let (sa, a) = before.split_once(": ")?;
    let (sb, b) = after.split_once(": ")?;
    if sa != sb {
        return None;
    }
    // Only the part that differs: `any of (X)` → `any of (Y)` compares X with Y, and so does
    // `case a.ix ⟨built by f: X⟩ else …` → `case a.ix ⟨built by g: Y⟩ else …`.
    let (a, b) = differing(a, b)?;
    let unwrap = |t: &str| -> String {
        let t = strip_outer(t);
        match t.strip_prefix("⟨built by ").and_then(|x| x.strip_suffix('⟩')).and_then(|x| x.split_once(": ")) {
            Some((_, shape)) => shape.to_string(),
            None => t.to_string(),
        }
    };
    let (a, b) = (unwrap(a), unwrap(b));
    let (a, b) = (a.as_str(), b.as_str());
    let key = |t: &str| -> String {
        let t = strip_outer(t);
        match t.strip_prefix('⟨').and_then(|x| x.strip_suffix('⟩')) {
            Some(inner) => {
                let inner = inner.strip_prefix("built by ").unwrap_or(inner);
                format!("⟨{}⟩", inner.split(['(', ',']).next().unwrap_or(inner).trim())
            }
            None => t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase(),
        }
    };
    // The terms of `y` left over once each term of `x` is matched, if every one is.
    let extra = |x: &[&str], y: &[&str]| -> Option<Vec<String>> {
        let mut left: Vec<&str> = y.to_vec();
        for t in x {
            let i = left.iter().position(|u| key(u) == key(t))?;
            left.remove(i);
        }
        (!left.is_empty()).then(|| left.iter().map(|t| strip_outer(t).to_string()).collect())
    };
    let terms = |s: &str, sep: &str| -> Vec<String> {
        let s = strip_outer(s);
        let mut v: Vec<String> = split_top(s, sep).into_iter().map(|t| t.trim().to_string()).collect();
        if v.len() == 1 {
            v = split_top(s, &sep.to_lowercase()).into_iter().map(|t| t.trim().to_string()).collect();
        }
        v
    };
    for (sep, grows, shrinks) in [(" AND ", Reach::Narrower as fn(String) -> Reach, Reach::Looser as fn(String) -> Reach), (" OR ", Reach::Wider, Reach::Tighter)] {
        let (ta, tb) = (terms(a, sep), terms(b, sep));
        let (ta, tb): (Vec<&str>, Vec<&str>) = (ta.iter().map(|s| s.as_str()).collect(), tb.iter().map(|s| s.as_str()).collect());
        if ta.len() == tb.len() {
            continue;
        }
        if let Some(e) = extra(&ta, &tb) {
            return Some(grows(e.join(sep)));
        }
        if let Some(e) = extra(&tb, &ta) {
            return Some(shrinks(e.join(sep)));
        }
    }
    None
}

/// The differing middles of two filters, cut outside atoms. `None` under a negation, where a
/// narrower term would mean more rows.
pub(crate) fn differing<'s>(a: &'s str, b: &'s str) -> Option<(&'s str, &'s str)> {
    let (ca, cb): (Vec<(usize, char)>, Vec<(usize, char)>) = (a.char_indices().collect(), b.char_indices().collect());
    // Whether a cut before char `i` of `cs` is outside every atom.
    let outside = |cs: &[(usize, char)], i: usize| cs[..i].iter().fold(0i32, |d, (_, c)| d + i32::from(*c == '⟨') - i32::from(*c == '⟩')) == 0;
    let mut p = ca.iter().zip(&cb).take_while(|(x, y)| x.1 == y.1).count();
    while p > 0 && !(outside(&ca, p) && matches!(ca[p - 1].1, ' ' | '(')) {
        p -= 1;
    }
    let max = ca.len().min(cb.len()) - p;
    let mut q = ca.iter().rev().zip(cb.iter().rev()).take(max).take_while(|(x, y)| x.1 == y.1).count();
    while q > 0 && !(outside(&ca, ca.len() - q) && matches!(ca[ca.len() - q].1, ' ' | ')')) {
        q -= 1;
    }
    let head = a[..ca.get(p).map_or(a.len(), |c| c.0)].to_lowercase();
    if head.contains("not ") || head.contains('!') {
        return None;
    }
    let cut = |s: &'s str, cs: &[(usize, char)]| &s[cs.get(p).map_or(s.len(), |c| c.0)..cs.get(cs.len() - q).map_or(s.len(), |c| c.0)];
    Some((cut(a, &ca), cut(b, &cb)))
}

/// `POST …/search {query}` → `{query, attrs?}`: "now also sends attrs".
fn body_delta(before: Option<&str>, after: Option<&str>) -> Option<String> {
    let fields = |s: &str| -> Option<Vec<String>> {
        let s = match s.rfind(" (") {
            Some(i) if s.ends_with(')') => &s[..i],
            _ => s,
        };
        let body = s.rsplit_once(" {")?.1.strip_suffix('}')?;
        Some(body.split(", ").map(|f| f.trim_end_matches('?').to_string()).filter(|f| !f.is_empty()).collect())
    };
    let (b, a) = (fields(before?)?, fields(after?)?);
    let added: Vec<&String> = a.iter().filter(|f| !b.contains(f)).collect();
    let removed: Vec<&String> = b.iter().filter(|f| !a.contains(f)).collect();
    let list = |xs: &[&String]| xs.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ");
    match (added.is_empty(), removed.is_empty()) {
        // Same fields: something else changed (timing, the endpoint), and says so itself.
        (true, true) => None,
        (false, true) => Some(format!("now also sends {}", list(&added))),
        (true, false) => Some(format!("no longer sends {}", list(&removed))),
        (false, false) => Some(format!("now also sends {}, no longer sends {}", list(&added), list(&removed))),
    }
}

/// `x ∈ {A}` → `x ∈ {A, B}`: widened, narrowed or changed.
fn set_change(before: &str, after: &str) -> String {
    let set = |p: &str| -> Option<(String, BTreeSet<String>)> {
        let (f, rest) = p.split_once(" ∈ ")?;
        let inner = rest.trim().strip_prefix('{')?.strip_suffix('}')?;
        Some((f.to_string(), inner.split(", ").map(str::to_string).collect()))
    };
    match (set(before), set(after)) {
        (Some((f1, a)), Some((f2, b))) if f1 == f2 && b.is_superset(&a) => "widened".into(),
        (Some((f1, a)), Some((f2, b))) if f1 == f2 && b.is_subset(&a) => "narrowed".into(),
        _ => "changed".into(),
    }
}

/// `src/a.ts::OrderService.cancel.$if#2` → the top-level entity id `src/a.ts::OrderService.cancel`.
fn top_level(model: &Model, id: &str) -> String {
    let mut cur = id.to_string();
    while let Some(p) = model.entities.get(&cur).and_then(|e| e.parent.clone()) {
        cur = p;
    }
    cur
}

/// The top-level entity whose body contains `loc`.
fn entity_at<'m>(model: &'m Model, loc: &Loc) -> Option<&'m str> {
    model
        .entities
        .values()
        .filter(|e| e.parent.is_none() && e.loc.file == loc.file && e.loc.line <= loc.line && loc.line <= e.end_line)
        .min_by_key(|e| e.end_line - e.loc.line)
        .map(|e| e.id.as_str())
}

pub(crate) fn build(a: &Model, b: &Model, m: &Matching, ops: &[Op]) -> Vec<ContractDiff> {
    let (kids_a, kids_b) = (closures_by_parent(a), closures_by_parent(b));
    let mut contracts: BTreeMap<String, ContractDiff> = BTreeMap::new();
    let backward: &BTreeMap<String, String> = &m.backward;

    // Changed code: every top-level entity with a changed body (its own or a closure's).
    let mut changed: BTreeSet<String> = BTreeSet::new();
    for (ida, idb) in &m.forward {
        let (ea, eb) = (&a.entities[ida], &b.entities[idb]);
        if ea.body_hash != eb.body_hash || ea.consts != eb.consts {
            changed.insert(top_level(b, idb));
        }
    }
    // … and every entity a behavioural op is about.
    for op in ops.iter().filter(|o| o.layer != Layer::Structural && o.subject.contains("::")) {
        if b.entities.contains_key(&op.subject) {
            changed.insert(top_level(b, &op.subject));
        }
    }
    // New functions and methods are part of the change, pure helpers included.
    for (id, e) in &b.entities {
        if e.parent.is_none()
            && e.kind != EntityKind::Constructor
            && !backward.contains_key(id)
            && (!e.outputs.is_empty() || b.summaries.get(id).is_some_and(|s| !s.effects.is_empty()))
        {
            changed.insert(id.clone());
        }
    }
    let routes_before: BTreeSet<String> = a.routes.iter().map(|r| r.key()).collect();
    // New handlers are new code even when nothing else points at them.
    let subs_before: BTreeSet<(String, String)> = a.subscriptions.iter().map(|s| (s.event.clone(), m.to_after(&s.handler))).collect();
    // A handler closure (`events.on('X', async (e) => …)`) has the behaviour; the function
    // registering it has none of its own.
    let mut handler_names: BTreeMap<String, String> = BTreeMap::new();
    for s in b.subscriptions.iter().filter(|s| !subs_before.contains(&(s.event.clone(), s.handler.clone()))) {
        let top = top_level(b, &s.handler);
        if top != s.handler {
            handler_names.insert(s.handler.clone(), format!("{}, on {}", short(&top), s.event));
            changed.remove(&top);
        }
        changed.insert(s.handler.clone());
    }
    for idb in changed {
        let Some(e) = b.entities.get(&idb) else { continue };
        if e.kind == EntityKind::Constructor {
            continue;
        }
        let routes = b.entry_points_reaching(&idb);
        let new_routes: Vec<String> = b.routes.iter().filter(|r| r.handler == idb && !routes_before.contains(&r.key())).map(|r| r.key()).collect();
        let (status, clauses) = match backward.get(&idb) {
            Some(ida) => ("changed", compare(&items(a, &kids_a, ida), &items(b, &kids_b, &idb))),
            None => ("new", compare(&[], &items(b, &kids_b, &idb))),
        };
        let name = handler_names.get(&idb).cloned().unwrap_or_else(|| short(&idb).to_string());
        contracts.insert(
            idb.clone(),
            ContractDiff { entity: idb.clone(), name, status: status.into(), loc: e.loc.clone(), routes, new_routes, clauses, ops: vec![] },
        );
    }
    // New route handlers that are closures (`router.post('/x', async (req) => …)`) are named by their route.
    for r in &b.routes {
        if routes_before.contains(&r.key()) {
            continue;
        }
        if let Some(c) = contracts.get_mut(&top_level(b, &r.handler)) {
            if !c.new_routes.contains(&r.key()) && c.status == "new" && b.entities.get(&r.handler).is_some_and(|h| h.parent.is_some()) {
                c.new_routes.push(r.key());
            }
        }
    }

    // Attach every operation to the contract it concerns.
    let find =
        |contracts: &BTreeMap<String, ContractDiff>, name: &str| contracts.values().find(|c| c.name == name || c.entity == name).map(|c| c.entity.clone());
    let mut program: Vec<(Clause, String)> = vec![];
    let mut schemas: BTreeMap<String, ContractDiff> = BTreeMap::new();
    for op in ops.iter().filter(|o| o.layer != Layer::Structural) {
        let at = |l: &Loc| entity_at(b, l).map(str::to_string);
        let owner: Option<String> = match op.kind.as_str() {
            k if k.starts_with("SCHEMA_") => {
                let local = op.subject.rsplit("::").next().unwrap_or(&op.subject);
                let (schema, field) = local.split_once('.').unwrap_or((local, "*"));
                let key = op.subject.split_once("::").map(|(m, _)| format!("{m}::{schema}")).unwrap_or_else(|| schema.to_string());
                let loc = op.evidence_after.first().or(op.evidence_before.first()).cloned().unwrap_or_else(|| Loc { file: String::new(), line: 0 });
                let c = schemas.entry(key.clone()).or_insert_with(|| ContractDiff {
                    entity: key.clone(),
                    name: schema.to_string(),
                    status: "schema".into(),
                    loc,
                    routes: vec![],
                    new_routes: vec![],
                    clauses: vec![],
                    ops: vec![],
                });
                let mark = match op.kind.as_str() {
                    "SCHEMA_FIELD_ADDED" => '+',
                    "SCHEMA_FIELD_REMOVED" => '-',
                    _ => '~',
                };
                let label = |v: &Option<String>| v.as_ref().map(|v| if field == "*" { v.clone() } else { format!("{field}: {v}") });
                let tag = match mark {
                    '+' => "new field",
                    '-' => "field removed",
                    _ if field == "*" => "whole schema",
                    _ => "validated differently",
                };
                c.clauses.push(Clause {
                    row: "input".into(),
                    mark,
                    before: label(&op.before),
                    after: label(&op.after),
                    tag: Some(tag.into()),
                    evidence_before: op.evidence_before.first().cloned(),
                    evidence_after: op.evidence_after.first().cloned(),
                });
                c.ops.push(op.id.clone());
                continue;
            }
            k if k.starts_with("STATE_TRANSITION") => {
                let writer = op.note.as_deref().and_then(|n| n.strip_prefix("new path via ")).and_then(|w| w.split(", ").next()).map(str::to_string);
                let owner = writer.and_then(|w| find(&contracts, &w)).or_else(|| op.evidence_after.first().and_then(at));
                let (field, to) = op.subject.split_once(" → ").unwrap_or((&op.subject, "?"));
                let states = |s: &Option<String>| s.as_ref().map(|v| format!("{field} → {to} from {v}"));
                let tag = match op.kind.as_str() {
                    "STATE_TRANSITION_SOURCE_WIDENED" | "STATE_TRANSITION_ADDED" => "more ways to reach this state",
                    "STATE_TRANSITION_SOURCE_NARROWED" | "STATE_TRANSITION_REMOVED" => "fewer ways to reach this state",
                    _ => "different ways to reach this state",
                };
                let cl = Clause {
                    row: "lifecycle".into(),
                    mark: if op.before.is_none() {
                        '+'
                    } else if op.after.is_none() {
                        '-'
                    } else {
                        '~'
                    },
                    before: states(&op.before),
                    after: states(&op.after),
                    tag: Some(tag.into()),
                    evidence_before: op.evidence_before.first().cloned(),
                    evidence_after: op.evidence_after.first().cloned(),
                };
                attach(&mut contracts, &mut program, owner, cl, &op.id);
                continue;
            }
            "INVARIANT_VIOLATED" => {
                let who = op.after.as_deref().map(|x| x.replace(" does not satisfy it", "")).unwrap_or_default();
                let owner = find(&contracts, &who).or_else(|| op.evidence_after.first().and_then(at));
                let cl = Clause {
                    row: "rule".into(),
                    mark: '!',
                    before: None,
                    after: Some(format!("breaks {} ({:.0}% of writers follow it)", op.subject, op.confidence * 100.0)),
                    tag: Some("inferred pattern".into()),
                    evidence_before: None,
                    evidence_after: op.evidence_after.first().cloned(),
                };
                attach(&mut contracts, &mut program, owner, cl, &op.id);
                continue;
            }
            "LAYER_VIOLATION_INTRODUCED" | "OWNERSHIP_VIOLATION_INTRODUCED" => {
                let owner = if op.kind.starts_with("OWNERSHIP") {
                    op.after.as_deref().and_then(|w| w.split("; ").next()).map(|w| top_level(b, w)).filter(|w| contracts.contains_key(w))
                } else {
                    op.evidence_after.first().and_then(at)
                };
                let base = |f: &str| f.rsplit('/').next().unwrap_or(f).to_string();
                let text = if op.kind.starts_with("LAYER") {
                    let (from, to) = op.subject.split_once(" → ").unwrap_or((&op.subject, "?"));
                    let files: Vec<String> = op.after.as_deref().unwrap_or("").split("; ").filter_map(|d| d.split_once(" → ")).map(|(_, t)| base(t)).collect();
                    format!("the {from} layer now calls {} in the {to} layer, which merak.toml forbids", files.join(", "))
                } else {
                    format!("writes {} outside the code that owns it", op.subject)
                };
                let cl = Clause {
                    row: "rule".into(),
                    mark: '!',
                    before: None,
                    after: Some(text),
                    tag: Some("declared in merak.toml".into()),
                    evidence_before: None,
                    evidence_after: op.evidence_after.first().cloned(),
                };
                attach(&mut contracts, &mut program, owner, cl, &op.id);
                continue;
            }
            // What an entity now reads (its writes are `post` items already).
            "DATAFLOW_EXPANDED" | "DATAFLOW_RESTRICTED" => {
                let owner = Some(top_level(b, &op.subject)).filter(|o| contracts.contains_key(o));
                let text = if op.kind == "DATAFLOW_EXPANDED" { op.after.as_deref() } else { op.before.as_deref() };
                let reads = text.and_then(|t| t.split("; ").find_map(|p| p.strip_prefix("reads "))).map(str::to_string);
                if let Some(reads) = reads {
                    let expanded = op.kind == "DATAFLOW_EXPANDED";
                    let cl = Clause {
                        row: "reads".into(),
                        mark: if expanded { '+' } else { '-' },
                        before: (!expanded).then(|| reads.clone()),
                        after: expanded.then_some(reads),
                        tag: Some(if expanded { "reads more data" } else { "reads less data" }.into()),
                        // The op's evidence is the function itself, not where it reads.
                        evidence_before: None,
                        evidence_after: None,
                    };
                    attach(&mut contracts, &mut program, owner, cl, &op.id);
                    continue;
                }
                owner
            }
            "UNCLASSIFIED_CHANGE" => {
                let owner = Some(top_level(b, &op.subject)).filter(|o| contracts.contains_key(o));
                let cl = Clause {
                    row: "calls".into(),
                    mark: '?',
                    before: op.before.clone(),
                    after: op.after.clone(),
                    tag: None,
                    evidence_before: op.evidence_before.first().cloned(),
                    evidence_after: op.evidence_after.first().cloned(),
                };
                attach(&mut contracts, &mut program, owner, cl, &op.id);
                continue;
            }
            // A new route belongs to the new code it runs: the handler, or (for a closure
            // registered on a router) the new function that closure calls.
            "ENTRYPOINT_ADDED" => {
                let handler = b.routes.iter().find(|r| r.key() == op.subject).map(|r| top_level(b, &r.handler));
                let runs = contracts
                    .values()
                    .find(|c| c.status == "new" && c.routes.contains(&op.subject) && Some(&c.entity) != handler.as_ref())
                    .map(|c| c.entity.clone());
                let owner = runs.or(handler);
                if let Some(c) = owner.as_ref().and_then(|o| contracts.get_mut(o)) {
                    if !c.new_routes.contains(&op.subject) {
                        c.new_routes.push(op.subject.clone());
                    }
                }
                owner
            }
            "EVENT_HANDLER_ADDED" => {
                let owner = b.subscriptions.iter().find(|s| s.event == op.subject).map(|s| {
                    if contracts.contains_key(&s.handler) {
                        s.handler.clone()
                    } else {
                        top_level(b, &s.handler)
                    }
                });
                if let Some(c) = owner.as_ref().and_then(|o| contracts.get_mut(o)) {
                    let label = format!("event {}", op.subject);
                    if !c.new_routes.contains(&label) {
                        c.new_routes.push(label);
                    }
                }
                owner
            }
            _ => b.entities.contains_key(&op.subject).then(|| top_level(b, &op.subject)),
        };
        // Contract-clause ops (auth, guards, filters, effects, dataflow …) are already in the diff.
        if let Some(c) = owner.and_then(|o| contracts.get_mut(&o)) {
            c.ops.push(op.id.clone());
        } else if !matches!(op.kind.as_str(), "BEHAVIOUR_ADDED" | "ENTRYPOINT_REMOVED" | "EVENT_HANDLER_REMOVED") {
            let cl = Clause {
                row: "other".into(),
                mark: '~',
                before: op.before.clone(),
                after: Some(format!("{} {}", op.kind.to_lowercase().replace('_', " "), op.subject)),
                tag: None,
                evidence_before: op.evidence_before.first().cloned(),
                evidence_after: op.evidence_after.first().cloned(),
            };
            program.push((cl, op.id.clone()));
        }
    }

    // New code is described where it starts: a new function whose clauses a new caller's
    // contract already contains (the service behind a new controller method) adds nothing.
    let new_ids: Vec<String> = contracts.values().filter(|c| c.status == "new").map(|c| c.entity.clone()).collect();
    let mut drop = BTreeSet::new();
    for id in &new_ids {
        let keys =
            |c: &ContractDiff| -> BTreeSet<(String, String)> { c.clauses.iter().map(|x| (x.row.clone(), x.after.clone().unwrap_or_default())).collect() };
        let callers: Vec<String> = b.entities.values().filter(|e| e.calls.iter().any(|c| &c.target == id)).map(|e| top_level(b, &e.id)).collect();
        let covered = callers.iter().any(|caller| {
            caller != id
                && contracts.get(caller).is_some_and(|cc| {
                    cc.status == "new" && {
                        let theirs = keys(cc);
                        let own_keys: BTreeSet<(String, String)> =
                            contracts[id].clauses.iter().map(|x| (x.row.clone(), x.after.clone().unwrap_or_default())).collect();
                        own_keys.iter().all(|(row, text)| theirs.iter().any(|(r2, t2)| r2 == row && same_key(row, text, t2)))
                    }
                })
        });
        if covered {
            drop.insert(id.clone());
        }
    }
    for id in &drop {
        if let Some(c) = contracts.remove(id) {
            // Keep its operations accounted for by the caller's contract.
            if let Some(owner) = contracts
                .values_mut()
                .find(|cc| cc.status == "new" && b.entities.values().any(|e| top_level(b, &e.id) == cc.entity && e.calls.iter().any(|x| &x.target == id)))
            {
                owner.ops.extend(c.ops);
            }
        }
    }

    // What a function returns and renders explains a change Merak could not type otherwise:
    // the `?` row is then noise. A new parameter does not: what is done with it is the change.
    for c in contracts.values_mut() {
        if c.clauses.iter().any(|x| matches!(x.row.as_str(), "returns" | "computes" | "renders") && x.mark != ' ') {
            c.clauses.retain(|x| x.mark != '?');
        }
    }

    // Keep contracts that say something: a changed clause or an attached operation.
    contracts.retain(|_, c| c.clauses.iter().any(|x| x.mark != ' ') || !c.ops.is_empty());
    // A new function no operation speaks for (a pure or extracted helper) belongs to the
    // change only when something that did change calls it; an extracted helper whose
    // callers behave as before is a refactor.
    let kept: BTreeSet<String> = contracts.keys().cloned().collect();
    contracts.retain(|id, c| {
        c.status != "new"
            || !c.ops.is_empty()
            || b.entities.values().any(|e| e.calls.iter().any(|x| &x.target == id) && kept.contains(&top_level(b, &e.id)) && &top_level(b, &e.id) != id)
    });
    let mut out: Vec<ContractDiff> = contracts.into_values().collect();
    out.extend(schemas.into_values());
    if !program.is_empty() {
        out.push(ContractDiff {
            entity: "*".into(),
            name: "Program".into(),
            status: "changed".into(),
            loc: Loc { file: String::new(), line: 0 },
            routes: vec![],
            new_routes: vec![],
            ops: program.iter().map(|(_, id)| id.clone()).collect(),
            clauses: program.into_iter().map(|(c, _)| c).collect(),
        });
    }
    for c in &mut out {
        c.clauses.sort_by_key(|x| ROWS.iter().position(|r| *r == x.row).unwrap_or(ROWS.len()));
    }
    out.sort_by_key(|c| (risk(c), c.name.clone()));
    out
}

/// Two items of a row describe the same thing (same field / requirement / effect target).
fn same_key(row: &str, a: &str, b: &str) -> bool {
    let head = |s: &str| -> String {
        let s = s.replace(" (by a callee)", "");
        match row {
            "post" => s.split([' ', ':']).next().unwrap_or("").to_string(),
            "effects" => s.split(" (").next().unwrap_or("").to_string(),
            _ => s,
        }
    };
    head(a) == head(b)
}

fn attach(contracts: &mut BTreeMap<String, ContractDiff>, program: &mut Vec<(Clause, String)>, owner: Option<String>, cl: Clause, op: &str) {
    match owner.and_then(|o| contracts.get_mut(&o)) {
        Some(c) => {
            c.clauses.push(cl);
            c.ops.push(op.to_string());
        }
        None => program.push((cl, op.to_string())),
    }
}

/// Read the riskiest first: broken rules, dropped access or checks, wider reach.
fn risk(c: &ContractDiff) -> u8 {
    let any = |row: &str, marks: &[char]| c.clauses.iter().any(|x| x.row == row && marks.contains(&x.mark));
    if c.has('!') {
        0
    } else if any("who", &['-', '~']) || any("pre", &['-']) || any("where", &['-']) || c.clauses.iter().any(|x| x.tag.as_deref().is_some_and(|t| t == "widened" || t.starts_with("wider:"))) {
        1
    } else if c.status == "changed" && !c.untyped_only() {
        2
    } else if c.status == "new" {
        3
    } else if c.status == "schema" {
        4
    } else {
        5
    }
}

#[cfg(test)]
mod reach_tests {
    use super::{reach, Reach};

    #[test]
    fn terms_anded_or_ored_in_move_the_reach() {
        let ctx = "⟨anchorPredicate(c), each c in sc.Context⟩";
        assert_eq!(
            reach("scoped: any of (⟨built by anchorPredicate⟩)", &format!("scoped: any of (⟨anchorPredicate(sc.Anchor)⟩ AND {ctx})")),
            Some(Reach::Narrower(ctx.into()))
        );
        assert_eq!(
            reach("p: case ⟨built by f⟩ else false end", &format!("p: case ⟨built by g: ⟨f(x)⟩ AND {ctx}⟩ else false end")),
            Some(Reach::Narrower(ctx.into()))
        );
        assert_eq!(reach("p: a = _ AND b = _", "p: a = _"), Some(Reach::Looser("b = _".into())));
        assert_eq!(reach("p: a = _", "p: a = _ OR b = _"), Some(Reach::Wider("b = _".into())));
        assert_eq!(reach("p: (a = _ OR b = _)", "p: (a = _)"), Some(Reach::Tighter("b = _".into())));
        // Under a negation an extra term means more rows, not fewer: not classified.
        assert_eq!(reach("p: not (a = _)", "p: not (a = _ AND b = _)"), None);
        // Another builder with the same terms, or another scope, says nothing.
        assert_eq!(reach("p: ⟨built by f⟩", "p: ⟨built by g⟩"), None);
        assert_eq!(reach("p: a = _", "q: a = _ AND b = _"), None);
    }
}
