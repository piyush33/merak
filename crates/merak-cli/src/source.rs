//! Loading source trees: from a directory, a git revision, or a scenario overlay.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

pub type Files = BTreeMap<String, String>;

const SKIP_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target", ".next", "coverage"];

fn wanted(path: &str) -> bool {
    merak_front_ts::is_supported(path) || path == "merak.toml"
}

pub fn from_dir(root: &Path) -> Result<Files> {
    let mut files = Files::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn walk(root: &Path, dir: &Path, files: &mut Files) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, files)?;
            }
            continue;
        }
        let rel = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
        if wanted(&rel) {
            files.insert(rel, std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
    Ok(())
}

/// Read every supported file of `rev` in the repository at `repo` (optionally under `prefix`).
pub fn from_git(repo: &Path, rev: &str, prefix: &str) -> Result<Files> {
    let out = Command::new("git").arg("-C").arg(repo).args(["ls-tree", "-r", "--name-only", rev]).output()?;
    if !out.status.success() {
        bail!("git ls-tree {rev}: {}", String::from_utf8_lossy(&out.stderr));
    }
    let mut files = Files::new();
    for path in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(rel) = path.strip_prefix(prefix) else { continue };
        let rel = rel.trim_start_matches('/');
        if !wanted(rel) || rel.split('/').any(|seg| SKIP_DIRS.contains(&seg)) {
            continue;
        }
        let blob = Command::new("git").arg("-C").arg(repo).args(["show", &format!("{rev}:{path}")]).output()?;
        files.insert(rel.to_string(), String::from_utf8_lossy(&blob.stdout).to_string());
    }
    Ok(files)
}

/// `base` with a scenario's `overlay/` applied and `delete = [...]` paths removed.
pub fn with_overlay(base: &Files, scenario: &Path) -> Result<Files> {
    let mut files = base.clone();
    let meta = std::fs::read_to_string(scenario.join("scenario.toml")).unwrap_or_default();
    let meta: toml::Value = toml::from_str(&meta).unwrap_or(toml::Value::Table(Default::default()));
    if let Some(del) = meta.get("delete").and_then(|d| d.as_array()) {
        for d in del.iter().filter_map(|d| d.as_str()) {
            files.remove(d);
        }
    }
    let overlay = scenario.join("overlay");
    if overlay.exists() {
        files.extend(from_dir(&overlay)?);
    }
    Ok(files)
}
