## Context

The MVP is a single Rust binary with explicit CLI (`src/cli.rs`), application (`src/app.rs`), typed schema-v1 models (`src/model.rs`), synced atomic replacement and append-only JSONL (`src/storage.rs`), and read-only Git subprocesses (`src/git.rs`). It keeps the latest session in `.relay/session.json`, all events in `.relay/events.jsonl`, and timestamped checkpoint/handoff/summary artifacts. Valid schema-v1 state must remain usable; malformed and unsupported state is currently rejected rather than overwritten.

Daily multi-terminal use exposes four coupled weaknesses: no writer serialization, no durable per-session index, no diagnosis of interrupted multi-file updates, and output designed only for humans. This design extends those boundaries without replacing them. Relay remains local-first, file-based, synchronous, inspectable, and free of daemon, database, hosted, transcript, model-provider, or Git-mutation behavior.

## Goals / Non-Goals

**Goals:**

- Serialize all project mutations across processes with fail-fast actionable errors and automatic kernel-backed release.
- Make inconsistent or damaged state discoverable without changing it by default.
- Preserve every session as an addressable record and support safe continuation lineage.
- Provide stable versioned JSON and concise bounded agent context.
- Make nested-directory and monorepo behavior explicit while reporting only Relay-project Git changes.
- Preserve MVP commands, schema-v1 readability, privacy guarantees, atomic replacement, append-only events, and read-only Git behavior.
- Keep each implementation slice independently testable and fault-injection friendly.

**Non-Goals:**

- Daemonization, network synchronization, SQLite, TUI, hosted service, plugins, automatic agent execution, transcript inspection, LLM calls, semantic summarization, or automatic content invention.
- Multiple active sessions in one Relay project.
- Transactional filesystem semantics across arbitrary crashes; the operation marker makes partial progress diagnosable rather than pretending multi-file atomicity.
- Rewriting ambiguous or unsupported data during diagnosis or migration.
- Packaging ecosystems beyond a release binary, shell completions, CI, and documented install steps.

## Decisions

### Advisory project lock with persistent metadata

Every mutating command (`start`, `checkpoint`, `handoff`, `finish`, `resume`, `note`, and explicit doctor repair) opens `.relay/lock`, attempts an exclusive non-blocking OS advisory lock, writes owner metadata (PID, command, acquired UTC time), and holds the file handle for the complete mutation. A busy lock fails immediately with ownership details and a retry instruction. The lock file remains as an ordinary inspectable file; only the kernel lock determines ownership. Process exit, unwind, and normal return release the lock automatically when the guard is dropped. An abandoned metadata record is refreshed on the next successful acquisition, so no timeout can steal a live lock.

Alternative: lock-file creation/removal and age-based deletion. Rejected because PID reuse, clock skew, and races can delete a live lock. Alternative: wait indefinitely. Rejected because terminal agents need actionable failure.

### Operation markers for multi-file mutation diagnosis

A mutating operation that spans files first atomically writes `.relay/operations/<operation-id>.json` with schema version, command, session, timestamp, and ordered expected effects. It advances the marker after durable steps and removes it only after the final write and directory sync. Events and immutable artifacts remain authoritative; the marker records incomplete intent, not invented outcomes. `doctor` reports any remaining marker and checks which expected effects exist.

`doctor --repair` performs only provably lossless cleanup, such as removing a completed operation marker after every referenced effect and event is present and valid. It refuses truncation, deletion of ambiguous artifacts, reconstruction of user-authored text, lifecycle guessing, and unsupported-schema rewrites. Repair actions are locked, listed before execution in JSON/human output, and recorded as `doctor.repaired` when an active supported log can safely accept the event.

Alternative: transactional database or wholesale event replay. Rejected as unnecessary and less inspectable. Alternative: silently clean temp/orphan files. Rejected because interruption evidence is diagnostically valuable.

### Compatible file layout with per-session records

The layout becomes:

```text
.relay/
├── session.json                 # compatibility/current pointer document
├── events.jsonl                 # append-only project history
├── lock                         # advisory-lock handle + owner metadata
├── sessions/<session-id>.json   # durable typed record for every session
├── operations/<operation-id>.json
├── checkpoints/*.json
├── handoffs/*.md
└── summaries/<session-id>.md
```

Existing root-level `summary-<id>.md` references remain valid and are not moved automatically. On first successful locked mutation of valid MVP state, Relay materializes the matching `sessions/<id>.json` record without changing its schema-v1 meaning. New session fields are optional with serde defaults (for example `resumed_from`) so valid MVP documents remain decodable. `session.json` continues to contain the current session for compatibility; per-session files are updated atomically when that session changes. Events remain the append-only audit trail.

A session selector accepts an exact UUID or an unambiguous UUID prefix. `list` reads session records plus recoverable legacy sessions from `session.started` events, `show` reports one session, and `history` reports ordered events with filters/limits. Ambiguous or unsupported records are errors, not skipped data.

Alternative: SQLite. Rejected because the expected personal history is cleanly supported by bounded directory scans and JSONL, while files remain easy to inspect and back up.

### Resume creates lineage instead of rewriting history

`relay resume <session>` requires no active session and a valid completed source session. It starts a new active session with a new ID, copies the explicit goal, records `resumed_from`, captures fresh Git state, appends `session.resumed`, and preserves the source unchanged. Resuming an active, unknown, ambiguous, corrupt, or unsupported session is rejected.

Alternative: mark the completed record active again. Rejected because it destroys historical lifecycle truth and complicates artifact ownership.

### Stable JSON envelope distinct from storage schema

Global `--json` switches supported commands to exactly one JSON document on stdout. Success uses:

```json
{"output_schema_version":1,"ok":true,"command":"status","data":{}}
```

Failures in JSON mode use the same version and command with `ok:false` and a stable error object on stderr. Command data is typed and additive; field names and enum values are snake_case. IDs, timestamps, state, relative artifact paths, Git availability, and diagnostics are structured values rather than embedded prose. Storage `schema_version` and output `output_schema_version` are separate contracts.

Human output remains default. `handoff --json` returns structured data including Markdown and relative saved path, but never prints an absolute project root. JSON is supported for at least `start`, `status`, `checkpoint`, `handoff`, `finish`, and `doctor`, and is used consistently by new query commands.

Alternative: serialize human strings in a JSON wrapper. Rejected because it is not useful or stable for agents.

### Structured notes are events, not a parallel notebook

`relay note <category> [text]` accepts one source: positional text, `--stdin`, or `--editor`. With no positional text, piped stdin is accepted; an interactive terminal without text opens `$VISUAL`, then `$EDITOR`, then a documented fallback. Blank notes are rejected. Categories are `progress`, `decision`, `blocker`, `observation`, and `next_action`. A `note.recorded` event stores category and text; no separate mutable note file is required.

Handoffs, final summaries, and context select relevant notes from the current session in event order, map categories into existing sections without claiming unrecorded facts, and obey output limits. Explicit handoff flags remain supported and take precedence only as additional explicit entries.

### Upward discovery with explicit scope and Git path bounding

Relay distinguishes:

- invocation directory: where the command was launched;
- Relay project root: directory containing the selected `.relay/`;
- Git repository root: discovered read-only metadata boundary, which may be an ancestor.

For commands other than an unscoped new `start`, default `auto` discovery walks upward to the nearest `.relay/` but never crosses the current Git worktree root. In a non-Git tree it may walk to the filesystem root. If none exists, commands fail without creating state. `start` in `auto` mode reuses a discovered `.relay/`; otherwise it creates `.relay/` in the invocation directory. Global `--scope current` disables upward discovery. `--scope repository` uses the Git worktree root and fails outside Git. The selected Relay root is reported in human output only as `.` or a relative relation where safe, never as an absolute private path in generated artifacts or JSON.

Git branch/commit refer to the containing repository, but cleanliness and changed paths are restricted with a Git pathspec to the Relay project subtree. Paths are normalized relative to the Relay project. A Relay root at repository root naturally sees the whole repository. Nested repositories and subproject `.relay/` directories therefore have deliberate nearest-root behavior.

Alternative: always use Git root. Rejected because monorepo subprojects would report unrelated changes. Alternative: never walk upward. Rejected because nested agent working directories are common and error-prone.

### Lossless non-UTF-8 Git path display

Git porcelain remains NUL-delimited bytes. Valid UTF-8 paths retain their existing string representation. Invalid bytes are rendered deterministically with `\xNN` escapes and an explicit `git_path_encoding: "escaped_bytes_v1"` field in structured output. Relay never asks the shell to reinterpret these strings and never mutates those paths.

Alternative: fail every snapshot on non-UTF-8. Rejected because Linux permits such paths and read-only diagnostics can remain useful. Lossy Unicode replacement is also rejected because it can collapse distinct paths.

### Bounded context and generated completions

`relay context` emits Markdown by default or the global JSON envelope. Filters select a session, note categories, event kinds, maximum notes/events/checkpoints, and a maximum character budget. Ordering is deterministic and truncation is explicit in output metadata; Relay cuts only at item boundaries. It includes only recorded facts and relative paths.

A `completions <bash|zsh|fish>` command uses `clap_complete` to write a script to stdout. CI runs format check, strict Clippy, and all tests on supported Rust. Documentation includes source/release installation and direct examples for `relay context` with Hermes, `hermes-omp`, and Codex CLI.

## Risks / Trade-offs

- [Advisory locks work only when all writers cooperate] → Relay owns all `.relay/` mutation paths, documents that manual edits bypass locking, and doctor detects resulting inconsistencies.
- [Network filesystems may implement locks or rename durability differently] → Document local-filesystem support as the reliability target and fail on lock errors instead of pretending exclusivity.
- [Persistent lock metadata can look stale] → Treat kernel lock state as authoritative and label metadata as last owner when unlocked.
- [Operation markers increase write count and can themselves be interrupted] → Write them atomically, validate independently, and allow doctor to report malformed markers without touching other state.
- [Scanning JSONL and session files grows linearly] → Apply command limits, scan once per invocation, and defer indexing/database work until measured personal histories justify it.
- [Upward discovery can select an unintended parent] → Stop at Git boundaries, prefer nearest `.relay/`, provide `--scope current` and `--scope repository`, and report selected scope.
- [Schema-v1 additive fields may be ignored by old binaries] → Preserve old required fields and event names; old binaries can read current pointer state but are not guaranteed to understand new commands/events.
- [A crash can leave a valid artifact and event but stale pointer] → Operation markers and doctor surface exact partial progress; no data is silently rolled back.
- [Character budgets are only a token proxy] → Document them as deterministic size bounds, not tokenizer-accurate limits.
- [Escaped byte paths are less friendly] → Preserve normal UTF-8 unchanged and expose the encoding marker only when needed.

## Migration Plan

1. Read and validate existing `session.json` and `events.jsonl` before any mutation; unsupported or corrupt input stops migration.
2. Acquire the project lock and create the new directories using existing symlink-containment checks.
3. If the current valid session lacks `sessions/<id>.json`, atomically copy its typed representation there and write/remove an operation marker around the materialization.
4. Keep `.relay/session.json`, `.relay/events.jsonl`, checkpoint paths, handoff paths, and root summary paths readable; do not relocate or delete legacy artifacts automatically.
5. New sessions write both current and per-session records and use the new summaries directory. Query commands merge supported legacy history without modifying it.
6. Rollback is use of the previous binary against schema-v1 current state. New event types remain append-only; users must not run an old and new Relay writer concurrently because old binaries do not participate in the new lock contract.
7. `doctor` is the preflight and post-migration verifier. Any repair requires `--repair`; ambiguous or destructive suggestions remain manual.

## Open Questions

None required before implementation. SQLite, automatic migration of unsupported future schemas, daemon coordination, model-backed summarization, tokenizers, remote synchronization, and richer packaging are explicitly deferred until real usage data justifies them.
