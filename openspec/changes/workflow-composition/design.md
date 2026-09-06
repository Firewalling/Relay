## Context

Relay workflow definitions are already discovered and validated by Rust, compiled to plan-v3, persisted in aggregate schema v5, and executed by one bounded DAG coordinator. Composition must remain a resolver concern.

## Decisions

### Schema

Schema v1 remains unchanged. Schema v2 keeps the existing ordered tasks field and adds an untagged workflow-instance node with id, workflow, optional depends_on, and scalar input mappings. A mapping is either a literal scalar or an explicit workflow_input reference. A child exposes outputs only through a top-level exports map from export name to from_task/output.

### Expansion

The resolver loads each discovered definition securely once, caches its parsed canonical identity, validates the reachable graph, detects cycles with a source path, resolves defaults and mappings, and expands child tasks into one plan. Instance paths use dot-separated safe structural names; nested source task IDs containing dots are rejected to avoid ambiguity. Child roots receive dependencies from the parent group; parent successors depend on child terminals. Output inputs are rewritten to the real expanded producer and existing output validation remains authoritative.

### Identity and resume

A composed source hash covers the root canonical definition hash, every referenced definition hash, and deterministic instance paths. Persisted composition metadata records root identity, referenced workflow/schema/hash, instance path, resolved inputs, and the resolved-plan hash. Resume uses only the persisted sanitized plan and optional metadata; live source files are consulted only for a non-blocking changed-definition notice.

### Bounds and security

Depth is capped at 8; source definitions and total instances at 64; direct instances at 16 per workflow; expanded dependencies at 512; outputs at 256; exports at 64; metadata at 16 KiB; and all normal plan/task/output bounds remain enforced. Discovery is project-local, non-recursive, sorted, no-follow, and symlink rejecting. No expressions, shell evaluation, remote lookup, cross-project references, or file duplication are added.

## Migration

WorkflowRunMetadata gains optional composition and resolved-plan fields. Existing run-state schema and schema-v1 workflow files remain readable without rewriting historical records. Handoff validation accepts workflow schema v2 while retaining its immutable snapshot shape and exact expanded task/output provenance.
