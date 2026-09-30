//! Just enough SQL to say what a query string does: whether it writes, the table it
//! works on and the conjuncts of its `WHERE` clause (Go's `database/sql`, sqlx, pgx).

#[derive(Debug, Clone, PartialEq)]
pub struct SqlInfo {
    pub write: bool,
    pub table: String,
    /// `WHERE` conjuncts, lower-cased outside string literals, with each placeholder
    /// replaced by what `value` says argument `n` (1-based) is, or `?`.
    pub filters: Vec<String>,
    /// The statement, normalised, with its WHERE conjuncts left out (`where …`) and
    /// placeholders as `?`: what the statement does besides filtering.
    pub skeleton: String,
}

const VERBS: &[&str] = &["select", "insert", "update", "delete", "merge", "upsert", "replace"];
const CLAUSE_ENDS: &[&str] = &["order", "group", "limit", "returning", "for", "having", "offset", "union", "except", "intersect", "on", "window", ";"];

pub fn parse(sql: &str, value: &dyn Fn(usize) -> Option<String>) -> Option<SqlInfo> {
    let toks = tokenize(sql);
    let low: Vec<String> = toks.iter().map(|t| if t.starts_with('\'') { t.clone() } else { t.to_lowercase() }).collect();
    let depth = depths(&low);
    // The statement's verb: the first at depth 0 (after any `WITH` CTEs, which are in parens).
    let v = (0..low.len()).find(|&i| depth[i] == 0 && VERBS.contains(&low[i].as_str()))?;
    let verb = low[v].as_str();
    let after = |kw: &str| (v..low.len()).find(|&i| depth[i] == 0 && low[i] == kw).and_then(|i| toks.get(i + 1));
    let table = match verb {
        "select" | "delete" => after("from"),
        "insert" | "merge" | "upsert" | "replace" => after("into"),
        "update" => toks.get(v + 1).filter(|t| !t.eq_ignore_ascii_case("only")).or_else(|| toks.get(v + 2)),
        _ => None,
    };
    let table = match table.map(String::as_str) {
        Some("(") | None => "<subquery>".to_string(),
        Some(t) => t.trim_matches(['"', '`', '[', ']']).to_string(),
    };

    let mut filters = vec![];
    let where_at = (v..low.len()).find(|&i| depth[i] == 0 && low[i] == "where");
    let where_end = where_at.map(|w| (w + 1..low.len()).find(|&i| depth[i] == 0 && CLAUSE_ENDS.contains(&low[i].as_str())).unwrap_or(low.len()));
    if let (Some(w), Some(end)) = (where_at, where_end) {
        let mut positional = 0;
        let mut cur: Vec<String> = vec![];
        let mut between = false;
        for i in w + 1..end {
            let t = low[i].as_str();
            if depth[i] == 0 && t == "and" && !between {
                filters.push(join(&cur));
                cur.clear();
                continue;
            }
            if t == "between" {
                between = true;
            } else if t == "and" {
                between = false;
            }
            let tok = if t == "?" {
                positional += 1;
                value(positional).unwrap_or_else(|| "?".into())
            } else if let Some(n) = t.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
                value(n).unwrap_or_else(|| "?".into())
            } else {
                low[i].clone()
            };
            cur.push(tok);
        }
        if !cur.is_empty() {
            filters.push(join(&cur));
        }
    }
    let write = verb != "select";
    let placeholder = |t: &String| if t == "?" || (t.starts_with('$') && t[1..].parse::<usize>().is_ok()) { "?".to_string() } else { t.clone() };
    let skeleton = match (where_at, where_end) {
        (Some(w), Some(end)) => {
            let mut toks: Vec<String> = low[..=w].iter().map(placeholder).collect();
            toks.push("…".into());
            toks.extend(low[end..].iter().map(placeholder));
            join(&toks)
        }
        _ => join(&low.iter().map(placeholder).collect::<Vec<_>>()),
    };
    Some(SqlInfo { write, table, filters, skeleton })
}

fn join(toks: &[String]) -> String {
    let s = toks.join(" ");
    let s = s.replace("( ", "(").replace(" )", ")").replace(" ,", ",").replace(" .", ".").replace(". ", ".");
    // A conjunct wrapped whole in parens reads the same without them.
    match s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        Some(inner) if !inner.contains('(') && !inner.contains(')') => inner.to_string(),
        _ => s,
    }
}

fn depths(toks: &[String]) -> Vec<i32> {
    let mut d = 0;
    toks.iter()
        .map(|t| {
            if t == ")" {
                d -= 1;
            }
            let here = d;
            if t == "(" {
                d += 1;
            }
            here
        })
        .collect()
}

pub(crate) fn tokenize(sql: &str) -> Vec<String> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && b.get(i + 1) == Some(&'-') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '\'' {
            let start = i;
            i += 1;
            while i < b.len() && !(b[i] == '\'' && b.get(i + 1) != Some(&'\'')) {
                i += if b[i] == '\'' { 2 } else { 1 };
            }
            i += 1;
            out.push(b[start..i.min(b.len())].iter().collect());
        } else if c.is_alphanumeric() || matches!(c, '_' | '$' | '"' | '`' | ':' | '@' | '.') {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || matches!(b[i], '_' | '$' | '"' | '`' | ':' | '@' | '.')) {
                i += 1;
            }
            out.push(b[start..i].iter().collect());
        } else if matches!((c, b.get(i + 1)), ('<' | '>' | '!', Some('=')) | ('<', Some('>'))) {
            out.push(b[i..i + 2].iter().collect());
            i += 2;
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_tables_and_filters() {
        let none = |_: usize| None;
        let q = parse("UPDATE orders SET status = $1 WHERE id = $2 AND status = 'PENDING'", &|n| (n == 1).then(|| "CANCELLED".into())).unwrap();
        assert!(q.write);
        assert_eq!(q.table, "orders");
        assert_eq!(q.filters, vec!["id = ?", "status = 'PENDING'"]);
        assert_eq!(q.skeleton, "update orders set status = ? where …");
        let q =
            parse("SELECT id, total FROM orders o JOIN users u ON u.id = o.user_id WHERE o.deleted_at IS NULL AND o.total BETWEEN ? AND ? ORDER BY id", &none)
                .unwrap();
        assert!(!q.write);
        assert_eq!(q.table, "orders");
        assert_eq!(q.filters, vec!["o.deleted_at is null", "o.total between ? and ?"]);
        let q = parse("WITH x AS (SELECT 1 FROM a WHERE b = 1) INSERT INTO \"orders\" (id) VALUES ($1) RETURNING id", &none).unwrap();
        assert!(q.write);
        assert_eq!(q.table, "orders");
        assert!(q.filters.is_empty());
    }
}

#[cfg(test)]
mod exists_tests {
    #[test]
    fn exists_subquery_is_one_conjunct() {
        let q = super::parse(
            "SELECT i.id FROM icons AS i WHERE i.id = $2 AND EXISTS ( SELECT 1 FROM feeds AS f INNER JOIN feed_icons AS fi ON fi.feed_id = f.id WHERE f.user_id = $1 AND fi.icon_id = $2 )",
            &|_| None,
        )
        .unwrap();
        assert_eq!(
            q.filters,
            vec!["i.id = ?", "exists (select 1 from feeds as f inner join feed_icons as fi on fi.feed_id = f.id where f.user_id = ? and fi.icon_id = ?)"]
        );
    }
}
