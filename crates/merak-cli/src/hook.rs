//! Claude Code hooks: a per-turn ledger of semantic transitions.
//!
//! `merak hook prompt --data <dir>` (UserPromptSubmit) snapshots the project's sources;
//! `merak hook stop --data <dir>` (Stop) derives the transition since that snapshot and,
//! when the turn changed behaviour, hands it back to the agent once (`{"decision": "block",
//! "reason": …}`) to check against what was asked. A hook never fails the session: errors exit 0.

use crate::source::{self, Files};
use anyhow::{Context, Result};
use merak_transition::{Layer, Op, Transition};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Lines of the report handed back to the agent.
const MAX_LINES: usize = 30;

pub enum Outcome {
    /// Nothing to say; let the agent stop.
    Quiet,
    /// Continue the turn with this message for the agent.
    Review(String),
}

pub fn run(event: &str, data: &Path) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let input: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    if std::env::var("MERAK_HOOK").is_ok_and(|v| v == "off") {
        return 0;
    }
    let result = match event {
        "prompt" => prompt(&input, data).map(|_| Outcome::Quiet),
        "stop" => stop(&input, data),
        other => {
            eprintln!("merak: unknown hook `{other}`");
            return 0;
        }
    };
    match result {
        Ok(Outcome::Quiet) => 0,
        // Claude Code 2.1.x honours `block` (not `continue`) and shows the reason to the agent.
        Ok(Outcome::Review(msg)) => {
            println!("{}", serde_json::json!({"decision": "block", "reason": msg}));
            0
        }
        Err(e) => {
            // Visible in verbose mode; never blocks the session.
            eprintln!("merak hook {event}: {e:#}");
            0
        }
    }
}

/// The project to snapshot: where the session started. Hooks receive the agent's current
/// directory as `cwd`, which moves when it runs `cd`, so the Stop hook uses the root saved
/// in the snapshot instead.
fn project(input: &Value) -> PathBuf {
    std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .or_else(|| input["cwd"].as_str().map(str::to_string))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    root: PathBuf,
    files: Files,
}

fn snapshot_path(input: &Value, data: &Path) -> PathBuf {
    let session: String = input["session_id"].as_str().unwrap_or("default").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    data.join("snapshots").join(format!("{session}.json"))
}

/// Snapshots older than this belong to sessions that are over.
const SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

pub fn prompt(input: &Value, data: &Path) -> Result<()> {
    let root = project(input);
    let files = source::from_dir(&root)?;
    let path = snapshot_path(input, data);
    let dir = path.parent().unwrap_or(data);
    std::fs::create_dir_all(dir)?;
    for old in std::fs::read_dir(dir)?.flatten() {
        if old.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > SNAPSHOT_TTL)) {
            let _ = std::fs::remove_file(old.path());
        }
    }
    std::fs::write(&path, serde_json::to_vec(&Snapshot { root, files })?).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn stop(input: &Value, data: &Path) -> Result<Outcome> {
    // Already continuing because of a Stop hook: report at most once per turn.
    if input["stop_hook_active"].as_bool().unwrap_or(false) {
        return Ok(Outcome::Quiet);
    }
    let path = snapshot_path(input, data);
    if !path.exists() {
        return Ok(Outcome::Quiet);
    }
    // A snapshot in another format (an older Merak) is not an error, just nothing to compare.
    let Ok(snap) = serde_json::from_slice::<Snapshot>(&std::fs::read(&path)?) else { return Ok(Outcome::Quiet) };
    let (before, after) = (snap.files, source::from_dir(&snap.root)?);
    if before == after {
        return Ok(Outcome::Quiet);
    }
    // One turn edits some files; it does not replace the tree. If most paths differ, this is
    // not the tree the snapshot came from: say nothing rather than report every file as new.
    let shared = before.keys().filter(|k| after.contains_key(*k)).count();
    if shared * 2 < before.len().min(after.len()) {
        return Ok(Outcome::Quiet);
    }
    let t = crate::diff(&before, &after)?;
    Ok(match report(&t) {
        Some(msg) => Outcome::Review(msg),
        None => Outcome::Quiet,
    })
}

/// The message for the agent, if the turn changed behaviour or design.
///
/// Only typed changes interrupt the agent. A turn whose only residue is
/// `UNCLASSIFIED_CHANGE` (UI code, formatting helpers, anything Merak does not model) stays
/// quiet; `merak_transition` still shows it on request.
pub fn report(t: &Transition) -> Option<String> {
    let ops: Vec<&Op> = t.ops.iter().filter(|o| o.layer != Layer::Structural).collect();
    if ops.iter().all(|o| o.kind == "UNCLASSIFIED_CHANGE") {
        return None;
    }
    // Shown as plain text in the Claude Code UI: no bold markers.
    // Contracts first; operations no contract accounts for as plain sentences.
    let mut lines: Vec<String> = merak_transition::render::contract_lines(t);
    if lines.is_empty() {
        lines = merak_transition::render::bullets(t).into_iter().map(|l| format!("- {}", l.replace("**", ""))).collect();
    }
    let more = lines.len().saturating_sub(MAX_LINES);
    lines.truncate(MAX_LINES);
    if more > 0 {
        lines.push(format!("- … and {more} more (call merak_transition for all of them)"));
    }
    let named = ops.iter().filter(|o| o.kind != "UNCLASSIFIED_CHANGE").count();
    Some(format!(
        "Merak checked what this turn's edits do. {} the user should know about:\n{}\n\n\
         Compare these with what the user asked. Fix any that are unintended (for example access that was widened, \
         a query filter that was removed, or a new side effect). Otherwise finish, and mention the ones that matter \
         in your reply in plain words. Changes Merak can't classify need your own judgement. This note comes once per turn.",
        if named == 1 { "1 behaviour change".to_string() } else { format!("{named} behaviour changes") },
        lines.join("\n")
    ))
}
