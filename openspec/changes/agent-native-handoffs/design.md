## Context

Relay is a synchronous Rust CLI with project-scoped `.relay` state, an append-only event log, durable sessions, checkpoints, generated handoffs, and aggregate run state. Rust owns plan execution and validates structured outputs against successful attempt provenance. This change adds continuity data only; it does not add an agent runtime.

## Decisions

### Structured snapshot

`HandoffRecord` uses schema version 2, UUID identity, UTC creation time, optional source-session snapshot, optional explicit source agent, required target agent, optional goal, bounded arrays of typed entries (`completed`, `decisions`, `blockers`, `next_actions`), an unavailable-section list, an immutable Git snapshot, and typed references for checkpoints, notes, runs, and outputs. Entries identify `explicit` versus `recorded_note` provenance. Missing values remain empty/null and are named in `unavailable`; Relay never infers task completion, decisions, blockers, ownership, or actions.

### Storage and rendering

Structured records append to `.relay/handoffs.jsonl`. Each record gets one generated `.relay/handoffs/<timestamp>-<uuid>.md` artifact for the existing human UX. Markdown is rendered from the record at creation and read time; it is derivative, not a second editable semantic store. Existing Markdown-only files are legacy records and remain untouched.

### CLI

`relay handoff <agent>` remains a simple compatible create form. `relay handoff create --to <agent>` is the explicit form. `relay handoff show <id>`, `relay handoff latest`, and `relay handoff list` inspect records. Create accepts existing explicit fields, optional source agent/goal, repeated run paths, and optional supersession. `relay context --handoff latest|<id>` consumes one immutable snapshot. Global `--json` continues to provide the existing output-schema-v1 envelope.

### Relevance

Creation selects current-session notes and checkpoints in event order, with explicit entries appended after recorded entries. Selection is bounded by count and byte limits. Explicit selectors win where supported; otherwise current-session records and newest supported checkpoints/notes are used. Runs are included only when explicitly referenced. No historical dump, embeddings, or semantic ranking is used.

### Run and output trust

A referenced run path must be canonical, regular, singly linked, project-local under `.relay/runs`, and parse as a supported aggregate state. Rust reuses aggregate state and output validation. Run status, workflow name/schema/hash/inputs, task statuses, and state path are copied as a snapshot. Only outputs present on successful task attempts and matching existing `OutputRecord` provenance are copied. Output paths become project-relative presentation paths; hashes, run ID, task, output name, attempt, attempt-run ID, type, size, and bounded JSON values remain exact. Failed, cancelled, running, indeterminate, stale, superseded, tampered, or mismatched outputs are never trusted.

### Immutability and migration

Creation snapshots Git, session, notes, checkpoints, run metadata, and output provenance. Later Git changes, notes, workflow edits, or run transitions cannot mutate a prior record. Legacy Markdown is read-only compatibility input and is never rewritten on read. Unsupported structured versions and malformed JSONL fail clearly.

### Bounds and security

Records, JSONL reads, rendered Markdown, goals, agent names, entries, references, changed files, notes, checkpoints, runs, outputs, JSON values, paths, and task snapshots have explicit limits. State paths reject traversal, symlink, hardlink, cross-project, and non-canonical aliases. Human rendering escapes control characters and project-root text. JSON serialization handles strings structurally; redacted argv values remain `<redacted>` and are never reconstructed. Free-text fields are persisted and are not a secret mechanism.
