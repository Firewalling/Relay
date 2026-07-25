## Context

Relay is a new local-first coordination CLI. The MVP must preserve a small amount of high-value session context across terminal agents without a daemon, cloud service, shell interception, runtime OpenSpec dependency, or automatic Git mutation. It must work in ordinary directories as well as Git worktrees, and its on-disk records must remain readable and recoverable.

## Goals / Non-Goals

**Goals:**

- Deliver one `relay` Rust binary with clear CLI, application, storage, Git, and presentation boundaries.
- Keep one project-local session record plus an append-only event history, checkpoint snapshots, handoffs, and a final summary.
- Make mutable JSON state updates atomic and report corruption rather than panicking or silently replacing data.
- Query Git through its stable command-line interface without changing repository state.
- Make handoff content explicit: users can provide completed work, decisions, failures, and next action rather than relying on inferred transcript meaning.

**Non-Goals:**

- A daemon, TUI, shell framework, command interception, hosted synchronization, model integration, agent execution, permission sandbox, plugin architecture, or automatic Git commit/stash/push.
- Multiple simultaneous active sessions within one `.relay/` directory.
- Perfect semantic inference from source files or chat transcripts.

## Decisions

### One project-local state directory

Commands operate on the current directory and use `.relay/session.json`, `.relay/events.jsonl`, `.relay/checkpoints/`, and `.relay/handoffs/`. A completed session remains preserved; a subsequent `start` may replace only the top-level session pointer while retaining timestamped artifacts and append-only events. This is simpler and more inspectable than SQLite or a global registry.

Alternative considered: search parent directories for `.relay/`. Rejected for the MVP because explicit current-directory scope is more predictable for scripts and avoids accidentally mutating a parent project.

### Typed JSON state and extensible JSON Lines events

`session.json` is a versioned typed document. Each event has a schema version, event id, timestamp, event type string, session id, and JSON payload. Event names use stable dotted identifiers such as `session.started`, leaving future event types additive. Markdown is generated only for human-facing handoffs and summaries.

Alternative considered: store everything as Markdown. Rejected because lifecycle checks and scripting require reliable structured fields.

### Safe replacement for mutable files

Relay writes mutable JSON and Markdown through a temporary file in the destination directory, flushes and syncs it, then persists it by rename. Existing malformed `session.json` causes an actionable error and is never overwritten. Event appends serialize a complete line, append it in one write, flush, and sync; malformed events are detected with a line number.

Alternative considered: direct truncate-and-write. Rejected because interruption could destroy the only active session record.

### Git integration through subprocesses

A small Git module invokes `git rev-parse`, `git status --porcelain`, and related read-only commands. Nonzero results indicating a non-repository become an available=false snapshot rather than a fatal error. Relay never calls Git commands that mutate history or the worktree.

Alternative considered: `git2`. Rejected for this slice because it increases compile and API complexity and can differ from command-line Git behavior.

### Explicit handoff note options

`relay handoff <agent>` accepts repeatable completed-work, decision, and failure options plus an optional suggested next action. These notes are stored in the handoff event and rendered with session/checkpoint/Git context. Missing sections say “Not recorded” rather than inventing conclusions.

Alternative considered: automatic summarization. Rejected because the MVP has no model integration and must not imply understanding it does not possess.

## Risks / Trade-offs

- [Concurrent Relay writers can race] → Use atomic state replacement; document that cross-process locking is deferred and reject invalid lifecycle state after every fresh read.
- [A process can be interrupted during event append] → Sync complete event lines and detect/report malformed lines; do not silently ignore corruption.
- [Git output has edge cases] → Use porcelain output and integration tests with spaces and non-Git directories; preserve paths as text without semantic analysis.
- [Top-level `session.json` represents only the latest session] → Preserve historical events and timestamped artifacts; full session archives are a later milestone.
- [Absolute project paths can be private] → Do not include the project path in generated handoffs or summaries; show only Git metadata and changed paths relative to the repository.

## Migration Plan

No migration is required for a new project. The session document includes a schema version so later releases can reject or migrate unsupported formats explicitly. Rollback consists of using the previous binary; Relay never modifies Git history or source files as part of lifecycle commands.

## Open Questions

- Should the next milestone add explicit `relay note` commands or keep note capture attached to handoff/checkpoint operations?
- Should project discovery later walk upward to a `.relay/` directory, and if so, how should nested projects opt out?
- Is advisory file locking required before persistent multi-terminal operation, or is optimistic atomic replacement sufficient for the next slice?
