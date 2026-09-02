## Why

Relay users currently repeat raw plan JSON for recurring DAGs. Reusable workflows provide version-controlled, named declarative definitions while keeping Relay's existing coordinator authoritative.

## What Changes

- Add project-local JSON workflow definitions under `relay-workflows/`.
- Validate and deterministically resolve named workflows, bounded scalar inputs, explicit argv references, task dependencies, retries, outputs, and artifact inputs.
- Compile each workflow into the existing plan schema and run machinery.
- Persist workflow identity, canonical definition hash, resolved inputs, and resolved sanitized plan in aggregate state.
- Add `workflow list`, `show`, `validate`, and `run` commands.
- Resume workflow runs from persisted plans even when the live definition changes, is deleted, or is malformed.

## Non-Goals

Lua, scripting, expressions, conditionals, loops, matrices, functions, imports, nested workflows, dynamic DAG mutation, schedules, triggers, daemons, remote execution, services, registries, secrets, environment expansion, shell evaluation, UI/TUI, caches, and model/provider integrations.

## Impact

Rust adds a workflow resolver and aggregate run-state schema v5 metadata. The existing Rust DAG scheduler, Go one-child supervisor, retry rules, output/artifact trust checks, cancellation, and crash reconciliation remain authoritative.
