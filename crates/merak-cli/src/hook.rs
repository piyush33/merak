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

fn project(input: &Value) -> PathBuf {
    input["cwd"].as_str().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn snapshot_path(input: &Value, data: &Path) -> PathBuf {
    let session: String = input["session_id"].as_str().unwrap_or("default").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    data.join("snapshots").join(format!("{session}.json"))
}

/// Snapshots older than this belong to sessions that are over.
const SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

pub fn prompt(input: &Value, data: &Path) -> Result<()> {
    let files = source::from_dir(&project(input))?;
    let path = snapshot_path(input, data);
    let dir = path.parent().unwrap_or(data);
    std::fs::create_dir_all(dir)?;
    for old in std::fs::read_dir(dir)?.flatten() {
        if old.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > SNAPSHOT_TTL)) {
            let _ = std::fs::remove_file(old.path());
        }
    }
    std::fs::write(&path, serde_json::to_vec(&files)?).with_context(|| format!("writing {}", path.display()))?;
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
    let before: Files = source::from_snapshot(&path)?;
    let after = source::from_dir(&project(input))?;
    if before == after {
        return Ok(Outcome::Quiet);
    }
    let t = crate::diff(&before, &after)?;
    Ok(match report(&t) {
        Some(msg) => Outcome::Review(msg),
        None => Outcome::Quiet,
    })
}

/// The message for the agent, if the turn changed behaviour or design.
pub fn report(t: &Transition) -> Option<String> {
    let ops: Vec<&Op> = t.ops.iter().filter(|o| o.layer != Layer::Structural).collect();
    if ops.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = ops.iter().map(|o| line(o)).collect();
    let more = lines.len().saturating_sub(MAX_LINES);
    lines.truncate(MAX_LINES);
    if more > 0 {
        lines.push(format!("- … and {more} more (call merak_transition for all of them)"));
    }
    Some(format!(
        "Merak: semantic transition of this turn's edits ({} behavioural/design change(s)):\n{}\n\n\
         Check each against what the user asked. If one is unintended (for example an access requirement or query \
         filter removed, or a new effect), fix it. Otherwise continue and, in your reply, briefly mention the \
         behavioural changes the user should know about. UNCLASSIFIED_CHANGE means Merak could not type the change: \
         judge those yourself. This message is sent once per turn.",
        ops.len(),
        lines.join("\n")
    ))
}

fn line(o: &Op) -> String {
    let subject = o.subject.split(" → ").map(|p| p.rsplit("::").next().unwrap_or(p)).collect::<Vec<_>>().join(" → ");
    let change = match (&o.before, &o.after) {
        (Some(b), Some(a)) => format!(": {} → {}", clip(b), clip(a)),
        (Some(b), None) => format!(": was {}", clip(b)),
        (None, Some(a)) => format!(": {}", clip(a)),
        (None, None) => String::new(),
    };
    let affects = if o.affects.is_empty() { String::new() } else { format!(" [affects {}]", o.affects.iter().take(3).cloned().collect::<Vec<_>>().join(", ")) };
    let at = o.evidence_after.first().or(o.evidence_before.first()).map(|l| format!(" ({l})")).unwrap_or_default();
    format!("- {} {subject}{change}{affects}{at}", o.kind)
}

fn clip(s: &str) -> String {
    const MAX: usize = 160;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    }
}
