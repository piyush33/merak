pub mod hook;
pub mod mcp;
pub mod source;

use anyhow::Result;
use merak_behaviour::Model;
use source::Files;
use std::time::Instant;

pub fn model(files: &Files) -> Result<Model> {
    let t = Instant::now();
    let program = merak_front_ts::lower_program(files);
    timing("lower", t);
    let t = Instant::now();
    let m = merak_behaviour::analyze(&program, files.get("merak.toml").map(String::as_str)).map_err(anyhow::Error::msg);
    timing("analyze", t);
    m
}

/// Sources of `base` and `head` (git revisions of `repo`; an empty `head` is the working tree).
pub fn load_range(repo: &std::path::Path, base: &str, head: &str, root: &str) -> Result<(Files, Files)> {
    let a = source::from_git(repo, base, root)?;
    let b = if head.is_empty() { source::from_dir(&repo.join(root))? } else { source::from_git(repo, head, root)? };
    Ok((a, b))
}

pub fn diff(before: &Files, after: &Files) -> Result<merak_transition::Transition> {
    let (a, b) = (model(before)?, model(after)?);
    let t = Instant::now();
    let tr = merak_transition::diff(&a, &b);
    timing("transition", t);
    Ok(tr)
}

/// Phase timings on stderr when `MERAK_TIMINGS` is set.
pub fn timing(phase: &str, since: Instant) {
    if std::env::var_os("MERAK_TIMINGS").is_some() {
        eprintln!("merak: {phase:<10} {:>8.1} ms", since.elapsed().as_secs_f64() * 1e3);
    }
}
