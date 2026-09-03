## 1. Structured records

- [x] Add schema-v2 handoff models and provenance enums.
- [x] Persist bounded immutable records in handoff JSONL.
- [x] Render generated Markdown from structured records.
- [x] Validate unknown fields, versions, bounds, and IDs.

## 2. Existing state integration

- [x] Capture session, notes, checkpoints, and Git snapshots.
- [x] Select current-session state deterministically and boundedly.
- [x] At creation, validate project-local run and workflow references.
- [x] Attach only trusted output and artifact provenance.

## 3. CLI and context

- [x] Preserve legacy handoff creation syntax.
- [x] Add handoff create/show/latest/list operations.
- [x] Add direct handoff context selection.
- [x] Return structured JSON and concise human output.

## 4. Compatibility and security

- [x] Read legacy Markdown without destructive migration.
- [x] Reject traversal, symlink, hardlink, and cross-project inputs.
- [x] Preserve redaction and escape control characters.
- [x] Keep records immutable after creation.

## 5. Verification and documentation

- [x] Add focused model, CLI, trust, and security tests.
- [x] Add real agent-A to agent-B scenario.
- [x] Update README and lifecycle/orchestration/validation docs.
- [x] Run complete Rust, Go, and integration verification.
