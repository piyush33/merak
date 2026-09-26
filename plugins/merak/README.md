# Merak for Claude Code

Gives Claude Code independent, typed evidence of what its edits did to your program's
behaviour: access rules, query filters, effects, validation schemas, state transitions, new code.

## What you get

- **Per-turn review (hooks).** When you send a prompt, Merak snapshots the project's sources.
  When Claude finishes the turn, Merak derives the semantic transition since that snapshot. If
  behaviour changed, Claude gets the transition once, with evidence and affected routes, and is
  asked to check it against your request before it stops:

  ```text
  Merak: semantic transition of this turn's edits (1 behavioural/design change(s)):
  - AUTH_WIDENED OrderPolicy.canCancel: User.role ∈ {ADMIN} → User.role ∈ {ADMIN, MANAGER} [affects POST /orders/:id/cancel] (src/policies/order.policy.ts:4)
  ```

  Refactors, logging-only and test-only turns stay silent. Claude Code labels any Stop hook that
  continues a turn as "Stop hook error" in its UI; for Merak that is the review, not a failure.
- **MCP tools.** `merak_transition` (a change's transition: uncommitted work by default, or
  `base`/`head`, with `root` for a subdirectory) and `merak_context` (what one function does:
  effects, access requirements, filters, guards, routes that reach it, callers).
- **`/merak:review`**: a behaviour-level review of the current changes, a branch or a range.

## Install

Build the binary and put it on your `PATH` (or set `MERAK_BIN`):

```sh
cargo install --path crates/merak-cli
```

Then add the repository as a plugin marketplace and install the plugin:

```sh
claude plugin marketplace add /path/to/merak
claude plugin install merak@merak
```

For development, `claude --plugin-dir plugins/merak` loads it for one session (it also finds
`target/release/merak` in this repository).

Both MCP tools are read-only. To skip their permission prompts, allow them in your settings:
`mcp__plugin_merak_merak__merak_transition`, `mcp__plugin_merak_merak__merak_context`.

## Settings

- `MERAK_HOOK=off` disables the per-turn review.
- `MERAK_BIN=/path/to/merak` selects the binary.
- Snapshots live in the plugin's data directory (`~/.claude/plugins/data/merak*/snapshots`),
  one per session, and are pruned after 7 days.

Merak analyses TypeScript/JavaScript today. See the [main README](../../README.md) for what it
models and how accurate it is.
