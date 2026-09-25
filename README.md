# Merak

**Semantic transitions for code changes.** Merak tells you what a change did to a
program's *behaviour and design*, not which lines moved.

```diff
- return user.role === "ADMIN";
+ return user.role === "ADMIN" || user.role === "MANAGER";
```

```text
auth widened  OrderPolicy.canCancel
  before:  User.role ∈ {ADMIN}
  after:   User.role ∈ {ADMIN, MANAGER}
  affects: POST /orders/:id/cancel
  evidence: src/policies/order.policy.ts:4
```

AI agents make code cheap to write and expensive to verify. A textual diff is the
wrong unit for that verification, and asking one LLM to review another LLM's diff
leaves out any independent evidence. Merak compiles source code into a **Program
Behaviour Model** (PBM), then derives the **semantic transition** between two
versions. That transition is a list of typed operations, and each one carries
evidence (`file:line`), an origin (`static` / `inferred` / `declared`) and a
confidence.

The snapshot model is only the substrate. **The transition is the product.**

## Status

Early (v0). The engine is written in Rust, and TypeScript/JavaScript is the first
supported source language. Integrations (a Claude Code plugin and a GitHub PR
comment) come after the transitions are trustworthy. See [Roadmap](#roadmap).

## Quick start

```sh
cargo build --release
# Two git revisions of the repository in the current directory
./target/release/merak diff main..my-branch
# Two directories
./target/release/merak diff --before ./v1 --after ./v2
# Machine-readable
./target/release/merak diff main.. --json
# The behaviour model itself
./target/release/merak model ./src-root
```

## What it detects

| Layer | Operations |
|---|---|
| Behaviour | `AUTH_WIDENED/NARROWED/CHANGED`, `GUARD_ADDED/REMOVED/CHANGED`, `VALIDATION_ADDED/REMOVED`, `EFFECT_ADDED/REMOVED`, `EFFECT_MADE_ASYNC/SYNC`, `STATE_TRANSITION_ADDED/REMOVED/SOURCE_WIDENED/SOURCE_NARROWED`, `DATAFLOW_EXPANDED/RESTRICTED`, `ENTRYPOINT_ADDED/REMOVED`, `EVENT_HANDLER_ADDED/REMOVED` |
| Design | `INVARIANT_VIOLATED` (inferred), `LAYER_VIOLATION_INTRODUCED/RESOLVED`, `OWNERSHIP_VIOLATION_INTRODUCED/RESOLVED` (declared) |
| Structure | `ENTITY_ADDED/REMOVED/MOVED/RENAMED`, `DEPENDENCY_ADDED/REMOVED`, `PURE_REFACTOR` |
| Residual | `UNCLASSIFIED_CHANGE`: calls changed in a way none of the above explains |

Effects are **transitive** and follow event subscriptions. Moving a refund from a
direct call into an `OrderCancelled` handler is reported as
`EFFECT_MADE_ASYNC` (`http POST payments.example.com`), not as "refund removed".
Extracting a helper or renaming a file is reported as a `PURE_REFACTOR` with no
behavioural operations.

`PURE_REFACTOR` is a claim, so Merak only makes it when it can back it up. Every
call that the catalog does not classify is also kept as a *call shape*: the target,
plus constant arguments and object-literal keys. A changed entity whose shapes
differ, and which no typed op explains, is reported as `UNCLASSIFIED_CHANGE`
(confidence 0.5) rather than counted as a refactor:

```text
unclassified change  AuthService.changePassword
  before:  UserRepository.update(_, {password})
  after:   UserRepository.update(_, {password, shouldChangePassword="false"})
  affects: POST /auth/change-password
```

## How it works

```text
source ──► merak-front-ts ──► Structural IR ──► merak-behaviour ──► PBM(A), PBM(B) ──► merak-transition ──► ops
            (oxc parser)       (merak-ir)        resolve · extract       │                 match · diff
                                                 summarise · infer       └─ merak.toml (declared design)
```

| Crate | Role |
|---|---|
| `merak-ir` | Language-neutral structural IR (functions, classes, statements, expressions, types) |
| `merak-front-ts` | Lowers TS/JS into the IR, using [oxc](https://github.com/oxc-project/oxc) |
| `merak-behaviour` | Symbol/type/call resolution, fact extraction, the effect catalog, transitive summaries, state machines, invariant mining, design rules |
| `merak-transition` | Entity matching across versions (including moves and renames), transition ops, Markdown rendering |
| `merak-cli` | `merak model`, `merak diff` (directories or git revisions) |

### Facts in the Program Behaviour Model

- **Calls** are resolved through constructor-injected fields (including inherited ones), parameter and local types, imports (relative, or through tsconfig `paths` / `baseUrl`), and superclasses.
- **Effects** come from `catalog.toml`, a mapping from external APIs to effects: Prisma, TypeORM, Kysely, fetch, axios, EventEmitter, fs, Redis, BullMQ. The mapping is seeded from CodeQL's JS framework models. Projects can extend it under `[catalog]` in `merak.toml`.
- **Entry points** are Express route registrations and NestJS decorators (`@Controller` + `@Get`/`@Post`/…, `@OnEvent`). Decorator rules are ordinary `[[route]]` / `[[subscribe]]` catalog entries without a `handler_arg`, so project decorators can be declared the same way.
- **Guards** are early-exit `if (…) throw|return` checks, normalised into predicates (`User.role ∈ {ADMIN}`, `Order.status ∈ {PENDING, CONFIRMED}`). Calls to predicate functions such as policies are expanded. Equivalent spellings compare equal: a trailing `if (c) { … }` and `if (!c) return; …`, `!(a <= b)` and `a > b`, and `xs.length > 0` and `xs.length !== 0`.
- **State machines** are derived from enum/literal field writes plus the state guards that protect them. `∅` means "on creation" and `*` means "unguarded".
- **Invariants** are *inferred* and carry a confidence. Two kinds are mined: co-writes (`Order.status := PAID ⇒ Order.paymentId is set`) and required guards (`Order.status := CANCELLED requires User.role ∈ {ADMIN}`).
- **Design** is *declared* in `merak.toml`:

```toml
[layers]
controller = ["src/controllers/**"]
service = ["src/services/**"]
repository = ["src/repositories/**"]

[[rules]]
from = "controller"
deny = ["repository"]

[ownership]
"Order.status" = ["src/services/**"]
```

When resolution fails (a dynamic target, an unresolved call), Merak records an
explicit `Unknown` instead of guessing. Test code (`*.spec.*`, `*.test.*`, `test/`,
`__tests__/`, `__mocks__/`) is not analyzed.

## Golden scenarios

`fixtures/orders-demo/` contains a small order service and eight scenario changes.
Each one pairs an `overlay/` with an `expected.json`. `cargo test` checks the
derived transition for each:

| Scenario | Expected transition |
|---|---|
| 01 managers can cancel | `AUTH_WIDENED` |
| 02 controller bypasses the service | `STATE_TRANSITION_SOURCE_WIDENED`, `INVARIANT_VIOLATED`, `LAYER_VIOLATION_INTRODUCED`, `OWNERSHIP_VIOLATION_INTRODUCED`, `ENTRYPOINT_ADDED` |
| 03 notify warehouse | `EFFECT_ADDED http POST warehouse.example.com` |
| 04 remove inventory validation | `VALIDATION_REMOVED` |
| 05 cancel confirmed orders | `STATE_TRANSITION_SOURCE_WIDENED {PENDING} → {CONFIRMED, PENDING}` |
| 06 force PAID without payment | `INVARIANT_VIOLATED Order.status := PAID ⇒ Order.paymentId is set` |
| 07 refund via event | `EFFECT_MADE_ASYNC`, `EVENT_HANDLER_ADDED` |
| 08 rename file + extract helper | `PURE_REFACTOR` only |

## Benchmark

`scripts/bench.py` runs `merak diff` over the recent commits of a real repository.
Here are the numbers on the last 60 commits touching the
[immich](https://github.com/immich-app/immich) server (NestJS + Kysely, about 59k lines of
non-test TypeScript), measured on an M-series laptop. Each diff runs the full pipeline for both
versions with no cache:

```sh
scripts/bench.py ~/src/immich --root server -n 60
```

| | |
|---|---|
| latency per commit diff | median 0.48 s, p90 0.51 s, max 0.95 s |
| model | 4.0k entities, 303 routes, 709 module dependencies, 363 DB effects |
| verdicts | 23 no change (test-only commits), 20 unclassified only, 13 typed behaviour, 4 pure refactor |

Parsing is about 85 ms per version and analysis about 100 ms. That is why there is no
incremental store yet: at this size a cache would save little and add a
correctness risk. `MERAK_TIMINGS=1` prints the phase timings.

## Roadmap

- [x] **M0–M4** TS front end, behaviour model, transition engine, inferred invariants, declared design rules, golden suite
- [x] **M5a** Real-repository benchmark (immich server): tsconfig path aliases, inherited injected fields, Kysely and NestJS models, one-process git loading, precompiled catalog (full diff 19.7 s → 0.5 s), guard canonicalisation, `UNCLASSIFIED_CHANGE` instead of false `PURE_REFACTOR`
- [ ] **M5b** Model query-builder predicates (Kysely `where`/joins as typed `QUERY_FILTER_ADDED/REMOVED`), the largest remaining source of `UNCLASSIFIED_CHANGE`
- [ ] **M5c** Comparison against sem/inspect on the same commits
- [ ] Incremental store, only once a repository is large enough to need it
- [ ] Claude Code plugin: an MCP server (`merak_transition`, `merak_context`) and a PostToolUse hook that keeps a per-turn transition ledger
- [ ] GitHub Action: semantic transition as a PR comment
- [ ] Intent layer: task/PR description → expected transition (LLM), compared against the actual transition
- [ ] More languages through tree-sitter front ends (Python next)

## Related work

Merak builds on ideas from [Joern](https://github.com/joernio/joern) (code property
graphs), [CodeQL](https://github.com/github/codeql) (framework models, dataflow),
[sem](https://github.com/Ataraxy-Labs/sem) (entity-level diff and impact),
[Aura](https://github.com/Naridon-Inc/aura) (a semantic plane over Git),
[Understand Anything](https://github.com/Egonex-AI/Understand-Anything)
(deterministic facts plus LLM enrichment) and GumTree/difftastic (AST matching).
Where they stop at entities, dependencies or intent, Merak focuses on *behavioural
transitions* derived mechanically from ordinary code.

## License

Dual-licensed under MIT or Apache-2.0.
