## Why

Relay already proves task attempt lifecycle, retry decisions, and conservative resume. It does not yet record what a successful attempt produced. Agents must guess from stdout or inspect arbitrary files, which breaks provenance and lets stale or failed-attempt data look valid.

## What Changes

- Add plan schema v3 declarations for bounded JSON outputs and declared file outputs.
- Give each declaration an attempt-scoped path through explicit `RELAY_OUTPUT_<NAME>` environment entries; never scrape stdout or expand shell syntax.
- Persist bounded output records on attempt history and point successful task records at the same records.
- Validate output identity, regular-file/link safety, size, hash, JSON validity, task identity, aggregate run ID, and attempt identity before promotion or consumption.
- Add typed dependency `inputs` that append deterministic argv flag/value pairs from trusted producer outputs.
- Bump aggregate run-state schema to v4 and migrate valid older states without reinterpreting old output fields.
- Preserve Go's one-child supervisor and protocol-v1 manifest; task outputs remain Rust-owned orchestration behavior using existing explicit environment request fields.

## Capabilities

### New Capabilities

- `structured-task-outputs`: Successful attempts can publish bounded JSON values and declared artifact metadata with exact provenance.
- `trusted-output-consumption`: Dependent tasks can consume trusted direct or transitive outputs through explicit argv construction.

### Modified Capabilities

- `orchestration-run-state`: Aggregate schema v4 preserves output history and successful-task output views.
- `orchestration-resume`: Terminal Go success reconciliation finalizes outputs exactly once without rerunning the attempt.

## Impact

Rust plan parsing, coordinator state, retry transitions, reconciliation, persistence, and rendering change. The Go supervisor receives only explicit environment entries already supported by execution protocol v1; it does not parse task output or own DAG state. JSON task values remain bounded in aggregate state; files remain external declared artifacts with bounded metadata and SHA-256 identity. No workflow language, expression engine, shell interpolation, upload, daemon, or artifact store is introduced.
