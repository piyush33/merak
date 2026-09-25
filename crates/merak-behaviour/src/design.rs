//! Declared design: layers, dependency rules and field ownership from `merak.toml`.

use crate::catalog::Catalog;
use crate::model::*;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub layers: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// `Type.field` → module globs allowed to write it.
    #[serde(default)]
    pub ownership: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub catalog: Option<Catalog>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub from: String,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
}

fn globs(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p) {
            b.add(g);
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

pub fn apply(model: &mut Model, config: &Config) {
    // Module → layer (first matching layer, in file order of the TOML table).
    let layer_globs: Vec<(String, GlobSet)> = config.layers.iter().map(|(k, v)| (k.clone(), globs(v))).collect();
    let modules: Vec<String> = model.entities.values().map(|e| e.module.clone()).collect();
    for m in modules {
        if let Some((l, _)) = layer_globs.iter().find(|(_, g)| g.is_match(&m)) {
            model.module_layers.insert(m, l.clone());
        }
    }

    let mut violations = vec![];
    for ((from, to), locs) in &model.module_deps {
        let (Some(lf), Some(lt)) = (model.module_layers.get(from), model.module_layers.get(to)) else { continue };
        if config.rules.iter().any(|r| &r.from == lf && r.deny.contains(lt)) {
            violations.push(Violation { kind: "layer".into(), subject: format!("{lf} → {lt}"), detail: format!("{from} → {to}"), evidence: locs.clone() });
        }
    }
    for (field, owners) in &config.ownership {
        let owners = globs(owners);
        for e in model.entities.values() {
            if owners.is_match(&e.module) {
                continue;
            }
            let locs: Vec<_> = e.writes.iter().filter(|w| &w.field == field).map(|w| w.loc.clone()).collect();
            if !locs.is_empty() {
                violations.push(Violation { kind: "ownership".into(), subject: field.clone(), detail: e.id.clone(), evidence: locs });
            }
        }
    }
    violations.sort();
    model.violations = violations;
}
