## ADDED Requirements

### Requirement: Diagnose state without changing it
`relay doctor` SHALL inspect the selected Relay project and report structured diagnostics with stable codes, severity, affected relative paths, and conservative guidance. By default it MUST NOT create, modify, rename, truncate, or delete Relay or Git state.

#### Scenario: Healthy project
- **WHEN** all supported state, events, references, artifacts, operation markers, and lifecycle combinations are valid
- **THEN** doctor reports a healthy result with no error diagnostics and makes no filesystem changes

#### Scenario: Intentionally damaged state
- **WHEN** state contains malformed JSON or JSONL, unsupported schema versions, missing referenced files, orphaned artifacts, invalid lifecycle fields, duplicate lifecycle events, or inconsistent current and historical records
- **THEN** doctor reports each detectable problem without discarding any data

### Requirement: Detect interrupted multi-file operations
Mutations spanning multiple files SHALL use durable operation markers, and doctor SHALL compare each marker’s declared steps with valid artifacts, events, and session records to identify incomplete operations.

#### Scenario: Interruption after artifact creation
- **WHEN** an operation stops after creating an artifact but before appending its event or updating session state
- **THEN** doctor reports the incomplete operation and the orphaned or unmatched artifact

#### Scenario: Interruption after event append
- **WHEN** an operation stops after appending its event but before updating the session record
- **THEN** doctor reports the stale session record and identifies the durable event as completed evidence

### Requirement: Repair only explicit lossless cases
Any repair SHALL require `doctor --repair`, acquire the mutation lock, enumerate its actions, and perform only changes whose correctness follows from valid durable evidence. Relay MUST refuse repairs that require truncating logs, deleting ambiguous artifacts, reconstructing user text, guessing lifecycle state, or rewriting unsupported schemas.

#### Scenario: Unsafe repair refused
- **WHEN** a corrupt event line or ambiguous lifecycle could be made superficially consistent only by discarding or inventing data
- **THEN** `doctor --repair` refuses the repair, preserves all files byte-for-byte, and returns manual guidance

#### Scenario: Completed marker cleanup
- **WHEN** an operation marker remains but every declared effect and reference is present, valid, and mutually consistent
- **THEN** explicit repair may remove only that completed marker and reports the performed action
