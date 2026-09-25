//! Merak behaviour layer: builds the Program Behaviour Model (PBM) from the
//! structural IR — effects, guards, state machines, summaries, inferred
//! invariants and declared design rules.

pub mod catalog;
pub mod design;
pub mod extract;
pub mod infer;
pub mod model;
pub mod pred;
pub mod resolve;
pub mod summary;

pub use model::*;

use merak_ir as ir;
use std::collections::BTreeSet;

/// Build the behaviour model of one program version.
/// `config` is the text of `merak.toml`, if the repository has one.
pub fn analyze(program: &ir::Program, config: Option<&str>) -> Result<Model, String> {
    let config = match config {
        Some(text) => design::Config::parse(text).map_err(|e| format!("merak.toml: {e}"))?,
        None => design::Config::default(),
    };
    let cat = catalog::Catalog::with_extensions(config.catalog.clone());
    let ix = resolve::Index::new(program);
    let envs = extract::build_envs(&ix);

    let mut model = Model::default();
    for m in program.modules.values() {
        for c in &m.classes {
            model.classes.insert(
                c.id.clone(),
                ClassInfo { id: c.id.clone(), name: c.name.clone(), module: m.path.clone(), methods: c.methods.clone(), loc: c.loc.clone() },
            );
        }
        for f in &m.functions {
            let env = envs.get(&f.id).cloned().unwrap_or_default();
            let e = extract::extract_entity(&ix, &cat, &env, f);
            model.routes.extend(e.routes.iter().cloned());
            model.subscriptions.extend(e.subscriptions.iter().cloned());
            model.entities.insert(e.id.clone(), e);
        }
    }

    let fields: BTreeSet<String> = model.entities.values().flat_map(|e| e.writes.iter().map(|w| w.field.clone())).collect();
    for f in fields {
        if let Some((rec, field)) = f.split_once('.') {
            if let Some(d) = ix.field_domain(rec, field) {
                model.field_domains.insert(f.clone(), d);
            }
        }
    }

    for e in model.entities.values() {
        for c in &e.calls {
            let Some(target) = model.entities.get(&c.target) else { continue };
            if target.module != e.module {
                model.module_deps.entry((e.module.clone(), target.module.clone())).or_default().push(c.loc.clone());
            }
        }
    }

    summary::compute(&mut model);
    model.invariants = infer::mine(&model);
    design::apply(&mut model, &config);
    Ok(model)
}
