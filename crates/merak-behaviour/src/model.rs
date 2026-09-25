//! The Program Behaviour Model (PBM): facts about one version of a program.
//!
//! Every fact keeps its evidence (source locations) and its origin, so later
//! layers can separate what was established from what was inferred.

use crate::pred::Pred;
use merak_ir::Loc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Established by deterministic analysis of the code.
    Static,
    /// Mined from patterns in the code; carries a confidence below 1.
    Inferred,
    /// Stated by the project in `merak.toml`.
    Declared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Function,
    Method,
    Constructor,
    Closure,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallFact {
    pub target: String,
    pub loc: Loc,
    /// The call only happens on some paths (inside an `if`, loop, or `catch`).
    pub conditional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EffectKey {
    /// `db_read`, `db_write`, `http`, `event_emit`, `fs_read`, `fs_write`, `cache`, `queue` …
    pub kind: String,
    /// Resource: table/model, host, event name, path …
    pub target: String,
    /// Optional operation detail, e.g. the HTTP method.
    pub method: Option<String>,
}

impl EffectKey {
    pub fn render(&self) -> String {
        match &self.method {
            Some(m) => format!("{} {} {}", self.kind, m, self.target),
            None => format!("{} {}", self.kind, self.target),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectFact {
    pub key: EffectKey,
    /// The external API that produced this effect, e.g. `axios.post()`.
    pub api: String,
    pub loc: Loc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteFact {
    /// `Type.field`
    pub field: String,
    /// Constant value written, if known (enum member values are resolved).
    pub value: Option<String>,
    /// Written as part of constructing a new object literal.
    pub creation: bool,
    pub loc: Loc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFail {
    Throw,
    Return,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardFact {
    /// What must hold for execution to continue past the guard.
    pub requires: Pred,
    pub on_fail: OnFail,
    pub loc: Loc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub event: String,
    pub handler: String,
    pub loc: Loc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub method: String,
    pub path: String,
    pub handler: String,
    pub loc: Loc,
}

impl Route {
    pub fn key(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unknown {
    pub question: String,
    pub loc: Loc,
}

/// Facts established directly from one entity's own body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub kind: EntityKind,
    pub module: String,
    pub class: Option<String>,
    pub parent: Option<String>,
    pub loc: Loc,
    pub end_line: u32,
    pub body_hash: String,
    pub calls: Vec<CallFact>,
    pub effects: Vec<EffectFact>,
    pub reads: BTreeMap<String, Loc>,
    pub writes: Vec<WriteFact>,
    pub guards: Vec<GuardFact>,
    /// For boolean predicate functions: what the return value means.
    pub returns: Option<Pred>,
    pub throws: bool,
    pub subscriptions: Vec<Subscription>,
    pub routes: Vec<Route>,
    pub unknowns: Vec<Unknown>,
    /// Every call as `target(argument shape)`. Covers what the typed facts above do
    /// not, so a transition never calls an entity unchanged when its calls changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_shapes: Vec<CallShape>,
}

/// A call not classified by the catalog, as `target(argument shape)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallShape {
    pub shape: String,
    /// The called entity, for program-internal calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub loc: Loc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Mode {
    Sync,
    /// Happens in a handler of the named event.
    Async,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectInfo {
    /// How the effect is reached: `sync`, or `async via <Event>`.
    pub modes: BTreeSet<String>,
    /// First hop from this entity (`direct` or the callee/event it goes through).
    pub via: BTreeSet<String>,
    /// Where the effect actually happens.
    pub evidence: BTreeSet<Loc>,
}

/// Transitive behaviour of an entity: its own facts plus everything reached.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    #[serde(with = "pairs")]
    pub effects: BTreeMap<EffectKey, EffectInfo>,
    /// Validator entities called on the way (directly or transitively).
    pub validations: BTreeMap<String, Loc>,
    /// `Type.field` written / read anywhere below this entity.
    pub writes: BTreeSet<String>,
    pub reads: BTreeSet<String>,
    /// Own guards with predicate calls expanded.
    pub requires: Vec<(String, Pred, Loc)>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StateEdge {
    /// Source states: concrete values, `*` (unguarded) or `∅` (on creation).
    pub from: BTreeSet<String>,
    pub to: String,
    pub writer: String,
    pub loc: Loc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateMachine {
    pub field: String,
    pub states: BTreeSet<String>,
    pub edges: Vec<StateEdge>,
}

impl StateMachine {
    pub fn sources_of(&self, to: &str) -> BTreeSet<String> {
        self.edges.iter().filter(|e| e.to == to).flat_map(|e| e.from.iter().cloned()).collect()
    }

    pub fn targets(&self) -> BTreeSet<String> {
        self.edges.iter().map(|e| e.to.clone()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invariant {
    /// Stable key used to match across versions.
    pub key: String,
    /// Human-readable statement.
    pub statement: String,
    /// State write the invariant is about (`Type.field`, value).
    pub field: String,
    pub value: String,
    pub requirement: Requirement,
    pub support: usize,
    pub confidence: f64,
    pub evidence: Vec<Loc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Requirement {
    /// Another field must be written with a non-null value.
    CoWrite(String),
    /// A guard (by key) must hold before the write.
    Guard(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Violation {
    /// `layer` or `ownership`.
    pub kind: String,
    pub subject: String,
    pub detail: String,
    pub evidence: Vec<Loc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub entities: BTreeMap<String, Entity>,
    /// class entity id → (name, module, method ids)
    pub classes: BTreeMap<String, ClassInfo>,
    pub summaries: BTreeMap<String, Summary>,
    pub validators: BTreeSet<String>,
    pub state_machines: BTreeMap<String, StateMachine>,
    pub routes: Vec<Route>,
    pub subscriptions: Vec<Subscription>,
    /// (from module, to module) → call sites.
    #[serde(with = "pairs")]
    pub module_deps: BTreeMap<(String, String), Vec<Loc>>,
    pub invariants: Vec<Invariant>,
    pub violations: Vec<Violation>,
    pub module_layers: BTreeMap<String, String>,
    /// Finite value domains of written enum / literal-union fields (`Type.field`).
    pub field_domains: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassInfo {
    pub id: String,
    pub name: String,
    pub module: String,
    pub methods: Vec<String>,
    pub loc: Loc,
}

impl Model {
    /// Entities that (transitively) call `id`, including `id` itself.
    pub fn callers_closure(&self, id: &str) -> BTreeSet<String> {
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for e in self.entities.values() {
            for c in &e.calls {
                reverse.entry(c.target.as_str()).or_default().push(e.id.as_str());
            }
            // An entity with a closure that reaches `id` is also affected.
            if let Some(p) = &e.parent {
                reverse.entry(e.id.as_str()).or_default().push(p.as_str());
            }
        }
        let mut seen = BTreeSet::new();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(callers) = reverse.get(cur.as_str()) {
                stack.extend(callers.iter().map(|s| s.to_string()));
            }
        }
        seen
    }

    /// HTTP entry points whose handlers reach `id`.
    pub fn entry_points_reaching(&self, id: &str) -> Vec<String> {
        let callers = self.callers_closure(id);
        let mut out: Vec<String> = self.routes.iter().filter(|r| callers.contains(&r.handler)).map(|r| r.key()).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Serialize maps with non-string keys as `[[key, value], …]` so they fit JSON.
mod pairs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(map: &BTreeMap<K, V>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(map.iter())
    }

    pub fn deserialize<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<K, V>, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}
