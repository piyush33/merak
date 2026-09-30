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
use crate::render::{also_line, blanks, calls_summary, clip, item, plural, restyled, wrap, SourceLine, WIDTH};
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
    evidence: Option<String>,
    /// How many clauses said the same thing.
    places: usize,
}

impl Line {
    fn rank(&self) -> u8 {
        match (self.glyph, self.kind) {
            ('!', _) => 0,
            ('▲', _) => 1,
            ('▼', _) => 2,
            (_, "ACCESS" | "CHECK") => 3,
            (_, "EFFECT" | "STATE") => 4,
            (_, "WRITES" | "READS" | "VALIDATES") => 5,
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
    let mut groups: Vec<(String, Vec<&ContractDiff>)> = vec![];
    for c in &t.contracts {
        let key = group(c);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cs)) => cs.push(c),
            None => groups.push((key, vec![c])),
        }
    }
    for (key, cs) in &groups {
        let blocks: Vec<(&ContractDiff, Vec<Line>, Vec<String>)> = cs
            .iter()
            .map(|c| {
                let (lines, same) = lines(c, &changing, source);
                (*c, lines, same)
            })
            .collect();
        let all: Vec<&Line> = blocks.iter().flat_map(|(_, ls, _)| ls).collect();
        let _ = writeln!(s, "### {key} · {}\n", badge(&all, cs));
        if let Some(sentence) = sentence(&blocks) {
            let _ = writeln!(s, "{sentence}\n");
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

/// The entry point that reaches a contract; its file when none does.
fn group(c: &ContractDiff) -> String {
    let mut routes: Vec<String> = c.new_routes.iter().map(|r| format!("`{r}` (new)")).collect();
    routes.extend(c.routes.iter().filter(|r| !c.new_routes.contains(r)).map(|r| format!("`{r}`")));
    match (c.status.as_str(), c.name.as_str()) {
        (_, "Program") => "Across the program".into(),
        ("schema", _) => "Validation schemas".into(),
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
fn sentence(blocks: &[(&ContractDiff, Vec<Line>, Vec<String>)]) -> Option<String> {
    let mut top: Vec<(u8, String)> = blocks
        .iter()
        .filter_map(|(c, ls, _)| ls.iter().filter(|l| l.glyph != '◆' && l.glyph != '?').min_by_key(|l| l.rank()).map(|l| (l.rank(), c, l)))
        .map(|(r, c, l)| {
            let routes = if c.new_routes.is_empty() { String::new() } else { format!(" (new route {})", c.new_routes.join(", ")) };
            (r, format!("`{}`{routes}: {}.", c.name, l.text))
        })
        .collect();
    top.sort_by_key(|(r, _)| *r);
    // Only what moves behaviour leads; an all-context group needs no sentence.
    let lead: Vec<String> = top.into_iter().filter(|(r, _)| *r <= 5).take(2).map(|(_, s)| s).collect();
    (!lead.is_empty()).then(|| lead.join(" "))
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
                Some((v, w)) if ["read", "write", "query", "emit", "queue"].contains(&v) => (format!("{v}s").replace("querys", "queries"), w.to_string()),
                _ => ("calls".to_string(), it.clone()),
            };
            match by_verb.iter_mut().find(|(v, _)| *v == verb) {
                Some((_, xs)) => xs.push(what),
                None => by_verb.push((verb, vec![what])),
            }
        }
    }
    (!by_verb.is_empty()).then(|| by_verb.into_iter().map(|(v, xs)| format!("{v} {}", xs.join(", "))).collect::<Vec<_>>().join(" · "))
}

/// `repository.CoOccurring · changed · repository.go:710`; a new function with its parameters.
fn title(c: &ContractDiff) -> String {
    let at = if c.loc.file.is_empty() { String::new() } else { format!(" · {}", c.loc) };
    match c.status.as_str() {
        "new" => {
            let params: Vec<&str> = c.clauses.iter().filter(|x| x.row == "takes").filter_map(|x| x.after.as_deref()).collect();
            format!("+ {}({}) · new{at}", c.name, params.join(", "))
        }
        "schema" => format!("{} · validation schema{at}", c.name),
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
    if let Some(e) = &l.evidence {
        out.push(format!("{pad}{e}"));
    }
    out
}

/// A contract's behaviour lines, riskiest first, and what stayed the same.
fn lines(c: &ContractDiff, changing: &BTreeSet<&str>, source: SourceLine) -> (Vec<Line>, Vec<String>) {
    let new = c.status == "new";
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
            places: 1,
        });
        used[outs[0]] = true;
        used[ins[0]] = true;
    }

    // A removed clause and an added one with the same shape: renamed, or one name swapped for
    // another that is itself changing.
    let mut renamed: Vec<(String, String)> = vec![];
    let mut folded: Vec<&str> = vec![];
    for i in 0..c.clauses.len() {
        if used[i] || c.clauses[i].mark != '-' || new {
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
        if !swapped.is_empty() {
            let (text, _) = phrase(b, false);
            let (now, _) = phrase(&c.clauses[j], false);
            out.push(Line {
                glyph: '~',
                kind: kind(&b.row),
                text: format!("{} now uses {}", noun(&b.row), swapped.join(", ")),
                detail: vec![format!("was  {}", strip_verb(&text)), format!("now  {}", strip_verb(&now))],
                evidence: evidence(&c.clauses[j], source, &c.loc.file),
                places: 1,
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
        // A call Merak cannot type is noise next to the typed changes that explain it.
        if x.mark == '?' && !untyped {
            continue;
        }
        if new && x.row == "effects" && x.after.as_deref() == Some("none found") {
            continue;
        }
        // A changed output that is markup: `RENDERS` lines say how.
        let markup = |v: &Option<String>| v.as_deref().is_some_and(|v| v.split_once(": ").is_some_and(|(_, val)| val.starts_with('<')));
        if x.row == "returns" && x.mark == '~' && markup(&x.before) && markup(&x.after) && markup_restyled {
            continue;
        }
        let (text, detail) = phrase(x, new);
        let g = glyph(x);
        // The same thing said twice: one line, citing the first place in the code.
        if let Some(l) = out.iter_mut().find(|l| l.glyph == g && l.text == text) {
            l.places += 1;
            let line = |e: &Option<String>| e.as_deref().and_then(|e| e.split_whitespace().next()?.parse::<u32>().ok());
            let other = evidence(x, source, &c.loc.file);
            if line(&other).is_some_and(|n| line(&l.evidence).is_none_or(|m| n < m)) {
                l.evidence = other;
            }
            continue;
        }
        out.push(Line { glyph: g, kind: kind(&x.row), text, detail, evidence: evidence(x, source, &c.loc.file), places: 1 });
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
            evidence: None,
            places: 1,
        });
    }
    out.sort_by_key(|l| l.rank());

    // What stayed the same, counted by kind.
    let mut same: BTreeMap<usize, (String, usize)> = BTreeMap::new();
    if !new {
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
        "reads" => "READS",
        "effects" => "EFFECT",
        "consts" => "CONSTANT",
        "renders" => "RENDERS",
        "lifecycle" => "STATE",
        "rule" => "RULE",
        "input" => "VALIDATES",
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
        || matches!(tag, "narrowed" | "reaches fewer rows" | "leaves more items out" | "new check" | "new requirement" | "fewer ways to reach this state");
    let wider = tag.starts_with("wider")
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
                None => format!("{}: {tag}", scope(&as_)),
            };
            return (text, detail);
        }
        ("where", '+') if x.tag.as_deref() == Some("joins in more data") => format!("{now}joins {as_}"),
        ("where", '-') if x.tag.as_deref() == Some("joins in less data") => format!("no longer joins {bs}"),
        ("where", '+') => format!("{}row filter: {as_}", if new { "" } else { "new " }),
        ("where", '-') => format!("row filter removed: {bs}"),
        ("skips", '+') => format!("{now}{}", skip(&as_)),
        ("skips", '-') => format!("no longer {}", skip(&bs)),
        ("pre" | "who", '+') => format!("{now}requires {as_}"),
        ("pre" | "who", '-') => format!("no longer requires {bs}"),
        ("returns" | "computes", '+') if new => output(&as_),
        ("returns" | "computes", '+') => format!("new output case: {}", output(&as_)),
        ("returns" | "computes", '-') => format!("output case removed: {}", output(&bs)),
        ("returns", '~') => {
            let (case, val) = as_.split_once(": ").unwrap_or(("", &as_));
            let was = bs.split_once(": ").map_or(bs.as_str(), |(_, v)| v);
            return (format!("{}: returns something else", case), vec![format!("was  {was}"), format!("now  {val}")]);
        }
        ("takes", '+') => format!("new parameter {as_}"),
        ("takes", '-') => format!("parameter removed: {bs}"),
        ("post", '+') => format!("{now}{}", write(&as_)),
        ("post", '-') => format!("no longer {}", write(&bs)),
        ("reads", '+') => format!("{now}reads {as_}"),
        ("reads", '-') => format!("no longer reads {bs}"),
        ("effects", '+') => item("effects", x.after.as_deref().unwrap_or("")),
        ("effects", '-') => format!("no longer: {}", item("effects", x.before.as_deref().unwrap_or(""))),
        ("effects", '~') => {
            let tag = x.tag.clone().unwrap_or_default();
            return (tag, vec![format!("was  {}", item("effects", x.before.as_deref().unwrap_or(""))), format!("now  {}", item("effects", x.after.as_deref().unwrap_or("")))]);
        }
        ("consts", '~') => {
            let v = |s: &str| s.split_once(" = ").map(|(_, v)| v.to_string()).unwrap_or_default();
            format!("{}: {} → {}", as_.split(" = ").next().unwrap_or(""), v(&bs), v(&as_))
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

/// `scoped: join …` → `scoped`.
fn scope(filter: &str) -> String {
    filter.split_once(": ").map_or("rows", |(s, _)| s).to_string()
}

/// `a in arms: skipped when C` → `skips each a in arms when C`.
fn skip(s: &str) -> String {
    match s.split_once(": skipped when ") {
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
            None => format!("sets {s}"),
        },
    }
}

/// One line of code behind a clause: the new line, or the old one for what was removed.
fn evidence(x: &Clause, source: SourceLine, file: &str) -> Option<String> {
    let (loc, before) = match x.mark {
        '-' => (x.evidence_before.as_ref()?, true),
        _ => (x.evidence_after.as_ref().or(x.evidence_before.as_ref())?, x.evidence_after.is_none()),
    };
    let text = source(loc, before)?;
    let at = if loc.file == file || file.is_empty() { loc.line.to_string() } else { format!("{}:{}", loc.file.rsplit('/').next().unwrap_or(&loc.file), loc.line) };
    let sign = if before { "-" } else { " " };
    let line = format!("{at:>4} {sign} {}", text.trim());
    Some(clip(&line).chars().take(WIDTH - TEXT).collect())
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
    let mut s = unatom(&blanks(s));
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
    s = rewrite_call(&s, "!(", |inner, before, after| {
        let inner = if inner.contains(' ') { format!("({inner})") } else { inner.to_string() };
        Some((before.to_string(), format!("not {inner}"), after.to_string()))
    });
    // `x ∈ {A}` → `x = A`; `x ∈ {A, B}` → `x is one of A, B`.
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(i) = rest.find(" ∈ {") {
        let tail = &rest[i + " ∈ {".len()..];
        let Some(j) = tail.find('}') else { break };
        let set = &tail[..j];
        out.push_str(&rest[..i]);
        out.push_str(&if set.contains(", ") { format!(" is one of {set}") } else { format!(" = {set}") });
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
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
