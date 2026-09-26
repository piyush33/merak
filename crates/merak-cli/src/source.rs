//! Loading source trees: from a directory, a git revision, or a scenario overlay.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

pub type Files = BTreeMap<String, String>;

const SKIP_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "target", ".next", "coverage"];

/// Test code is not product behaviour.
const TEST_DIRS: &[&str] = &["test", "tests", "__tests__", "__mocks__", "e2e"];

fn wanted(path: &str) -> bool {
    if merak_front_ts::is_project_config(path) || path == "merak.toml" {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    merak_front_ts::is_supported(path) && !name.contains(".spec.") && !name.contains(".test.") && !path.split('/').any(|seg| TEST_DIRS.contains(&seg))
}

/// Sources saved by `merak snapshot`.
pub fn from_snapshot(path: &Path) -> Result<Files> {
    let bytes = std::fs::read(path).with_context(|| format!("reading snapshot {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
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
    let blobs = git_tree(repo, rev, prefix)?;
    let texts = git_blobs(repo, blobs.iter().map(|(_, oid)| oid.as_str()))?;
    Ok(blobs.into_iter().map(|(rel, _)| rel).zip(texts).collect())
}

/// `(path relative to prefix, blob id)` of every supported file of `rev` under `prefix`.
pub fn git_tree(repo: &Path, rev: &str, prefix: &str) -> Result<Vec<(String, String)>> {
    let prefix = prefix.trim_matches('/');
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(["ls-tree", "-r", "-z", "--full-tree", rev]);
    if !prefix.is_empty() {
        cmd.args(["--", prefix]);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("git ls-tree {rev}: {}", String::from_utf8_lossy(&out.stderr));
    }
    let mut blobs = vec![];
    for entry in String::from_utf8_lossy(&out.stdout).split('\0') {
        // `<mode> blob <oid>\t<path>`
        let Some((meta, path)) = entry.split_once('\t') else { continue };
        let mut meta = meta.split(' ');
        let (Some(_), Some("blob"), Some(oid)) = (meta.next(), meta.next(), meta.next()) else { continue };
        let rel = if prefix.is_empty() { Some(path) } else { path.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) };
        let Some(rel) = rel else { continue };
        if wanted(rel) && !rel.split('/').any(|seg| SKIP_DIRS.contains(&seg)) {
            blobs.push((rel.to_string(), oid.to_string()));
        }
    }
    Ok(blobs)
}

/// Contents of the given blobs, in order, read through one `git cat-file --batch`.
pub fn git_blobs<'a>(repo: &Path, oids: impl Iterator<Item = &'a str>) -> Result<Vec<String>> {
    use std::io::{BufRead, BufReader, Read, Write};
    let oids: Vec<&str> = oids.collect();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning git cat-file")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let request = oids.iter().map(|o| format!("{o}\n")).collect::<String>();
    // Write from a thread so a full stdout pipe cannot deadlock us.
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
    let mut out = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut texts = Vec::with_capacity(oids.len());
    let mut header = String::new();
    for oid in &oids {
        header.clear();
        out.read_line(&mut header)?;
        // `<oid> blob <size>` or `<oid> missing`
        let size: usize = match header.trim_end().rsplit_once(' ') {
            Some((_, n)) if !header.contains(" missing") => n.parse().with_context(|| format!("cat-file header {header:?}"))?,
            _ => bail!("git cat-file: blob {oid} missing"),
        };
        let mut buf = vec![0; size + 1]; // content plus trailing newline
        out.read_exact(&mut buf)?;
        buf.pop();
        texts.push(String::from_utf8_lossy(&buf).into_owned());
    }
    writer.join().expect("writer thread")?;
    child.wait()?;
    Ok(texts)
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
