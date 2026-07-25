## ADDED Requirements

### Requirement: Start a local session
Relay SHALL create `.relay/` in the current project directory, record the goal and UTC start time, and capture the current Git branch and commit when the directory belongs to a Git repository. Relay MUST reject a start when `session.json` describes an active session and MUST NOT overwrite malformed session state.

#### Scenario: Start in a Git repository
- **WHEN** a user runs `relay start "Improve Fedora installer"` in a Git repository with no active session
- **THEN** Relay creates an active session, appends `session.started`, captures branch and commit when available, and prints a concise summary

#### Scenario: Start outside Git
- **WHEN** a user starts a session in a directory that is not in a Git repository
- **THEN** Relay creates the session successfully and records Git as unavailable

#### Scenario: Reject a second active session
- **WHEN** a user runs `relay start` while the current `.relay/session.json` is active
- **THEN** Relay returns an actionable error and preserves the existing session and events

#### Scenario: Reject corrupt state
- **WHEN** `.relay/session.json` exists but is not valid supported session JSON
- **THEN** Relay returns an actionable corruption error and does not replace that file

### Requirement: Show session status
Relay SHALL display the active or completed goal, state, start time, current Git branch and commit, current working-tree cleanliness, event count, most recent checkpoint, and current or most recently used agent when known. Relay SHALL represent unavailable values explicitly and work outside Git.

#### Scenario: Status with current Git changes
- **WHEN** a user runs `relay status` during a session in a dirty Git repository
- **THEN** Relay reports the current branch, commit, dirty state, event count, and latest checkpoint without modifying Git

#### Scenario: Status outside Git
- **WHEN** a user runs `relay status` for a session outside a Git repository
- **THEN** Relay displays the session and identifies Git context as unavailable

### Requirement: Create a checkpoint
Relay SHALL capture a timestamp, optional message, current commit, branch, cleanliness, and changed files; write a readable checkpoint artifact; and append `checkpoint.created`. Relay MUST NOT commit, stash, stage, or otherwise modify Git state.

#### Scenario: Checkpoint dirty work
- **WHEN** tracked or untracked files are changed and the user runs `relay checkpoint "Rust detection fixed"`
- **THEN** Relay records those paths and current Git metadata without changing the index, worktree, or history

#### Scenario: Checkpoint outside Git
- **WHEN** the user checkpoints an active session outside Git
- **THEN** Relay records an explicit unavailable Git snapshot and succeeds

### Requirement: Finish a session
Relay SHALL capture final Git state, write a final Markdown summary, append `session.finished`, atomically mark the session completed, preserve existing session data, and print the saved summary path. Relay MUST reject finishing when there is no active session.

#### Scenario: Finish active session
- **WHEN** a user runs `relay finish` during an active session
- **THEN** Relay saves a final summary, records the final event and Git state, marks the session completed, and reports the summary location

#### Scenario: Refuse an invalid finish
- **WHEN** no active session exists or the latest session is already completed
- **THEN** Relay returns an actionable error without deleting or silently replacing session data

### Requirement: Preserve inspectable and recoverable state
Relay SHALL use human-readable JSON, JSON Lines, and Markdown, use atomic replacement for mutable state files, and return errors rather than panic for ordinary filesystem, Git, lifecycle, or corruption failures. Event records SHALL include a schema version, stable event type, event id, session id, timestamp, and extensible structured payload.

#### Scenario: Interrupted mutable-file update
- **WHEN** Relay replaces a mutable JSON or Markdown file
- **THEN** it writes and syncs a temporary file in the same directory before atomically persisting the destination

#### Scenario: Malformed event log
- **WHEN** an event log line cannot be decoded
- **THEN** Relay reports the affected line and does not invent an event count or silently discard the malformed record
