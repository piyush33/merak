//! Golden scenarios: each `fixtures/orders-demo/scenarios/*` applies an overlay
//! to the base app; the derived transition must match `expected.json`.

use merak_cli::source;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/orders-demo")
}

fn run(name: &str) {
    let root = fixtures();
    let base = source::from_dir(&root.join("base")).unwrap();
    let scenario = root.join("scenarios").join(name);
    let after = source::with_overlay(&base, &scenario).unwrap();
    let t = merak_cli::diff(&base, &after).unwrap();
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(scenario.join("expected.json")).unwrap()).unwrap();

    let got: Vec<(String, String)> = t.ops.iter().map(|o| (o.kind.clone(), o.subject.clone())).collect();
    let want: Vec<(String, String)> =
        expected["ops"].as_array().unwrap().iter().map(|o| (o["kind"].as_str().unwrap().to_string(), o["subject"].as_str().unwrap().to_string())).collect();
    let report = merak_transition::render::detailed(&t, Some(name));
    let mut missing: Vec<_> = want.iter().filter(|w| !got.contains(w)).collect();
    let mut extra: Vec<_> = got.iter().filter(|g| !want.contains(g)).collect();
    missing.sort();
    extra.sort();
    assert!(missing.is_empty() && extra.is_empty(), "{name}\nmissing: {missing:#?}\nextra: {extra:#?}\n\n{report}");

    for w in expected["ops"].as_array().unwrap() {
        let op = t.ops.iter().find(|o| o.kind == w["kind"].as_str().unwrap() && o.subject == w["subject"].as_str().unwrap()).unwrap();
        for field in ["before", "after"] {
            if let Some(v) = w.get(field).and_then(Value::as_str) {
                let actual = if field == "before" { &op.before } else { &op.after };
                assert_eq!(actual.as_deref(), Some(v), "{name}: {} {} `{field}`\n\n{report}", op.kind, op.subject);
            }
        }
        assert!(!op.evidence_before.is_empty() || !op.evidence_after.is_empty() || op.kind == "PURE_REFACTOR", "{name}: {} has no evidence", op.kind);
    }
    if let Some(forbid) = expected.get("forbid_layers").and_then(Value::as_array) {
        for layer in forbid.iter().filter_map(Value::as_str) {
            let bad: Vec<_> = t.ops.iter().filter(|o| serde_json::to_value(o.layer).unwrap() == layer).map(|o| &o.kind).collect();
            assert!(bad.is_empty(), "{name}: forbidden {layer} ops {bad:?}\n\n{report}");
        }
    }
}

macro_rules! golden {
    ($($test:ident => $dir:literal),* $(,)?) => {
        $( #[test] fn $test() { run($dir); } )*
    };
}

golden! {
    s01_manager_can_cancel => "01-manager-can-cancel",
    s02_controller_bypasses_policy => "02-controller-bypasses-policy",
    s03_notify_warehouse => "03-notify-warehouse",
    s04_remove_inventory_validation => "04-remove-inventory-validation",
    s05_cancel_confirmed_orders => "05-cancel-confirmed-orders",
    s06_force_paid_without_payment => "06-force-paid-without-payment",
    s07_refund_via_event => "07-refund-via-event",
    s08_pure_refactor => "08-pure-refactor",
}

#[test]
fn identical_trees_have_no_transition() {
    let base = source::from_dir(&fixtures().join("base")).unwrap();
    let t = merak_cli::diff(&base, &base).unwrap();
    assert!(t.ops.is_empty(), "{:#?}", t.ops);
}

#[test]
fn git_revision_loads_like_directory() {
    let base = fixtures().join("base");
    let tmp = std::env::temp_dir().join(format!("merak-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let sub = tmp.join("app");
    std::fs::create_dir_all(&sub).unwrap();
    for (rel, text) in source::from_dir(&base).unwrap() {
        let p = sub.join(&rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git").arg("-C").arg(&tmp).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "base"]);
    let from_git = source::from_git(&tmp, "HEAD", "app").unwrap();
    assert_eq!(from_git, source::from_dir(&base).unwrap());
    assert!(source::from_git(&tmp, "HEAD", "ap").unwrap().is_empty(), "prefix must match whole path segments");
    let _ = std::fs::remove_dir_all(&tmp);
}
