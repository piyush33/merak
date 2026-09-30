//! SQL front end: `.sql` files → `merak_ir` structural IR.
//!
//! Each file is a module. `CREATE FUNCTION` / `PROCEDURE` / `VIEW` are entities; the file's
//! other statements (a migration, a setup script) are its `<script>` entity. Every SQL
//! statement becomes a call to `$sql("…")`, which the behaviour layer reads for tables,
//! row conditions and column lineage. plpgsql control flow (`IF`, loops, `RAISE EXCEPTION`,
//! `RETURN QUERY`) becomes IR control flow. Tables (`CREATE TABLE`, `ALTER TABLE`, indexes)
//! are classes whose field tags are the column definitions, so they are compared as schemas.

use merak_ir as ir;
use std::collections::BTreeMap;

pub fn is_supported(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".sql")
}

/// Larger files are data dumps, not behaviour.
const MAX_BYTES: usize = 1 << 20;

pub fn lower_program(files: &BTreeMap<String, String>) -> ir::Program {
    let mut program = ir::Program::default();
    for (path, text) in files.iter().filter(|(p, t)| is_supported(p) && t.len() <= MAX_BYTES) {
        program.modules.insert(path.clone(), lower_module(path, text));
    }
    program
}

pub fn lower_module(path: &str, text: &str) -> ir::Module {
    let mut lw =
        Lowerer { path: path.to_string(), code: blank_comments(text), module: ir::Module { path: path.to_string(), ..Default::default() }, lines: vec![] };
    lw.lines = line_starts(&lw.code);
    let mut script = vec![];
    for (start, end) in split(&lw.code, 0, lw.code.len()) {
        lw.top(start, end, &mut script);
    }
    if !script.is_empty() {
        let first = script.first().map(stmt_line).unwrap_or(1);
        let last = script.last().map(stmt_line).unwrap_or(first);
        lw.push_function("<script>", vec![], None, script, first, last);
    }
    lw.module
}

struct Lowerer {
    path: String,
    /// The file with comments blanked out (offsets and lines preserved).
    code: String,
    module: ir::Module,
    lines: Vec<usize>,
}

impl Lowerer {
    fn loc(&self, offset: usize) -> ir::Loc {
        let line = match self.lines.binary_search(&offset) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        ir::Loc { file: self.path.clone(), line: line as u32 }
    }

    fn sql(&self, text: &str, offset: usize) -> ir::Stmt {
        let loc = self.loc(offset);
        // Lines kept (each trimmed) so a condition can be traced to its line.
        let text: String = text.trim().trim_end_matches(';').lines().map(str::trim).collect::<Vec<_>>().join("\n");
        let call = ir::Call { callee: ir::Expr::Ident("$sql".into()), args: vec![ir::Expr::Str(text)], loc: loc.clone() };
        ir::Stmt::Expr(ir::Expr::Call(Box::new(call)), loc)
    }

    fn push_function(&mut self, name: &str, params: Vec<ir::Param>, ret: Option<String>, body: Vec<ir::Stmt>, line: u32, end_line: u32) {
        let body_hash = ir::body_hash(&body);
        self.module.functions.push(ir::Function {
            id: format!("{}::{name}", self.path),
            name: name.to_string(),
            kind: ir::FunctionKind::Function,
            class: None,
            parent: None,
            params,
            return_type: ret.map(|r| ir::TypeRef::Named(r, vec![])),
            body,
            is_async: false,
            exported: true,
            loc: ir::Loc { file: self.path.clone(), line },
            end_line,
            body_hash,
            decorators: vec![],
        });
    }

    /// One top-level statement.
    fn top(&mut self, start: usize, end: usize, script: &mut Vec<ir::Stmt>) {
        let text = self.code[start..end].trim().to_string();
        let at = start + (self.code[start..end].len() - self.code[start..end].trim_start().len());
        let words = words(&text, 8);
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        let create = w.first() == Some(&"create");
        let kind_at = if create { skip(&w, 1, &["or", "replace", "materialized", "temporary", "temp", "unlogged", "unique", "recursive"]) } else { 1 };
        match (w.first().copied(), w.get(kind_at).copied()) {
            (Some("begin" | "commit" | "rollback" | "end" | "set" | "reset" | "grant" | "revoke" | "comment" | "analyze" | "vacuum"), _) if !create => {}
            (Some("create"), Some("function" | "procedure")) => self.function(&text, at),
            (Some("create"), Some("view")) => self.view(&text, at),
            (Some("create"), Some("table")) => self.create_table(&text, at),
            (Some("create"), Some("index")) => self.create_index(&text, at),
            (Some("alter"), Some("table")) => self.alter_table(&text, at),
            (Some("drop"), Some("index")) => {
                let name = last_name(&words_after(&text, &["drop", "index", "concurrently", "if", "exists"]));
                for c in &mut self.module.classes {
                    c.fields.remove(&format!("index {name}"));
                    c.field_tags.remove(&format!("index {name}"));
                }
                script.push(self.sql(&text, at));
            }
            (Some("do"), _) => {
                if let Some((b, e)) = dollar_body(&self.code, at, end) {
                    let body = self.plpgsql(b, e);
                    script.extend(body);
                }
            }
            _ => script.push(self.sql(&text, at)),
        }
    }

    /// `CREATE [OR REPLACE] FUNCTION name(params) RETURNS t LANGUAGE l AS $$ body $$`.
    fn function(&mut self, text: &str, at: usize) {
        let Some(open) = text.find('(') else { return };
        let head = &text[..open];
        let name = last_name(head.split_whitespace().last().unwrap_or(""));
        let close = match_paren(text, open).unwrap_or(text.len());
        let params = text[open + 1..close]
            .split(',')
            .filter_map(|p| {
                let p = p.split(" default ").next().unwrap_or(p).split(":=").next().unwrap_or(p);
                let ws: Vec<&str> = p.split_whitespace().filter(|x| !matches!(x.to_lowercase().as_str(), "in" | "out" | "inout" | "variadic")).collect();
                match ws.as_slice() {
                    [] => None,
                    [t] => Some(ir::Param { name: "_".into(), ty: Some(ir::TypeRef::Named(t.to_string(), vec![])) }),
                    [n, t @ ..] => Some(ir::Param { name: n.to_string(), ty: Some(ir::TypeRef::Named(t.join(" "), vec![])) }),
                }
            })
            .collect();
        let rest = &text[close.min(text.len())..];
        let low = rest.to_lowercase();
        let ret = low.find("returns").map(|r| {
            let after = &rest[r + 7..];
            let stop = ["language", " as ", "\nas", " as\n", "$"].iter().filter_map(|k| after.to_lowercase().find(k)).min().unwrap_or(after.len());
            after[..stop].split_whitespace().collect::<Vec<_>>().join(" ")
        });
        let base = at + close;
        let body = match dollar_body(&self.code, base, at + text.len()) {
            Some((b, e)) => self.plpgsql(b, e),
            // `AS 'select …'`: a quoted body.
            None => match rest.find('\'') {
                Some(q) => vec![self.sql(rest[q + 1..].trim_end_matches(['\'', ';']), base + q + 1)],
                None => vec![],
            },
        };
        let line = self.loc(at).line;
        let end = self.loc(at + text.len()).line;
        self.push_function(&name, params, ret, body, line, end);
    }

    /// `CREATE [OR REPLACE] [MATERIALIZED] VIEW name AS query`.
    fn view(&mut self, text: &str, at: usize) {
        let low = text.to_lowercase();
        let Some(as_at) = find_word(&low, "as") else { return };
        let head = words_after(&text[..as_at], &["create", "or", "replace", "materialized", "view", "if", "not", "exists"]);
        let name = last_name(head.split(['(', ' ']).next().unwrap_or(&head));
        let query = &text[as_at + 2..];
        let body = vec![self.sql(query, at + as_at + 2)];
        let (line, end) = (self.loc(at).line, self.loc(at + text.len()).line);
        self.push_function(&name, vec![], Some("view".into()), body, line, end);
    }

    fn table(&mut self, name: &str, at: usize) -> &mut ir::Class {
        if !self.module.classes.iter().any(|c| c.name == name) {
            let loc = self.loc(at);
            self.module.classes.push(ir::Class {
                id: format!("{}::{name}", self.path),
                name: name.to_string(),
                extends: None,
                fields: BTreeMap::new(),
                methods: vec![],
                field_tags: BTreeMap::new(),
                exported: true,
                loc,
                decorators: vec![],
            });
        }
        self.module.classes.iter_mut().find(|c| c.name == name).expect("just ensured")
    }

    fn column(&mut self, table: &str, def: &str, at: usize) {
        let def = def.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut parts = def.splitn(2, ' ');
        let (Some(col), rest) = (parts.next(), parts.next().unwrap_or("")) else { return };
        let col = col.trim_matches('"').to_string();
        let ty = rest.split_whitespace().next().unwrap_or("").to_string();
        let c = self.table(table, at);
        c.fields.insert(col.clone(), ir::TypeRef::Named(ty, vec![]));
        c.field_tags.insert(col, rest.to_lowercase());
    }

    /// `CREATE TABLE [IF NOT EXISTS] name (col type …, constraint …)`.
    fn create_table(&mut self, text: &str, at: usize) {
        let Some(open) = text.find('(') else { return };
        let name = last_name(&words_after(&text[..open], &["create", "temporary", "temp", "unlogged", "table", "if", "not", "exists"]));
        let close = match_paren(text, open).unwrap_or(text.len());
        self.table(&name, at);
        for def in split_top(&text[open + 1..close], ',') {
            let first = def.split_whitespace().next().unwrap_or("").to_lowercase();
            if matches!(first.as_str(), "constraint" | "primary" | "unique" | "foreign" | "check" | "exclude" | "like") {
                let d = def.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
                let c = self.table(&name, at);
                c.fields.insert(format!("constraint {d}"), ir::TypeRef::Named("constraint".into(), vec![]));
                c.field_tags.insert(format!("constraint {d}"), d);
            } else {
                self.column(&name, &def, at);
            }
        }
    }

    /// `ALTER TABLE t ADD COLUMN c type, DROP COLUMN d, ALTER COLUMN e TYPE …`.
    fn alter_table(&mut self, text: &str, at: usize) {
        let rest = words_after(text, &["alter", "table", "if", "exists", "only"]);
        let (name, actions) = rest.split_once(char::is_whitespace).unwrap_or((&rest, ""));
        let name = last_name(name);
        for action in split_top(actions, ',') {
            let a = action.trim();
            let low = a.to_lowercase();
            if low.starts_with("add") && !low.starts_with("add constraint") {
                let def = words_after(a, &["add", "column", "if", "not", "exists"]);
                self.column(&name, &def, at);
            } else if low.starts_with("drop column") || (low.starts_with("drop") && !low.starts_with("drop constraint")) {
                let col = words_after(a, &["drop", "column", "if", "exists"]);
                let col = col.split_whitespace().next().unwrap_or("").trim_matches('"').to_string();
                let c = self.table(&name, at);
                c.fields.remove(&col);
                c.field_tags.remove(&col);
            } else {
                let d = a.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
                let c = self.table(&name, at);
                c.fields.insert(format!("alter {d}"), ir::TypeRef::Named("alter".into(), vec![]));
                c.field_tags.insert(format!("alter {d}"), d);
            }
        }
    }

    /// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON table (cols) [WHERE …]`: part of the table.
    fn create_index(&mut self, text: &str, at: usize) {
        let low = text.to_lowercase();
        let Some(on) = find_word(&low, "on") else { return };
        let name = last_name(&words_after(&text[..on], &["create", "unique", "index", "concurrently", "if", "not", "exists"]));
        let after = text[on + 2..].trim();
        let (table, def) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
        let unique = if low.starts_with("create unique") { "unique " } else { "" };
        let def = format!("{unique}{}", def.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase());
        let key = format!("index {name}");
        let c = self.table(&last_name(table), at);
        c.fields.insert(key.clone(), ir::TypeRef::Named("index".into(), vec![]));
        c.field_tags.insert(key, def);
    }

    /// A plpgsql (or SQL-function) body between byte offsets `b..e` of the file.
    fn plpgsql(&self, b: usize, e: usize) -> Vec<ir::Stmt> {
        enum Frame {
            If { branches: Vec<(Option<String>, Vec<ir::Stmt>)>, at: usize },
            Loop { iter: Option<ir::Expr>, body: Vec<ir::Stmt>, at: usize },
            Handler { branches: Vec<(String, Vec<ir::Stmt>)>, body: Vec<ir::Stmt>, at: usize },
        }
        fn current<'f>(frames: &'f mut [Frame], out: &'f mut Vec<ir::Stmt>) -> &'f mut Vec<ir::Stmt> {
            match frames.last_mut() {
                Some(Frame::If { branches, .. }) => &mut branches.last_mut().expect("a branch").1,
                Some(Frame::Loop { body, .. }) => body,
                Some(Frame::Handler { branches, body, .. }) => match branches.last_mut() {
                    Some((_, b)) => b,
                    None => body,
                },
                None => out,
            }
        }
        let mut out = vec![];
        let mut frames: Vec<Frame> = vec![];
        let mut declaring = false;
        for (s, t) in split(&self.code, b, e) {
            let mut text = self.code[s..t].trim();
            let mut at = s + (self.code[s..t].len() - self.code[s..t].trim_start().len());
            // Statements can follow block keywords in the same piece: `BEGIN UPDATE …`.
            loop {
                if text.is_empty() {
                    break;
                }
                let low = text.to_lowercase();
                let first = low.split_whitespace().next().unwrap_or("").to_string();
                let rest_after = |n: usize| -> (&str, usize) {
                    let mut idx = 0;
                    for _ in 0..n {
                        let trimmed = &text[idx..];
                        let lead = trimmed.len() - trimmed.trim_start().len();
                        let word_end = trimmed[lead..].find(char::is_whitespace).map(|p| p + lead).unwrap_or(trimmed.len());
                        idx += word_end;
                    }
                    let r = &text[idx..];
                    let lead = r.len() - r.trim_start().len();
                    (r.trim(), idx + lead)
                };
                match first.as_str() {
                    "declare" => {
                        declaring = true;
                        let (r, k) = rest_after(1);
                        text = r;
                        at += k;
                        continue;
                    }
                    "begin" => {
                        declaring = false;
                        let (r, k) = rest_after(1);
                        text = r;
                        at += k;
                        continue;
                    }
                    // `v_rows integer;`: a local variable, not a column.
                    _ if declaring => {
                        if !first.is_empty() && first != "begin" {
                            let loc = self.loc(at);
                            current(&mut frames, &mut out).push(ir::Stmt::Let { name: first.trim_matches('"').to_string(), ty: None, init: None, loc });
                        }
                        break;
                    }
                    "if" | "elsif" | "elseif" => {
                        let Some(then) = find_word(&low, "then") else { break };
                        let cond = text[first.len()..then].trim().to_string();
                        if first == "if" {
                            frames.push(Frame::If { branches: vec![(Some(cond), vec![])], at });
                        } else if let Some(Frame::If { branches, .. }) = frames.last_mut() {
                            branches.push((Some(cond), vec![]));
                        }
                        let r = text[then + 4..].trim_start();
                        at += text.len() - r.len();
                        text = r;
                        continue;
                    }
                    "else" => {
                        if let Some(Frame::If { branches, .. }) = frames.last_mut() {
                            branches.push((None, vec![]));
                        }
                        let (r, k) = rest_after(1);
                        text = r;
                        at += k;
                        continue;
                    }
                    "exception" => {
                        let body = std::mem::take(current(&mut frames, &mut out));
                        frames.push(Frame::Handler { branches: vec![], body, at });
                        let (r, k) = rest_after(1);
                        text = r;
                        at += k;
                        continue;
                    }
                    "when" if matches!(frames.last(), Some(Frame::Handler { .. })) => {
                        let Some(then) = find_word(&low, "then") else { break };
                        if let Some(Frame::Handler { branches, .. }) = frames.last_mut() {
                            branches.push((text[4..then].trim().to_string(), vec![]));
                        }
                        let r = text[then + 4..].trim_start();
                        at += text.len() - r.len();
                        text = r;
                        continue;
                    }
                    "end" => {
                        let second = low.split_whitespace().nth(1).unwrap_or("");
                        let done = match (second, frames.last()) {
                            ("if", Some(Frame::If { .. })) | ("loop", Some(Frame::Loop { .. })) => frames.pop(),
                            ("" | "case", Some(Frame::Handler { .. })) => frames.pop(),
                            _ => None,
                        };
                        let stmt = match done {
                            Some(Frame::If { branches, at }) => {
                                let loc = self.loc(at);
                                let mut chain: Vec<ir::Stmt> = vec![];
                                for (cond, body) in branches.into_iter().rev() {
                                    chain = match cond {
                                        Some(c) => vec![ir::Stmt::If { test: ir::Expr::Opaque(normalise(&c)), then: body, otherwise: chain, loc: loc.clone() }],
                                        None => body,
                                    };
                                }
                                Some(ir::Stmt::Block(chain))
                            }
                            Some(Frame::Loop { iter, body, at }) => Some(ir::Stmt::Loop { binding: None, iter, body, loc: self.loc(at) }),
                            Some(Frame::Handler { branches, body, at }) => {
                                let handler = branches.into_iter().flat_map(|(_, b)| b).collect();
                                Some(ir::Stmt::Try { body, handler, finalizer: vec![], loc: self.loc(at) })
                            }
                            _ => None,
                        };
                        if let Some(st) = stmt {
                            current(&mut frames, &mut out).push(st);
                        }
                        break;
                    }
                    "for" | "foreach" | "while" | "loop" => {
                        let Some(lp) = find_word(&low, "loop") else { break };
                        // `FOR r IN SELECT … LOOP`: the query runs once.
                        let head = &text[..lp];
                        let iter = find_word(&head.to_lowercase(), "in").map(|i| head[i + 2..].trim()).filter(|q| {
                            let q = q.to_lowercase();
                            q.starts_with("select") || q.starts_with("with") || q.starts_with('(')
                        });
                        let iter = iter.map(|q| match self.sql(q, at) {
                            ir::Stmt::Expr(e, _) => e,
                            _ => unreachable!(),
                        });
                        frames.push(Frame::Loop { iter, body: vec![], at });
                        let r = text[lp + 4..].trim_start();
                        at += text.len() - r.len();
                        text = r;
                        continue;
                    }
                    _ => {}
                }
                let loc = self.loc(at);
                let stmt = match first.as_str() {
                    "return" => {
                        let (r, k) = rest_after(1);
                        if r.to_lowercase().starts_with("query") {
                            let q = r[5..].trim_start();
                            match self.sql(q, at + k + (r.len() - q.len())) {
                                ir::Stmt::Expr(e, l) => Some(ir::Stmt::Return(Some(e), l)),
                                _ => None,
                            }
                        } else if r.is_empty() {
                            Some(ir::Stmt::Return(None, loc))
                        } else {
                            Some(ir::Stmt::Return(Some(ir::Expr::Opaque(normalise(r))), loc))
                        }
                    }
                    "raise" => {
                        let level = low.split_whitespace().nth(1).unwrap_or("");
                        let msg = text.find('\'').map(|q| text[q + 1..].split('\'').next().unwrap_or("").to_string());
                        if matches!(level, "notice" | "warning" | "info" | "log" | "debug") {
                            let call =
                                ir::Call { callee: ir::Expr::Ident("$raise".into()), args: vec![ir::Expr::Str(msg.unwrap_or_default())], loc: loc.clone() };
                            Some(ir::Stmt::Expr(ir::Expr::Call(Box::new(call)), loc))
                        } else {
                            Some(ir::Stmt::Throw(ir::Expr::Str(msg.unwrap_or_else(|| normalise(text))), loc))
                        }
                    }
                    "perform" => Some(self.sql(&format!("select {}", &text[7..]), at)),
                    "get" | "null" | "continue" | "exit" | "commit" | "rollback" | "open" | "close" | "fetch" | "move" => None,
                    "call" => Some(self.sql(text, at)),
                    _ => {
                        // `v := (SELECT …)`: the query runs; other assignments are local.
                        let assign = low.find(":=").filter(|&p| !low[..p].contains(' ') || low[..p].split_whitespace().count() == 1);
                        match assign {
                            Some(p) => {
                                let expr = text[p + 2..].trim();
                                let el = expr.to_lowercase();
                                el.contains("select").then(|| self.sql(&format!("select {expr}"), at))
                            }
                            None => Some(self.sql(text, at)),
                        }
                    }
                };
                if let Some(st) = stmt {
                    current(&mut frames, &mut out).push(st);
                }
                break;
            }
        }
        out
    }
}

fn stmt_line(s: &ir::Stmt) -> u32 {
    match s {
        ir::Stmt::Expr(_, l) | ir::Stmt::Return(_, l) | ir::Stmt::Throw(_, l) => l.line,
        ir::Stmt::If { loc, .. } | ir::Stmt::Loop { loc, .. } | ir::Stmt::Try { loc, .. } | ir::Stmt::Let { loc, .. } => loc.line,
        ir::Stmt::Jump(l) => l.line,
        ir::Stmt::Block(b) => b.first().map(stmt_line).unwrap_or(1),
    }
}

/// Whitespace collapsed: formatting is not behaviour.
fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').trim().to_string()
}

fn line_starts(s: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(s.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
    v
}

/// `--` and `/* */` comments replaced by spaces, strings and dollar-quoted bodies kept.
fn blank_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let mut dollar: Option<Vec<u8>> = None;
    while i < b.len() {
        if let Some(tag) = &dollar {
            if b[i..].starts_with(tag) {
                i += tag.len();
                dollar = None;
                continue;
            }
            // Inside a function body comments still count as comments.
        }
        if let Some(q) = quote {
            if b[i] == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b[i] {
            b'\'' | b'"' => quote = Some(b[i]),
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                while i < b.len() && !b[i..].starts_with(b"*/") {
                    if b[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
                if i < b.len() {
                    out[i] = b' ';
                    out[i + 1] = b' ';
                    i += 2;
                }
                continue;
            }
            b'$' if dollar.is_none() => {
                if let Some(tag) = dollar_tag(&b[i..]) {
                    i += tag.len();
                    dollar = Some(tag);
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// `$tag$` / `$$` at the start of `b`.
fn dollar_tag(b: &[u8]) -> Option<Vec<u8>> {
    if b.first() != Some(&b'$') {
        return None;
    }
    let end = b[1..].iter().position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))? + 1;
    (b.get(end) == Some(&b'$') && !b[1..end].first().is_some_and(u8::is_ascii_digit)).then(|| b[..=end].to_vec())
}

/// The first dollar-quoted body in `code[from..to]`, as byte offsets of its contents.
fn dollar_body(code: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let b = code.as_bytes();
    let mut i = from;
    while i < to {
        if b[i] == b'$' {
            if let Some(tag) = dollar_tag(&b[i..]) {
                let start = i + tag.len();
                let end = code[start..].find(std::str::from_utf8(&tag).ok()?).map(|p| start + p)?;
                return Some((start, end));
            }
        }
        i += 1;
    }
    None
}

/// Statements of `code[from..to]`: split at `;` outside quotes, dollar quotes and parentheses.
fn split(code: &str, from: usize, to: usize) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out = vec![];
    let (mut i, mut start, mut depth) = (from, from, 0i32);
    let mut quote: Option<u8> = None;
    while i < to {
        let c = b[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'$' => {
                if let Some(tag) = dollar_tag(&b[i..]) {
                    let body = i + tag.len();
                    let tag_s = std::str::from_utf8(&tag).unwrap_or("$$");
                    i = code[body..to].find(tag_s).map(|p| body + p + tag.len()).unwrap_or(to);
                    continue;
                }
            }
            b';' if depth <= 0 => {
                if !code[start..i].trim().is_empty() {
                    out.push((start, i));
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if !code[start..to].trim().is_empty() {
        out.push((start, to));
    }
    out
}

fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = vec![];
    let (mut depth, mut cur, mut quote) = (0i32, String::new(), None);
    for c in s.chars() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, _) if c == sep && depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn match_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut quote = None;
    for (i, c) in s[open..].char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

fn words(s: &str, n: usize) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == '(').filter(|w| !w.is_empty()).take(n).map(str::to_lowercase).collect()
}

fn skip(w: &[&str], from: usize, words: &[&str]) -> usize {
    let mut k = from;
    while k < w.len() && words.contains(&w[k]) {
        k += 1;
    }
    k
}

/// `s` with leading words from `drop` removed (case-insensitive).
fn words_after(s: &str, drop: &[&str]) -> String {
    let mut rest = s.trim();
    loop {
        let (w, r) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if !w.is_empty() && drop.contains(&w.to_lowercase().as_str()) {
            rest = r.trim_start();
        } else {
            return rest.to_string();
        }
    }
}

/// `public."Orders"` → `Orders`.
fn last_name(s: &str) -> String {
    let s = s.trim().trim_end_matches(['(', ';']);
    s.rsplit('.').next().unwrap_or(s).trim_matches('"').to_string()
}

/// Byte position of `word` as a whole word in lower-cased `s`.
fn find_word(s: &str, word: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(p) = s[from..].find(word) {
        let at = from + p;
        let before = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_');
        let end = at + word.len();
        let after = end >= b.len() || !(b[end].is_ascii_alphanumeric() || b[end] == b'_');
        if before && after {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
BEGIN;
CREATE TABLE IF NOT EXISTS stock (
    item_id text NOT NULL,
    qty integer DEFAULT 0
);
ALTER TABLE stock ADD COLUMN IF NOT EXISTS stack_number text;
CREATE INDEX IF NOT EXISTS stock_item_idx ON stock (item_id);

CREATE OR REPLACE FUNCTION refresh_stock(p_date date DEFAULT DATE '2026-05-01') RETURNS integer
LANGUAGE plpgsql
AS $fn$
DECLARE
    v_rows integer;
BEGIN
    -- comment; with a semicolon
    UPDATE status SET started_at = now() WHERE id = 1;
    IF p_date IS NULL THEN
        RAISE EXCEPTION 'no date';
    END IF;
    TRUNCATE stock;
    INSERT INTO stock (item_id, qty) SELECT item_id, count(*) FROM moves WHERE moved_at > p_date GROUP BY item_id;
    GET DIAGNOSTICS v_rows = ROW_COUNT;
    RETURN v_rows;
END;
$fn$;
COMMIT;
"#;

    #[test]
    fn functions_tables_and_indexes() {
        let m = lower_module("db/stock.sql", FILE);
        let f = m.functions.iter().find(|f| f.name == "refresh_stock").unwrap();
        assert_eq!(f.params[0].name, "p_date");
        assert_eq!(f.return_type, Some(ir::TypeRef::Named("integer".into(), vec![])));
        let kinds: Vec<&str> = f
            .body
            .iter()
            .map(|s| match s {
                ir::Stmt::Expr(..) => "sql",
                ir::Stmt::Block(b) if matches!(b.first(), Some(ir::Stmt::If { .. })) => "if",
                ir::Stmt::Return(..) => "return",
                ir::Stmt::Let { .. } => "let",
                _ => "?",
            })
            .collect();
        assert_eq!(kinds, ["let", "sql", "if", "sql", "sql", "return"], "{:#?}", f.body);
        let stock = m.classes.iter().find(|c| c.name == "stock").unwrap();
        assert_eq!(stock.field_tags["stack_number"], "text");
        assert_eq!(stock.field_tags["qty"], "integer default 0");
        assert_eq!(stock.field_tags["index stock_item_idx"], "(item_id)");
        assert!(m.functions.iter().all(|f| f.name != "<script>"), "BEGIN/COMMIT only");
    }
}
