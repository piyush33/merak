//! MCP server (stdio, JSON-RPC 2.0, newline-delimited) exposing Merak to coding agents.
//!
//! Tools:
//! - `merak_transition`: the semantic transition of a change (working tree by default).
//! - `merak_context`: what one function or method does, and what reaches it.

use crate::source::{self, Files};
use anyhow::{anyhow, Result};
use merak_behaviour::Model;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Tool results are cut here; agents get the rest by narrowing the request.
const MAX_OUTPUT: usize = 24_000;

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            write(&mut out, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}))?;
            continue;
        };
        // Notifications (no id) get no response.
        let Some(id) = msg.get("id").cloned() else { continue };
        let method = msg["method"].as_str().unwrap_or("");
        let response = match handle(method, &msg["params"]) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": e.to_string()}}),
        };
        write(&mut out, &response)?;
    }
    Ok(())
}

fn write(out: &mut impl Write, v: &Value) -> Result<()> {
    writeln!(out, "{v}")?;
    out.flush()?;
    Ok(())
}

fn handle(method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "merak", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Merak derives what a code change does to a program's behaviour (auth, query filters, effects, validation, state transitions), with file:line evidence. Use merak_context before changing a function to see what it does and which routes reach it; use merak_transition after editing to check the behavioural effect of your change against what was asked."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let args = &params["arguments"];
            let result = match params["name"].as_str().unwrap_or("") {
                "merak_transition" => transition(args),
                "merak_context" => context(args),
                other => Err(anyhow!("unknown tool `{other}`")),
            };
            Ok(match result {
                Ok(text) => json!({"content": [{"type": "text", "text": truncate(text)}]}),
                Err(e) => json!({"content": [{"type": "text", "text": format!("merak: {e:#}")}], "isError": true}),
            })
        }
        other => Err(anyhow!("method not found: {other}")),
    }
}

fn tools() -> Value {
    let common = json!({
        "repo": {"type": "string", "description": "Repository directory (default: the project directory)."},
        "root": {"type": "string", "description": "Analyze only this subdirectory, e.g. `server` (default: the whole repository)."}
    });
    let mut transition_props = common.clone();
    transition_props["base"] = json!({"type": "string", "description": "Base revision (default `HEAD`)."});
    transition_props["head"] = json!({"type": "string", "description": "Head revision (default: the working tree, uncommitted changes included)."});
    transition_props["json"] = json!({"type": "boolean", "description": "Return the operations as JSON instead of Markdown."});
    let mut context_props = common;
    context_props["entity"] = json!({"type": "string", "description": "Function or method: `mergePeople`, `PersonService.mergePeople`, or a full id `src/x.ts::PersonService.mergePeople`."});
    json!([
        {
            "name": "merak_transition",
            "description": "Semantic transition of a code change: typed, evidence-backed operations such as AUTH_WIDENED, QUERY_FILTER_REMOVED, EFFECT_ADDED, SCHEMA_FIELD_CHANGED, GUARD_REMOVED, BEHAVIOUR_ADDED (new code), with the HTTP routes each affects. PURE_REFACTOR means no behaviour changed; UNCLASSIFIED_CHANGE means calls changed in a way Merak does not model (review those by hand). Defaults to uncommitted changes against HEAD.",
            "inputSchema": {"type": "object", "properties": transition_props}
        },
        {
            "name": "merak_context",
            "description": "Behaviour of one function or method in the current working tree: effects it reaches (DB reads/writes, HTTP, events, queues, files), access requirements, query filters, guards, validations, the HTTP routes that reach it, its callers and callees. Use before changing it.",
            "inputSchema": {"type": "object", "properties": context_props, "required": ["entity"]}
        }
    ])
}

fn repo_and_root(args: &Value) -> (PathBuf, String) {
    // Claude Code sets CLAUDE_PROJECT_DIR for the servers it starts.
    let default = std::env::var("CLAUDE_PROJECT_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
    let repo = args["repo"].as_str().map(PathBuf::from).unwrap_or(default);
    let root = args["root"].as_str().unwrap_or("").trim_matches('/').to_string();
    (repo, root)
}

fn transition(args: &Value) -> Result<String> {
    let (repo, root) = repo_and_root(args);
    let base = args["base"].as_str().unwrap_or("HEAD");
    let head = args["head"].as_str().unwrap_or("");
    let (a, b) = crate::load_range(&repo, base, head, &root)?;
    let t = crate::diff(&a, &b)?;
    if args["json"].as_bool().unwrap_or(false) {
        return Ok(serde_json::to_string_pretty(&t)?);
    }
    let label = if head.is_empty() { format!("{base}..working tree") } else { format!("{base}..{head}") };
    Ok(merak_transition::render::markdown(&t, Some(&label)))
}

fn context(args: &Value) -> Result<String> {
    let (repo, root) = repo_and_root(args);
    let query = args["entity"].as_str().ok_or_else(|| anyhow!("`entity` is required"))?;
    let files: Files = source::from_dir(&repo.join(&root))?;
    let m = crate::model(&files)?;
    let found = find(&m, query);
    match found.as_slice() {
        [] => Err(anyhow!("no function or method matches `{query}`")),
        [id] => Ok(describe_entity(&m, id)),
        many => Ok(format!(
            "`{query}` matches {} entities; ask again with one of:\n{}",
            many.len(),
            many.iter().map(|id| format!("- `{id}`")).collect::<Vec<_>>().join("\n")
        )),
    }
}

/// Entities matching a full id, a qualified name (`Class.method`) or a bare name.
fn find(m: &Model, query: &str) -> Vec<String> {
    if m.entities.contains_key(query) {
        return vec![query.to_string()];
    }
    let qualified = |id: &str| id.rsplit("::").next().unwrap_or(id).to_string();
    let exact: Vec<String> = m.entities.keys().filter(|id| qualified(id) == query).cloned().collect();
    if !exact.is_empty() {
        return exact;
    }
    m.entities.values().filter(|e| e.parent.is_none() && e.name == query).map(|e| e.id.clone()).collect()
}

fn describe_entity(m: &Model, id: &str) -> String {
    let e = &m.entities[id];
    let s = &m.summaries[id];
    let mut out = format!("## `{}`\n\n{} at `{}`\n", id.rsplit("::").next().unwrap_or(id), format!("{:?}", e.kind).to_lowercase(), e.loc);
    let section = |out: &mut String, title: &str, items: Vec<String>| {
        if !items.is_empty() {
            out.push_str(&format!("\n**{title}**\n"));
            for i in items {
                out.push_str(&format!("- {i}\n"));
            }
        }
    };
    section(&mut out, "Reached from routes", m.entry_points_reaching(id).into_iter().map(|r| format!("`{r}`")).collect());
    section(
        &mut out,
        "Effects (transitive)",
        m.effects_outside_checks(id)
            .into_iter()
            .map(|(k, i)| {
                format!(
                    "{} ({}) at {}",
                    k.render(),
                    i.modes.iter().cloned().collect::<Vec<_>>().join(", "),
                    i.evidence.iter().take(2).map(|l| format!("`{l}`")).collect::<Vec<_>>().join(", ")
                )
            })
            .collect(),
    );
    section(&mut out, "Access requirements", s.access.values().map(|a| format!("{} at `{}`", a.requirement, a.loc)).collect());
    section(&mut out, "Guards (must hold to continue)", s.requires.iter().map(|(_, p, l)| format!("{} at `{l}`", p.render())).collect());
    let mut filters: Vec<String> = m
        .entities
        .values()
        .filter(|x| x.id == id || x.id.starts_with(&format!("{id}.")))
        .flat_map(|x| &x.filters)
        .map(|f| format!("{} at `{}`", f.expr, f.loc))
        .collect();
    filters.sort();
    section(&mut out, "Query filters", filters);
    section(
        &mut out,
        "Checks reached (their own reads are not listed above)",
        s.validations.iter().map(|(v, l)| format!("{} (call at `{l}`)", v.rsplit("::").next().unwrap_or(v))).collect(),
    );
    section(&mut out, "Writes", s.writes.iter().cloned().collect());
    let callers: BTreeSet<String> =
        m.entities.values().filter(|x| x.calls.iter().any(|c| c.target == id)).map(|x| x.id.rsplit("::").next().unwrap_or(&x.id).to_string()).collect();
    section(&mut out, "Called by", callers.into_iter().collect());
    let callees: BTreeSet<String> =
        e.calls.iter().filter(|c| m.entities.contains_key(&c.target)).map(|c| c.target.rsplit("::").next().unwrap_or(&c.target).to_string()).collect();
    section(&mut out, "Calls", callees.into_iter().collect());
    section(&mut out, "Unknowns", e.unknowns.iter().take(10).map(|u| format!("{} at `{}`", u.question, u.loc)).collect());
    out
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_OUTPUT {
        let mut cut = MAX_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n\n… (truncated; narrow the request with `root`, or ask for one entity with merak_context)");
    }
    text
}
