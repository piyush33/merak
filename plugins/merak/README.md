# Merak for Claude Code

Gives Claude Code independent, typed evidence of what its edits did to your program's
behaviour: access rules, query filters, effects, validation schemas, state transitions, new code.

## What you get

- **Per-turn review (hooks).** When you send a prompt, Merak snapshots the project's sources.
  When Claude finishes the turn, Merak derives the semantic transition since that snapshot. If
  behaviour changed, Claude gets the transition once, with evidence and affected routes, and is
  asked to check it against your request before it stops:

  ```text
  Merak checked what this turn's edits do. 1 behaviour change the user should know about:
  OrderPolicy.canCancel · changed
    ~ who       User.role ∈ {ADMIN} → User.role ∈ {ADMIN, MANAGER}  (widened)  src/policies/order.policy.ts:5
  ```

  Only typed changes interrupt Claude. Refactors, logging, tests, and turns whose only residue
  is `UNCLASSIFIED_CHANGE` (UI code and anything else Merak does not model) stay silent; ask
  with `/merak:review` or `merak_transition` to see those. Claude Code labels any Stop hook that
  continues a turn as "Stop hook error" in its UI; for Merak that is the review, not a failure.
- **MCP tools.** `merak_transition` (a change as behaviour contracts, before and after, with the code behind each change; `view: plain` or `ops` for the other presentations: uncommitted work by default, or
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

To try it for one session in any project, give the **absolute** path (a relative one is
resolved against the project you start Claude in, and a missing directory is skipped quietly):

```sh
cd /path/to/your/project
claude --plugin-dir /path/to/merak/plugins/merak
```

The launcher finds the binary through `MERAK_BIN`, `PATH`, `~/.cargo/bin` (where
`cargo install` puts it, often not on `PATH`) or `target/release` in this repository.
Check that the plugin is active with `/mcp` (a `plugin:merak:merak` server) or `/merak:review`.

Both MCP tools are read-only. To skip their permission prompts, allow them in your settings:
`mcp__plugin_merak_merak__merak_transition`, `mcp__plugin_merak_merak__merak_context`.

## Settings

- `MERAK_HOOK=off` disables the per-turn review.
- `MERAK_BIN=/path/to/merak` selects the binary.
- Snapshots live in the plugin's data directory (`~/.claude/plugins/data/merak*/snapshots`),
  one per session, and are pruned after 7 days.

Merak analyses TypeScript/JavaScript today. See the [main README](../../README.md) for what it
models and how accurate it is.
