## Context

Relay already validates closed plan JSON, schedules bounded DAG tasks, persists aggregate state, reconciles exact supervisor attempts, and trusts only validated structured outputs and artifacts. Workflow resolution must add no scheduler or runtime logic.

## Decisions

### Definitions

Definitions are direct JSON files at `<project>/relay-workflows/`. Directory discovery is project-scoped, non-recursive, deterministic, bounded, and rejects symlinks. Each file must use a `.json` extension; the validated definition name is its identity. Files are version-control friendly; `.relay` remains transient runtime state.

### Schema v1

A closed workflow object contains `schema_version`, `name`, ordered `tasks`, and optional `inputs` map. Inputs use only `string`, `integer`, or `boolean`, with `required` and typed `default`. Tasks use literal absolute `program`, literal `arguments` plus typed `{"workflow_input":"name"}` references, dependencies, existing task configuration, existing `retry`, existing `outputs`, and existing typed output `inputs`. Workflow-level output aliases and workflow composition are not supported.

Task timeout defaults to 60,000 ms; other task defaults match current plan defaults. Compiled plans are schema v3, so existing output/artifact validation is reused.

### Resolution

JSON duplicate keys are rejected before decoding. Unknown fields, unsupported versions, invalid identifiers, missing required inputs, wrong scalar types, duplicate CLI inputs, unknown references, cycles, task/output errors, path violations, and bounds fail before preflight or child launch. Input values become literal argv elements; no textual interpolation, shell, environment, or expression evaluation exists.

### Identity and resume

Canonical hash is SHA-256 over serde's normalized typed workflow representation: map keys sorted by `BTreeMap`, defaults materialized, and formatting excluded. Fresh state stores optional workflow metadata in aggregate schema v5: name, workflow schema version, definition hash, and resolved scalar inputs. The already-persisted sanitized compiled plan remains execution authority.

A workflow resume reads persisted state and plan without reparsing the live definition. It requires matching persisted workflow name. A valid current definition is hashed only to report a changed-definition notice; missing, malformed, or symlinked current definitions do not block safe resume. Direct `relay run` continues to require and compare its plan as before.

### CLI

`relay workflow list`, `relay workflow show <name>`, `relay workflow validate <name> [--input name=value]`, and `relay workflow run <name> [--input name=value]` use current global JSON and project-scope options. Run-only options mirror direct runs: `--jobs`, `--supervisor`, `--state-path`, and `--resume`.

### Bounds and security

Workflow files are capped at 1 MiB, definitions at 64 per project, names and input names at 48 bytes, inputs at 16, input strings at 4 KiB, tasks at the existing 64-task plan cap, and persisted workflow metadata at 16 KiB. Compiled plans enforce the existing 1 MiB plan, output, artifact, path, retry, and state bounds after input expansion and again after working-directory resolution. Name-derived paths are constructed only after identifier validation. Workflow input text remains untrusted literal argv and is not a secret mechanism.

## Migration

Aggregate state becomes schema v5. Decoding v1-v4 state preserves existing attempts and adds no workflow metadata; v1-v4 states with unexpected workflow metadata are rejected. New direct plan runs store no workflow metadata. No plan schema changes are required.
