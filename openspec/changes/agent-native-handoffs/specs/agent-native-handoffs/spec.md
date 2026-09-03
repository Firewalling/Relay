## ADDED Requirements

### Requirement: Persist structured handoff snapshots
Relay MUST create schema-versioned, UUID-identified, immutable structured handoff records containing explicit bounded entries, unavailable markers, session context, Git state, and typed references. Records MUST distinguish explicit statements, recorded notes, derived Git state, and trusted run/output provenance.

#### Scenario: Minimal handoff
- **WHEN** an active session creates a handoff without runs
- **THEN** Relay persists one bounded structured record, renders Markdown from it, and leaves missing categories explicitly unavailable.

#### Scenario: Full handoff
- **WHEN** a handoff supplies completed work, decisions, blockers, next actions, and a trusted run
- **THEN** Relay records those entries plus selected session/checkpoint/note/Git/run/output snapshots with provenance.

### Requirement: Preserve legacy handoffs
Relay MUST read existing Markdown-only handoff artifacts and render them without destructive migration. Malformed or unsupported structured records MUST fail clearly.

### Requirement: Validate trusted references
Relay MUST accept run references only from the selected project and MUST attach outputs only from successful trusted attempts with matching task, run, attempt, path, size, hash, type, and attempt-run provenance. It MUST not copy artifact contents into records.

### Requirement: Preserve historical snapshots independently
After a structured handoff is persisted, Relay MUST treat its ledger record as authoritative for the historical snapshot. Ordinary handoff and context reads MUST validate the ledger record and handoff-store safety without requiring referenced run state, output artifacts, checkpoints, workflow definitions, Git state, or other mutable external objects to remain present or unchanged. Consumers that require a current artifact MUST validate it separately.

#### Scenario: Read after referenced state changes
- **WHEN** a valid handoff's referenced run, output, checkpoint, workflow, or Git state is changed or deleted
- **THEN** handoff --show, handoff --latest, handoff --list, and context --handoff still return the unchanged historical snapshot.

### Requirement: Consume handoffs through context
`relay context --handoff latest` and exact handoff selection MUST return the immutable structured record in the existing JSON envelope or a concise deterministic Markdown rendering. Selection and output MUST remain bounded and deterministic.

### Requirement: Protect project and terminal boundaries
Handoff paths, run paths, output paths, references, IDs, names, free text, legacy Markdown, and JSONL records MUST enforce project containment, symlink/hardlink safety where applicable, size limits, and control-character-safe human rendering. Redacted values MUST remain redacted.

### Requirement: Retain existing runtime boundary
The change MUST use existing Rust lifecycle/storage/Git/run/workflow/output logic and MUST NOT add provider APIs, autonomous behavior, agent spawning, daemons, networking, remote execution, or a second scheduler.
