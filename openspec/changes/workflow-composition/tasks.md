## 1. Schema and resolver

- [x] Add schema-v2 workflow-instance nodes and explicit exports.
- [x] Cache secure project-local definitions per resolution.
- [x] Add bounded recursive expansion and deterministic cycle errors.
- [x] Namespace IDs and wire group dependencies.
- [x] Resolve typed child inputs and defaults.
- [x] Rewrite explicit child output exports.
- [x] Persist composition and resolved-plan identity.

## 2. Compatibility and inspection

- [x] Reuse existing plan scheduler and provenance paths.
- [x] Preserve schema-v1, direct-plan, resume, and handoff behavior.
- [x] Add resolved workflow inspection.

## 3. Verification

- [x] Add resolver, cycle, bounds, namespace, input, export, and hash tests.
- [x] Add real Go composed run, retry, output, parallel, and resume scenario.
- [x] Update workflow, protocol, validation, and OpenSpec documentation.
