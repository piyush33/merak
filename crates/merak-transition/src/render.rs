//! Markdown rendering of a transition, for terminals and PR comments.

use crate::{Layer, Op, Transition};
use std::fmt::Write;

fn section(layer: Layer) -> &'static str {
    match layer {
        Layer::Behaviour => "Behaviour",
        Layer::Design => "Design",
        Layer::Structural => "Structure",
    }
}

fn short(s: &str) -> String {
    // `src/services/order.service.ts::OrderService.cancelOrder` → `OrderService.cancelOrder`
    s.split(" → ").map(|p| p.rsplit("::").next().unwrap_or(p)).collect::<Vec<_>>().join(" → ")
}

fn title(kind: &str) -> String {
    kind.replace('_', " ").to_lowercase()
}

pub fn markdown(t: &Transition, header: Option<&str>) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## Semantic transition{}", header.map(|h| format!(" — {h}")).unwrap_or_default());
    let _ = writeln!(
        s,
        "\n{} behavioural/design change(s) across {} changed entit{}.\n",
        t.behavioural().count(),
        t.stats.entities_changed,
        if t.stats.entities_changed == 1 { "y" } else { "ies" }
    );
    if t.ops.is_empty() {
        s.push_str("_No semantic change detected._\n");
        return s;
    }
    for layer in [Layer::Behaviour, Layer::Design, Layer::Structural] {
        let ops: Vec<&Op> = t.ops.iter().filter(|o| o.layer == layer).collect();
        if ops.is_empty() {
            continue;
        }
        let _ = writeln!(s, "### {}\n", section(layer));
        for op in ops {
            let _ = write!(s, "- **{}** `{}`", title(&op.kind), short(&op.subject));
            match (&op.before, &op.after) {
                (Some(b), Some(a)) => {
                    let _ = write!(s, "\n  - before: {b}\n  - after: {a}");
                }
                (Some(b), None) => {
                    let _ = write!(s, "\n  - was: {b}");
                }
                (None, Some(a)) => {
                    let _ = write!(s, "\n  - now: {a}");
                }
                (None, None) => {}
            }
            if let Some(n) = &op.note {
                let _ = write!(s, "\n  - {n}");
            }
            if !op.affects.is_empty() {
                let _ = write!(s, "\n  - affects: {}", op.affects.iter().map(|a| format!("`{a}`")).collect::<Vec<_>>().join(", "));
            }
            let mut ev: Vec<String> = op.evidence_after.iter().map(|l| format!("`{l}`")).collect();
            if ev.is_empty() {
                ev = op.evidence_before.iter().map(|l| format!("`{l}` (before)")).collect();
            }
            if !ev.is_empty() {
                ev.truncate(4);
                let _ = write!(s, "\n  - evidence: {}", ev.join(", "));
            }
            match op.origin {
                merak_behaviour::Origin::Static => {}
                merak_behaviour::Origin::Declared => s.push_str("\n  - origin: declared in merak.toml"),
                merak_behaviour::Origin::Inferred => {
                    let _ = write!(s, "\n  - origin: inferred, confidence {:.2}", op.confidence);
                }
            }
            s.push('\n');
        }
        s.push('\n');
    }
    s
}
