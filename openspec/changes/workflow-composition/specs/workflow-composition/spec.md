## ADDED Requirements

### Requirement: Compile-time composition

Relay SHALL resolve schema-v2 workflow-instance nodes and all reachable project-local child definitions before preflight. The result SHALL be one ordinary plan-v3 DAG executed by the existing Rust scheduler and one-child Go supervisor. Composition SHALL NOT spawn Relay subprocesses or a nested scheduler.

### Requirement: Bounded deterministic expansion

The resolver SHALL reject malformed names, duplicate nodes, missing references, direct or indirect cycles, and expansion beyond the documented depth, source, instance, dependency, output, export, metadata, task, and plan limits. Cycle errors SHALL include a deterministic workflow path. Definition bytes SHALL be loaded securely once per resolution and canonical hashes SHALL ignore formatting while including the complete source graph identity.

### Requirement: Namespaced dependencies

Each child task SHALL receive a deterministic instance-path ID. Parent dependencies SHALL attach to child roots and successors SHALL depend on child terminals. ID ambiguity, overlong names, and collisions SHALL fail before launch. Direct schema-v1 task IDs SHALL retain existing behavior.

### Requirement: Typed mappings and exports

Child inputs SHALL accept only scalar literals or explicit parent workflow-input references. Required values, defaults, types, unknown mappings, and duplicate mappings SHALL be validated before launch. A child output SHALL be parent-visible only through an explicit export; parent references SHALL resolve to the original expanded task output and SHALL use existing output/artifact validation and provenance.

### Requirement: Immutable composed runs

Fresh composed runs SHALL persist the source-graph identity, root identity, composition instances, resolved child inputs, schema versions, resolved-plan identity, and sanitized expanded plan in existing workflow metadata and aggregate state. Resume SHALL use that persisted plan without re-expanding live definitions. Handoff snapshots SHALL retain exact expanded task and output provenance without copying artifacts.

### Requirement: Compatibility and inspection

Schema-v1 workflows, direct relay run plans, existing retry/cancellation/output/reconciliation behavior, and historical state SHALL remain compatible. Workflow show SHALL offer structured resolved-plan inspection without changing execution semantics.

### Requirement: Security boundaries

Child lookup SHALL remain bounded to the selected project-local discovery set. Traversal, remote or cross-project references, symlinked directories/files, unsafe IDs, shell interpolation, secret leakage in metadata, stale definitions, and untrusted outputs SHALL be rejected or remain governed by existing no-follow, redaction, state, and provenance checks.
