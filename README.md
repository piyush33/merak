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

### In Claude Code

[`plugins/merak`](plugins/merak) is a Claude Code plugin: an MCP server (`merak_transition`,
`merak_context`), a `/merak:review` skill, and a per-turn hook. When Claude finishes a turn
that changed behaviour, the hook hands it this turn's transition once, to check against what
was asked:

```text
Merak: semantic transition of this turn's edits (1 behavioural/design change(s)):
- AUTH_WIDENED OrderPolicy.canCancel: User.role ∈ {ADMIN} → User.role ∈ {ADMIN, MANAGER} [affects POST /orders/:id/cancel] (src/policies/order.policy.ts:4)
```

```sh
cargo install --path crates/merak-cli
claude plugin marketplace add /path/to/merak && claude plugin install merak@merak
```

The CLI also has `merak mcp` (stdio MCP server), `merak snapshot --out f` and
`merak diff --since f` (diff a snapshot against the working tree).

## What it detects

| Layer | Operations |
|---|---|
| Behaviour | `AUTH_WIDENED/NARROWED/CHANGED`, `GUARD_ADDED/REMOVED/CHANGED`, `VALIDATION_ADDED/REMOVED`, `EFFECT_ADDED/REMOVED`, `EFFECT_MADE_ASYNC/SYNC`, `STATE_TRANSITION_ADDED/REMOVED/SOURCE_WIDENED/SOURCE_NARROWED`, `DATAFLOW_EXPANDED/RESTRICTED`, `QUERY_FILTER_ADDED/REMOVED/CHANGED`, `SCHEMA_FIELD_ADDED/REMOVED/CHANGED`, `SCHEMA_CHANGED`, `BEHAVIOUR_ADDED`, `ENTRYPOINT_ADDED/REMOVED`, `EVENT_HANDLER_ADDED/REMOVED` |
| Design | `INVARIANT_VIOLATED` (inferred), `LAYER_VIOLATION_INTRODUCED/RESOLVED`, `OWNERSHIP_VIOLATION_INTRODUCED/RESOLVED` (declared) |
| Structure | `ENTITY_ADDED/REMOVED/MOVED/RENAMED`, `DEPENDENCY_ADDED/REMOVED`, `LOGGING_CHANGED`, `PURE_REFACTOR` |
| Residual | `UNCLASSIFIED_CHANGE`: calls changed in a way none of the above explains |

Effects are **transitive** and follow event subscriptions. Moving a refund from a
direct call into an `OrderCancelled` handler is reported as
`EFFECT_MADE_ASYNC` (`http POST payments.example.com`), not as "refund removed".
Extracting a helper or renaming a file is reported as a `PURE_REFACTOR` with no
behavioural operations.

`PURE_REFACTOR` is a claim, so Merak only makes it when it can back it up. Every
call that the catalog does not classify is also kept as a *call shape*: the target,
plus constant arguments and object-literal keys, plus the *path conditions* it runs
under (enclosing `if`s and earlier `return`/`throw`/`continue`), in canonical form, so
`if (c) { f() }` and `if (!c) continue; f()` agree while a condition added around a call does not. A changed entity whose shapes
differ, and which no typed op explains, is reported as `UNCLASSIFIED_CHANGE`
(confidence 0.5) rather than counted as a refactor:

A query predicate is behaviour too: which rows a query reaches. Here, from immich,
a person query that used to match only visible faces now matches faces whose visibility is unset as well:

```text
query filter changed  PersonRepository.getAllWithoutFaces
  before:  asset_face.isVisible is true
  after:   (asset_face.isVisible is null ∨ asset_face.isVisible = true)
```

When nothing typed explains a change, Merak says so:

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
- **Query filters** are the predicates a query builder applies: `where`, `whereRef`, `having`, join `on`/`onRef`, and expression-builder forms (`eb(col, op, v)`, `eb.and/or/not/exists`). They are rendered canonically (`asset.ownerId = _`, `(a ∨ b)`, `exists(album_asset where album_asset.albumId = album.id)`), with constants and enum members resolved and other values shown as `_`. A predicate passed as a callback (`where((eb) => …)`), or kept in a local, is rendered into the call it is passed to. Filters belong to the top-level function, closures included, so a new `$if(cond, (qb) => qb.where(…))` narrows the method's query. Untyped callback parameters (`qb`, `eb`, `join`, `trx`) get their builder type from `[[callback]]` catalog rules, so subqueries inside them count as reads. CTE names are not tables.
- **Access requirements** are object-argument keys such as `permission`, `role`, `scope` or `admin` at calls (`requireAccess({ auth, permission: Permission.AlbumRead })`) and decorators (`@Authenticated({ permission: … })`), with `public` counted as a grant. An entity's requirements are its own plus the constant ones of everything it reaches. So when a helper starts taking the permission from its callers, only the callers whose permission actually changed are reported. Dropping a requirement or adding a grant is `AUTH_WIDENED`, the reverse is `AUTH_NARROWED`. Decorator-only changes count even though the body is unchanged. The keys are `[access]` in the catalog.
- **Validation schemas** are module constants built from a schema library (`zod`, configurable under `[schema]`): each field is rendered as its constraints (`name: string().regex(/^[^/]*$/)`, `page: coerce.number().int().min(1).optional()`), following shape constants, `.extend({…})` and schemas built from other schemas, with documentation calls (`describe`, `meta`, …) left out. Refinement closures are named after their schema (`SharedLinkCreateSchema.superRefine`).
- **New code** is described, not only listed: an added function or method gets a `BEHAVIOUR_ADDED` op with the effects it reaches (writes first, leaving out the reads of the access checks it calls), its access requirements, query filters, guards, validations and fields written. A helper that was only extracted from unchanged callers is not described, so `PURE_REFACTOR` still holds.
- **Logging** (`console.*`, `Logger`, `*Logger` / `LoggingRepository` classes) is kept apart from behaviour: a changed log call is a structural `LOGGING_CHANGED`, and it prevents a `PURE_REFACTOR` claim.
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
| model | 4.0k entities, 303 routes, 709 module dependencies, 534 DB effects, 942 query filters |
| verdicts | 23 no change (test-only commits), 21 typed behaviour, 13 unclassified only, 2 pure refactor, 1 logging only |

Modelling query predicates (M5b) cut `UNCLASSIFIED_CHANGE` from 53 to 45 ops, and moved 4 commits from
"unclassified only" to typed behaviour: 18 query filter ops, among them an ownership check that moved
from a `person` subquery to `asset.ownerId`, and a widened visibility filter. It also made subquery
reads visible (DB effects in the model rose from 363 to 534). Access requirements and logging (M5b′) took
it to 36: permission changes such as `asset.share → asset.update` on `PUT /memories/:id/assets` are now
`AUTH_CHANGED`, and a new `album.read` check on three search endpoints is `AUTH_NARROWED`. What remains is
mostly behaviour Merak does not model yet: response headers, raw SQL maintenance, image pipelines,
ordering and pagination. The comparison below then found two false `PURE_REFACTOR` claims; adding path
conditions to call shapes fixed both (unclassified rose to 43, pure refactors fell from 4 to 2).

## Comparison with sem and inspect

`scripts/compare.py` runs Merak, [sem](https://github.com/Ataraxy-Labs/sem) and
[inspect](https://github.com/Ataraxy-Labs/inspect) over the same commits, all in local,
deterministic modes (`sem diff --format json` without cloud, `inspect diff --format json`
without an LLM), scoped to the same code (non-test server TypeScript). The ground truth is
`scripts/labels/immich.json`: 103 behavioural findings in the 39 immich commits that change server
code. The labels were written from the raw diffs by reviewers who were not told about Merak, so
they are not shaped by what Merak models.

```sh
scripts/compare.py run ~/src/immich --root server -n 60 --out runs/compare --sem …/sem --inspect …/inspect
scripts/compare.py score runs/compare --labels scripts/labels/immich.json -v
```

| per labeled finding (n=103) | |
|---|---|
| sem lists the entity as changed | 97% |
| inspect lists the entity | 97% |
| inspect rates it High/Critical | 12% |
| inspect ranks it in its top 3 | 37% |
| **Merak names what changed** (typed op) | **40%** |
| Merak describes the new code it is in (`BEHAVIOUR_ADDED`) | 21% |
| Merak flags it without a type (`UNCLASSIFIED_CHANGE`) | 27% |
| Merak misses it | 12% |

| | Merak | sem | inspect |
|---|---|---|---|
| median latency | 0.58 s | 0.01 s | 0.96 s |
| items per commit with code changes (median) | 1 op | 4 entities | 4 entities |
| "no behaviour change" claims | 2, both correct (0 false) | none | none |

The tools answer different questions. sem says *which* entities changed, almost always correctly and
fast. inspect ranks them by structural risk: blast radius and change classification. On this set its
ranking rarely matched where the behaviour actually changed, for example `getAllWithoutFaces` widening
the faces it returns was rated Medium, 6th of 14. Merak says *what* changed (`asset_face.isVisible is true →
(isVisible is null ∨ isVisible = true)`, `permission=asset.share → asset.update` on `PUT /memories/:id/assets`)
and is the only one to make a checked "no behaviour change" claim.

At M5c, 39% of the findings were missed: 23 in new code (only `ENTITY_ADDED`), 9 in module-level
zod schemas, 5 in changed entities with no op and 3 in migrations. Describing new code and modelling
schemas (M6) brought misses to 12%. A "described" finding is a weaker match than a typed one: it sits
in new code whose description lists effects, requirements and guards, not an op about that finding.
What remains: handler maps and default values (5), migrations and table definitions (3), a response
mapper, a promise helper, and two labels that are debatable (OpenAPI metadata).

Parsing is about 85 ms per version and analysis about 100 ms. That is why there is no
incremental store yet: at this size a cache would save little and add a
correctness risk. `MERAK_TIMINGS=1` prints the phase timings.

## Roadmap

- [x] **M0–M4** TS front end, behaviour model, transition engine, inferred invariants, declared design rules, golden suite
- [x] **M5a** Real-repository benchmark (immich server): tsconfig path aliases, inherited injected fields, Kysely and NestJS models, one-process git loading, precompiled catalog (full diff 19.7 s → 0.5 s), guard canonicalisation, `UNCLASSIFIED_CHANGE` instead of false `PURE_REFACTOR`
- [x] **M5b** Query-builder predicates: Kysely `where`/`whereRef`/`having`/join `on` and expression-builder filters as `QUERY_FILTER_ADDED/REMOVED/CHANGED`, callback parameter typing, subquery reads, CTEs (`UNCLASSIFIED_CHANGE` 53 → 45 on immich)
- [x] **M5b′** Access requirements from call/decorator arguments as `AUTH_*` ops (including decorator-only changes); logging as `LOGGING_CHANGED` (`UNCLASSIFIED_CHANGE` 45 → 36)
- [ ] Residue: 3-argument joins (`innerJoin(t, a, b)`) as filters, ordering/pagination, enforcement mode (`requireAccess` throws, `checkAccess` filters), response headers
- [x] **M5c** Comparison against sem/inspect on the same commits, with independently labeled ground truth (`scripts/compare.py`, `scripts/labels/immich.json`); fixed two false `PURE_REFACTOR` claims with path conditions on call shapes
- [x] **M6** New code described (`BEHAVIOUR_ADDED`) and zod validation schemas modelled (`SCHEMA_*`): misses 39% → 12% on the labeled comparison, still no false `PURE_REFACTOR`
- [ ] Residue: migrations (SQL data changes), handler maps and default values, `UNCLASSIFIED_CHANGE` on ordering/pagination and response headers
- [ ] Incremental store, only once a repository is large enough to need it
- [x] Claude Code plugin: MCP server (`merak_transition`, `merak_context`), `/merak:review` skill, and a per-turn ledger (UserPromptSubmit snapshot, Stop review once per turn); verified in live headless sessions
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
