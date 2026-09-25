pub mod source;

use anyhow::Result;
use merak_behaviour::Model;
use source::Files;

pub fn model(files: &Files) -> Result<Model> {
    let program = merak_front_ts::lower_program(files);
    merak_behaviour::analyze(&program, files.get("merak.toml").map(String::as_str)).map_err(anyhow::Error::msg)
}

pub fn diff(before: &Files, after: &Files) -> Result<merak_transition::Transition> {
    Ok(merak_transition::diff(&model(before)?, &model(after)?))
}
