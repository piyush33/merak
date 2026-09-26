---
name: review
description: Review the behavioural effect of code changes with Merak's semantic transition (auth, query filters, effects, validation schemas, state transitions, new code). Use when the user asks what a change does, wants a behaviour-level review of uncommitted work, a branch or a commit range, or before committing or opening a PR.
---

# Behaviour review with Merak

1. Call the `merak_transition` MCP tool (from this plugin) for the change under review. Use the tool, not a shell command: there is no `npx merak`. Defaults are the uncommitted changes against `HEAD`; pass `base`/`head` for a branch or range (`base: "main"`), and `root` in a monorepo (for example `server`).
2. Report, most important first:
   - **Access**: `AUTH_WIDENED`, `AUTH_CHANGED`, `GUARD_REMOVED`, `VALIDATION_REMOVED`, loosened `SCHEMA_FIELD_*`. Say who can now do what, and which routes (`affects`) are exposed.
   - **Data scope**: `QUERY_FILTER_REMOVED/CHANGED` (a query can now reach more rows), `DATAFLOW_*`.
   - **Effects**: `EFFECT_ADDED/REMOVED`, `EFFECT_MADE_ASYNC`, new routes and event handlers.
   - **Design**: `INVARIANT_VIOLATED`, layer or ownership violations.
   - **New code**: `BEHAVIOUR_ADDED` descriptions. Point out new code that writes data without an access requirement or guard.
3. For each `UNCLASSIFIED_CHANGE`, read the evidence lines and say in one sentence what changed. Merak could not type it, so do not skip it.
4. Only if the transition is `PURE_REFACTOR` (and nothing else), say that no behaviour changed.
5. Cite `file:line` evidence from the ops. Use `merak_context` on an entity to explain its callers, routes or effects when that helps.

Keep it short: a reviewer should see in a few lines what behaviour changed and what to double-check.
