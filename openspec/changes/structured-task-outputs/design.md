## Context

Relay's built-in coordinator is Rust-owned. A plan is decoded before launch, the aggregate run state persists a fresh attempt identity before activation, Go supervises one child, and both live completion and resume reconciliation enter `apply_completion`. Existing Go execution responses retain bounded stream metadata but are not task data. Aggregate state is capped at 4 MiB and must not become an artifact blob store.

## Goals / Non-Goals

**Goals:**

- Make successful task products explicit, bounded, inspectable, and attempt-bound.
- Preserve failed, cancelled, timed-out, indeterminate, stale, and superseded attempts as non-producers.
- Keep downstream wiring typed, deterministic, direct-argv-only, and small.
- Preserve existing Go protocol responsibility and conservative resume behavior.

**Non-Goals:**

- Stdout inference, shell interpolation, templates, expressions, workflow definitions, scripting runtimes, remote storage, uploads, deduplication, caching, or generalized dataflow.
- Copying arbitrary artifact contents into aggregate JSON.

## Decisions

### Plan schema v3

`PlanTask.outputs` is an optional ordered array of closed declarations:

```json
{"name":"summary","type":"json","path":"summary.json"}
```

`type` is `json` or `file`. Names match `[a-z][a-z0-9_]{0,63}` and are unique per task. Paths are nonempty relative paths made only of normal components, with a 512-byte declaration bound. Each task has at most four outputs and four inputs. `PlanTask.inputs` is an optional ordered array of closed declarations:

```json
{"from_task":"inspect","output":"summary","as":"--summary"}
```

The producer task/output must exist and be a direct or transitive dependency. Plans v1 and v2 remain readable; output/input fields are rejected unless schema version is v3.

### Attempt-scoped output paths

For attempt run ID `A`, Relay uses `<project>/.relay/runs/outputs/A/<declared-path>`. Before launch it rejects existing final paths and symlinked path components, creates only required parents, and passes one `RELAY_OUTPUT_MANIFEST` JSON map from exact names to actual paths in the existing explicit supervisor environment request. Producer programs write these paths themselves. A retry receives a new attempt run ID and therefore cannot inherit a previous output path.

### Output records and trust

Successful attempts store `OutputRecord` entries with `name`, `type`, `path`, `size_bytes`, `sha256`, bounded JSON `value` (including JSON `null`), `producer_task`, aggregate `run_id`, `attempt`, and `attempt_run_id`. `json` files are read through a no-follow file descriptor, hashed, parsed as JSON, re-encoded, and bounded to 2 KiB. `file` outputs store `null` value metadata, are hashed without retaining contents, and are bounded to 64 MiB; serialized metadata is bounded to 2 KiB. Linux rejects files with a hardlink count other than one. The task-level output view is a copy of the latest successful attempt's records, never an independent namespace.

No output record is created for a non-successful execution. Any missing, malformed, changed, mismatched, unsafe, or oversized declared output makes an otherwise successful execution indeterminate and exposes no outputs. Consumers revalidate file identity and JSON value before argv construction.

### Durability and reconciliation

Ordering:

1. Persist `Running` attempt number, aggregate run ID, attempt run ID, and supervisor path.
2. Prepare attempt-scoped output parents.
3. Run one child through Go; Go durably writes its terminal attempt record.
4. Rust accepts only a response matching the current attempt run ID.
5. Rust validates every declared output from the opened file identity.
6. Rust atomically persists attempt and task output records with aggregate state.

If the coordinator dies before step 6, state still shows a running attempt. Resume validates the existing terminal Go record and repeats steps 5–6 without launching another attempt. If evidence is missing, malformed, stale, active, or mismatched, existing indeterminate/refusal behavior wins. Repeated resume sees already-promoted terminal state and does not duplicate records.

### State evolution and bounds

Aggregate state becomes schema v4. Valid aggregate v1, v2, and v3 records are decoded through existing migration paths and receive no fabricated outputs. New output metadata is bounded by declaration count, JSON size, artifact metadata size, and the existing 4 MiB encoded-state check. A regression test covers maximum structured metadata.

### Security boundary

Output declarations are validated before launch. Expected paths are generated from the selected project root and validated attempt identity; persisted paths must equal regenerated paths. No-follow final opens, regular-file checks, hardlink checks, bounded reads, size checks, hash checks, and post-read size checks address symlinks, aliases, oversized files, and mutation during reads. Resume and downstream consumption repeat metadata/content validation, so filesystem existence alone is never proof. Structured values are intentionally persisted; plans must not declare secrets as outputs.

## Alternatives

- Scrape stdout: rejected because human output is not a typed producer contract.
- Fixed project paths: rejected because retries and stale files would share identity.
- Global output map or expression syntax: rejected because it creates implicit dataflow language.
- Go-owned output records: rejected because Rust owns plan, aggregate state, retries, and reconciliation.
- Artifact copying/blob store: rejected because files remain user-owned declared artifacts.

## Migration

No old plan or aggregate field is reinterpreted as an output. Old plan schemas remain valid with empty output/input lists. Old aggregate schemas migrate to v4 through current safe decode paths. A v4 state with malformed output records fails validation rather than dropping or promoting them.
