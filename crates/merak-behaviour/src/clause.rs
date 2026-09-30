//! Row filters assembled by program functions, as text: `⟨anchorPredicate(sc.Anchor)⟩ AND
//! ⟨anchorPredicate(c), each c in sc.Context⟩`. A `⟨…⟩` atom is a builder whose own text is
//! not shown; `(`…`)` group SQL as usual.

/// `s` split on `sep` where it is not nested in parentheses or an atom.
pub fn split_top<'s>(s: &'s str, sep: &str) -> Vec<&'s str> {
    let mut out = vec![];
    let (mut depth, mut start) = (0i32, 0);
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let c = rest.chars().next().unwrap_or(' ');
        match c {
            '(' | '⟨' => depth += 1,
            ')' | '⟩' => depth -= 1,
            _ if depth == 0 && rest.starts_with(sep) => {
                out.push(&s[start..i]);
                i += sep.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += c.len_utf8();
    }
    out.push(&s[start..]);
    out
}

/// Whether `s` combines terms with a top-level `AND` / `OR`.
fn combines(s: &str) -> bool {
    [" AND ", " OR ", " and ", " or "].iter().any(|sep| split_top(s, sep).len() > 1)
}

/// Parentheses that group a single term are dropped: `(⟨a⟩) AND (⟨b⟩)` → `⟨a⟩ AND ⟨b⟩`.
/// Call parentheses (`count(x)`, `⟨f(x)⟩`) are kept.
pub fn unparen(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut drop = vec![false; chars.len()];
    let (mut stack, mut atom) = (vec![], 0);
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '⟨' => atom += 1,
            '⟩' => atom -= 1,
            '(' if atom == 0 => stack.push(i),
            ')' if atom == 0 => {
                if let Some(o) = stack.pop() {
                    let bare = o == 0 || matches!(chars[o - 1], ' ' | '(');
                    let inner: String = chars[o + 1..i].iter().collect();
                    if bare && !combines(&inner) {
                        drop[o] = true;
                        drop[i] = true;
                    }
                }
            }
            _ => {}
        }
    }
    chars.iter().zip(&drop).filter(|(_, d)| !**d).map(|(c, _)| *c).collect()
}

/// `(x)` → `x` when the parentheses enclose all of it.
pub fn strip_outer(s: &str) -> &str {
    let t = s.trim();
    if !t.starts_with('(') {
        return t;
    }
    let mut depth = 0;
    for (i, c) in t.char_indices() {
        match c {
            '(' | '⟨' => depth += 1,
            ')' | '⟩' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return if i + 1 == t.len() { strip_outer(&t[1..i]) } else { t };
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_terms_lose_their_grouping() {
        assert_eq!(unparen("(⟨a(x)⟩) AND (⟨b(c), each c in cs⟩)"), "⟨a(x)⟩ AND ⟨b(c), each c in cs⟩");
        assert_eq!(unparen("((a) AND (b)) OR count(x)"), "(a AND b) OR count(x)");
        assert_eq!(split_top("⟨a AND b⟩ AND (c AND d)", " AND "), ["⟨a AND b⟩", "(c AND d)"]);
        assert_eq!(strip_outer("(a) AND (b)"), "(a) AND (b)");
        assert_eq!(strip_outer("((a AND b))"), "a AND b");
    }
}
