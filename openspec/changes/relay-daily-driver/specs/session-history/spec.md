## ADDED Requirements

### Requirement: Preserve addressable session records
Relay SHALL maintain one typed per-session record under an inspectable `.relay/sessions/` layout while retaining `session.json` as the current-session compatibility record. Starting a later session MUST NOT overwrite or delete an earlier per-session record, events, or artifacts.

#### Scenario: Several completed sessions
- **WHEN** a user completes several sessions in one Relay project
- **THEN** every session remains independently addressable with its original ID, goal, timestamps, lifecycle, lineage, Git snapshots, events, and referenced artifacts

#### Scenario: Valid legacy MVP state
- **WHEN** supported schema-v1 MVP state has a current session but no per-session record
- **THEN** Relay can query it and the next locked mutation materializes a compatible historical record without changing its meaning

### Requirement: List and show sessions
`relay list` SHALL show sessions in deterministic reverse chronological order, and `relay show <session>` SHALL resolve an exact UUID or unambiguous UUID prefix and display that session without making it current.

#### Scenario: Historical lookup
- **WHEN** a user selects a unique completed session from `list` and runs `show`
- **THEN** Relay displays that session and its recorded facts while leaving current state unchanged

#### Scenario: Ambiguous selector
- **WHEN** a selector matches more than one session
- **THEN** Relay returns an actionable ambiguity error and does not choose arbitrarily

### Requirement: Query append-only history
`relay history` SHALL return project or selected-session events in deterministic event order with explicit filters and limits and MUST report malformed or unsupported records rather than silently skipping them.

#### Scenario: Session history filter
- **WHEN** a user requests history for one session with a limit
- **THEN** Relay returns at most that many matching events and identifies whether output was truncated

### Requirement: Resume by creating continuation lineage
`relay resume <session>` SHALL require no active session and a valid completed source session, create a new active session with a new ID and fresh initial Git snapshot, preserve the source unchanged, record `resumed_from`, and append `session.resumed`.

#### Scenario: Resume completed session
- **WHEN** a user resumes a valid completed historical session
- **THEN** Relay creates a distinct active continuation with the source goal and lineage and reports both IDs

#### Scenario: Reject invalid resume
- **WHEN** the source is active, unknown, ambiguous, corrupt, unsupported, or another active session exists
- **THEN** Relay rejects resume without modifying the source, current session, event log, or artifacts
