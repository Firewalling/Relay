## ADDED Requirements

### Requirement: Serialize project mutations
Relay SHALL acquire an exclusive project-level advisory lock before reading state for any mutating command and SHALL hold it until every mutation step has completed or failed. Relay MUST use a non-blocking acquisition and MUST NOT wait indefinitely.

#### Scenario: Concurrent mutations
- **WHEN** one Relay process holds the project lock and another process attempts a mutating command in the same Relay project
- **THEN** the second process fails promptly with an actionable busy error while the first process remains the only writer

#### Scenario: Independent projects
- **WHEN** Relay processes mutate two different Relay projects concurrently
- **THEN** each process can acquire its own project lock and proceed independently

### Requirement: Release abandoned locks safely
Relay SHALL rely on operating-system advisory lock lifetime so that normal completion, returned errors, panics, and process termination release ownership. Relay MUST NOT steal a lock solely because metadata is old.

#### Scenario: Process exits while holding lock
- **WHEN** a process terminates without completing while it owns the lock
- **THEN** a later Relay process can acquire the released kernel lock and refresh owner metadata without manual deletion

#### Scenario: Command fails after acquisition
- **WHEN** a mutating command returns an error after acquiring the lock
- **THEN** a subsequent process can acquire the lock

### Requirement: Provide lock diagnostics
The lock file SHALL contain inspectable last-owner metadata including process ID, command, and acquisition time, and a busy error SHALL include available ownership details and a retry instruction without exposing unrelated private data.

#### Scenario: Busy lock reports owner
- **WHEN** lock metadata is valid and another process owns the advisory lock
- **THEN** Relay identifies the owning PID and command and tells the caller to retry after that process finishes
