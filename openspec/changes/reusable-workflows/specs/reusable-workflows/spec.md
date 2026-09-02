## ADDED Requirements

### Requirement: Version-controlled definitions

Relay SHALL discover non-recursive JSON workflow definitions from `<project>/relay-workflows/`. Discovery SHALL be project-scoped, deterministic, bounded, and SHALL reject symlinked workflow directories and files. A definition name SHALL be a safe identifier; JSON filenames are not workflow identity.

### Requirement: Closed declarative schema

A workflow SHALL contain schema version 1, a name, zero or more typed scalar inputs, and one to 64 tasks. Unknown fields, duplicate JSON keys, unsupported versions, invalid identifiers, duplicate task IDs, unknown dependencies, cycles, invalid task configuration, and invalid output declarations SHALL be rejected before child launch. Workflow resolution SHALL be deterministic, bounded, inspectable, and side-effect free.

### Requirement: Typed input substitution

Inputs SHALL support only string, integer, and boolean values with required and default fields. CLI values SHALL be validated before execution. Task arguments SHALL accept literal strings or explicit `workflow_input` references. Resolution SHALL append literal argv values and SHALL never evaluate shell syntax, environment expansion, templates, or expressions.

### Requirement: Existing orchestration authority

A valid workflow SHALL compile into Relay's existing plan schema and execute through its existing Rust DAG scheduler and Go one-child supervisor. Existing dependency, bounded parallelism, retry, cancellation, output, artifact, and crash reconciliation semantics SHALL remain authoritative.

### Requirement: Immutable workflow runs

Fresh workflow runs SHALL persist workflow name, workflow schema version, canonical definition SHA-256, resolved scalar inputs, and the resolved sanitized plan in aggregate state schema v5. Workflow resume SHALL use that persisted plan and SHALL not require the live definition to be present or valid. A valid changed live definition MAY be reported but SHALL not alter execution.

### Requirement: Inspectable CLI

Relay SHALL provide `workflow list`, `workflow show <name>`, `workflow validate <name>`, and `workflow run <name>`. Human output SHALL remain concise; JSON output SHALL preserve typed workflow metadata. Validation SHALL launch no child and mutate no runtime state.

### Requirement: Explicit limitations

Workflow-level output aliases, composition, imports, scripting, expressions, conditionals, loops, matrices, dynamic tasks, schedules, triggers, daemon mode, remote execution, network services, secret management, environment templating, and model/provider integrations SHALL remain unavailable.
