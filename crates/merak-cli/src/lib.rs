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
