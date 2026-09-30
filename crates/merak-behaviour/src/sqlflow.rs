//! What a whole SQL statement does: every table it reads and writes (through CTEs, unions,
//! joins, subqueries and `LATERAL`), its row conditions (`WHERE` and `JOIN … ON`, per CTE),
//! schema changes, and column lineage: which base-table columns each written column comes
//! from, followed through CTEs.
//!
//! A recursive reading of the token stream, not a full SQL grammar: enough for the
//! statements applications write (PostgreSQL-flavoured), and silent about the rest.

use crate::sql::tokenize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Flow {
    /// Base tables read (CTE names excluded).
    pub reads: BTreeSet<String>,
    /// Tables written: INSERT, UPDATE, DELETE, MERGE, TRUNCATE.
    pub writes: BTreeSet<String>,
    /// Schema changes: (`create table`, `inventory_live_stock`) …
    pub ddl: Vec<(String, String)>,
    /// Row conditions, `scope: condition`, where scope is the CTE (or table) they apply in.
    pub filters: Vec<String>,
    /// Written column → where its value comes from (`table.column`s, or a literal).
    pub columns: BTreeMap<String, String>,
}

/// Analyze one statement. `value(n)` is what placeholder `$n` / the n-th `?` stands for.
pub fn analyze(sql: &str, value: &dyn Fn(usize) -> Option<String>) -> Option<Flow> {
    analyze_with(sql, value, &|_| false)
}

/// As `analyze`; `is_var(name)` says a bare name is a variable or parameter of the
/// enclosing function (`v_started`), not a column.
pub fn analyze_with(sql: &str, value: &dyn Fn(usize) -> Option<String>, is_var: &dyn Fn(&str) -> bool) -> Option<Flow> {
    let raw = tokenize(sql);
    if raw.is_empty() {
        return None;
    }
    let mut positional = 0;
    let t: Vec<String> = raw
        .iter()
        .map(|x| {
            if x.starts_with('\'') {
                return x.clone();
            }
            let low = x.to_lowercase();
            if low == "?" {
                positional += 1;
                return value(positional).map(|v| format!("'{v}'")).unwrap_or_else(|| "?".into());
            }
            match low.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
                Some(n) => value(n).map(|v| format!("'{v}'")).unwrap_or_else(|| "?".into()),
                None => low,
            }
        })
        .collect();
    let mut close = vec![usize::MAX; t.len()];
    let mut stack = vec![];
    for (i, x) in t.iter().enumerate() {
        match x.as_str() {
            "(" => stack.push(i),
            ")" => {
                if let Some(o) = stack.pop() {
                    close[o] = i;
                }
            }
            _ => {}
        }
    }
    let mut a = Analyzer { t: &t, close, ctes: vec![], flow: Flow::default(), is_var };
    // Every statement of a script (`CREATE …; INSERT …; DELETE …`).
    let mut start = 0;
    for (i, x) in t.iter().enumerate().chain(std::iter::once((t.len(), &";".to_string()))) {
        if x == ";" {
            if start < i {
                a.query(start, i, "", true);
            }
            start = i + 1;
        }
    }
    Some(a.flow)
}

/// A column's origin: base `table.column`s, or the literal text it is set to.
type Sources = BTreeSet<String>;

#[derive(Clone)]
enum Src {
    Table(String),
    Cte(String),
    /// A subquery or set-returning function, by its output columns (a function has one: `*`).
    Derived(Vec<(String, Sources)>),
}

struct Analyzer<'t> {
    t: &'t [String],
    close: Vec<usize>,
    /// CTEs in scope, innermost last: name → output columns.
    ctes: Vec<HashMap<String, Vec<(String, Sources)>>>,
    flow: Flow,
    is_var: &'t dyn Fn(&str) -> bool,
}

const CLAUSES: &[&str] = &["select", "from", "where", "group", "having", "order", "limit", "offset", "window", "for", "returning", "set", "using", "on"];
const JOIN_WORDS: &[&str] = &["join", "left", "right", "full", "inner", "cross", "outer", "natural", "lateral"];
const SET_OPS: &[&str] = &["union", "except", "intersect"];
const KEYWORDS: &[&str] = &[
    "and",
    "or",
    "not",
    "null",
    "is",
    "true",
    "false",
    "case",
    "when",
    "then",
    "else",
    "end",
    "as",
    "distinct",
    "on",
    "in",
    "exists",
    "between",
    "like",
    "ilike",
    "any",
    "all",
    "some",
    "interval",
    "date",
    "timestamp",
    "timestamptz",
    "time",
    "cast",
    "array",
    "current_date",
    "current_timestamp",
    "desc",
    "asc",
    "nulls",
    "first",
    "last",
    "over",
    "partition",
    "by",
    "filter",
    "within",
    "group",
    "order",
    "from",
    "select",
    "where",
    "limit",
    "default",
    "excluded",
    "row",
    "rows",
    "unbounded",
    "preceding",
    "following",
    "current",
    "zone",
    "at",
    "similar",
    "escape",
    "collate",
    "text",
];

impl Analyzer<'_> {
    fn at(&self, i: usize) -> &str {
        self.t.get(i).map(String::as_str).unwrap_or("")
    }

    /// Top-level (depth 0 within `lo..hi`) positions of tokens satisfying `f`.
    fn top(&self, lo: usize, hi: usize, f: &dyn Fn(&str) -> bool) -> Vec<usize> {
        let mut out = vec![];
        let mut i = lo;
        while i < hi {
            if self.t[i] == "(" && self.close[i] != usize::MAX {
                i = self.close[i] + 1;
                continue;
            }
            if f(&self.t[i]) {
                out.push(i);
            }
            i += 1;
        }
        out
    }

    /// Split `lo..hi` at top-level separators.
    fn split(&self, lo: usize, hi: usize, f: &dyn Fn(&str) -> bool) -> Vec<(usize, usize)> {
        let mut out = vec![];
        let mut start = lo;
        for p in self.top(lo, hi, f) {
            out.push((start, p));
            start = p + 1;
        }
        out.push((start, hi));
        out.into_iter().filter(|(a, b)| a < b).collect()
    }

    fn text(&self, lo: usize, hi: usize) -> String {
        if lo >= hi || hi > self.t.len() {
            return String::new();
        }
        // A call reads `f(x)`, a keyword `in (…)`.
        let mut s = String::new();
        for (k, x) in self.t[lo..hi].iter().enumerate() {
            let prev = if k > 0 { self.t[lo + k - 1].as_str() } else { "" };
            if k > 0 && !(x == "(" && is_ident(prev) && !KEYWORDS.contains(&prev) && !matches!(prev, "and" | "or" | "not" | "in")) {
                s.push(' ');
            }
            s.push_str(x);
        }
        s.replace("( ", "(").replace(" )", ")").replace(" ,", ",").replace(" .", ".").replace(". ", ".")
    }

    fn cte(&self, name: &str) -> Option<&Vec<(String, Sources)>> {
        self.ctes.iter().rev().find_map(|f| f.get(name))
    }

    fn wrapped(&self, lo: usize, hi: usize) -> bool {
        hi > lo + 1 && self.t[lo] == "(" && self.close[lo] == hi - 1
    }

    /// A query or statement; returns its output columns. `record` says whether its row
    /// conditions are the statement's own (not those of a subquery inside an expression).
    fn query(&mut self, lo: usize, hi: usize, scope: &str, record: bool) -> Vec<(String, Sources)> {
        if lo >= hi {
            return vec![];
        }
        if self.wrapped(lo, hi) {
            return self.query(lo + 1, hi - 1, scope, record);
        }
        let mut i = lo;
        let mut pushed = false;
        if self.t[i] == "with" {
            self.ctes.push(HashMap::new());
            pushed = true;
            i += 1;
            if self.at(i) == "recursive" {
                i += 1;
            }
            loop {
                let name = self.at(i).trim_matches('"').to_string();
                i += 1;
                let mut names: Vec<String> = vec![];
                if self.at(i) == "(" {
                    let c = self.close[i];
                    names = self.t[i + 1..c].iter().filter(|x| *x != ",").map(|x| x.trim_matches('"').to_string()).collect();
                    i = c + 1;
                }
                if self.at(i) == "as" {
                    i += 1;
                }
                while matches!(self.at(i), "not" | "materialized") {
                    i += 1;
                }
                if self.at(i) != "(" || self.close[i] == usize::MAX {
                    break;
                }
                let c = self.close[i];
                let mut cols = self.query(i + 1, c, &name, record);
                for (k, n) in names.into_iter().enumerate() {
                    if let Some(col) = cols.get_mut(k) {
                        col.0 = n;
                    }
                }
                self.ctes.last_mut().expect("pushed").insert(name, cols);
                i = c + 1;
                if self.at(i) == "," {
                    i += 1;
                    continue;
                }
                break;
            }
        }
        let out = match self.at(i) {
            "select" | "values" | "(" => self.union(i, hi, scope, record),
            "insert" | "upsert" | "replace" => self.insert(i, hi, record),
            "update" => self.update(i, hi, record),
            "delete" => self.delete(i, hi, record),
            "merge" => {
                let into = self.top(i, hi, &|x| x == "into").first().map(|p| p + 1);
                if let Some(p) = into {
                    self.flow.writes.insert(clean(self.at(p)));
                }
                if let Some(u) = self.top(i, hi, &|x| x == "using").first() {
                    self.sources(u + 1, hi, "", false);
                }
                vec![]
            }
            "truncate" => {
                for x in &self.t[i + 1..hi] {
                    if !matches!(x.as_str(), "table" | "only" | "," | "restart" | "identity" | "cascade" | "continue" | "restrict") {
                        self.flow.writes.insert(clean(x));
                    }
                }
                vec![]
            }
            "perform" => self.union(i, hi, scope, record),
            "create" | "alter" | "drop" => {
                self.ddl(i, hi);
                vec![]
            }
            _ => vec![],
        };
        if pushed {
            self.ctes.pop();
        }
        out
    }

    /// `SELECT … UNION [ALL] SELECT …`: columns named by the first arm, sources merged by position.
    fn union(&mut self, lo: usize, hi: usize, scope: &str, record: bool) -> Vec<(String, Sources)> {
        let arms = self.split(lo, hi, &|x| SET_OPS.contains(&x));
        let mut out: Vec<(String, Sources)> = vec![];
        for (a, b) in arms {
            let a = if matches!(self.at(a), "all" | "distinct") { a + 1 } else { a };
            // A trailing ORDER BY / LIMIT of the whole union sits in the last arm: harmless.
            let cols = if self.wrapped(a, b) { self.query(a + 1, b - 1, scope, record) } else { self.core(a, b, scope, record) };
            if out.is_empty() {
                out = cols;
            } else {
                for (k, (_, s)) in cols.into_iter().enumerate() {
                    if let Some(o) = out.get_mut(k) {
                        o.1.extend(s);
                    }
                }
            }
        }
        out
    }

    /// Positions of the top-level clause keywords of one SELECT/UPDATE/DELETE core.
    fn clauses(&self, lo: usize, hi: usize) -> Vec<(usize, String)> {
        self.top(lo, hi, &|x| CLAUSES.contains(&x))
            .into_iter()
            .filter(|&p| {
                // `on` belongs to FROM (join conditions, DISTINCT ON), `for` only as FOR UPDATE.
                let x = self.t[p].as_str();
                !(x == "on"
                    || (x == "for" && !matches!(self.at(p + 1), "update" | "share" | "no" | "key"))
                    || (x == "order" && self.at(p + 1) != "by")
                    || (x == "group" && self.at(p + 1) != "by"))
            })
            .map(|p| (p, self.t[p].clone()))
            .collect()
    }

    fn clause(&self, cl: &[(usize, String)], name: &str, hi: usize) -> Option<(usize, usize)> {
        let k = cl.iter().position(|(_, n)| n == name)?;
        let start = cl[k].0 + 1;
        let end = cl.get(k + 1).map(|(p, _)| *p).unwrap_or(hi);
        Some((start, end))
    }

    fn core(&mut self, lo: usize, hi: usize, scope: &str, record: bool) -> Vec<(String, Sources)> {
        if self.at(lo) == "values" {
            return vec![];
        }
        let cl = self.clauses(lo, hi);
        let from = self.clause(&cl, "from", hi);
        let srcs = match from {
            Some((a, b)) => self.sources(a, b, scope, record),
            None => vec![],
        };
        let scope = if scope.is_empty() { first_table(&srcs) } else { scope.to_string() };
        if let Some((a, b)) = self.clause(&cl, "where", hi) {
            self.conditions(a, b, &scope, &srcs, record);
        }
        if let Some((a, b)) = self.clause(&cl, "having", hi) {
            self.subqueries(a, b);
        }
        let Some((mut a, b)) = self.clause(&cl, "select", hi) else { return vec![] };
        if self.at(a) == "distinct" {
            a += 1;
            if self.at(a) == "on" && self.at(a + 1) == "(" {
                a = self.close[a + 1] + 1;
            }
        } else if self.at(a) == "all" {
            a += 1;
        }
        let mut cols = vec![];
        for (x, y) in self.split(a, b, &|t| t == ",") {
            self.subqueries(x, y);
            let (expr_hi, alias) = match self.top(x, y, &|t| t == "as").last() {
                Some(&p) => (p, Some(self.at(p + 1).trim_matches('"').to_string())),
                None if y - x >= 2 && is_ident(self.at(y - 1)) && !is_operator(self.at(y - 2)) && !KEYWORDS.contains(&self.at(y - 1)) => {
                    (y - 1, Some(self.at(y - 1).trim_matches('"').to_string()))
                }
                None => (y, None),
            };
            let name = alias.unwrap_or_else(|| {
                let last = strip_cast(self.at(expr_hi.saturating_sub(1)));
                last.rsplit('.').next().unwrap_or(&last).trim_matches('"').to_string()
            });
            if self.at(x) == "*" || self.at(x).ends_with(".*") {
                // `SELECT *` / `t.*`: every column of the sources, by name.
                for s in &srcs {
                    if let (_, Src::Cte(n)) = s {
                        if let Some(cc) = self.cte(n) {
                            cols.extend(cc.clone());
                        }
                    }
                }
                continue;
            }
            let src = self.resolve(x, expr_hi, &srcs);
            cols.push((name, src));
        }
        cols
    }

    /// FROM items and joins: the sources by alias, recording tables read and join conditions.
    fn sources(&mut self, lo: usize, hi: usize, scope: &str, record: bool) -> Vec<(String, Src)> {
        let mut out: Vec<(String, Src)> = vec![];
        let mut i = lo;
        while i < hi {
            // Join words and commas between items.
            let mut kind = vec![];
            while i < hi && (JOIN_WORDS.contains(&self.at(i)) || self.at(i) == ",") {
                if self.at(i) != "," && self.at(i) != "lateral" && self.at(i) != "outer" {
                    kind.push(self.at(i).to_string());
                }
                i += 1;
            }
            if i >= hi {
                break;
            }
            let (src, name, mut j) = if self.at(i) == "(" && self.close[i] != usize::MAX {
                let c = self.close[i];
                let cols = self.query(i + 1, c, "", record && false);
                (Src::Derived(cols), String::new(), c + 1)
            } else if self.at(i + 1) == "(" && self.close[i + 1] != usize::MAX {
                // A set-returning function: its value comes from its arguments.
                let c = self.close[i + 1];
                self.subqueries(i + 2, c);
                let args = self.resolve(i + 2, c, &out);
                (Src::Derived(vec![("*".into(), args)]), self.at(i).to_string(), c + 1)
            } else {
                let n = clean(self.at(i));
                let src = if self.cte(&n).is_some() {
                    Src::Cte(n.clone())
                } else {
                    self.flow.reads.insert(n.clone());
                    Src::Table(n.clone())
                };
                (src, n, i + 1)
            };
            // Alias, optionally with `AS`, optionally with a column list.
            if self.at(j) == "as" {
                j += 1;
            }
            let mut alias = name.rsplit('.').next().unwrap_or(&name).to_string();
            if j < hi && is_ident(self.at(j)) && !KEYWORDS.contains(&self.at(j)) && !JOIN_WORDS.contains(&self.at(j)) && self.at(j) != "using" {
                alias = self.at(j).trim_matches('"').to_string();
                j += 1;
                if self.at(j) == "(" && self.close[j] != usize::MAX {
                    j = self.close[j] + 1;
                }
            }
            let joined = !kind.is_empty();
            out.push((alias, src));
            // `ON cond` / `USING (cols)` up to the next join or comma.
            let next = (j..hi).find(|&p| {
                let x = self.at(p);
                (x == "," || JOIN_WORDS.contains(&x)) && self.depth_eq(j, p)
            });
            let end = next.unwrap_or(hi);
            if self.at(j) == "on" {
                self.subqueries(j + 1, end);
                if record && joined {
                    let what = format!("{} {}", kind.join(" "), self.text(i, j).split(" as ").next().unwrap_or(""));
                    let label = if scope.is_empty() { first_table(&out) } else { scope.to_string() };
                    self.flow.filters.push(format!("{label}: {} on {}", what.trim(), self.text(j + 1, end)));
                }
            }
            i = end;
        }
        out
    }

    /// Whether `p` is at the same parenthesis depth as `from`.
    fn depth_eq(&self, from: usize, p: usize) -> bool {
        let mut d = 0i32;
        for x in &self.t[from..p] {
            match x.as_str() {
                "(" => d += 1,
                ")" => d -= 1,
                _ => {}
            }
        }
        d == 0
    }

    /// WHERE conjuncts, recorded under `scope`; subqueries inside are read.
    fn conditions(&mut self, lo: usize, hi: usize, scope: &str, _srcs: &[(String, Src)], record: bool) {
        self.subqueries(lo, hi);
        if !record {
            return;
        }
        let mut between = false;
        let mut start = lo;
        let mut parts = vec![];
        for p in self.top(lo, hi, &|x| x == "and" || x == "between") {
            if self.t[p] == "between" {
                between = true;
                continue;
            }
            if between {
                between = false;
                continue;
            }
            parts.push((start, p));
            start = p + 1;
        }
        parts.push((start, hi));
        for (a, b) in parts {
            if a < b {
                let text = self.text(a, b);
                let text = match text.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
                    Some(inner) if !inner.contains('(') => inner.to_string(),
                    _ => text,
                };
                self.flow.filters.push(if scope.is_empty() { text } else { format!("{scope}: {text}") });
            }
        }
    }

    /// Subqueries inside an expression: what they read (their conditions belong to the expression).
    fn subqueries(&mut self, lo: usize, hi: usize) {
        let mut i = lo;
        while i < hi {
            if self.t[i] == "(" && self.close[i] != usize::MAX && self.close[i] <= hi {
                let c = self.close[i];
                if matches!(self.at(i + 1), "select" | "with") {
                    self.query(i + 1, c, "", false);
                } else {
                    self.subqueries(i + 1, c);
                }
                i = c + 1;
                continue;
            }
            i += 1;
        }
    }

    /// The base columns an expression's value comes from; a literal when there are none.
    fn resolve(&self, lo: usize, hi: usize, srcs: &[(String, Src)]) -> Sources {
        let mut out = Sources::new();
        let mut i = lo;
        while i < hi {
            let x = self.at(i);
            // `ORDER BY` inside an aggregate, a window (`OVER (…)`) or an aggregate `FILTER (…)`
            // decides order or which rows count, not where the value comes from.
            if x == "order" && self.at(i + 1) == "by" {
                let mut d = 0i32;
                while i < hi {
                    match self.at(i) {
                        "(" => d += 1,
                        ")" if d == 0 => break,
                        ")" => d -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                continue;
            }
            if matches!(x, "over" | "filter") && self.at(i + 1) == "(" && self.close[i + 1] != usize::MAX {
                i = self.close[i + 1] + 1;
                continue;
            }
            // Nested subquery: its first column.
            if x == "(" && matches!(self.at(i + 1), "select" | "with") && self.close[i] != usize::MAX {
                i = self.close[i] + 1;
                continue;
            }
            let next = self.at(i + 1);
            if !is_ident(x) || next == "(" || KEYWORDS.contains(&x) {
                i += 1;
                continue;
            }
            let name = strip_cast(x);
            let (qual, col) = match name.rsplit_once('.') {
                Some((q, c)) => (Some(q.rsplit('.').next().unwrap_or(q).to_string()), c.to_string()),
                None => (None, name.clone()),
            };
            let col = col.trim_matches('"').to_string();
            let lookup = |s: &Src| -> Option<Sources> {
                match s {
                    Src::Table(t) => Some(BTreeSet::from([format!("{t}.{col}")])),
                    Src::Cte(n) => self.cte(n).and_then(|cols| cols.iter().find(|(c, _)| *c == col).map(|(_, s)| s.clone())),
                    Src::Derived(cols) => cols.iter().find(|(c, _)| *c == col || c == "*").map(|(_, s)| s.clone()),
                }
            };
            let found = match &qual {
                Some(q) => srcs.iter().find(|(a, _)| a == q).and_then(|(_, s)| lookup(s)),
                None if (self.is_var)(&col) => None,
                None => {
                    // A source alias used as a value (`lbl` from `regexp_split_to_table(…) AS lbl`).
                    if let Some((_, Src::Derived(cols))) = srcs.iter().find(|(a, _)| *a == col) {
                        cols.first().map(|(_, s)| s.clone())
                    } else {
                        let derived: Vec<Sources> = srcs.iter().filter(|(_, s)| !matches!(s, Src::Table(_))).filter_map(|(_, s)| lookup(s)).collect();
                        let tables: Vec<&String> = srcs.iter().filter_map(|(_, s)| if let Src::Table(t) = s { Some(t) } else { None }).collect();
                        match (derived.into_iter().next(), tables.as_slice()) {
                            (Some(s), _) => Some(s),
                            (None, [t]) => Some(BTreeSet::from([format!("{t}.{col}")])),
                            _ => None,
                        }
                    }
                }
            };
            match found {
                Some(s) => out.extend(s),
                // A variable or parameter (`v_started`, `p_default_date`).
                None if qual.is_none() => {
                    out.insert(col);
                }
                None => {
                    out.insert(name);
                }
            }
            i += 1;
        }
        if out.is_empty() && lo < hi {
            out.insert(self.text(lo, hi));
        }
        out
    }

    fn insert(&mut self, lo: usize, hi: usize, record: bool) -> Vec<(String, Sources)> {
        let mut i = lo + 1;
        if self.at(i) == "into" {
            i += 1;
        }
        let table = clean(self.at(i));
        self.flow.writes.insert(table.clone());
        i += 1;
        if self.at(i) == "as" {
            i += 2;
        }
        let mut names: Vec<String> = vec![];
        if self.at(i) == "(" && !matches!(self.at(i + 1), "select" | "with") {
            let c = self.close[i];
            names = self.t[i + 1..c].iter().filter(|x| *x != ",").map(|x| x.trim_matches('"').to_string()).collect();
            i = c + 1;
        }
        // `ON CONFLICT` / `RETURNING` end the source query (a join's `ON` does not).
        let end =
            self.top(i, hi, &|x| x == "on" || x == "returning").into_iter().find(|&p| self.t[p] == "returning" || self.at(p + 1) == "conflict").unwrap_or(hi);
        if self.at(i) == "values" {
            for (a, b) in self.split(i + 1, end, &|x| x == ",") {
                if self.wrapped(a, b) {
                    for (k, (x, y)) in self.split(a + 1, b - 1, &|t| t == ",").into_iter().enumerate() {
                        if let Some(n) = names.get(k) {
                            let v = self.text(x, y);
                            self.flow.columns.entry(format!("{table}.{n}")).or_insert(v);
                        }
                    }
                }
            }
            return vec![];
        }
        // The source query's conditions are about the rows it reads, not the target.
        let cols = self.query(i, end, "", record);
        for (k, (n, s)) in cols.into_iter().enumerate() {
            let name = names.get(k).cloned().unwrap_or(n);
            self.flow.columns.insert(format!("{table}.{name}"), render_sources(&s));
        }
        vec![]
    }

    fn update(&mut self, lo: usize, hi: usize, record: bool) -> Vec<(String, Sources)> {
        let mut i = lo + 1;
        if self.at(i) == "only" {
            i += 1;
        }
        let table = clean(self.at(i));
        self.flow.writes.insert(table.clone());
        let mut srcs = vec![(table.clone(), Src::Table(table.clone()))];
        if is_ident(self.at(i + 1)) && self.at(i + 1) != "set" {
            srcs[0].0 = self.at(if self.at(i + 1) == "as" { i + 2 } else { i + 1 }).to_string();
        }
        let cl = self.clauses(i, hi);
        if let Some((a, b)) = self.clause(&cl, "from", hi) {
            let more = self.sources(a, b, &table, record);
            srcs.extend(more);
        }
        if let Some((a, b)) = self.clause(&cl, "set", hi) {
            for (x, y) in self.split(a, b, &|t| t == ",") {
                if self.at(x + 1) == "=" {
                    self.subqueries(x + 2, y);
                    let col = strip_cast(self.at(x));
                    let col = col.rsplit('.').next().unwrap_or(&col).to_string();
                    let s = self.resolve(x + 2, y, &srcs);
                    self.flow.columns.insert(format!("{table}.{col}"), render_sources(&s));
                }
            }
        }
        if let Some((a, b)) = self.clause(&cl, "where", hi) {
            self.conditions(a, b, &table, &srcs, record);
        }
        vec![]
    }

    fn delete(&mut self, lo: usize, hi: usize, record: bool) -> Vec<(String, Sources)> {
        let mut i = lo + 1;
        if self.at(i) == "from" {
            i += 1;
        }
        if self.at(i) == "only" {
            i += 1;
        }
        let table = clean(self.at(i));
        self.flow.writes.insert(table.clone());
        let mut srcs = vec![(table.clone(), Src::Table(table.clone()))];
        let cl = self.clauses(i, hi);
        if let Some((a, b)) = self.clause(&cl, "using", hi) {
            let more = self.sources(a, b, &table, record);
            srcs.extend(more);
        }
        if let Some((a, b)) = self.clause(&cl, "where", hi) {
            self.conditions(a, b, &table, &srcs, record);
        }
        vec![]
    }

    /// CREATE / ALTER / DROP: what kind of object and which.
    fn ddl(&mut self, lo: usize, hi: usize) {
        let words: Vec<&str> = self.t[lo..hi].iter().map(String::as_str).collect();
        let skip = ["or", "replace", "unique", "temporary", "temp", "unlogged", "materialized", "concurrently", "if", "not", "exists", "only"];
        let verb = words[0];
        let mut k = 1;
        while k < words.len() && skip.contains(&words[k]) {
            k += 1;
        }
        let Some(kind) = words.get(k) else { return };
        k += 1;
        while k < words.len() && skip.contains(&words[k]) {
            k += 1;
        }
        let object = words.get(k).map(|w| clean(w)).unwrap_or_default();
        // An index is about its table: `CREATE INDEX i ON t (…)`.
        let object = match (*kind, words.iter().position(|w| *w == "on")) {
            ("index", Some(p)) if verb == "create" => format!("{object} on {}", clean(words.get(p + 1).copied().unwrap_or(""))),
            _ => object,
        };
        self.flow.ddl.push((format!("{verb} {kind}"), object));
    }
}

fn first_table(srcs: &[(String, Src)]) -> String {
    srcs.iter()
        .find_map(|(a, s)| match s {
            Src::Table(t) | Src::Cte(t) => Some(t.clone()),
            Src::Derived(_) => (!a.is_empty()).then(|| a.clone()),
        })
        .unwrap_or_default()
}

fn render_sources(s: &Sources) -> String {
    if s.len() == 1 {
        s.iter().next().cloned().unwrap_or_default()
    } else {
        format!("{{{}}}", s.iter().cloned().collect::<Vec<_>>().join(", "))
    }
}

fn clean(x: &str) -> String {
    x.trim_matches(['"', '`', '[', ']']).replace('"', "")
}

fn strip_cast(x: &str) -> String {
    x.split("::").next().unwrap_or(x).to_string()
}

fn is_ident(x: &str) -> bool {
    x.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '"') && !x.starts_with('\'')
}

fn is_operator(x: &str) -> bool {
    matches!(x, "=" | "<" | ">" | "<=" | ">=" | "<>" | "!=" | "+" | "-" | "*" | "/" | "%" | "||" | "," | "(" | "and" | "or" | "not" | "then" | "else" | "when")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(sql: &str) -> Flow {
        analyze(sql, &|_| None).unwrap()
    }

    #[test]
    fn lineage_through_ctes_unions_and_lateral() {
        let before = "INSERT INTO stage (item_id, stack_number)
            WITH latest AS (SELECT sku_id AS item_id, remarks AS stack_number FROM inventory_count WHERE deleted_at IS NULL)
            SELECT item_id, lc.stack_number FROM latest lc";
        let f = flow(before);
        assert_eq!(f.columns["stage.stack_number"], "inventory_count.remarks");
        assert_eq!(f.reads, BTreeSet::from(["inventory_count".to_string()]));
        assert_eq!(f.filters, ["latest: deleted_at is null"]);

        let after = "INSERT INTO stage (item_id, stack_number)
            WITH src AS (
                SELECT sku_id AS item_id, remarks AS raw FROM inventory_count WHERE deleted_at IS NULL
                UNION ALL
                SELECT pvv.zoho_item_id::text, g.stack_numbers FROM grn_line_items g
                  JOIN inventory_bill_items ibi ON ibi.id = g.inventory_bill_item_id
                  JOIN product_variants pvv ON pvv.id = ibi.variant_id
                 WHERE g.deleted_at IS NULL
                UNION ALL
                SELECT bi.item_id, bi.stack_numbers FROM bill_items bi JOIN bills b ON b.bill_id = bi.bill_id WHERE b.status <> 'void'
            ),
            labels AS (
                SELECT ss.item_id, btrim(lbl) AS label FROM src ss, LATERAL regexp_split_to_table(ss.raw, '[,;/|]') AS lbl
                 WHERE lower(btrim(lbl)) NOT IN ('na', 'n/a')
            ),
            agg AS (SELECT item_id, string_agg(label, ', ') AS stack_number FROM labels GROUP BY item_id)
            SELECT a.item_id, sa.stack_number FROM agg a LEFT JOIN agg sa ON sa.item_id = a.item_id";
        let f = flow(after);
        assert_eq!(f.columns["stage.stack_number"], "{bill_items.stack_numbers, grn_line_items.stack_numbers, inventory_count.remarks}");
        assert_eq!(
            f.reads,
            ["bill_items", "bills", "grn_line_items", "inventory_bill_items", "inventory_count", "product_variants"].iter().map(|s| s.to_string()).collect()
        );
        assert!(f.filters.contains(&"src: b.status <> 'void'".to_string()), "{:?}", f.filters);
        assert!(f.filters.contains(&"src: join inventory_bill_items ibi on ibi.id = g.inventory_bill_item_id".to_string()), "{:?}", f.filters);
        assert!(f.filters.contains(&"labels: lower(btrim(lbl)) not in ('na', 'n/a')".to_string()), "{:?}", f.filters);
        assert_eq!(f.writes, BTreeSet::from(["stage".to_string()]));
    }

    #[test]
    fn every_statement_of_a_script() {
        let f = flow("CREATE INDEX i ON t (a); INSERT INTO tomb (id) SELECT id FROM entries WHERE status = 'removed' AND feed_id IN (SELECT id FROM feeds); DELETE FROM entries WHERE status = 'removed'");
        assert!(f.filters.contains(&"entries: feed_id in (select id from feeds)".to_string()), "{:?}", f.filters);
        assert_eq!(f.writes, ["entries", "tomb"].iter().map(|s| s.to_string()).collect());
        assert!(f.reads.contains("feeds"));
    }

    #[test]
    fn updates_deletes_and_ddl() {
        let f = analyze("UPDATE orders SET status = $1, total = total + 1 WHERE id = $2", &|n| (n == 1).then(|| "CANCELLED".into())).unwrap();
        assert_eq!(f.columns["orders.status"], "'CANCELLED'");
        assert_eq!(f.columns["orders.total"], "orders.total");
        assert_eq!(f.filters, ["orders: id = ?"]);
        let f = flow("CREATE INDEX IF NOT EXISTS i ON inventory_live_stock (item_id)");
        assert_eq!(f.ddl, [("create index".to_string(), "i on inventory_live_stock".to_string())]);
        let f = flow("DELETE FROM sessions s USING users u WHERE u.id = s.user_id AND u.disabled");
        assert_eq!(f.writes, BTreeSet::from(["sessions".to_string()]));
        assert_eq!(f.reads, BTreeSet::from(["users".to_string()]));
    }
}
