//! Names across a call: a callee's parameters by what a call passes there, so the callee's
//! facts can be read in its caller's terms (summary instantiation).

use crate::model::{CallFact, Entity};
use std::collections::BTreeMap;

/// A helper's parameters by name, each with what a call passes there, in the terms of the
/// function the helpers are inlined into (`None`: not expressible there, `{…}` or a closure).
pub type Names = BTreeMap<String, Option<String>>;

/// The names of `t`'s parameters at call `c`, its arguments read through `outer` (the names
/// of the helper `c` is made from, when helpers nest).
pub fn call_names(t: &Entity, c: &CallFact, outer: &Names) -> Names {
    let usable = |a: &String| !a.contains('…') && !a.contains("<fn");
    let mut names: Names = t
        .params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let name = p.split(": ").next().unwrap_or(p).trim_end_matches('?').trim_start_matches("...");
            let arg = c.args.get(i).filter(|a| usable(a)).and_then(|a| rename(a, outer));
            is_ident(name).then(|| (name.to_string(), arg))
        })
        .collect();
    // A rendered component: `function Title({ who, size = 2 }: …)` given `<Title who={member}/>`.
    if let Some(first) = t.params.first().filter(|_| !c.props.is_empty()) {
        for (prop, local) in destructured(first) {
            let arg = c.props.iter().find(|(k, _)| *k == prop).map(|(_, v)| v).filter(|v| usable(v)).and_then(|v| rename(v, outer));
            names.insert(local, arg);
        }
    }
    names
}

/// The names a destructured parameter binds, each with the property it reads:
/// `{ who, a: b, c = 1, ...rest }: T` → `(who, who)`, `(a, b)`, `(c, c)`.
pub fn destructured(param: &str) -> Vec<(String, String)> {
    let Some(body) = param.trim().strip_prefix('{') else { return vec![] };
    let mut depth = 0;
    let end = body.char_indices().find(|&(_, ch)| {
        match ch {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' if depth > 0 => depth -= 1,
            '}' => return true,
            _ => {}
        }
        false
    });
    let Some((end, _)) = end else { return vec![] };
    let mut out = vec![];
    let (mut depth, mut start) = (0, 0);
    let inner = &body[..end];
    for (i, ch) in inner.char_indices().chain([(inner.len(), ',')]) {
        match ch {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                let item = inner[start..i].trim();
                start = i + 1;
                let item = item.split('=').next().unwrap_or(item).trim();
                let (prop, local) = match item.split_once(':') {
                    Some((p, l)) => (p.trim(), l.trim()),
                    None => (item, item),
                };
                if is_ident(prop) && is_ident(local) {
                    out.push((prop.to_string(), local.to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

/// `s` with each parameter of `names` replaced by its argument; `None` when one has none.
pub fn rename(s: &str, names: &Names) -> Option<String> {
    rewrite_roots(s, &|x: &str| match names.get(x) {
        Some(arg) => arg.clone(),
        None => Some(x.to_string()),
    })
}

/// `s` renamed, or as written when it cannot be.
pub fn renamed(s: &str, names: &Names) -> String {
    rename(s, names).unwrap_or_else(|| s.to_string())
}

pub fn is_ident(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// `s` with each root name (an identifier not after a `.`, outside quotes) rewritten by `f`;
/// `None` when `f` cannot rewrite one.
pub fn rewrite_roots(s: &str, f: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            out.push(ch);
            if ch == q {
                quote = None;
            }
            i += 1;
        } else if matches!(ch, '"' | '\'' | '`') {
            quote = Some(ch);
            out.push(ch);
            i += 1;
        } else if ch.is_alphabetic() || ch == '_' || ch == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if start > 0 && chars[start - 1] == '.' {
                out.push_str(&word);
            } else {
                out.push_str(&f(&word)?);
            }
        } else {
            out.push(ch);
            i += 1;
        }
    }
    Some(out)
}
