## MODIFIED Requirements

### Requirement: Start a local session
Relay SHALL create or reuse `.relay/` at the selected project scope, record the goal and UTC start time in both current and per-session records, and capture the current scoped Git branch and commit when available. Relay MUST acquire the project mutation lock, reject a start when any supported record describes an active session, preserve valid prior sessions, and MUST NOT overwrite malformed or unsupported state.

#### Scenario: Start in a Git repository
- **WHEN** a user runs `relay start "Improve Fedora installer"` in a selected Git-backed scope with no active session
- **THEN** Relay creates an active addressable session, appends `session.started`, captures branch and commit when available, and prints a concise human or JSON summary

#### Scenario: Start outside Git
- **WHEN** a user starts a session in a directory that is not in a Git repository
- **THEN** Relay creates the session successfully and records Git as unavailable

#### Scenario: Reject a second active session
- **WHEN** a user runs `relay start` while the selected Relay project has an active session
- **THEN** Relay returns an actionable error and preserves every current and historical record and event

#### Scenario: Reject corrupt state
- **WHEN** selected Relay state is malformed, unsupported, or internally inconsistent
- **THEN** Relay returns an actionable error, suggests `relay doctor`, and does not replace that data

### Requirement: Show session status
Relay SHALL display the current active or completed goal, ID, state, start time, lineage, scoped Git branch and commit, scoped working-tree cleanliness, event count, most recent checkpoint, current or most recently used agent, and selected project scope when known. Relay SHALL represent unavailable values explicitly, work outside Git, support nested discovery, and expose equivalent typed JSON.

#### Scenario: Status with current Git changes
- **WHEN** a user runs `relay status` during a session in a dirty selected project scope
- **THEN** Relay reports current scoped Git facts, event count, and latest checkpoint without modifying Git

#### Scenario: Status outside Git
- **WHEN** a user runs `relay status` for a session outside a Git repository
- **THEN** Relay displays the session and identifies Git context as unavailable

### Requirement: Create a checkpoint
Relay SHALL acquire the project lock; capture a timestamp, optional message, current commit, branch, scoped cleanliness, and scoped changed files; atomically write a readable checkpoint artifact; append `checkpoint.created`; and expose human or structured JSON output. Relay MUST NOT commit, stash, stage, or otherwise modify Git state.

#### Scenario: Checkpoint dirty work
- **WHEN** tracked or untracked files are changed within scope and the user runs `relay checkpoint "Rust detection fixed"`
- **THEN** Relay records only in-scope paths and current Git metadata without changing HEAD, index, worktree contents, or stash state

#### Scenario: Checkpoint outside Git
- **WHEN** the user checkpoints an active session outside Git
- **THEN** Relay records an explicit unavailable Git snapshot and succeeds

### Requirement: Finish a session
Relay SHALL acquire the project lock; capture final scoped Git state; write a final Markdown summary including relevant recorded notes; append `session.finished`; atomically mark both current and per-session records completed; preserve all existing sessions; and print human or structured JSON output with the saved relative summary path. Relay MUST reject finishing when there is no active session.

#### Scenario: Finish active session
- **WHEN** a user runs `relay finish` during an active session
- **THEN** Relay saves a final summary, records final event and Git state, marks that session completed in both records, and reports the summary location

#### Scenario: Refuse an invalid finish
- **WHEN** no active session exists or the selected current session is already completed
- **THEN** Relay returns an actionable error without deleting or silently replacing current or historical data

### Requirement: Preserve inspectable and recoverable state
Relay SHALL use human-readable JSON, JSON Lines, and Markdown; use atomic replacement for mutable state and operation-marker files; append synced complete event lines; reject unsafe symlinked state paths; and return errors rather than panic for ordinary filesystem, lock, Git, lifecycle, schema, or corruption failures. Multi-file changes SHALL leave enough durable intent for doctor to identify interruption.

#### Scenario: Interrupted mutable-file update
- **WHEN** Relay replaces a mutable JSON or Markdown file
- **THEN** it writes and syncs a temporary file in the same directory before atomically persisting the destination

#### Scenario: Malformed event log
- **WHEN** an event log line cannot be decoded
- **THEN** Relay reports the affected line and does not invent an event count or silently discard the malformed record

#### Scenario: Interrupted multi-file update
- **WHEN** Relay is interrupted between artifact creation, event append, and session update
- **THEN** already durable records remain intact and doctor can identify the incomplete operation without automatic data loss
