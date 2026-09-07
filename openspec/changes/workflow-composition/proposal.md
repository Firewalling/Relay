## Why

Larger Relay workflows currently repeat task DAGs. Project-local reusable workflows need a deterministic composition mechanism without adding runtime nesting, another scheduler, or a programming language.

## What Changes

- Extend the closed workflow schema with bounded schema-v2 workflow-instance nodes and explicit child exports.
- Resolve the complete project-local source graph at compile time into one ordinary plan-v3 DAG.
- Namespace task IDs by deterministic instance path and wire group roots/terminals into existing dependencies.
- Persist source-graph identity, resolved child inputs, composition instances, and resolved-plan identity with the existing workflow metadata.
- Add resolved workflow inspection and preserve existing workflow, direct-plan, output, retry, crash, resume, and handoff behavior.

## Non-Goals

Runtime nested workflows, child Relay processes, Lua, scripting, expressions, conditionals, loops, matrices, dynamic DAG mutation, remote imports, registries, package management, network execution, daemons, triggers, UI/TUI, and model/provider integration.

## Impact

Rust owns composition resolution and immutable metadata. The existing Rust scheduler, Go one-child supervisor, retry model, output/artifact provenance, cancellation, reconciliation, state storage, and handoff snapshots remain authoritative. Schema-v1 workflows and direct relay run plans remain compatible.
