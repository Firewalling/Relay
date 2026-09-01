## ADDED Requirements

### Requirement: Typed output declarations

The coordinator MUST accept only closed `json` and `file` output declarations in plan schema v3. It MUST reject unknown fields, duplicate names, invalid names, invalid types, empty paths, absolute paths, traversal components, symlink escapes, and declarations beyond configured compile-time bounds.

#### Scenario: Valid declarations are accepted

- **WHEN** task declares unique lowercase names with `json` or `file` types and relative normal-component paths
- **THEN** Relay accepts plan and provides attempt-scoped output paths through explicit environment entries

#### Scenario: Invalid declarations are refused

- **WHEN** declaration has an unknown field, duplicate name, invalid name/type, traversal, absolute path, or exceeds a bound
- **THEN** Relay rejects plan before starting any supervisor

### Requirement: Successful-attempt trust

Relay MUST persist an output only when it can bind output name, task, aggregate run ID, attempt number, attempt run ID, declaration, and persisted artifact identity to one matching successful terminal execution.

#### Scenario: JSON output is finalized

- **WHEN** matching terminal execution succeeds and declared JSON file is regular, safe, bounded, valid JSON
- **THEN** Relay stores bounded value, size, hash, path, and complete producer provenance on attempt and task records

#### Scenario: File artifact is finalized

- **WHEN** matching terminal execution succeeds and declared file is regular, safe, bounded, and hashable
- **THEN** Relay stores bounded metadata and does not copy artifact contents into aggregate state

#### Scenario: Non-successful execution produces files

- **WHEN** failed, cancelled, timed-out, indeterminate, or superseded attempt leaves files
- **THEN** Relay stores no trusted outputs and never exposes those files as producer outputs

### Requirement: Retry isolation

Each retry MUST use a fresh attempt run ID and output directory. A successful retry MUST expose only its own output records while preserving failed attempt history without trusted outputs.

#### Scenario: Failed attempt precedes success

- **WHEN** first attempt fails after writing a declared file and second attempt succeeds
- **THEN** task output points to second attempt identity and first attempt remains non-producing

### Requirement: Crash reconciliation

Resume MUST reuse existing conservative supervisor reconciliation. A trusted terminal success after coordinator crash MUST finalize outputs exactly once without rerunning the task. Missing, active, stale, malformed, or mismatched evidence MUST remain indeterminate or refused.

#### Scenario: Terminal supervisor success precedes aggregate write

- **WHEN** Go terminal record and declared output exist but aggregate state still marks attempt running
- **THEN** resume validates both, promotes output once, and launches no replacement attempt

### Requirement: Explicit downstream consumption

A consumer MAY reference only an existing output from a direct or transitive dependency with a trusted successful producer record. Relay MUST append deterministic literal argv flag/value pairs and MUST NOT guess paths or evaluate expressions.

#### Scenario: JSON dependency is consumed

- **WHEN** consumer references trusted JSON output with `as` argument
- **THEN** Relay appends one deterministic serialized JSON argv value

#### Scenario: File dependency is consumed

- **WHEN** consumer references trusted file output with `as` argument
- **THEN** Relay revalidates path, size, and hash, then appends its absolute path

#### Scenario: Untrusted dependency is referenced

- **WHEN** producer failed, is blocked, is indeterminate, or output identity changed
- **THEN** consumer is not launched

### Requirement: Bounded agent-facing output

JSON run output MUST expose task status, attempt history, trusted output metadata, and structured values only within bounds. Human output MUST list output names concisely without dumping hashes or artifact contents.

#### Scenario: Output is inspected through JSON

- **WHEN** agent requests structured run output
- **THEN** response exposes producer, attempt, run identity, type, path, size, hash, and bounded JSON value without artifact blobs
