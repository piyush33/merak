//! The behaviour view: what changed, what it means, where the evidence is.
//!
//! Contracts are grouped by the entry point that reaches them (by file when none does). A
//! group opens with its direction (`▲ wider`, `▼ narrower` …), a sentence naming its
//! weightiest changes and what it touches outside the program. Each function then lists its
//! changed clauses as behaviour lines — `FILTER`, `INPUT`, `WRITES` … — riskiest first, each
//! with one line of code as evidence. Clauses that only renamed things fold into one line,
//! unchanged clauses into a count.
//!
//! Every phrase is a template over the contract diff: nothing is inferred beyond it.

use crate::contract::{differing, Clause, ContractDiff};
use crate::render::{also_line, blanks, call_name, calls_summary, clip, item, plural, restyled, wrap, SourceLine, WIDTH};
use crate::Transition;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

/// Column where a line's text starts: `  ▼ FILTER    `.
const TEXT: usize = 14;

/// One behaviour line: `▼ FILTER  rows must now also match …`.
struct Line {
    glyph: char,
    kind: &'static str,
    text: String,
    /// `was …` / `now …` under the text.
    detail: Vec<String>,
    /// Lines of code behind it, usually one.
    evidence: Vec<String>,
    /// How many clauses said the same thing.
    places: usize,
    /// When the line changes how the work is done rather than what comes out (a filter moved
    /// into SQL, a query made conditional, an index): that, said for a summary.
    work: Option<String>,
}

impl Line {
    fn rank(&self) -> u8 {
        match (self.glyph, self.kind) {
            ('!', _) => 0,
            ('+', "EFFECT") if self.text.starts_with("now deletes") => 1,
            ('▲', _) => 1,
            ('▼', _) => 2,
            ('~', "FILTER") => 3,
            ('+', "CALLS") => 4,
            ('~', "CHECK") => 4,
            (_, "ACCESS" | "CHECK" | "PASSES" | "BUILDS") => 3,
            (_, "EFFECT" | "STATE") => 4,
            ('~', "OUTPUT" | "RENDERS" | "CONSTANT") => 4,
            (_, "HANDLES") => 3,
            (_, "WRITES" | "VALIDATES") => 5,
            (_, "READS") => 6,
            (_, "FILTER") => 6,
            (_, "OUTPUT" | "COMPUTES" | "RENDERS") => 7,
            (_, "INPUT") => 8,
            ('◆', _) => 10,
            ('?', _) => 11,
            _ => 9,
        }
    }
}

pub fn render(t: &Transition, header: Option<&str>, source: SourceLine) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## Behaviour change{}\n", header.map(|h| format!(" · {h}")).unwrap_or_default());
    if t.contracts.is_empty() {
        let _ = writeln!(s, "{}", crate::render::markdown(t, None).lines().skip(2).collect::<Vec<_>>().join("\n"));
        return s;
    }
    // Names that are themselves changing: a clause that swaps one for another changed, it
    // was not renamed.
    let changing: BTreeSet<&str> = t.contracts.iter().map(|c| c.name.rsplit('.').next().unwrap_or(&c.name)).collect();
    // Functions this change adds, by name.
    let mut fresh: BTreeSet<String> = t.contracts.iter().filter(|c| c.status == "new").map(|c| last(&c.name).to_string()).collect();
    fresh.extend(t.ops.iter().filter(|o| o.kind == "ENTITY_ADDED").map(|o| last(&o.subject).to_string()));
    let mut groups: Vec<(String, Vec<&ContractDiff>)> = vec![];
    for c in &t.contracts {
        let key = group(home(t, c));
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cs)) => cs.push(c),
            None => groups.push((key, vec![c])),
        }
    }
    // The code that changed, then the new code it calls.
    for (_, cs) in groups.iter_mut() {
        cs.sort_by_key(|c| c.status == "new");
    }
    for (key, cs) in &groups {
        let blocks: Vec<(&ContractDiff, Vec<Line>, Vec<String>)> = cs
            .iter()
            .map(|c| {
                let (lines, same) = lines(t, c, &changing, &fresh, source);
                (*c, lines, same)
            })
            .collect();
        let all: Vec<&Line> = blocks.iter().flat_map(|(_, ls, _)| ls).collect();
        // Nothing reaches more or less, and the work done changed: say that first.
        // Serving kept data can serve it stale: a change of freshness, not just of work.
        let risky = all.iter().any(|l| matches!(l.glyph, '!' | '▲' | '▼') || l.work.as_deref().is_some_and(|w| w.contains("from memory")));
        let worked = work(&blocks);
        let badge = if !risky && !worked.is_empty() {
            let unknown = all.iter().any(|l| l.glyph == '?' && l.work.is_none());
            format!("≈ same results, different work{}", if unknown { " · ? check by hand" } else { "" })
        } else {
            badge(&all, cs)
        };
        let _ = writeln!(s, "### {key} · {badge}\n");
        let mut lead = if worked.is_empty() { sentence(&blocks) } else { vec![] };
        for w in worked {
            if !lead.contains(&w) {
                lead.push(w);
            }
        }
        // New code a lead line names: what it does, so the line says what the change does.
        for l in lead.iter_mut() {
            let named: Vec<String> = l.split("new `").skip(1).filter_map(|r| r.split('`').next()).map(str::to_string).collect();
            for f in named {
                if let Some(d) = does(t, &f) {
                    if let Some(stripped) = l.strip_suffix('.') {
                        *l = format!("{stripped} (`{f}` {d}).");
                    }
                }
            }
        }
        match lead.len() {
            0 => {}
            1 => {
                let _ = writeln!(s, "{}\n", lead[0]);
            }
            _ => {
                let _ = writeln!(s, "{}\n", lead.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n"));
            }
        }
        if let Some(touches) = touches(cs) {
            for l in wrap(&format!("    {:<w$}", "touches", w = TEXT - 2), &touches) {
                let _ = writeln!(s, "{l}");
            }
            s.push('\n');
        }
        for (c, lines, same) in &blocks {
            let _ = writeln!(s, "    {}", title(c));
            for l in lines {
                for out in show(l) {
                    let _ = writeln!(s, "    {out}");
                }
            }
            if !same.is_empty() {
                let _ = writeln!(s, "    {:<w$}{}", "  same", same.join(" · "), w = TEXT);
            }
            s.push('\n');
        }
    }
    if let Some(also) = also_line(t) {
        let _ = writeln!(s, "{also}\n");
    }
    s.push_str("`▲` wider · `▼` narrower · `+` added · `-` removed · `~` changed · `◆` renamed · `!` rule broken · `?` not modelled: check by hand\n");
    s
}

/// A new function called by other code of this change: its caller's story tells it.
fn helper(c: &ContractDiff) -> bool {
    c.status == "new" && !c.callers.is_empty() && c.new_routes.is_empty()
}

/// What a new function does outside itself, in a few words: `reads brands, products; writes
/// service.cacheDocs`. `None` when it only computes.
fn does(t: &Transition, name: &str) -> Option<String> {
    let c = t.contracts.iter().find(|c| c.status == "new" && last(&c.name) == name)?;
    let reads: Vec<String> =
        c.clauses.iter().filter(|x| x.row == "effects").filter_map(|x| x.after.as_deref()).filter_map(|a| a.strip_prefix("db_read ")).map(str::to_string).collect();
    let writes: Vec<String> = c
        .clauses
        .iter()
        .filter(|x| x.row == "post")
        .filter_map(|x| x.after.as_deref())
        .filter(|a| !a.ends_with("(by a callee)"))
        .map(|a| a.split([' ', ':']).next().unwrap_or(a).to_string())
        .collect();
    let mut parts = vec![];
    if !reads.is_empty() {
        parts.push(format!("reads {}", reads.join(", ")));
    }
    if !writes.is_empty() {
        parts.push(format!("writes {}", writes.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Where a new helper that no route reaches is read: with the contract that calls it.
fn home<'t>(t: &'t Transition, c: &'t ContractDiff) -> &'t ContractDiff {
    let mut cur = c;
    let mut seen = BTreeSet::new();
    while cur.status == "new" && cur.new_routes.is_empty() && seen.insert(cur.entity.as_str()) {
        match cur.callers.first().and_then(|id| t.contracts.iter().find(|x| &x.entity == id)) {
            Some(caller) => cur = caller,
            None => break,
        }
    }
    cur
}

/// The entry point that reaches a contract; its file when none does.
fn group(c: &ContractDiff) -> String {
    let mut routes: Vec<String> = c.new_routes.iter().map(|r| format!("`{r}` (new)")).collect();
    routes.extend(c.routes.iter().filter(|r| !c.new_routes.contains(r)).map(|r| format!("`{r}`")));
    match (c.status.as_str(), c.name.as_str()) {
        (_, "Program") => "Across the program".into(),
        ("schema", _) => "Data shapes".into(),
        _ if routes.len() > 3 => format!("{} and {} more", routes[..2].join(", "), routes.len() - 2),
        _ if !routes.is_empty() => routes.join(", "),
        _ => format!("`{}`", c.loc.file),
    }
}

/// `▼ narrower`, `▲ wider · ▼ narrower`, `! rule broken` … or `+ new` / `~ changed`.
fn badge(lines: &[&Line], cs: &[&ContractDiff]) -> String {
    let has = |g: char| lines.iter().any(|l| l.glyph == g);
    let mut parts = vec![];
    for (g, word) in [('!', "rule broken"), ('▲', "wider"), ('▼', "narrower")] {
        if has(g) {
            parts.push(format!("{g} {word}"));
        }
    }
    if parts.is_empty() {
        parts.push(if cs.iter().all(|c| c.status == "new") || cs.iter().any(|c| !c.new_routes.is_empty()) {
            "+ new".into()
        } else if lines.iter().all(|l| l.glyph == '?') {
            "? not modelled".into()
        } else {
            "~ changed".into()
        });
    }
    parts.join(" · ")
}

/// What matters most in a group, in a sentence: the weightiest line of each function, up to two.
fn sentence(blocks: &[(&ContractDiff, Vec<Line>, Vec<String>)]) -> Vec<String> {
    // A new helper's details are its caller's story, not the group's headline.
    let mut top: Vec<(u8, String)> = blocks
        .iter()
        .filter(|(c, _, _)| !(c.status == "new" && !c.callers.is_empty() && c.new_routes.is_empty()))
        .filter_map(|(c, ls, _)| {
            // New code is what it gives back, first: `returns <Placeholder> when …; otherwise <Image …>`.
            let outputs: Vec<&str> = ls.iter().filter(|l| c.status == "new" && l.kind == "OUTPUT").map(|l| l.text.as_str()).take(2).collect();
            if !outputs.is_empty() {
                return Some((3, c, outputs.join("; ")));
            }
            // New code that only acts (a script): what it touches; a script, statement by statement.
            let sql: Vec<String> = ls.iter().filter(|l| l.kind == "SQL").map(|l| l.text.split(':').next().unwrap_or(&l.text).split(" where ").next().unwrap_or("").to_string()).collect();
            if !sql.is_empty() {
                return Some((4, c, sql.join("; ")));
            }
            let effects: Vec<&str> = ls.iter().filter(|l| c.status == "new" && l.kind == "EFFECT").map(|l| l.text.as_str()).take(3).collect();
            if !effects.is_empty() {
                return Some((4, c, effects.join("; ")));
            }
            ls.iter().filter(|l| l.glyph != '◆' && l.glyph != '?').min_by_key(|l| l.rank()).map(|l| (l.rank(), c, l.text.clone()))
        })
        .map(|(r, c, text)| {
            let routes = if c.new_routes.is_empty() { String::new() } else { format!(" (new route {})", c.new_routes.join(", ")) };
            (r, format!("`{}`{routes}: {text}.", c.name))
        })
        .collect();
    top.sort_by_key(|(r, _)| *r);
    // Only what moves behaviour leads; an all-context group needs no sentence.
    let mut lead: Vec<String> = top.into_iter().filter(|(r, _)| *r <= 5).take(2).map(|(_, s)| s).collect();
    // Data that now goes through new code: what that code does with it, and what the
    // function builds to hand it.
    for (c, ls, _) in blocks {
        for l in ls.iter().filter(|l| l.kind == "PASSES") {
            let Some(g) = l.text.split('`').nth(1) else { continue };
            if let Some((_, gl, _)) = blocks.iter().find(|(h, _, _)| h.status == "new" && last(&h.name) == g) {
                let what: Vec<&str> = gl.iter().filter(|x| matches!(x.kind, "FILTER" | "CHECK" | "WRITES" | "EFFECT")).map(|x| x.text.as_str()).take(2).collect();
                if !what.is_empty() {
                    lead.push(format!("`{g}` {}.", what.join("; ")));
                }
            }
            for b in ls.iter().filter(|b| b.kind == "BUILDS") {
                lead.push(format!("`{}` {}.", c.name, b.text));
            }
        }
    }
    lead.dedup();
    lead
}

/// What a group's work changes say, for its lead: `` `Name`: loads only rows where … ``.
fn work(blocks: &[(&ContractDiff, Vec<Line>, Vec<String>)]) -> Vec<String> {
    let mut out = vec![];
    for (c, ls, _) in blocks {
        // A helper's work is its caller's story, except serving kept data: that changes what callers see.
        for l in ls.iter().filter(|l| !helper(c) || l.work.as_deref().is_some_and(|w| w.contains("from memory"))) {
            if let Some(w) = &l.work {
                let item = format!("`{}`: {w}.", c.name);
                if !out.contains(&item) {
                    out.push(item);
                }
            }
        }
    }
    out
}

/// What a group touches outside the program, by verb: `reads brands, products · writes orders`.
fn touches(cs: &[&ContractDiff]) -> Option<String> {
    let mut by_verb: Vec<(String, Vec<String>)> = vec![];
    let mut seen = BTreeSet::new();
    for c in cs {
        for x in c.clauses.iter().filter(|x| x.row == "effects" && x.mark != '-') {
            let Some(a) = x.after.as_deref().filter(|a| *a != "none found") else { continue };
            let it = item("effects", a);
            if !seen.insert(it.clone()) {
                continue;
            }
            let (verb, what) = match it.split_once(' ') {
                Some((v, w)) if ["read", "write", "query", "emit", "queue", "delete"].contains(&v) => (format!("{v}s").replace("querys", "queries"), w.to_string()),
                _ => ("calls".to_string(), it.clone()),
            };
            match by_verb.iter_mut().find(|(v, _)| *v == verb) {
                Some((_, xs)) => xs.push(what),
                None => by_verb.push((verb, vec![what])),
            }
        }
    }
    // What it does right away; what runs later (goroutines, events) is counted, not listed.
    let later = by_verb.iter().flat_map(|(_, xs)| xs).filter(|x| x.contains("(later")).count();
    let mut parts: Vec<String> = by_verb
        .into_iter()
        .filter_map(|(v, xs)| {
            let now: Vec<String> = xs.into_iter().filter(|x| !x.contains("(later")).collect();
            if now.is_empty() {
                return None;
            }
            let more = now.len().saturating_sub(8);
            let shown = now.into_iter().take(8).collect::<Vec<_>>().join(", ");
            Some(if more > 0 { format!("{v} {shown} and {more} more") } else { format!("{v} {shown}") })
        })
        .collect();
    if later > 0 {
        parts.push(plural(later, "more effect later (goroutines, events)", "more effects later (goroutines, events)"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `repository.CoOccurring · changed · repository.go:710`; a new function with its parameters.
fn title(c: &ContractDiff) -> String {
    let at = if c.loc.file.is_empty() { String::new() } else { format!(" · {}", c.loc) };
    match c.status.as_str() {
        "new" => {
            let params: Vec<&str> = c.clauses.iter().filter(|x| x.row == "takes").filter_map(|x| x.after.as_deref()).collect();
            let moved = c.moved_from.as_deref().map(|f| format!(" · moved out of `{f}`")).unwrap_or_default();
            format!("+ {}({}) · new{moved}{at}", c.name, params.join(", "))
        }
        "schema" => format!("{} · data shape{at}", c.name),
        _ => format!("{} · changed{at}", c.name),
    }
}

/// A line and what hangs under it, indented to the text column.
fn show(l: &Line) -> Vec<String> {
    let places = if l.places > 1 { format!("   ({} places)", l.places) } else { String::new() };
    let head = format!("  {} {:<9} ", l.glyph, l.kind);
    let mut out = wrap(&head, &format!("{}{places}", l.text));
    let pad = " ".repeat(TEXT);
    // `removes  a b c …` wraps under `a`, not under `removes`.
    for d in &l.detail {
        match d.find("  ").map(|i| i + d[i..].len() - d[i..].trim_start().len()) {
            Some(i) => out.extend(wrap(&format!("{pad}{}", &d[..i]), &d[i..])),
            None => out.extend(wrap(&pad, d)),
        }
    }
    for e in &l.evidence {
        out.push(format!("{pad}{e}"));
    }
    out
}

/// A contract's behaviour lines, riskiest first, and what stayed the same.
fn lines(t: &Transition, c: &ContractDiff, changing: &BTreeSet<&str>, fresh: &BTreeSet<String>, source: SourceLine) -> (Vec<Line>, Vec<String>) {
    let new = c.status == "new";
    // A new SQL script reads statement by statement.
    if new && c.name == "<script>" {
        if let Some(ls) = script_lines(c, source) {
            return (ls, vec![]);
        }
    }
    let mut out: Vec<Line> = vec![];
    let mut used = vec![false; c.clauses.len()];
    let changed = |x: &Clause| x.mark != ' ';

    // One parameter out and one in: the input was replaced (or renamed, when the type is the same).
    let takes: Vec<usize> = (0..c.clauses.len()).filter(|&i| c.clauses[i].row == "takes").collect();
    let (outs, ins): (Vec<usize>, Vec<usize>) = (
        takes.iter().copied().filter(|&i| c.clauses[i].mark == '-').collect(),
        takes.iter().copied().filter(|&i| c.clauses[i].mark == '+').collect(),
    );
    if !new && outs.len() == 1 && ins.len() == 1 {
        let (b, a) = (&c.clauses[outs[0]], &c.clauses[ins[0]]);
        let (bt, at) = (b.before.as_deref().unwrap_or(""), a.after.as_deref().unwrap_or(""));
        let ty = |v: &str| v.rsplit_once(": ").map(|(_, t)| t.to_string());
        // `{ a, b }: Props` → `{ a, b, c }: Props`: which fields it takes now.
        let (glyph, text) = match (fields(bt), fields(at)) {
            (Some(fb), Some(fa)) if ty(bt) == ty(at) => {
                let added: Vec<&String> = fa.iter().filter(|f| !fb.contains(f)).collect();
                let removed: Vec<&String> = fb.iter().filter(|f| !fa.contains(f)).collect();
                let list = |xs: &[&String]| xs.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ");
                match (added.is_empty(), removed.is_empty()) {
                    (false, true) => ('+', format!("now also takes {}", list(&added))),
                    (true, false) => ('-', format!("no longer takes {}", list(&removed))),
                    _ => ('~', format!("now takes {} instead of {}", list(&added), list(&removed))),
                }
            }
            _ if ty(bt) == ty(at) => ('◆', format!("{bt} → {at}")),
            _ => ('~', format!("{bt} → {at}")),
        };
        out.push(Line {
            glyph,
            kind: "INPUT",
            text,
            detail: vec![],
            evidence: evidence(a, source, &c.loc.file),
            places: 1, work: None
        });
        used[outs[0]] = true;
        used[ins[0]] = true;
    }

    // A removed clause and an added one with the same shape: renamed, or one name swapped for
    // another that is itself changing.
    let mut renamed: Vec<(String, String)> = vec![];
    let mut folded: Vec<&str> = vec![];
    for i in 0..c.clauses.len() {
        // A handler on another element, another element rendered: not renames.
        if used[i] || c.clauses[i].mark != '-' || new || matches!(c.clauses[i].row.as_str(), "handles" | "renders") {
            continue;
        }
        let b = &c.clauses[i];
        let pair = (0..c.clauses.len()).find_map(|j| {
            let a = &c.clauses[j];
            (!used[j] && a.mark == '+' && a.row == b.row).then(|| renames(b.before.as_deref()?, a.after.as_deref()?).map(|r| (j, r))).flatten()
        });
        let Some((j, pairs)) = pair else { continue };
        used[i] = true;
        used[j] = true;
        let swapped: Vec<String> =
            pairs.iter().filter(|(x, y)| changing.contains(x.as_str()) || changing.contains(y.as_str())).map(|(x, y)| format!("{y} instead of {x}")).collect();
        // `<Image>` → `<FallbackImage>` in what it renders: which element it now renders, when.
        if !swapped.is_empty() && b.row == "renders" {
            let after = c.clauses[j].after.clone().unwrap_or_default();
            let content = after.rsplit_once(": ").map_or(after.as_str(), |(c, _)| c);
            let when = content.split_once(" if ").map(|(_, w)| format!(" when {}", plain(w))).unwrap_or_default();
            let swaps: Vec<String> =
                pairs.iter().filter(|(x, y)| changing.contains(x.as_str()) || changing.contains(y.as_str())).map(|(x, y)| format!("<{y}> instead of <{x}>")).collect();
            out.push(Line {
                glyph: '~',
                kind: "RENDERS",
                text: format!("now renders {}{when}", swaps.join(", ")),
                detail: vec![],
                evidence: evidence(&c.clauses[j], source, &c.loc.file),
                places: 1,
                work: None,
            });
        } else if !swapped.is_empty() {
            let (text, _) = phrase(b, false);
            let (now, _) = phrase(&c.clauses[j], false);
            out.push(Line {
                glyph: '~',
                kind: kind(&b.row),
                text: format!("{} now uses {}", noun(&b.row), swapped.join(", ")),
                detail: vec![format!("was  {}", strip_verb(&text)), format!("now  {}", strip_verb(&now))],
                evidence: evidence(&c.clauses[j], source, &c.loc.file),
                places: 1, work: None
            });
        } else {
            folded.push(&b.row);
            for p in pairs {
                if !renamed.contains(&p) {
                    renamed.push(p);
                }
            }
        }
    }

    // Everything else, one line per clause, merged when two say the same.
    let untyped = c.untyped_only();
    let markup_restyled = c.clauses.iter().any(|x| x.row == "renders" && x.mark != ' ');
    for (i, x) in c.clauses.iter().enumerate() {
        if used[i] || !changed(x) || (new && x.row == "takes") {
            continue;
        }
        // A call Merak cannot type is noise next to the typed changes that explain it, unless
        // it is a call into new code: that is what the function now does.
        // A callback that came or went with a handler: the HANDLES lines say what it did.
        if x.mark == '?' && c.clauses.iter().any(|y| y.row == "handles" && y.mark != ' ') && {
            let names = called(x.before.as_deref().unwrap_or("")).into_iter().chain(called(x.after.as_deref().unwrap_or(""))).collect::<Vec<_>>();
            !names.is_empty() && names.iter().all(|n| n.ends_with("callback") || n.starts_with("<fn") || n.starts_with("on"))
        } {
            continue;
        }
        if x.mark == '?' && !untyped {
            let came = called(x.after.as_deref().unwrap_or(""));
            let gone = called(x.before.as_deref().unwrap_or(""));
            let new_code: Vec<String> = came.iter().filter(|n| fresh.contains(*n) && !gone.contains(n)).map(|n| format!("`{n}`")).collect();
            let linked: String = c.clauses.iter().filter(|y| matches!(y.row.as_str(), "passes" | "fills" | "skips")).filter_map(|y| y.after.clone()).collect::<Vec<_>>().join(" ");
            let new_code: Vec<String> = new_code.into_iter().filter(|n| !linked.contains(n.trim_matches('`'))).collect();
            if !new_code.is_empty() {
                out.push(Line {
                    glyph: '+',
                    kind: "CALLS",
                    text: format!("now calls new {}", new_code.join(", ")),
                    detail: vec![],
                    evidence: call_sites(t, c, &came.iter().filter(|n| fresh.contains(*n)).cloned().collect::<Vec<_>>(), source),
                    places: 1,
                    work: None,
                });
            }
            continue;
        }
        if new && x.row == "effects" && x.after.as_deref() == Some("none found") {
            continue;
        }
        // A text constant it starts or stops using (`StatusPaid = "paid"`) is context; a map
        // (`SORT_ICONS = {…}`) says what it can show, so it stays.
        if x.row == "consts" && matches!(x.mark, '+' | '-') && x.before.as_deref().or(x.after.as_deref()).is_some_and(|v| v.split_once(" = ").is_some_and(|(_, v)| v.starts_with('"'))) {
            continue;
        }
        // A changed output that is markup: `RENDERS` lines say how.
        let markup = |v: &Option<String>| v.as_deref().is_some_and(|v| v.split_once(": ").is_some_and(|(_, val)| val.starts_with('<')));
        if x.row == "returns" && x.mark == '~' && markup(&x.before) && markup(&x.after) && markup_restyled {
            continue;
        }
        // Calls Merak cannot type, when all it now calls is new code: the new code's own
        // lines say what it does.
        if x.mark == '?' {
            let (gone, came) = (called(x.before.as_deref().unwrap_or("")), called(x.after.as_deref().unwrap_or("")));
            // What it passes and builds already names every new call: nothing is left to check by hand.
            let linked: String = c.clauses.iter().filter(|y| matches!(y.row.as_str(), "passes" | "fills")).filter_map(|y| y.after.clone()).collect::<Vec<_>>().join(" ");
            if gone.is_empty() && !came.is_empty() && came.iter().all(|n| linked.contains(&format!("{n}("))) {
                continue;
            }
            if gone.is_empty() && !came.is_empty() && came.iter().all(|n| fresh.contains(n)) {
                let names: Vec<String> = came.iter().map(|n| format!("`{n}`")).collect();
                out.push(Line {
                    glyph: '+',
                    kind: "CALLS",
                    text: format!("now calls new {}", names.join(", ")),
                    detail: vec![],
                    evidence: call_sites(t, c, &came, source),
                    places: 1, work: None
                });
                continue;
            }
            let sites = call_sites(t, c, &came, source);
            let evidence = if sites.is_empty() { evidence(x, source, &c.loc.file) } else { sites };
            // Calls that now run on one branch only: the work done on the other is gone.
            if let Some(text) = conditional(x.before.as_deref().unwrap_or(""), x.after.as_deref().unwrap_or(""), fresh) {
                out.push(Line { glyph: '?', kind: "CALLS", text: text.clone(), detail: vec![], evidence, places: 1, work: Some(text) });
                continue;
            }
            let (text, _) = phrase(x, new);
            out.push(Line { glyph: '?', kind: "CALLS", text, detail: vec![], evidence, places: 1, work: None });
            continue;
        }
        let (mut text, detail) = phrase(x, new);
        if x.row == "passes" {
            if let Some(g) = x.tag.as_deref().and_then(|t| t.strip_prefix("now through ")).filter(|g| fresh.contains(*g)) {
                text = text.replacen(&format!("through `{g}`"), &format!("through new `{g}`"), 1);
            }
        }
        // Nothing narrows or widens in code that did not exist.
        let g = if new { x.mark } else { glyph(x) };
        // The same thing said twice: one line, citing the first place in the code.
        if let Some(l) = out.iter_mut().find(|l| l.glyph == g && l.text == text) {
            l.places += 1;
            let line = |e: &[String]| e.first().and_then(|e| e.split_whitespace().next()?.parse::<u32>().ok());
            let other = evidence(x, source, &c.loc.file);
            if line(&other).is_some_and(|n| line(&l.evidence).is_none_or(|m| n < m)) {
                l.evidence = other;
            }
            continue;
        }
        out.push(Line { glyph: g, kind: kind(&x.row), text, detail, evidence: evidence(x, source, &c.loc.file), places: 1, work: None });
    }
    // One skip rule out and one in over the same items: the rule changed, which way is for
    // the reader to judge.
    for (i, x) in c.clauses.iter().enumerate().filter(|(_, x)| x.row == "skips" && x.mark == '-') {
        let over = |t: &str| t.split(": skipped when ").next().unwrap_or("").to_string();
        let b = x.before.clone().unwrap_or_default();
        let ins: Vec<&Clause> = c.clauses.iter().filter(|y| y.row == "skips" && y.mark == '+' && over(y.after.as_deref().unwrap_or("")) == over(&b)).collect();
        let outs: Vec<usize> = (0..c.clauses.len()).filter(|&j| c.clauses[j].row == "skips" && c.clauses[j].mark == '-' && over(c.clauses[j].before.as_deref().unwrap_or("")) == over(&b)).collect();
        // As many in as out: paired in order.
        if new || used[i] || ins.len() != outs.len() {
            continue;
        }
        let k = outs.iter().position(|&j| j == i).unwrap_or(0);
        let a = ins[k].after.clone().unwrap_or_default();
        let (was, now) = (strip_verb(&skip(&plain(&b))), strip_verb(&skip(&plain(&a))));
        if let Some(pos) = out.iter().position(|l| l.kind == "FILTER" && l.text == format!("no longer {was}")) {
            out.remove(pos);
        }
        if let Some(pos) = out.iter().position(|l| l.kind == "FILTER" && l.text == format!("now {now}")) {
            let ev = out[pos].evidence.clone();
            let places = out[pos].places;
            out.remove(pos);
            let each = over(&a);
            let each = if each == "loop" { "an iteration".to_string() } else { format!("each {each}") };
            // `id is not in docByID` → `docFor fails`, `docFor` new: a lookup instead of a skip.
            let missing = plain(b.split(": skipped when ").nth(1).unwrap_or("")).split_once(" is not in ").map(|(k, m)| (k.to_string(), m.to_string()));
            let via = fresh.iter().find(|f| now.contains(&format!("{f} fails")) || now.contains(&format!("{f}(")));
            // `a or b` → `a or c`: the conditions that joined and that left.
            let terms = |t: &str| -> Vec<String> { t.split_once(" when ").map_or("", |(_, w)| w).split(" or ").map(str::to_string).collect() };
            let (tw, tn) = (terms(&was), terms(&now));
            let joined: Vec<&String> = tn.iter().filter(|x| !tw.contains(x)).collect();
            let left: Vec<&String> = tw.iter().filter(|x| !tn.contains(x)).collect();
            let list = |xs: &[&String]| xs.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(" or ");
            let text = match (missing, via) {
                (Some((k, m)), Some(f)) => format!("{k} missing from {m} is now looked up with new `{f}` instead of skipped"),
                _ if !joined.is_empty() && joined.len() + left.len() < tn.len() + tw.len() => {
                    let gone = if left.is_empty() { String::new() } else { format!(", no longer when {}", list(&left)) };
                    format!("{each} is now also skipped when {}{gone}", list(&joined))
                }
                _ => format!("skip rule for {each} changed"),
            };
            out.push(Line { glyph: '~', kind: "FILTER", text, detail: vec![format!("was  {was}"), format!("now  {now}")], evidence: ev, places, work: None });
        }
    }
    // New reads of one kind: one line (`read brands, products, …`), at the first one's code.
    let reads: Vec<usize> = (0..out.len()).filter(|&i| out[i].kind == "EFFECT" && out[i].glyph == '+' && out[i].text.starts_with("read ")).collect();
    if reads.len() > 1 {
        let names: Vec<String> = reads.iter().map(|&i| out[i].text.trim_start_matches("read ").to_string()).collect();
        out[reads[0]].text = format!("reads {}", names.join(", "));
        for &i in reads[1..].iter().rev() {
            out.remove(i);
        }
    }
    // One element out and one in under the same condition: what it renders now instead.
    let parts = |t: &str| -> (String, String) {
        let content = t.rsplit_once(": ").map_or(t, |(c, _)| c);
        match content.split_once(" if ") {
            Some((el, w)) => (el.to_string(), w.to_string()),
            None => (content.to_string(), String::new()),
        }
    };
    let element = |x: &Clause| !parts(x.before.as_deref().or(x.after.as_deref()).unwrap_or("")).0.ends_with(" itself");
    let outs: Vec<&Clause> = c.clauses.iter().filter(|x| !new && x.row == "renders" && x.mark == '-' && element(x)).collect();
    let ins: Vec<&Clause> = c.clauses.iter().filter(|x| !new && x.row == "renders" && x.mark == '+' && element(x)).collect();
    for b in &outs {
        let (eb, wb) = parts(b.before.as_deref().unwrap_or(""));
        let same_when: Vec<&&Clause> = ins.iter().filter(|a| parts(a.after.as_deref().unwrap_or("")).1 == wb).collect();
        let others = outs.iter().filter(|o| parts(o.before.as_deref().unwrap_or("")).1 == wb).count();
        if same_when.len() != 1 || others != 1 || !eb.starts_with('<') || eb.ends_with(" itself") {
            continue;
        }
        let a = same_when[0];
        let (ea, _) = parts(a.after.as_deref().unwrap_or(""));
        if !ea.starts_with('<') || ea.ends_with(" itself") {
            continue;
        }
        // `<SORT_ICONS[sort]>` with `SORT_ICONS = {relevance: ArrowUpDown, …}`: which elements, by what.
        let options = ea
            .strip_prefix('<')
            .and_then(|x| x.strip_suffix('>'))
            .and_then(|x| x.split_once('['))
            .and_then(|(m, k)| {
                let map = c.clauses.iter().find_map(|y| y.after.as_deref().filter(|v| y.row == "consts" && v.starts_with(&format!("{m} = {{"))))?;
                let inner = map.split_once(" = ")?.1.strip_prefix('{')?.strip_suffix('}')?;
                let vals: Vec<String> = crate::render::top_level_args(inner).into_iter().filter_map(|kv| kv.split_once(": ").map(|(_, v)| v.to_string())).collect();
                Some(format!(" ({} by {})", join_or(&vals), k.trim_end_matches(']')))
            })
            .unwrap_or_default();
        let when = if wb.is_empty() { String::new() } else { format!(" when {}", plain(&wb)) };
        let (tb, ta) = (format!("no longer renders {}", plain(b.before.as_deref().unwrap_or(""))), format!("now renders {}", plain(a.after.as_deref().unwrap_or(""))));
        out.retain(|l| !(l.kind == "RENDERS" && (l.text == tb || l.text == ta)));
        // The element picked from a map shown beside text from another map on the same key
        // (`<SORT_ICONS[sort]/> {SORT_LABELS[sort]}`): what a person sees in each state.
        if let Some(seen) = shown_by_key(c, &ea, &eb) {
            let (k, rows, label_map) = seen;
            let icons = ea.trim_start_matches('<').split('[').next().unwrap_or("").to_string();
            out.retain(|l| {
                !(l.kind == "CONSTANT" && (l.text.starts_with(&format!("{label_map}: ")) || l.text.starts_with(&format!("now uses {icons} = "))))
            });
            // `ArrowUp "Price"`, per state, now and before.
            let side = |now: bool| -> String {
                rows.iter()
                    .filter_map(|r| r.split_once("  →  ").map(|(b, a)| if now { a.to_string() } else { b.split_whitespace().skip(1).collect::<Vec<_>>().join(" ") }))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push(Line {
                glyph: '~',
                kind: "RENDERS",
                text: format!("by {k}{when}, now shows {} (was {})", side(true), side(false)),
                detail: rows,
                evidence: evidence(a, source, &c.loc.file),
                places: 1,
                work: None,
            });
            continue;
        }
        out.push(Line {
            glyph: '~',
            kind: "RENDERS",
            text: format!("now renders {ea}{options} instead of {eb}{when}"),
            detail: vec![],
            evidence: evidence(a, source, &c.loc.file),
            places: 1,
            work: None,
        });
    }
    // `<button> itself: button[aria-label=X][title=Y]`: what it now says about itself.
    for l in out.iter_mut().filter(|l| l.kind == "RENDERS" && l.text.contains(" itself")) {
        // `[title=SORT_NEXT[sort]][aria-label=…]`: top-level brackets only.
        let told = |t: &str| -> String {
            let style = t.rsplit_once(": ").map_or(t, |(_, s)| s);
            let (mut out, mut cur, mut depth) = (vec![], String::new(), 0);
            for ch in style.chars() {
                match ch {
                    '[' if depth == 0 => {
                        depth = 1;
                        cur.clear();
                    }
                    '[' => {
                        depth += 1;
                        cur.push(ch);
                    }
                    ']' if depth == 1 => {
                        depth = 0;
                        out.push(std::mem::take(&mut cur));
                    }
                    ']' => {
                        depth -= 1;
                        cur.push(ch);
                    }
                    _ if depth > 0 => cur.push(ch),
                    _ => {}
                }
            }
            out.join(", ")
        };
        let el = l.text.split(" itself").next().unwrap_or("").rsplit(' ').next().unwrap_or("").to_string();
        let t = told(&l.text);
        if l.glyph == '+' {
            l.text = format!("{el} now tells {t}");
        } else if l.glyph == '-' {
            l.text = format!("{el} no longer tells {t}");
        }
    }
    // Columns a new script sets, by table: `material_groups: title := Lighting, slug := lighting …`.
    let sets: Vec<usize> = (0..out.len()).filter(|&i| new && out[i].kind == "WRITES" && out[i].text.starts_with("sets ") && out[i].text.contains(" := ")).collect();
    if sets.len() > 2 {
        let mut by_table: Vec<(String, Vec<String>)> = vec![];
        for &i in &sets {
            let t = out[i].text.trim_start_matches("sets ");
            let (field, value) = t.split_once(" := ").unwrap_or((t, ""));
            let (table, col) = field.rsplit_once('.').unwrap_or(("", field));
            let value = if value.chars().count() > 30 { format!("{}…", value.chars().take(29).collect::<String>()) } else { value.to_string() };
            match by_table.iter_mut().find(|(x, _)| x == table) {
                Some((_, cs)) => cs.push(format!("{col} := {value}")),
                None => by_table.push((table.to_string(), vec![format!("{col} := {value}")])),
            }
        }
        let first = sets[0];
        for &i in sets[1..].iter().rev() {
            out.remove(i);
        }
        let ev = out[first].evidence.clone();
        out.remove(first);
        for (k, (table, cols)) in by_table.into_iter().enumerate() {
            out.insert(first + k, Line { glyph: '+', kind: "WRITES", text: format!("sets {table}: {}", cols.join(", ")), detail: vec![], evidence: ev.clone(), places: 1, work: None });
        }
    }
    // A check that moved to another call and gained a way through: `err(catchUp) = null` →
    // `err(refresh) = null or …`. One line: which call it checks now, and what else lets it pass.
    let gone_checks: Vec<usize> = (0..out.len()).filter(|&i| out[i].kind == "CHECK" && out[i].text.starts_with("no longer requires ")).collect();
    let new_checks: Vec<usize> = (0..out.len()).filter(|&i| out[i].kind == "CHECK" && out[i].text.starts_with("now requires ")).collect();
    if let ([g], [n]) = (gone_checks.as_slice(), new_checks.as_slice()) {
        let was = out[*g].text.trim_start_matches("no longer requires ").to_string();
        let now = out[*n].text.trim_start_matches("now requires ").to_string();
        let terms: Vec<&str> = now.split(" or ").collect();
        let hit = terms.iter().position(|t| renames(&was, t).is_some_and(|ps| ps.iter().all(|(x, y)| fresh.contains(y) || changing.contains(y.as_str()) || fresh.contains(x))));
        if let Some(k) = hit {
            let swaps: Vec<String> = renames(&was, terms[k]).unwrap_or_default().into_iter().map(|(x, y)| format!("`{y}` instead of `{x}`")).collect();
            let rest: Vec<&str> = terms.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, t)| *t).collect();
            let (glyph, text) = if rest.is_empty() {
                ('~', format!("check now uses {}", swaps.join(", ")))
            } else {
                ('▲', format!("check now uses {}, and also lets it through when {}", swaps.join(", "), rest.join(" or ")))
            };
            let ev = out[*n].evidence.clone();
            let (hi, lo) = if g > n { (*g, *n) } else { (*n, *g) };
            out.remove(hi);
            out.remove(lo);
            out.push(Line { glyph, kind: "CHECK", text, detail: vec![format!("was  requires {was}"), format!("now  requires {now}")], evidence: ev, places: 1, work: None });
        }
    }
    // A new component's handlers: one line, `<button> click, <input> change …`.
    let hs: Vec<usize> = (0..out.len()).filter(|&i| new && out[i].kind == "HANDLES").collect();
    if hs.len() > 1 {
        let names: Vec<String> = hs.iter().map(|&i| out[i].text.split(": ").next().unwrap_or("").replacen(" on ", " ", 1)).collect();
        let more = names.len().saturating_sub(6);
        let shown = names.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
        out[hs[0]].text = format!("reacts to {shown}{}", if more > 0 { format!(" and {more} more") } else { String::new() });
        for &i in hs[1..].iter().rev() {
            out.remove(i);
        }
    }
    // Fields written by callees: one line, grouped by type.
    let by_callees: Vec<usize> = (0..out.len()).filter(|&i| out[i].kind == "WRITES" && out[i].glyph == '+' && out[i].text.ends_with(" (in a callee)")).collect();
    if by_callees.len() > 1 {
        let fields: Vec<String> =
            by_callees.iter().map(|&i| format!("{} is written", out[i].text.trim_start_matches("now ").trim_start_matches("writes ").trim_end_matches(" (in a callee)"))).collect();
        let grouped = crate::render::compress_writes(fields);
        let at = by_callees[0];
        out[at].text = format!("{}writes {} (in callees)", if new { "" } else { "now " }, grouped.join(" · ").replace(" written", ""));
        out[at].evidence = vec![];
        for &i in by_callees[1..].iter().rev() {
            out.remove(i);
        }
    }
    // Loop variables renamed along the way are not worth naming.
    let named: Vec<String> = renamed.iter().filter(|(a, b)| a.chars().count() > 2 || b.chars().count() > 2).map(|(a, b)| format!("{a} → {b}")).collect();
    if !folded.is_empty() {
        let what = if named.is_empty() { String::new() } else { format!(": {}", named.join(", ")) };
        let row = folded[0];
        let (one, many) = if folded.iter().all(|r| *r == row) { (noun(row), format!("{}s", noun(row))) } else { ("clause", "clauses".into()) };
        out.push(Line {
            glyph: '◆',
            kind: "RENAMED",
            text: format!("{} unchanged but for names{what}", plural(folded.len(), one, &many)),
            detail: vec![],
            evidence: vec![],
            places: 1, work: None
        });
    }
    // Lists a loop runs through that gained or lost items (`for _, ddl := range […]`): the items.
    for x in c.clauses.iter().filter(|x| x.row == "skips" && x.mark == '-') {
        let Some((var, items_b, when)) = listed(x.before.as_deref().unwrap_or("")) else { continue };
        let Some(j) = (0..c.clauses.len()).find(|&j| {
            c.clauses[j].row == "skips" && c.clauses[j].mark == '+' && listed(c.clauses[j].after.as_deref().unwrap_or("")).is_some_and(|(v, _, w)| v == var && w == when)
        }) else {
            continue;
        };
        let (_, items_a, _) = listed(c.clauses[j].after.as_deref().unwrap_or("")).unwrap_or_default();
        let added: Vec<String> = items_a.iter().filter(|v| !items_b.contains(v)).map(|v| sql_short(v)).collect();
        let removed: Vec<String> = items_b.iter().filter(|v| !items_a.contains(v)).map(|v| sql_short(v)).collect();
        // The two loop filters were the list; drop the lines they made.
        let tb = format!("no longer {}", strip_verb(&skip(&plain(x.before.as_deref().unwrap_or("")))));
        let ta = format!("now {}", strip_verb(&skip(&plain(c.clauses[j].after.as_deref().unwrap_or("")))));
        out.retain(|l| !(l.kind == "FILTER" && (l.text == tb || l.text == ta)));
        for (glyph, what, verb) in [('+', &added, "now also runs"), ('-', &removed, "no longer runs")] {
            if what.is_empty() {
                continue;
            }
            let indexes = what.iter().all(|w| w.starts_with("create index"));
            let listing = what.join("; ");
            out.push(Line {
                glyph,
                kind: if indexes { "SCHEMA" } else { "RUNS" },
                text: format!("{verb} {listing}"),
                detail: vec![],
                evidence: evidence(&c.clauses[j], source, &c.loc.file),
                places: 1,
                work: indexes.then(|| format!("{} {}", if glyph == '+' { "adds" } else { "drops" }, listing.replace("create ", ""))),
            });
        }
    }
    // Work changes: the same results by other means.
    let reads_left = c.clauses.iter().any(|x| x.row == "effects" && x.mark == ' ' && x.after.as_deref().is_some_and(|a| a.starts_with("db_read")));
    for l in out.iter_mut().filter(|l| l.work.is_none()) {
        l.work = match (l.kind, l.glyph) {
            ("FILTER", '~') if l.text.contains("moved into the query of") => {
                let q = l.detail.iter().find_map(|d| d.strip_prefix("now  ")).unwrap_or("");
                let by = l.text.split('`').nth(1).unwrap_or("");
                Some(format!("loads only rows where {q}, instead of filtering them in a loop (the query of `{by}`)"))
            }
            ("FILTER", '~') if l.text.contains("is now checked in memory, not in the query") => {
                Some(format!("{} (the query reads every row)", l.text))
            }
            ("FILTER", '~') if l.text.starts_with("rewritten as a subquery") => {
                let q = l.detail.iter().find_map(|d| d.strip_prefix("now  ")).unwrap_or("");
                Some(format!("a filter is rewritten as {q} (check that it reaches the same rows)"))
            }
            ("EFFECT", '-') if reads_left && l.text.starts_with("no longer: read ") => Some(format!("its queries no longer read {}", l.text.trim_start_matches("no longer: read "))),
            // A unique index is a constraint (inserts can fail): not the same results.
            ("EFFECT", '+') if l.text.starts_with("create index") => {
                Some(format!("adds {}", l.text.split(" (later").next().unwrap_or(&l.text).trim_start_matches("create ")))
            }
            ("OUTPUT", '+' | '~') if l.text.contains("this.") && l.text.contains(" when ") => {
                let t = l.text.trim_start_matches("new output case: ").trim_start_matches("returns ");
                let (v, when) = t.split_once(" when ").unwrap_or((t, ""));
                Some(format!("returns {} from memory, without loading again, when {when} (check how stale it can get)", v.replace("this.", "")))
            }
            _ => None,
        };
    }
    out.sort_by_key(|l| l.rank());

    // What stayed the same, counted by kind.
    let mut same: BTreeMap<usize, (String, usize)> = BTreeMap::new();
    if !new || c.moved_from.is_some() {
        for x in c.clauses.iter().filter(|x| x.mark == ' ' && !matches!(x.row.as_str(), "takes" | "effects")) {
            let at = crate::contract::ROWS.iter().position(|r| *r == x.row).unwrap_or(99);
            same.entry(at).or_insert_with(|| (x.row.clone(), 0)).1 += 1;
        }
    }
    let same = same.into_values().map(|(row, n)| unchanged(&row, n)).collect();
    (out, same)
}

/// `3 output cases`, `18 row filters` …
fn unchanged(row: &str, n: usize) -> String {
    let (one, many) = match row {
        "who" => ("access rule", "access rules"),
        "returns" => ("output case", "output cases"),
        "computes" => ("computation", "computations"),
        "pre" => ("check", "checks"),
        "skips" => ("skip rule", "skip rules"),
        "where" => ("row filter", "row filters"),
        "post" => ("write", "writes"),
        "consts" => ("constant", "constants"),
        "renders" => ("rendered piece", "rendered pieces"),
        "reads" => ("read", "reads"),
        "handles" => ("handler", "handlers"),
        _ => ("other clause", "other clauses"),
    };
    plural(n, one, many)
}

/// The behavioural primitive a clause row belongs to.
fn kind(row: &str) -> &'static str {
    match row {
        "takes" => "INPUT",
        "who" => "ACCESS",
        "returns" => "OUTPUT",
        "computes" => "COMPUTES",
        "pre" => "CHECK",
        "skips" | "where" => "FILTER",
        "post" => "WRITES",
        "passes" => "PASSES",
        "fills" => "BUILDS",
        "reads" => "READS",
        "effects" => "EFFECT",
        "consts" => "CONSTANT",
        "renders" => "RENDERS",
        "handles" => "HANDLES",
        "lifecycle" => "STATE",
        "rule" => "RULE",
        "input" => "FIELD",
        "calls" => "CALLS",
        _ => "OTHER",
    }
}

/// What one clause of a row is called: `skip rule`, `row filter` …
fn noun(row: &str) -> &'static str {
    match row {
        "skips" => "skip rule",
        "where" => "row filter",
        "returns" => "output case",
        "pre" => "check",
        "who" => "access rule",
        "post" => "write",
        _ => "clause",
    }
}

/// `now skips …` → `skips …`, for was/now pairs.
fn strip_verb(s: &str) -> String {
    for p in ["now ", "no longer ", "new output case: ", "output case removed: ", "new row filter: ", "row filter removed: "] {
        if let Some(r) = s.strip_prefix(p) {
            return r.to_string();
        }
    }
    s.to_string()
}

/// Which way a clause moves behaviour: `▼` fewer rows or items reach it, stricter access;
/// `▲` more; otherwise its mark.
fn glyph(x: &Clause) -> char {
    if matches!(x.mark, '!' | '?' | ' ') {
        return x.mark;
    }
    let tag = x.tag.as_deref().unwrap_or("");
    let narrower = tag.starts_with("narrower")
        || tag.starts_with("now set only when empty")
        || matches!(tag, "narrowed" | "reaches fewer rows" | "leaves more items out" | "new check" | "new requirement" | "fewer ways to reach this state");
    let wider = tag.starts_with("wider")
        || tag.starts_with("now overwrites")
        || matches!(tag, "widened" | "reaches more rows" | "no longer leaves these out" | "check dropped" | "requirement dropped" | "more ways to reach this state");
    match (narrower, wider) {
        (true, _) => '▼',
        (_, true) => '▲',
        _ => x.mark,
    }
}

/// A clause as a behaviour phrase and the was/now lines under it. `new`: the whole function
/// is new, so nothing is "now" or "new" in it.
fn phrase(x: &Clause, new: bool) -> (String, Vec<String>) {
    let b = x.before.as_deref().map(plain);
    let a = x.after.as_deref().map(plain);
    let (bs, as_) = (b.clone().unwrap_or_default(), a.clone().unwrap_or_default());
    let now = if new { "" } else { "now " };
    let was_now = || vec![format!("was  {bs}"), format!("now  {as_}")];
    let text = match (x.row.as_str(), x.mark) {
        ("where", '~') => {
            let tag = x.tag.as_deref().unwrap_or("");
            if let Some(note) = tag.strip_prefix("moved into memory: ") {
                let was = plain(x.before.as_deref().unwrap_or(""));
                return (format!("{was} is now checked in memory by {note}, not in the query"), vec![]);
            }
            if tag == "also takes conditions built at run time" {
                return (format!("{}: {tag}", scope(&as_)), vec![format!("was  {}", plain(&bs)), format!("now  {}", plain(&as_))]);
            }
            // `narrower: rows must now also match X`: the tag says it; the filters differ where shown.
            let detail = match (x.before.as_deref().and_then(|v| v.split_once(": ")), x.after.as_deref().and_then(|v| v.split_once(": "))) {
                (Some((_, fb)), Some((_, fa))) => match differing(fb, fa) {
                    Some((db, da)) => vec![format!("was  {}", plain(db)), format!("now  {}", plain(da))],
                    None => was_now(),
                },
                _ => was_now(),
            };
            let text = match tag.split_once(": ") {
                Some((_, what)) => plain(what),
                None if scope(&as_) == "rows" => tag.to_string(),
                None => format!("{}: {tag}", scope(&as_)),
            };
            return (text, detail);
        }
        ("where", '+') if x.tag.as_deref() == Some("joins in more data") => format!("{now}joins {as_}"),
        ("where", '-') if x.tag.as_deref() == Some("joins in less data") => format!("no longer joins {bs}"),
        ("where", '+') => format!("{}row filter: {as_}", if new { "" } else { "new " }),
        ("where", '-') => format!("row filter removed: {bs}"),
        ("skips", '~') if x.tag.as_deref() == Some("moved into the query") => {
            let each = bs.split(": skipped when ").next().unwrap_or(&bs);
            let (by, query) = as_.split_once(": ").unwrap_or(("", &as_));
            return (format!("filter on {each} moved into the query of `{by}`"), vec![format!("was  {}", skip(&bs)), format!("now  {query}")]);
        }
        ("skips", '+') => format!("{now}{}", skip(&as_)),
        ("skips", '-') => format!("no longer {}", skip(&bs)),
        // `passes newSlug (base != "")` → `now rejects input unless base != "" (checked by newSlug)`.
        ("pre", '+') if as_.starts_with("passes ") => {
            let rest = as_.trim_start_matches("passes ");
            match rest.split_once(" (") {
                Some((f, req)) => format!("{now}rejects the request unless {} (checked by `{f}`)", req.trim_end_matches(')')),
                None => format!("{now}rejects the request when `{rest}` rejects it"),
            }
        }
        ("pre" | "who", '+') => format!("{now}requires {as_}"),
        ("pre" | "who", '-') => format!("no longer requires {bs}"),
        ("returns" | "computes", '+') if new => output(&as_),
        ("returns" | "computes", '+') => format!("new output case: {}", output(&as_)),
        ("returns" | "computes", '-') => format!("output case removed: {}", output(&bs)),
        ("returns", '~') => {
            // A value built in steps (`a; = b when c; …`): which steps changed, and how.
            let raw = |v: &Option<String>| v.as_deref().and_then(|v| v.split_once(": ")).map(|(_, x)| x.to_string()).unwrap_or_default();
            // Markup: the classes and attributes that differ.
            let (rb, ra) = (raw(&x.before), raw(&x.after));
            if rb.starts_with('<') && ra.starts_with('<') {
                let words = |s: &str| -> Vec<String> {
                    s.split(|c: char| c.is_whitespace() || matches!(c, '.' | '<' | '>' | '{' | '}' | '(' | ')' | ','))
                        // Event attributes are the HANDLES lines' to tell.
                        .filter(|w| !w.is_empty() && !w.starts_with('/') && !(w.len() > 2 && w.starts_with("on") && w[2..].starts_with(|c: char| c.is_ascii_uppercase())))
                        .map(str::to_string)
                        .collect()
                };
                let (wb, wa) = (words(&rb), words(&ra));
                let added: Vec<&String> = wa.iter().filter(|w| !wb.contains(w)).collect();
                let removed: Vec<&String> = wb.iter().filter(|w| !wa.contains(w)).collect();
                let case = as_.split_once(": ").map_or("", |(c, _)| c).to_string();
                let list = |xs: &[&String]| xs.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(" ");
                let mut parts = vec![];
                if !added.is_empty() {
                    parts.push(format!("adds {}", list(&added)));
                }
                if !removed.is_empty() {
                    parts.push(format!("drops {}", list(&removed)));
                }
                if !parts.is_empty() && added.len() + removed.len() <= 12 {
                    return (format!("{case}: markup {}", parts.join(", ")), vec![]);
                }
            }
            if let Some(lines) = steps_changed(&raw(&x.before), &raw(&x.after)) {
                let text = lines.join("; ");
                return (text, vec![]);
            }
            let (case, val) = as_.split_once(": ").unwrap_or(("", &as_));
            let was = bs.split_once(": ").map_or(bs.as_str(), |(_, v)| v);
            return (format!("{}: returns something else", case), vec![format!("was  {was}"), format!("now  {val}")]);
        }
        ("passes", _) => {
            // `f(a, b)` → name and arguments, compared one by one.
            let parse = |s: &str| -> (String, Vec<String>) {
                match s.split_once('(') {
                    Some((n, rest)) => (n.to_string(), crate::render::top_level_args(rest.strip_suffix(')').unwrap_or(rest))),
                    None => (s.to_string(), vec![]),
                }
            };
            let ((name, ab), (_, aa)) = (parse(x.before.as_deref().unwrap_or("")), parse(x.after.as_deref().unwrap_or("")));
            let Some(i) = (0..ab.len().min(aa.len())).find(|&i| ab[i] != aa[i]) else { return (format!("passes something else to `{name}`"), vec![]) };
            let (old, new) = (plain(&ab[i]), plain(&aa[i]));
            // The call with the changed argument as `…`, other long ones shortened.
            let ctx: Vec<String> = ab.iter().enumerate().map(|(j, v)| if j == i || v.chars().count() > 32 { "…".to_string() } else { plain(v) }).collect();
            let at = format!("{name}({})", ctx.join(", "));
            return match x.tag.as_deref().and_then(|t| t.strip_prefix("now through ")) {
                Some(g) => (format!("{old} now goes through `{g}` before {at}"), vec![format!("now  {new}")]),
                None if ctx.iter().all(|v| v == "…") => (format!("`{name}` now gets {new} instead of {old}"), vec![]),
                None => (format!("`{name}` now gets {new} instead of {old}"), vec![format!("in   {at}")]),
            };
        }
        ("fills", _) => return (fills(&as_), if x.mark == '~' { vec![format!("was  {}", fills(&bs))] } else { vec![] }),
        ("post", '~') if x.tag.as_deref().is_some_and(|t| t.starts_with("now overwrites") || t.starts_with("now set only when empty")) => {
            let col = as_.split(" := ").next().unwrap_or("").rsplit('.').next().unwrap_or("").to_string();
            let text = match x.tag.as_deref() {
                Some(t) if t.starts_with("now overwrites") => format!("now overwrites an existing {col} (it was set only when empty)"),
                _ => format!("{col} is now set only when empty (it was overwritten)"),
            };
            return (text, was_now());
        }
        // `each <button> on onMouseEnter: setActive(i)`.
        ("handles", m) => {
            let split = |t: &str| -> (String, String, String) {
                let (head, does) = t.split_once(": ").unwrap_or((t, ""));
                let (el, ev) = head.split_once(" on ").unwrap_or((head, ""));
                (el.to_string(), event_words(ev), does.to_string())
            };
            let (el, ev, does) = split(if m == '-' { &bs } else { &as_ });
            return match m {
                '-' => (format!("{el} no longer reacts to {ev} (it ran {does})"), vec![]),
                '+' if new => (format!("{el} on {ev}: {does}"), vec![]),
                '+' => (format!("{el} now reacts to {ev}: {does}"), vec![]),
                _ => {
                    let (_, _, was) = split(&bs);
                    (format!("{el} on {ev} now runs {does}"), vec![format!("was  {was}")])
                }
            };
        }
        ("takes", '+') => format!("new parameter {as_}"),
        ("takes", '-') => format!("parameter removed: {bs}"),
        ("post", '+') => format!("{now}{}", write(&as_)),
        ("post", '-') => format!("no longer {}", write(&bs)),
        ("reads", '+') => format!("{now}reads {as_}"),
        ("reads", '-') => format!("no longer reads {bs}"),
        ("effects", '+') if new => item("effects", x.after.as_deref().unwrap_or("")),
        // `read brands` → `now reads brands`; `GET /x` → `now calls GET /x`, and when.
        ("effects", '+') => {
            let it = item("effects", x.after.as_deref().unwrap_or(""));
            let when = x.when.as_deref().map(|w| format!(" when {}", plain(w))).unwrap_or_default();
            match it.split_once(' ') {
                Some(("delete", rest)) => format!("now deletes {rest} rows{}{when}", where_of(x)),
                Some((v @ ("read" | "write" | "emit" | "queue"), rest)) => format!("now {v}s {rest}{when}"),
                Some(("query", rest)) => format!("now queries {rest}{when}"),
                Some((m, _)) if m.chars().all(|c| c.is_ascii_uppercase()) => format!("now calls {it}{when}"),
                _ => format!("now: {it}{when}"),
            }
        }
        ("effects", '-') => format!("no longer: {}", item("effects", x.before.as_deref().unwrap_or(""))),
        ("effects", '~') => {
            let tag = x.tag.clone().unwrap_or_default();
            return (tag, vec![format!("was  {}", item("effects", x.before.as_deref().unwrap_or(""))), format!("now  {}", item("effects", x.after.as_deref().unwrap_or("")))]);
        }
        ("consts", '~') => {
            let v = |s: &str| s.split_once(" = ").map(|(_, v)| v.to_string()).unwrap_or_default();
            let name = as_.split(" = ").next().unwrap_or("").to_string();
            let (vb, va) = (v(&bs), v(&as_));
            // A map: the keys whose value changed, `relevance "Sort" → "Price"`.
            let entries = |m: &str| -> Option<Vec<(String, String)>> {
                let inner = m.strip_prefix('{')?.strip_suffix('}')?;
                Some(crate::render::top_level_args(inner).into_iter().filter_map(|kv| kv.split_once(": ").map(|(k, v)| (k.to_string(), v.to_string()))).collect())
            };
            match (entries(&vb), entries(&va)) {
                (Some(eb), Some(ea)) => {
                    let mut parts = vec![];
                    for (k, a) in &ea {
                        match eb.iter().find(|(kb, _)| kb == k) {
                            Some((_, b)) if b != a => parts.push(format!("{k} {b} → {a}")),
                            None => parts.push(format!("{k} {a} (new)")),
                            _ => {}
                        }
                    }
                    for (k, b) in eb.iter().filter(|(k, _)| !ea.iter().any(|(ka, _)| ka == k)) {
                        parts.push(format!("{k} {b} (removed)"));
                    }
                    format!("{name}: {}", parts.join(", "))
                }
                _ => match (vb.strip_prefix('[').and_then(|x| x.strip_suffix(']')), va.strip_prefix('[').and_then(|x| x.strip_suffix(']'))) {
                    // A list: what joined it and what left.
                    (Some(lb), Some(la)) => {
                        let (ib, ia) = (crate::render::top_level_args(lb), crate::render::top_level_args(la));
                        let added: Vec<&String> = ia.iter().filter(|x| !ib.contains(x)).collect();
                        let removed: Vec<&String> = ib.iter().filter(|x| !ia.contains(x)).collect();
                        let list = |xs: &[&String]| xs.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ");
                        let mut parts = vec![];
                        if !added.is_empty() {
                            parts.push(format!("now also {}", list(&added)));
                        }
                        if !removed.is_empty() {
                            parts.push(format!("no longer {}", list(&removed)));
                        }
                        if parts.is_empty() {
                            format!("{name}: reordered")
                        } else {
                            format!("{name}: {}", parts.join("; "))
                        }
                    }
                    _ => format!("{name}: {vb} → {va}"),
                },
            }
        }
        ("consts", '+') => format!("{now}uses {as_}"),
        ("consts", '-') => format!("no longer uses {bs}"),
        ("renders", '~') => {
            let text = restyled(x).unwrap_or_else(|| format!("{bs}  →  {as_}"));
            // Too long for one line: which content, then its styles before and after.
            match (text.chars().count() > WIDTH - TEXT, text.split_once(": "), text.rsplit_once("  →  ")) {
                (true, Some((content, _)), Some((was, now))) => {
                    let was = was.strip_prefix(&format!("{content}: ")).unwrap_or(was);
                    // `div.a.b` → `div.b.c`: the classes it lost and gained.
                    let classes = |s: &str| s.trim().split('.').map(str::to_string).collect::<Vec<_>>();
                    let (cb, ca) = (classes(was), classes(now));
                    let detail = if cb.first() == ca.first() {
                        let gone: Vec<&str> = cb[1..].iter().filter(|c| !ca.contains(c)).map(String::as_str).collect();
                        let came: Vec<&str> = ca[1..].iter().filter(|c| !cb.contains(c)).map(String::as_str).collect();
                        let mut d = vec![];
                        if !came.is_empty() {
                            d.push(format!("adds     {}", came.join(" ")));
                        }
                        if !gone.is_empty() {
                            d.push(format!("removes  {}", gone.join(" ")));
                        }
                        d
                    } else {
                        vec![format!("was  {was}"), format!("now  {now}")]
                    };
                    let tag = cb.first().filter(|t| ca.first() == Some(t)).map(|t| format!(" ({t})")).unwrap_or_default();
                    return (format!("{content}: styled differently{tag}"), detail);
                }
                _ => text,
            }
        }
        ("renders", '+') => format!("{now}renders {as_}"),
        ("renders", '-') => format!("no longer renders {bs}"),
        ("calls", '?') => {
            let s = calls_summary(x.before.as_deref().unwrap_or(""), x.after.as_deref().unwrap_or(""));
            let s = s.trim_start_matches(": ").to_string();
            if s.is_empty() {
                "calls changed".into()
            } else {
                s
            }
        }
        ("lifecycle" | "input", _) | (_, '!') => {
            let text = a.clone().or(b.clone()).unwrap_or_default();
            let tag = x.tag.as_deref().map(|t| format!(" ({t})")).unwrap_or_default();
            return (format!("{text}{tag}"), if x.mark == '~' && b.is_some() && a.is_some() { was_now() } else { vec![] });
        }
        (_, '~') => {
            let tag = x.tag.clone().unwrap_or_else(|| "changed".into());
            return (tag, was_now());
        }
        _ => a.or(b).unwrap_or_default(),
    };
    (text, vec![])
}

/// The fields a destructured parameter takes: `{ query, visible, }: Props` → `[query, visible]`.
fn fields(param: &str) -> Option<Vec<String>> {
    let inner = param.trim().strip_prefix('{')?.split_once('}')?.0;
    Some(inner.split(',').map(|f| f.split([':', '=']).next().unwrap_or(f).trim().to_string()).filter(|f| !f.is_empty()).collect())
}

/// `ddl in ["a", "b"]: skipped when c` → `(ddl, [a, b], c)`.
fn listed(skip: &str) -> Option<(String, Vec<String>, String)> {
    let (over, when) = skip.split_once(": skipped when ")?;
    let (var, list) = over.split_once(" in [")?;
    let list = list.strip_suffix(']')?;
    let items = crate::render::top_level_args(list).into_iter().map(|v| v.trim_matches('"').to_string()).collect();
    Some((var.to_string(), items, when.to_string()))
}

/// A SQL statement in a few words: `CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (c)` →
/// `create index i on t (c)`.
fn sql_short(sql: &str) -> String {
    let s = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = s.to_lowercase();
    if lower.starts_with("create ") || lower.starts_with("drop ") || lower.starts_with("alter ") {
        lower.replace(" concurrently", "").replace(" if not exists", "").replace(" if exists", "")
    } else {
        s
    }
}

/// Calls that now run on one branch only: `Count(_)` → `Count(_) when x is set`, beside new
/// calls under the other branch. `None` when no call became conditional.
fn conditional(before: &str, after: &str, fresh: &BTreeSet<String>) -> Option<String> {
    let bs: Vec<&str> = before.split("; ").filter(|x| !x.is_empty()).collect();
    let mut only: Vec<(String, Vec<String>)> = vec![];
    let mut added: Vec<(String, Vec<String>)> = vec![];
    let push = |into: &mut Vec<(String, Vec<String>)>, when: &str, name: String| match into.iter_mut().find(|(w, _)| w == when) {
        Some((_, ns)) => {
            if !ns.contains(&name) {
                ns.push(name);
            }
        }
        None => into.push((when.to_string(), vec![name])),
    };
    for a in after.split("; ").filter(|x| !x.is_empty()) {
        let Some((base, when)) = a.split_once(" when ") else { continue };
        let name = last(&call_name(base)).to_string();
        if bs.contains(&base) {
            push(&mut only, when, format!("`{name}`"));
        } else if !bs.iter().any(|b| last(&call_name(b)) == name) {
            push(&mut added, when, format!("{}`{name}`", if fresh.contains(&name) { "new " } else { "" }));
        }
    }
    if only.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = only.iter().map(|(w, ns)| format!("runs {} only when {}", ns.join(", "), plain(w))).collect();
    parts.extend(added.iter().map(|(w, ns)| format!("calls {} when {}", ns.join(", "), plain(w))));
    Some(parts.join("; "))
}

/// `OrderService.cancel` → `cancel`.
fn last(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name).rsplit('.').next().unwrap_or(name)
}

/// The functions a list of call shapes calls, by name, in order: `f(_); g(_) when c` → `[f, g]`.
fn called(shapes: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for n in shapes.split("; ").filter(|x| !x.is_empty()).map(call_name) {
        let n = last(&n).to_string();
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// `seen ← add(a, b) · add(c)` → `builds seen with add from a, b, c`.
fn fills(s: &str) -> String {
    let Some((local, calls)) = s.split_once(" ← ") else { return s.to_string() };
    let calls: Vec<&str> = calls.split(" · ").collect();
    let heads: BTreeSet<&str> = calls.iter().map(|c| c.split('(').next().unwrap_or(c)).collect();
    match heads.iter().next() {
        Some(g) if heads.len() == 1 => {
            let args: Vec<&str> = calls.iter().map(|c| c.strip_prefix(g).and_then(|r| r.strip_prefix('(')).and_then(|r| r.strip_suffix(')')).unwrap_or(c)).collect();
            format!("builds `{local}` with `{g}` from {}", args.join(", "))
        }
        _ => format!("builds `{local}` with {}", calls.join(", ")),
    }
}

/// A value built in steps, `start; = next when c; = other when d`, compared step by step:
/// `` `x` now only when e `` for a step that gained a condition, `` `x` added when c `` for a
/// new one. `None` when it is not built in steps or the start changed.
fn steps_changed(before: &str, after: &str) -> Option<Vec<String>> {
    let parse = |v: &str| -> Vec<(String, Option<String>)> {
        crate::render::top_level_args(&v.replace("; ", ",\u{1}"))
            .into_iter()
            .map(|s| s.trim_start_matches('\u{1}').trim().trim_start_matches("= ").to_string())
            .map(|s| match s.split_once(" when ") {
                Some((v, c)) => (v.to_string(), Some(c.to_string())),
                None => (s, None),
            })
            .collect()
    };
    let (b, a) = (parse(before), parse(after));
    if b.len() < 2 || a.len() < 2 || b[0] != a[0] {
        return None;
    }
    // `append(blockers, `${n} variant(s) still need axis values`)` → `… variant(s) still need axis values`.
    let short = |v: &str| {
        let v = match v.strip_prefix("append(").and_then(|r| r.strip_suffix(')')) {
            Some(args) => crate::render::top_level_args(args).get(1).cloned().unwrap_or_else(|| v.to_string()),
            None => v.to_string(),
        };
        let mut text = String::new();
        let mut depth = 0;
        let mut chars = v.trim_matches('`').chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '$' && chars.peek() == Some(&'{') && depth == 0 {
                chars.next();
                depth = 1;
                text.push('…');
                continue;
            }
            if depth > 0 {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                continue;
            }
            text.push(ch);
        }
        let v = plain(&text);
        if v.chars().count() > 70 {
            format!("{}…", v.chars().take(69).collect::<String>())
        } else {
            v
        }
    };
    let mut out = vec![];
    for (v, c) in &a[1..] {
        match b[1..].iter().find(|(bv, _)| bv == v) {
            Some((_, bc)) if bc == c => {}
            Some((_, bc)) => {
                let extra = match (bc, c) {
                    (Some(bc), Some(c)) => c.strip_prefix(&format!("{bc} ∧ ")).map(|x| format!("now only when {}", plain(x))),
                    (None, Some(c)) => Some(format!("now only when {}", plain(c))),
                    _ => None,
                };
                let what = extra.unwrap_or_else(|| format!("now when {} (was when {})", c.as_deref().map(plain).unwrap_or_default(), bc.as_deref().map(plain).unwrap_or_default()));
                out.push(format!("`{}` {what}", short(v)));
            }
            None => out.push(format!("`{}` added{}", short(v), c.as_deref().map(|c| format!(" when {}", plain(c))).unwrap_or_default())),
        }
    }
    for (v, _) in b[1..].iter().filter(|(bv, _)| !a[1..].iter().any(|(av, _)| av == bv)) {
        out.push(format!("`{}` no longer added", short(v)));
    }
    (!out.is_empty()).then_some(out)
}

/// The rows a delete removes, from the filter written at the same place: ` where cart_id = ?`.
fn where_of(x: &Clause) -> String {
    x.tag.as_deref().and_then(|t| t.strip_prefix("where ")).map(|w| format!(" where {}", plain(w))).unwrap_or_default()
}

/// An element picked from a constant map by `k` (`<ICONS[k]>`, before a fixed `<Icon>`), shown
/// beside a constant label map on the same key (`{LABELS[k]}`): each state as a person sees it,
/// `price_asc: ArrowUpDown "Price ↑" → ArrowUp "Price"`. With the key, the rows, the label map.
fn shown_by_key(c: &ContractDiff, ea: &str, eb: &str) -> Option<(String, Vec<String>, String)> {
    let (icons, k) = ea.strip_prefix('<')?.strip_suffix('>')?.split_once('[')?;
    let k = k.trim_end_matches(']');
    let map = |text: &str| -> Option<Vec<(String, String)>> {
        let inner = text.split_once(" = ")?.1.strip_prefix('{')?.strip_suffix('}')?;
        Some(crate::render::top_level_args(inner).into_iter().filter_map(|kv| kv.split_once(": ").map(|(a, b)| (a.to_string(), b.to_string()))).collect())
    };
    let consts = |name: &str| c.clauses.iter().find(|x| x.row == "consts" && x.after.as_deref().is_some_and(|v| v.starts_with(&format!("{name} = {{"))));
    let icons_now = map(consts(icons)?.after.as_deref()?)?;
    // The label map shown on the same key, read from what it renders.
    let label = c.clauses.iter().filter(|x| x.row == "renders").filter_map(|x| x.after.as_deref()).find_map(|t| {
        let i = t.find(&format!("[{k}]}}"))?;
        let start = t[..i].rfind('{')? + 1;
        Some(t[start..i].to_string())
    })?;
    let lc = consts(&label)?;
    let labels_now = map(lc.after.as_deref()?)?;
    let labels_then = map(lc.before.as_deref().or(lc.after.as_deref())?)?;
    let fixed = eb.trim_start_matches('<').trim_end_matches('>');
    let mut rows = vec![];
    for (key, icon) in &icons_now {
        let now_label = labels_now.iter().find(|(x, _)| x == key).map(|(_, v)| v.as_str()).unwrap_or("?");
        let then_label = labels_then.iter().find(|(x, _)| x == key).map(|(_, v)| v.as_str()).unwrap_or("?");
        if fixed == icon && then_label == now_label {
            continue;
        }
        rows.push(format!("{key:<11} {fixed} {then_label}  →  {icon} {now_label}"));
    }
    (!rows.is_empty()).then(|| (k.to_string(), rows, label))
}

/// A new SQL script, one line per statement: `INSERT material_groups: title := Lighting, …
/// where not exists (…)`. Facts belong to the statement starting at or before their line.
fn script_lines(c: &ContractDiff, source: SourceLine) -> Option<Vec<Line>> {
    let line_of = |x: &Clause| x.evidence_after.as_ref().map(|l| l.line);
    // Statements start where they write (or read) and where they set columns.
    let mut starts: BTreeSet<u32> = c.clauses.iter().filter(|x| matches!(x.row.as_str(), "effects" | "post")).filter_map(line_of).collect();
    if starts.is_empty() {
        return None;
    }
    let first = *starts.iter().next()?;
    starts.insert(first);
    let owner = |l: u32| starts.range(..=l).next_back().copied().unwrap_or(first);
    struct Stmt {
        tables: Vec<String>,
        reads: Vec<String>,
        sets: Vec<String>,
        wheres: Vec<String>,
        loc: Option<merak_ir::Loc>,
    }
    let mut stmts: BTreeMap<u32, Stmt> = BTreeMap::new();
    for x in &c.clauses {
        let Some(l) = line_of(x) else { continue };
        let st = stmts.entry(owner(l)).or_insert(Stmt { tables: vec![], reads: vec![], sets: vec![], wheres: vec![], loc: None });
        let after = x.after.clone().unwrap_or_default();
        match x.row.as_str() {
            "effects" => {
                let (kind, rest) = after.split_once(' ').unwrap_or(("", &after));
                let table = rest.split(' ').next().unwrap_or(rest).to_string();
                if kind == "db_read" {
                    st.reads.push(table);
                } else if !st.tables.contains(&table) {
                    st.tables.push(table);
                }
                if st.loc.is_none() {
                    st.loc = x.evidence_after.clone();
                }
            }
            "post" => {
                let (field, value) = after.split_once(" := ").unwrap_or((&after, ""));
                let (table, col) = field.rsplit_once('.').unwrap_or(("", field));
                let value = if value.is_empty() { "\"\"".to_string() } else { value.to_string() };
                let value = if value.chars().count() > 30 { format!("{}…", value.chars().take(29).collect::<String>()) } else { value };
                st.sets.push(format!("{col} := {value}"));
                // A statement whose table an earlier one already wrote: known by what it sets.
                if !table.is_empty() && !st.tables.iter().any(|t| t == table) {
                    st.tables.push(table.to_string());
                }
                if st.loc.is_none() {
                    st.loc = x.evidence_after.clone();
                }
            }
            "where" => {
                let pred = after.split_once(": ").filter(|(t, _)| !t.contains(' ')).map_or(after.as_str(), |(_, p)| p);
                // Joins say where rows come from, not which are touched.
                if !pred.split_whitespace().next().is_some_and(|w| matches!(w, "join" | "left" | "right" | "inner" | "full" | "cross")) {
                    st.wheres.push(plain(pred));
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    for (start, st) in stmts {
        let loc = st.loc.clone();
        // The statement's verb, as written on its first line.
        let verb = loc
            .as_ref()
            .and_then(|l| source(l, false))
            .and_then(|t| t.split_whitespace().next().map(|w| w.to_uppercase()))
            .filter(|w| w.chars().all(|c| c.is_ascii_alphabetic()))
            .unwrap_or_else(|| "RUNS".into());
        // `WITH ranked AS (…) UPDATE …`: what it does, not how it starts.
        let verb = if verb == "WITH" && !st.sets.is_empty() { "UPDATE".to_string() } else { verb };
        let target = if st.tables.is_empty() { format!("reads {}", st.reads.join(", ")) } else { st.tables.join(", ") };
        let mut text = format!("{verb} {target}");
        if !st.sets.is_empty() {
            text.push_str(&format!(": {}", st.sets.join(", ")));
        }
        if !st.wheres.is_empty() {
            let w = st.wheres.join(" and ");
            let w = if w.chars().count() > 160 { format!("{}…", w.chars().take(159).collect::<String>()) } else { w };
            text.push_str(&format!(" where {w}"));
        }
        let evidence = loc.as_ref().and_then(|l| source(l, false)).map(|t| format!("{:>4}   {}", start, clip(t.trim()))).into_iter().collect();
        out.push(Line { glyph: '+', kind: "SQL", text, detail: vec![], evidence, places: 1, work: None });
    }
    Some(out)
}

/// `this.searches.through` → `searches.through`, not inside a longer name (`isthis.x`).
fn strip_this(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("this.") {
        let before = &rest[..i];
        out.push_str(before);
        if before.chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            out.push_str("this.");
        }
        rest = &rest[i + "this.".len()..];
    }
    out.push_str(rest);
    out
}

/// `a, b or c`.
fn join_or(xs: &[String]) -> String {
    match xs {
        [] => String::new(),
        [x] => x.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// `onMouseEnter` → `mouse enter`.
fn event_words(ev: &str) -> String {
    let mut out = String::new();
    for (i, c) in ev.trim_start_matches("on").chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push(' ');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// `scoped: join …` → `scoped`.
fn scope(filter: &str) -> String {
    filter.split_once(": ").map_or("rows", |(s, _)| s).to_string()
}

/// `a in arms: skipped when C` → `skips each a in arms when C`.
fn skip(s: &str) -> String {
    match s.split_once(": skipped when ") {
        Some(("loop", when)) => format!("skips an iteration when {when}"),
        Some((each, when)) => format!("skips each {each} when {when}"),
        None => format!("skips {s}"),
    }
}

/// `when C: v` → `returns v when C`; `otherwise: v` → `otherwise returns v`.
fn output(s: &str) -> String {
    match s.split_once(": ") {
        Some(("otherwise", v)) => format!("otherwise returns {v}"),
        Some((case, v)) if case.starts_with("when ") => format!("returns {v} {case}"),
        _ => format!("returns {s}"),
    }
}

/// `Scope.Context is written` → `writes Scope.Context`; `x := v` → `sets x := v`.
fn write(s: &str) -> String {
    match s.strip_suffix(" is written") {
        Some(f) => format!("writes {f}"),
        None => match s.strip_suffix(" is written (by a callee)") {
            Some(f) => format!("writes {f} (in a callee)"),
            None if s.ends_with(":=") || s.ends_with(":= ") => format!("sets {} \"\"", s.trim_end()),
            None => format!("sets {s}"),
        },
    }
}

/// One line of code behind a clause: the new line, or the old one for what was removed.
fn evidence(x: &Clause, source: SourceLine, file: &str) -> Vec<String> {
    let (loc, before) = match x.mark {
        '-' => match x.evidence_before.as_ref() {
            Some(l) => (l, true),
            None => return vec![],
        },
        _ => match x.evidence_after.as_ref().or(x.evidence_before.as_ref()) {
            Some(l) => (l, x.evidence_after.is_none()),
            None => return vec![],
        },
    };
    code_line(loc, before, source, file).into_iter().collect()
}

/// `  225   push(…)`, or `synonyms.go:93   …` from another file.
fn code_line(loc: &merak_ir::Loc, before: bool, source: SourceLine, file: &str) -> Option<String> {
    let text = source(loc, before)?;
    let at = if loc.file == file || file.is_empty() { loc.line.to_string() } else { format!("{}:{}", loc.file.rsplit('/').next().unwrap_or(&loc.file), loc.line) };
    let sign = if before { "-" } else { " " };
    let line = format!("{at:>4} {sign} {}", text.trim());
    Some(clip(&line).chars().take(WIDTH - TEXT).collect())
}

/// For calls Merak cannot type: the first line in the function that calls each name, from
/// the operation's evidence.
fn call_sites(t: &Transition, c: &ContractDiff, names: &[String], source: SourceLine) -> Vec<String> {
    let locs: Vec<&merak_ir::Loc> =
        t.ops.iter().filter(|o| o.kind == "UNCLASSIFIED_CHANGE" && c.ops.contains(&o.id)).flat_map(|o| &o.evidence_after).filter(|l| l.file == c.loc.file).collect();
    let mut out = vec![];
    for n in names {
        let call = format!("{n}(");
        let hit = locs.iter().find(|l| source(l, false).is_some_and(|text| text.contains(&call)));
        if let Some(line) = hit.and_then(|l| code_line(l, false, source, &c.loc.file)) {
            if !out.contains(&line) {
                out.push(line);
            }
        }
    }
    out
}

/// Words that are not names: a clause that swaps one for another changed.
const KEYWORDS: &[&str] = &[
    "when", "skipped", "in", "is", "not", "set", "each", "any", "of", "all", "and", "or", "AND", "OR", "NOT", "else", "end", "case", "join", "left", "on",
    "otherwise", "true", "false", "null", "nil", "undefined", "len", "ok", "exists", "written",
];

/// The consistent renames that turn `a` into `b`, when they have the same shape: `len(anchors)`
/// → `len(scopes)` is `[(anchors, scopes)]`. `None` when anything but a name differs.
fn renames(a: &str, b: &str) -> Option<Vec<(String, String)>> {
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.len() != tb.len() {
        return None;
    }
    let (mut fwd, mut bwd): (BTreeMap<&str, &str>, BTreeMap<&str, &str>) = (BTreeMap::new(), BTreeMap::new());
    let mut out = vec![];
    for (x, y) in ta.iter().zip(&tb) {
        let name = |t: &str| t.starts_with(|c: char| c.is_alphabetic() || c == '_') && !KEYWORDS.contains(&t);
        if x != y && !(name(x) && name(y)) {
            return None;
        }
        match (fwd.get(x.as_str()), bwd.get(y.as_str())) {
            (Some(m), _) if *m != y => return None,
            (_, Some(m)) if *m != x => return None,
            (Some(_), _) => {}
            _ => {
                fwd.insert(x, y);
                bwd.insert(y, x);
                if x != y {
                    out.push((x.clone(), y.clone()));
                }
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Names (with dots split off) and single symbols; spaces dropped.
fn tokens(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if !c.is_whitespace() {
            out.push(c.to_string());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A predicate as people read it: `len(xs) ∈ {0} ∨ !(done)` → `xs is empty or not done`,
/// `⟨built by f: X⟩` → `X`, `ok(f) is not set` → `f fails`.
pub(crate) fn plain(s: &str) -> String {
    // The method's own object (`this.`, a Go receiver) goes without saying.
    let s = strip_this(s);
    let s = s.replace(" ⟨dynamic⟩", " + conditions built at run time").replace(" ∉ {}", " != \"\"").replace(" ∈ {}", " = \"\"");
    let mut s = unatom(&blanks(&s));
    // `<fn addDimensionWords.FieldsFunc>`: a function literal, as written.
    while let Some(i) = s.find("<fn ") {
        let Some(j) = s[i..].find('>').map(|j| j + i) else { break };
        s.replace_range(i..=j, "func(…)");
    }
    s = rewrite_call(&s, "len(", |inner, before, after| {
        if let Some(rest) = after.strip_prefix(" ∈ {0}") {
            Some((before.to_string(), format!("{inner} is empty"), rest.to_string()))
        } else if let Some(rest) = after.strip_prefix(" > 0") {
            Some((before.to_string(), format!("{inner} is not empty"), rest.to_string()))
        } else {
            before.strip_suffix("0 < ").map(|b| (b.to_string(), format!("{inner} is not empty"), after.to_string()))
        }
    });
    s = rewrite_call(&s, "ok(", |inner, before, after| {
        if let Some(rest) = after.strip_prefix(" is not set") {
            Some((before.to_string(), format!("{inner} fails"), rest.to_string()))
        } else {
            after.strip_prefix(" is set").map(|rest| (before.to_string(), format!("{inner} succeeds"), rest.to_string()))
        }
    });
    // Go's `strings.Contains(s, x)`, `HasPrefix`, `HasSuffix`, read as words; negated ones too.
    for (f, yes, no) in [("strings.Contains(", "contains", "does not contain"), ("strings.HasPrefix(", "starts with", "does not start with"), ("strings.HasSuffix(", "ends with", "does not end with")] {
        s = rewrite_call(&s, f, |inner, before, after| {
            let parts = crate::render::top_level_args(inner);
            let [x, y] = parts.as_slice() else { return None };
            // `!(strings.HasSuffix(w, "ss"))`: the negation wraps the call alone.
            match (before.strip_suffix("!("), after.strip_prefix(')')) {
                (Some(b), Some(a)) => Some((b.to_string(), format!("{x} {no} {y}"), a.to_string())),
                _ => Some((before.to_string(), format!("{x} {yes} {y}"), after.to_string())),
            }
        });
    }
    s = rewrite_call(&s, "!(", |inner, before, after| {
        let inner = if inner.contains(' ') { format!("({inner})") } else { inner.to_string() };
        Some((before.to_string(), format!("not {inner}"), after.to_string()))
    });
    // `m.has(k)` → `k is in m`; `not m.has(k)` → `k is not in m`.
    while let Some(i) = s.find(".has(") {
        let start = i + ".has(".len();
        let Some(end) = close(&s[start..]).map(|e| e + start) else { break };
        let obj_at = s[..i].char_indices().rev().find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '.')).map_or(0, |(j, c)| j + c.len_utf8());
        let (obj, key) = (s[obj_at..i].to_string(), s[start..end].to_string());
        let (from, text) = match s[..obj_at].strip_suffix("not ") {
            Some(b) => (b.len(), format!("{key} is not in {obj}")),
            None => (obj_at, format!("{key} is in {obj}")),
        };
        s = format!("{}{text}{}", &s[..from], &s[end + 1..]);
    }
    // `x ∈ {A}` → `x = A`; `x ∈ {A, B}` → `x is one of A, B`.
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(i) = rest.find(" ∈ {") {
        let tail = &rest[i + " ∈ {".len()..];
        let Some(j) = tail.find('}') else { break };
        let set = &tail[..j];
        out.push_str(&rest[..i]);
        // An empty string in a set is `""`, not nothing: `{, null}` → `"", null`.
        let items: Vec<String> = set.split(", ").map(|v| if v.is_empty() { "\"\"".to_string() } else { v.to_string() }).collect();
        out.push_str(&if items.len() > 1 { format!(" is one of {}", items.join(", ")) } else { format!(" = {}", items[0]) });
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    // `x ∉ {a}` → `x != a`; `x ∉ {a, b}` → `x is not one of a, b`.
    let mut out2 = String::new();
    let mut rest = out.as_str();
    while let Some(i) = rest.find(" ∉ {") {
        let tail = &rest[i + " ∉ {".len()..];
        let Some(j) = tail.find('}') else { break };
        let set = &tail[..j];
        out2.push_str(&rest[..i]);
        out2.push_str(&match set {
            "" => " != \"\"".to_string(),
            "null" => " is not null".to_string(),
            s if s.contains(", ") => format!(" is not one of {s}"),
            s => format!(" != {s}"),
        });
        rest = &tail[j + 1..];
    }
    out2.push_str(rest);
    let out = out2;
    // `err(f) = null`: what Merak calls the error `f` returns.
    let out = rewrite_call(&out, "err(", |inner, before, after| after.strip_prefix(" = null").map(|rest| (before.to_string(), format!("{inner} returns no error"), rest.to_string())));
    out.replace(" ∉ {null}", " is not null")
        .replace(" = null", " is null")
        .replace(" ∨ ", " or ").replace(" ∧ ", " and ").replace(" ∉ ", " not in ")
}

/// Rewrite every `name(inner)` whose surroundings `f` recognises; `f` gets the inner text,
/// the text before and after the call, and returns the new before, call and after.
fn rewrite_call(s: &str, open: &str, f: impl Fn(&str, &str, &str) -> Option<(String, String, String)>) -> String {
    let mut s = s.to_string();
    let mut from = 0;
    while let Some(i) = s[from..].find(open).map(|i| i + from) {
        // Not a longer name ending in it: `isOk(`, `xlen(`.
        let prev = s[..i].chars().next_back();
        let start = i + open.len();
        let Some(end) = close(&s[start..]).map(|e| e + start) else { break };
        if prev.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            from = start;
            continue;
        }
        match f(&s[start..end], &s[..i], &s[end + 1..]) {
            Some((before, call, after)) => {
                from = before.len() + call.len();
                s = format!("{before}{call}{after}");
            }
            None => from = start,
        }
    }
    s
}

/// Index of the `)` closing an already opened parenthesis.
fn close(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(i),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Atoms (`⟨…⟩`) as their content, innermost first: `⟨built by f⟩` → `f(…)`,
/// `⟨built by f: X⟩` → `X`, `⟨f(c), each c in xs⟩` → `f(c) for each c in xs`.
fn unatom(s: &str) -> String {
    let mut s = s.to_string();
    while let Some(i) = s.rfind('⟨') {
        let Some(j) = s[i..].find('⟩').map(|j| j + i) else { break };
        let inner = &s[i + '⟨'.len_utf8()..j];
        let mut text = match inner.strip_prefix("built by ") {
            Some(b) => match b.split_once(": ") {
                Some((_, shape)) => shape.to_string(),
                None => format!("{b}(…)"),
            },
            None => inner.to_string(),
        };
        text = text.replace(", each ", " for each ");
        let whole = i == 0 && j + '⟩'.len_utf8() == s.len();
        if !whole && (text.contains(" AND ") || text.contains(" OR ")) {
            text = format!("({text})");
        }
        s = format!("{}{text}{}", &s[..i], &s[j + '⟩'.len_utf8()..]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{plain, renames};

    #[test]
    fn predicates_read_as_words() {
        assert_eq!(plain("len(scopes) ∈ {0} ∨ limit <= 0"), "scopes is empty or limit <= 0");
        assert_eq!(plain("0 < len(a.context) ∧ len(raw) ∈ {0} ∧ !(a.match.Entity.IsTerminal())"), "a.context is not empty and raw is empty and not a.match.Entity.IsTerminal()");
        assert_eq!(plain("ok(scopePredicate) is not set"), "scopePredicate fails");
        assert_eq!(plain("User.role ∈ {ADMIN, OWNER}"), "User.role is one of ADMIN, OWNER");
        assert_eq!(plain("status ∈ {open}"), "status = open");
        assert_eq!(plain("!(dimensionWords.has(singular(w)))"), "singular(w) is not in dimensionWords");
        assert_eq!(plain("own.has(singular(w))"), "singular(w) is in own");
        assert_eq!(plain("!(strings.HasSuffix(w, \"ss\")) ∧ strings.Contains(k, looseKey(a, b))"), "w does not end with \"ss\" and k contains looseKey(a, b)");
        assert_eq!(plain("err(store.FeedExists) = null"), "store.FeedExists returns no error");
        assert_eq!(plain("icon(store.IconByID) ∉ {null}"), "icon(store.IconByID) is not null");
        assert_eq!(plain("⟨anchorPredicate(c), each c in sc.Context⟩"), "anchorPredicate(c) for each c in sc.Context");
        assert_eq!(plain("any of (⟨built by anchorPredicate⟩)"), "any of (anchorPredicate(…))");
        assert_eq!(
            plain("case a.ix ⟨built by scopePredicate: ⟨f(sc.Anchor)⟩ AND ⟨f(c), each c in sc.Context⟩⟩ else false end"),
            "case a.ix (f(sc.Anchor) AND f(c) for each c in sc.Context) else false end"
        );
        // Longer names ending in a rewritten one are left alone.
        assert_eq!(plain("isOk(x) is not set"), "isOk(x) is not set");
    }

    #[test]
    fn same_shape_with_other_names_is_a_rename() {
        assert_eq!(
            renames("m in anchorMatches: skipped when len(partnersByAnchor) <= i", "a in arms: skipped when len(partnersByScope) <= i"),
            Some(vec![("m".into(), "a".into()), ("anchorMatches".into(), "arms".into()), ("partnersByAnchor".into(), "partnersByScope".into())])
        );
        // Another operator, number or keyword is not a rename.
        assert_eq!(renames("len(xs) <= i", "len(xs) < i"), None);
        assert_eq!(renames("x > 5", "x > 6"), None);
        assert_eq!(renames("a ∧ b", "a ∨ b"), None);
        assert_eq!(renames("x is set", "x is not set"), None);
        // Inconsistent: one name mapped to two.
        assert_eq!(renames("f(a, a)", "f(b, c)"), None);
        assert_eq!(renames("f(a)", "f(a)"), None);
    }
}
