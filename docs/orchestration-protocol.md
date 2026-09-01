# Relay orchestration process protocol v1

Relay exposes a negotiated, versioned external process boundary for a possible
future orchestrator. The read-only `relay protocol` command reports compiled
capabilities without discovering a project, reading or validating `.relay`, or
creating files. The canonical v1 manifest is
[`protocol/manifest-v1.json`](../protocol/manifest-v1.json).

Relay also includes two capabilities that manifest v1 does not advertise:
`relay supervise`, a project-independent Rust launcher for the standalone Go
one-child supervisor at `cmd/relay-supervisor`, and the built-in Rust
`relay run` bounded-parallel DAG coordinator described separately below. Go
supervises one child attempt; it is not a scheduler, queue, daemon, remote
executor, or Lua runtime. Child termination and child-output capture remain
exclusively in Go rather than being duplicated by Rust. Allowed Git-aware Rust
operations may invoke `git`; existing note editor behavior remains outside
protocol v1.

## Negotiation and versions

A consumer first runs `relay --json --scope current protocol` by direct argument
vector and inspects the success envelope's `data` object. It must accept only a
`protocol_version` it implements. Protocol version, Relay crate version, JSON
output schema version, and on-disk state schema version are independent:

- `protocol_version: 1` identifies this orchestration contract.
- `relay_version` identifies the Relay binary that reported it.
- `output_schema_version: 1` identifies Relay's success and error envelopes.
- aggregate run-state schema_version: 4 identifies the Rust-owned DAG state schema with bounded retry history and task-output provenance; immutable protocol manifest remains v1.

`relay protocol` is project-independent. Global options may precede or follow
the subcommand, and even `--scope repository` does not trigger repository
lookup for this command.

## Transport and framing

The transport is `process` with `direct_argv` invocation. A consumer constructs
an argument vector and passes each element directly to an operating-system
process API. It must not concatenate arguments into a command string or invoke
a shell.

Examples of complete argument vectors are:

```text
["relay", "--json", "--scope", "current", "status"]
["relay", "--json", "--scope", "current", "context", "--max-chars", "8000"]
["relay", "--json", "--scope", "current", "note", "decision", "--stdin"]
```

Each invocation attempts to start one process. Once the operating system has
started Relay, an exact global `--json` anywhere before a literal `--` requests
JSON failure framing. If Clap rejects the arguments, Relay exits with Clap's
nonzero code, writes no stdout, and writes exactly one compact
output-schema-v1 error envelope to stderr. The envelope has `command: "cli"`,
`error.code: "argument_error"`, and an actionable normalized Clap diagnostic
without the redundant leading `error: `. Without that JSON request, argument
failures remain ordinary Clap stderr text.

After parsing, failure to read the current directory produces the same
output-schema-v1 error envelope shape when JSON was requested, using the parsed
command name and `error.code: "current_directory"`; human output remains
unchanged. When JSON was requested, an invocation that reaches an application
operation has exactly one JSON document of response framing: success exits with
code 0, writes one output-schema-v1 success document to stdout, and writes
nothing to stderr;
application failure exits nonzero, writes one output-schema-v1 error document
to stderr, and writes no success document to stdout. There is no streaming
session, JSON Lines framing, multiplexing, or persistent process. The
manifest's `one_document` field describes this application-response framing.

Successful help and version requests remain ordinary shell-facing Clap text,
including with `--json`; they are not JSON responses or protocol failures.
Consumers must still treat a mismatched exit status, stream,
application-response document count, or schema version as a protocol failure.
If the operating system fails to start Relay, no Relay process exists to emit
an envelope, so the consumer must handle that process-API failure separately.

The fixed invocation prefix is:

```text
["--json", "--scope", "current"]
```

The selected project working directory is supplied separately as the child
process working directory; it is not an argv placeholder.

## Allowed operations

Protocol v1 allows exactly this ordered subset:

| Operation | Argument template | Mutating | Standard input |
| --- | --- | --- | --- |
| `status` | `["status"]` | no | none |
| `context` | `["context", "<options...>"]` | no | none |
| `note` | `["note", "<category>", "--stdin"]` | yes | UTF-8 note text |
| `checkpoint` | `["checkpoint", "[message]"]` | yes | none |
| `handoff` | `["handoff", "<agent>", "<detail-options...>"]` | yes | none |
| `finish` | `["finish"]` | yes | none |

Angle-bracket and square-bracket entries document required, repeated, or
optional argument positions. They are not literal argument values, shell
syntax, variables, or interpolation expressions. A consumer replaces each
placeholder with separate validated argv elements. In particular, it must not
pass `<options...>`, `<category>`, `[message]`, `<agent>`, or
`<detail-options...>` literally and must never shell-expand them. No Relay
command outside this table is part of protocol v1.

`note` is the only v1 operation with protocol stdin: its note body is supplied
as UTF-8 bytes and `--stdin` is present in argv. The other operations receive no
stdin data.

## Component responsibilities

- **Rust** owns all `.relay` state, state validation, locking, Relay command
  behavior, JSON envelopes, protocol manifest reporting, the
  project-independent `supervise` launcher, and the built-in bounded-parallel DAG
  coordinator. Rust exclusively owns aggregate coordinator state, including
  the default project path under `.relay/runs`; it never writes Go attempt
  records and reads only the bounded identity fields needed for the legacy-v1
  startup proof described below.
- **Go** owns strict decoding of the canonical manifest, one versioned
  execution request, direct process launch, filtered child environments,
  bounded output metadata, cancellation and timeout handling, process-group
  cleanup, and one versioned attempt record outside `.relay`. It supervises
  one child and never schedules DAG tasks or reads or writes Rust project state.
- **The externally advertised orchestrator remains unimplemented.** The
  immutable manifest's `orchestrator.owner: "go"` and `implemented: false`
  describe a future external process-operation capability; they do not assign
  the built-in scheduler to Go. Protocol v1 does not advertise the built-in
  Rust `run` command, so that coordinator does not retroactively change the
  manifest. Advertising it, advertising parallel execution, or changing
  allowed operations requires a future protocol version and fixtures.
- **Lua** remains the owner of a future workflow layer and remains
  `implemented: false`; there is no workflow language, parser, runtime, or
  sandbox.

The Rust aggregate run-state schema and Go attempt-record schema are separately
versioned subordinate contracts, not extensions to Rust's existing project
state schema or to `manifest-v1.json`. Existing Rust commands, schema-v1
projects, and protocol-v1 consumers therefore remain compatible.

## Built-in Rust bounded-parallel DAG coordinator

`relay run` is a one-shot, project-scoped Rust coordinator above the typed
one-child supervisor boundary:

```text
relay run [--jobs <N>] [--supervisor <ABSOLUTE_PATH>] [--state-path <ABSOLUTE_PATH>] <PLAN>
relay run [--jobs <N>] [--supervisor <ABSOLUTE_PATH>] --resume <ABSOLUTE_STATE_PATH> <PLAN>
```

Rust reads the plan from `PLAN`, discovers the current project for its default
aggregate state location, and uses the invocation directory as a task's
working directory unless that task supplies an absolute one. Every task is
launched by direct argv: `program` and each `arguments` element remain separate
OS arguments and are never parsed, interpolated, or evaluated by a shell.

### Closed plan v1, v2, and v3

Plan JSON is a closed schema that rejects unknown fields:

```json
{"schema_version": 1, "tasks": [{"id": "build", "program": "/usr/bin/make", "timeout_ms": 60000}]}
```

Schema v1 has no retry field. Schema v2 adds this optional per-task object:

`"retry": {"max_attempts": 3, "delay_ms": 1000}`

The field is invalid in v1; both plan versions remain closed schemas.
`tasks` contains 1–64 entries in stable input order. Each task has a unique ID
matching `[A-Za-z0-9][A-Za-z0-9._-]{0,47}`, optional `depends_on`, an absolute
`program`, default-empty `arguments`, an optional absolute
`working_directory`, and a required finite `timeout_ms` from 1 ms through 24
hours. Dependencies must exist, cannot repeat or refer to the task itself, and
the graph must be acyclic.

The optional execution controls are `termination_grace_ms` (default 2,000;
maximum 60,000), `stdout_limit_bytes` and `stderr_limit_bytes` (default 1 MiB
each; maximum 8 MiB each), `log_limit_bytes` (default 64 KiB; maximum 4 MiB),
and `redact_argument_indexes` (at most 32 valid, unique indexes whose declared
values total at most 64 KiB). All other numeric limits have a minimum of one.
Schema v2 retry policy has `max_attempts` from
1 through 10, including the initial attempt, and `delay_ms` from 0 through
86,400,000. Missing `retry` means no retry. Plan schema v3 additionally
allows typed `outputs` and explicit `inputs`; the plan document is bounded to
1 MiB and NUL bytes are rejected in executable, working-directory, argument,
output, and input strings.

### Structured task outputs

Plan schema v3 output declarations are an ordered array of closed objects:
`{"name": "summary", "type": "json", "path": "summary.json"}` or
`type: "file"`. Names are lowercase ASCII environment-safe identifiers.
Paths are relative normal components only; Relay rejects traversal, absolute
paths, symlinked components, and unsafe hardlinks. Relay creates
`<project>/.relay/runs/outputs/<attempt-run-id>/`, then passes each actual path
to the child as one `RELAY_OUTPUT_MANIFEST` JSON map from exact output names to
paths through existing explicit environment request fields. Relay never reads human stdout as task data.

JSON output is parsed, re-encoded, and bounded to 2 KiB. File outputs are not
copied into state: Relay records path, size, SHA-256, producer task, aggregate
run ID, attempt number, and attempt run ID after hashing at most 64 MiB. At most
four outputs and four inputs are allowed per task; the output manifest is
bounded to 4 KiB. Aggregate state remains bounded by the existing 4 MiB
encoded-state limit; a regression test covers maximum structured metadata.

Durability sequence is deliberate: Rust persists the running attempt identity;
Relay prepares its attempt output directory; Go runs one child and durably writes
its terminal record; Rust accepts only a matching successful terminal result;
Rust opens declared files without following final symlinks, checks regular-file
and link-count identity, hashes and validates them; Rust atomically persists
output records with the terminal attempt. A crash before the final write leaves
outputs unpromoted. Resume validates the same supervisor result and files, then
promotes once without rerunning. Missing, malformed, changed, stale, failed,
cancelled, timed-out, indeterminate, or mismatched evidence remains untrusted.

Consumers declare ordered `inputs` such as
`{"from_task":"inspect","output":"summary","as":"--summary"}`.
Producer task/output must exist, be a direct or transitive dependency, and have
a trusted successful record. JSON becomes one deterministic argv value; file
outputs become their revalidated absolute path. There is no implicit namespace,
filesystem guessing, shell expansion, expression language, upload, or artifact
store.
### Real repository validation example

Local smoke used schema-v3 `inspect -> test -> summarize` tasks in a Relay
checkout. `inspect` ran `/usr/bin/git status --short` and produced bounded JSON;
`test` ran `/usr/bin/cargo test --lib --all-features` and produced a report
artifact; `summarize` consumed both through explicit inputs. Actual Go supervisor
run completed with three trusted outputs. Resume completed in 0.04 seconds with
no task rerun. Separate real retry run recorded attempt 1 failed with no output
record and attempt 2 succeeded with the only task output. A paused terminal Go
success was followed by coordinator kill; resume reported reconciliation
`completed`, promoted the JSON output, and the producer counter remained one.


### Aggregate and attempt state ownership

Aggregate run-state v4 is a closed Rust schema. By default Rust atomically
persists it at `<project>/.relay/runs/<run-uuid>.json`; `--state-path` selects
an absolute custom path, which remains Rust-owned. Valid aggregate v1 records
omit `jobs` and imply sequential `jobs: 1`; v2 records include `jobs` but
no retry history. Clean v1/v2/v3 records migrate to v4 on safe resume. Running
records are reconciled from strict Go evidence before scheduling; unresolved
running and indeterminate records remain non-resumable.
Custom paths containing a `.relay` component are accepted only beneath the
current project's `.relay/runs` directory. Symlinked state-path components
are rejected; an adjacent advisory lock prevents two coordinators from owning
the same run, and resume reads are bounded to 4 MiB. Aggregate state uses
compact JSON; retained failure codes are capped at 64 bytes and messages at
256 bytes. Attempt reasons retain only termination classifications, avoiding
large diagnostic duplication; the maximum valid 64-task, ten-attempt history
is covered by a size-bound regression test below 4 MiB.
The 4 MiB aggregate cap remains enforced after bounded output metadata is added;
the maximum structured-output state is covered by a dedicated regression test.

Run status is one of `running`, `completed`, `failed`, `cancelled`, or
`indeterminate`; task status is one of `pending`, `running`, `succeeded`,
`failed`, `cancelled`, `blocked`, or `indeterminate`.
The aggregate contains schema and run identity, creation/update timestamps,
run status, requested jobs limit, ordered task records, retry deadlines, and a
sanitized copy of the closed plan. Each task's bounded `attempts` list retains
sequential attempt number, fresh UUID, Go record path, terminal status, reason,
and bounded execution summary. Closed decoding and resume validation reject
unknown fields, malformed history, and inconsistent aggregate/task state.

Arguments selected by `redact_argument_indexes` are stored as `<redacted>`;
redacted argument values are not copied into aggregate state. Before every launch,
Rust persists `running` plus a fresh attempt UUID and Go record path. A clean
typed supervisor response is persisted in its attempt history. Captured stdout,
stderr, and unrequested environment values remain excluded; declared structured
values and bounded artifact metadata enter state only after output validation.

Each attempt's Go-owned record is under
`${TMPDIR}/relay/supervisor/<attempt-uuid>.json`. Validation guarantees that
path is outside Rust-owned `.relay`, but `TMPDIR` itself may point inside the
selected project. Rust never overwrites earlier attempt records, and Go never
reads or writes `.relay`.

JSON task-outcome output contains `{ "state_path": ..., "run": ...,
"reconciliation": [...] }`; `run` equals full persisted aggregate state.
Reconciliation entries identify task, attempt, classification, and bounded
reason. Human output reports run ID/status, task statuses, reasons, paths, and
reconciliation details. Only `completed` exits zero. Clean `failed` or
`cancelled` outcomes retain `ok: true` on stdout; supervisor boundary errors
remain `ok: false` on stderr with persisted state and reconciliation data.

### Scheduling, failure, cancellation, and resume

The coordinator permits up to N active tasks from `--jobs` (default `1`). It
repeatedly fills available slots by selecting the first input-order ready task
whose dependencies succeeded and whose retry deadline has elapsed. It persists
each launch as `running` with a fresh attempt UUID and Go record path before
launch, then waits for completion. `--jobs 1` preserves sequential behavior;
selection stays deterministic when completion order varies.

A task succeeds only for typed `completed`, `exit_zero`, and exit code zero.
Schema-v2 retry policy reruns only known non-cancellation failures: nonzero
exit, start failure, timeout, and definite non-cancellation forced cleanup.
Transport/protocol errors, indeterminate outcomes, `CancelledBeforeLaunch`,
and cancellation outcomes never retry. Retry history is bounded by ten total
attempts per task, including initial execution; Relay does not infer idempotency.

A waiting retry remains `pending` with `retry_ready_at`; it consumes no job
slot, leaves descendants unready, and lets unrelated ready work continue. An
exhausted failure blocks descendants transitively. A boundary error after an
attempt was persisted as running makes that attempt and run indeterminate; it
does not become rerunnable work.

Catchable `SIGINT` or `SIGTERM` is latched globally and forwarded through a
race-safe registry to every active Go supervisor. Registration during startup
rechecks the latch, and once cancellation is latched no new DAG attempt is
scheduled. Clean cancellation results are persisted per attempt; pending work,
including a waiting retry, remains resumable after clean exit. Coordinator state
failures stop launches, drain and join workers, preserve uncertain attempts as
indeterminate, and best-effort persist before returning the original error.

Resume requires plan again. Rust compares sanitized form and validates
attempt history and aggregate consistency. It reconciles running attempts
before scheduling: strict terminal Go records update existing attempts in place,
with live completion and retry semantics. Verified active attempts refuse
resume without duplicate launch. Missing, malformed, mismatched, stale, or
unverifiable work becomes indeterminate; terminal and blocked tasks never rerun.
A future retry deadline is polled without holding a job slot.

### Coordinator limits and non-guarantees

The coordinator and supervisor execution path is currently Linux-only. The
jobs limit is bounded by the plan's 64-task maximum; `--jobs 1` is deliberate
sequential behavior, not hidden parallelism. There is no daemon, queue,
network or remote execution, authentication, provider, workflow inference, Lua
runtime, sandbox, global retry engine, exponential backoff, jitter, or
transcript scraping. Go's Linux process-group cleanup covers the direct child
and descendants that remain in that group, subject to supervisor limitations.
Neither the coordinator nor the supervisor guarantees containment or cleanup of a process
that detaches into another session/group, nor recovery after supervisor
`SIGKILL`, kernel failure, or machine loss; use an external cgroup or service
boundary when those guarantees are required.

## Go one-child supervisor

The built `relay-supervisor` executable reads exactly one JSON execution request
from stdin, up to 1 MiB. A valid request produces one output-schema-v1 success
envelope on stdout with `command: "supervise"` and the execution result in
`data`. Child nonzero exits, start failures, timeouts, cancellations, and
forced kills are execution data, so the supervisor process still exits zero
when it reports them successfully. Malformed or unsupported requests and
supervisor infrastructure failures produce one output-schema-v1 error envelope
on stderr and a nonzero supervisor exit. There is no JSON Lines stream or
second child.

When launched through Rust, the executable installs catchable
`SIGINT`/`SIGTERM` handling and tags that cancellation source before the private
`RELAY_SUPERVISOR_READY_FD` handshake or request framing. Go strictly parses
the inherited descriptor as 3 or greater, writes exactly one ASCII `R`
readiness byte after signal registration, and then waits without reading the
already-buffered request. Rust publishes the supervisor PID, enables
forwarding, forwards any latched signal, writes exactly one ASCII `A`, and
closes its endpoint. Go requires that exact `A` followed by EOF before it closes
its endpoint and proceeds. An invalid descriptor or any write,
acknowledgement, trailing-data, or close failure produces the ordinary
structured error on stderr and a nonzero exit; the supervisor never reads or
executes the request.

For direct manual invocation without the readiness variable, request framing is
read and decoded with the operating system's default signal disposition, so
`SIGINT` or `SIGTERM` terminates promptly even while stdin remains open. Signal
cancellation handling is installed after a complete valid request is decoded
and before its child execution begins.

For the bundled supervisor, this two-way barrier means Go owns
`SIGINT`/`SIGTERM` before advertising `R`, while `A` proves Rust has made that
ownership reachable through safe forwarding before Go can start request
execution. The launcher also supports legacy v1 supervisors through the
state-creation proof described below. No child is launched until the complete
request has been read, decoded, and validated. Malformed and unsupported
requests retain their error framing even if cancellation is already pending.
Once a valid request is decoded, an early cancellation is persisted and
reported in the ordinary success envelope as `accepted` → `cancelled`, without
a child start.

Before decoding requests, Go strictly decodes the same embedded
[`manifest-v1.json`](../protocol/manifest-v1.json) used by Rust. It requires the
v1 direct-argv, no-shell, stream, ownership, and ordered-operation contract.
The execution request reuses `protocol_version: 1` and the established envelope
fields; it does not add fields to the immutable manifest.

### Rust launcher

The command is exactly:

```text
relay supervise [options] -- <ABSOLUTE_PROGRAM> [ARG...]
```

The literal `--` is required to separate launcher options from the direct child
argv. An absolute `--supervisor PATH` takes precedence over an absolute
`RELAY_SUPERVISOR` path, which takes precedence over a `relay-supervisor`
sibling beside the executing `relay` binary. Discovery never searches `PATH`.
The remaining options are
`--run-id`, `--state-path`, `--working-directory`, `--timeout-ms`,
`--termination-grace-ms`, `--stdout-limit-bytes`, `--stderr-limit-bytes`,
`--log-limit-bytes`, and repeatable `--redact-argument-index`. The default run
ID is a UUID. The default state path is
`<temporary-directory>/relay-runs/<run-id>.json`; `--state-path` selects a
custom absolute Go-owned record path outside lexical `.relay`. A prior record
at that custom path may be reused: Go replaces it atomically for the new run.

Rust validates the absolute program, run ID, limits, redaction indexes,
absolute working directory, and state path before constructing every v1 request
field with an empty user environment. It rejects an encoded request, including
its terminating newline, above 1 MiB before spawning the supervisor. Otherwise
it writes and closes the one-document request immediately after spawn, waits
for startup proof, and then concurrently drains each response stream through
EOF while retaining at most 32 MiB. It accepts only one strict envelope:
exit status zero requires a success envelope on stdout and empty stderr;
nonzero requires an error envelope on stderr and empty stdout.

The Rust boundary strictly decodes the envelope, execution result, and every
nested v1 execution object into closed typed structures; unknown fields,
unknown enum values, missing fields, and wrong field types are rejected. It
then validates cross-field consistency for lifecycle, termination, failure,
timeout, cancellation, timestamps, stream counts, lifecycle history, and
observation ordering/accounting. It binds the result to the request by
requiring execution protocol `1`, the requested run ID and program, the exact
requested arguments with declared positions redacted, and the configured
timeout, termination grace, and observation-log limit. It also requires output
schema `1`, command `supervise`, a nonempty supervisor version, and the proper
success/error stream and exit-status pairing.

Execution outcomes—including child nonzero exit, start failure, timeout, and
cancellation—are transport success. The launcher's JSON data is
`{ "state_path": ..., "execution": ... }`; human output summarizes lifecycle,
reason, exit status, duration, state path, and retained stdout/stderr byte
counts.

The readiness transport is an additive private launcher optimization and safety
handshake, not a field or requirement of JSON execution protocol v1. For each
Linux launch, Rust creates a private close-on-exec Unix stream socketpair and
normalizes both endpoints to descriptors 3 or greater even when a standard
descriptor is closed. In the supervisor's pre-exec setup Rust marks every
descriptor from 3 upward close-on-exec, then makes only the supervisor socket
endpoint inheritable; descriptors 0–2 are left alone. Thus an unrelated ambient
inheritable descriptor cannot cross the supervisor exec boundary. Rust passes
the supervisor endpoint number only in `RELAY_SUPERVISOR_READY_FD` and closes
its copy after spawn.

Rust publishes the supervisor PID and enables signal forwarding after either
of two startup proofs within five seconds. The current handshake starts with
exactly one ASCII `R` byte from Go. Rust then publishes the PID, forwards any
latched signal, writes exactly one ASCII `A`, and closes its launcher endpoint;
Go requires `A` followed by EOF before processing the request. For an older or
custom v1 supervisor that ignores the private environment variable, Rust
instead accepts creation or replacement of the requested regular state file
when a bounded decode shows `run_state_version: 1`, a nonempty supervisor
version, execution protocol `1`, and the requested run ID. Rust snapshots any
preexisting file before spawn, so an unchanged stale record cannot supply this
proof; atomic replacement for explicit state-path reuse remains supported.
Rust also checks this proof when the readiness socket reaches EOF or the
supervisor has already exited, so a legacy v1 supervisor may atomically persist
its matching record, emit its valid response, and exit immediately. This
compatibility check does not make the Go record a Rust-owned result source—the
final typed response remains authoritative.

EOF or supervisor exit before `R` without a valid state proof, invalid readiness
data, acknowledgement failure, and timeout are transport failures that trigger
cleanup. After startup proof, Rust forwards pending or new `SIGINT`/`SIGTERM`
only to the active supervisor PID while waiting. It observes supervisor exit
without reaping, disarms forwarding while that PID still cannot be reused, and
only then reaps the process. Go alone performs child-group cleanup, with the
limitations below. The launcher is project-independent and neither discovers nor
reads `.relay`. It does not reconcile existing Go records; the Go supervisor
writes the requested record at `state_path` for each attempt.

### Execution request

All paths are absolute. Unknown fields, trailing JSON documents, NUL bytes,
unsupported versions, unsafe limits, and a `state_path` under any `.relay`
component are rejected before launch.

| Field | Meaning |
| --- | --- |
| `protocol_version` | Must be `1`. |
| `run_id` | Stable 1–128 character identifier matching `[A-Za-z0-9][A-Za-z0-9._-]*`. |
| `program` | Absolute executable path, passed directly to the OS. |
| `arguments` | Ordered argument vector; elements are never concatenated or shell-evaluated. |
| `redact_argument_indexes` | At most 32 argument positions replaced by `<redacted>` in result/run-state argv metadata; the indexed values may total at most 64 KiB. If retained output contains a declared value or ends with its truncated prefix, the entire retained stream text becomes `<redacted>`. |
| `working_directory` | Absolute child working directory, separate from argv. |
| `environment` | Ordered `{name,value}` entries; policy and precedence are below. |
| `timeout_ms` | `0` disables the execution timeout only; bounded internal cleanup and drain deadlines still apply. The maximum is 24 hours. |
| `termination_grace_ms` | Grace period before escalation; default 2,000 ms, maximum 60,000 ms. |
| `stdout_limit_bytes`, `stderr_limit_bytes` | Per-stream retained-byte limits; default 1 MiB each, maximum 8 MiB each. |
| `log_limit_bytes` | Bound for output-observation metadata; default 64 KiB, maximum 4 MiB. |
| `state_path` | Absolute Go-owned JSON record path outside `.relay`. |

The absolute-program requirement avoids ambient `PATH` lookup by the
supervisor. `PATH` may still be present in the deliberately constructed child
environment for programs that explicitly launch their own subprocesses.

### Lifecycle and result

Normal started executions record
`accepted` → `starting` → `running` → the terminal lifecycle. An OS start
failure records `accepted` → `starting` → `failed_to_start`. Cancellation
observed before starting may instead record `accepted` → `cancelled`. The final
lifecycle is one of:

- `completed` for a natural zero or nonzero direct-child exit after its `Wait`
  completes and any required in-group descendant cleanup is confirmed;
- `failed_to_start` when the OS cannot start the executable;
- `timed_out` when timeout termination completes during the grace period;
- `cancelled` when cancellation termination completes during the grace period;
- `killed` when timeout, cancellation, or surviving-descendant cleanup requires
  forced process-group termination and its result is confirmed; or
- `protocol_failure` for malformed framing or supervisor infrastructure
  failure, including an unconfirmed direct-child reap, process-group cleanup,
  or drain shutdown. Protocol failures use the error envelope and are also
  written to an already-created run record when persistence remains available.

`termination_reason` removes ambiguity between zero/nonzero completion, start
error, graceful or forced timeout/cancellation, graceful or forced descendant
cleanup, and protocol failure. `exit_code` is populated only after the direct
child's `Wait` completes; no result infers it from `ProcessState` before then.
`descendants_terminated` and `descendants_killed` mean the corresponding
in-group cleanup was confirmed, not merely that a signal was sent. Results
include run and protocol versions, supervisor version, sanitized
program/arguments/environment metadata, accepted/start/completion UTC
timestamps, duration in milliseconds, exit code when the OS provides one,
timeout/cancellation/termination details, both streams, lifecycle history, and
bounded output observations. Lifecycle and output observations share monotonic
sequence numbers, making the observed order explicit without claiming that
concurrent stdout/stderr reads reproduce an unknowable byte-level interleaving.

### Timeout, cancellation, and process trees

On Linux, the supervisor launches one direct child in a new process group.
Request timeout, caller cancellation, and catchable supervisor
`SIGINT`/`SIGTERM` first send graceful termination to that group. The timeout
clock starts immediately after a successful child start, before running-state
persistence. After `termination_grace_ms`, the group receives forced
termination. Direct-child `Wait` runs independently of stream draining, so an
inherited stdout or stderr descriptor cannot prevent reaping that child. After
that wait, if a non-zombie in-group descendant remains, the supervisor applies
the same graceful-then-forced cleanup before returning. After forced
termination, direct-child reaping and process-group confirmation have a 250 ms
window. Inability to confirm both produces `protocol_failure` with
`termination_error`, rather than a successful termination result.

The Linux process-tree check retains a full `/proc` scan: it detects
leader-exited, non-zombie in-group descendants and distinguishes them from
zombies. Termination and cleanup poll at a 20 ms cadence, reducing the former
5 ms scan frequency while preserving detection within each bounded window.

Process groups cannot contain a descendant that deliberately calls `setsid` or
moves itself to another group. The supervisor cannot detect or clean up a
detached process after it closes the inherited streams, so a successful result
confirms only the direct child and the original process group; it does not prove
that no detached descendant exists. Untrusted children that may detach, an
uncatchable supervisor `SIGKILL`, kernel failure, or machine loss require an
external cgroup or service containment boundary.

### Environment and secret policy

The child never receives the complete parent environment. Only `LANG`,
`LC_ALL`, `LC_CTYPE`, `PATH`, `SYSTEMROOT`, `TEMP`, `TMP`, `TMPDIR`, `TZ`, and
`WINDIR` may be inherited. User names must be portable uppercase names.
Sensitive-name fragments such as `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL`,
`PRIVATE_KEY`, `ACCESS_KEY`, `API_KEY`, `COOKIE`, and `BEARER` are filtered
from inherited data and rejected in user data without echoing their values.

Precedence is deterministic: allowed inherited values are lowest, user entries
replace them in request order (the last duplicate wins), and required
`RELAY_PROTOCOL_VERSION`, `RELAY_RUN_ID`, and
`RELAY_SUPERVISOR_VERSION` values always win. Result and persisted environment
metadata contain sorted names, sources, the public inherited allowlist, and
counts only—never values. Declared sensitive argument positions are redacted
from argument metadata. If a declared value appears in retained stdout/stderr,
the complete retained stream text is conservatively replaced by `<redacted>`;
this single bounded decision also covers overlapping values and truncation at
a secret prefix. Callers must not put undeclared secrets in argv or arrange for
a child to print independently obtained secrets; arbitrary output cannot be
classified reliably.

### Bounded output and observations

The supervisor owns distinct stdout and stderr pipes, closes its parent writer
copies immediately after a successful `Start`, and drains both streams
concurrently. This prevents the parent-held writers from prolonging EOF and
prevents either stream from blocking the other. Draining continues after either
retention limit is reached. After child and process-group handling, drains get
250 ms to finish naturally. The supervisor then closes any remaining readers
and gives those goroutines a final 250 ms to stop. Requiring forced reader
closure, or failing to stop afterward, is `protocol_failure` with
`termination_error`, not a completed result.
`bytes_observed`, `bytes_retained`, and `truncated` describe original bytes;
invalid UTF-8 in retained bytes is replaced with U+FFFD before JSON encoding.
The bounded observation log stores only sequence, stream, and chunk byte count,
never output content. It reports its configured limit, accounted bytes, and a
truncation flag.

### Atomic run state

The versioned Go-owned record is:

```text
run_state_version: 2
state_path: <the requested Go record path>
supervisor_version: 0.1.0
process_identity: <the supervisor and optional child identities>
execution: <the current or complete execution result>
```

It is written at accepted, starting, running, and terminal transitions with
mode 0600. Accepted and starting records contain supervisor identity; after
child start, Go records child identity before publishing running state. Each
replacement is encoded with a 140 MiB ceiling, written to a same-directory
temporary file, synced, closed, atomically renamed, and followed by a directory
sync. On Linux, persistence pins the validated parent directory by file
descriptor, verifies its identity, and performs target open/rename relative to
that handle so a path swap cannot redirect the operation. Failed replacement
leaves prior valid record untouched and removes the temporary file. Reload
rejects unknown fields, multiple documents, oversized records, and unsupported
versions; version 1 remains readable for legacy terminal recovery. State files
and parent directories may not be symbolic links, and neither lexical nor
resolved path may enter Rust-owned .relay.

### Crash reconciliation

relay run --resume inspects persisted running attempts before it schedules
anything. Rust reads exact supervisor state path recorded in aggregate, opens
it without following a file symlink, and requires one bounded JSON record. A
terminal result is promoted only when its wrapper and execution data bind to
the attempt UUID, requested state path, program, sanitized argv, execution
options, supervisor version, and protocol version. Rust then applies same
completion transition used by live worker results, including summary, reason,
dependency, cancellation, and retry handling.

Each attempt is classified independently:

- completed, failed, or cancelled: Go record is terminal and passes existing
  strict Rust response validation. Aggregate running attempt is updated in
  place; no new history entry is created.
- still_running: version-2 active record is valid and both required process
  identities still match. Resume refuses with message "attempt still active;
  resume cannot continue this branch yet"; it never duplicates or adopts that
  process.
- indeterminate: record is absent, partial, malformed, stale, mismatched,
  protocol-failed, terminal-invalid, or process identity cannot be proven. The
  attempt becomes indeterminate and descendants remain blocked.
- refused: attempt already persisted as indeterminate. No retry or new evidence
  is inferred.

Version 2 records add state_path and process_identity. On Linux each process
identity contains PID, /proc/<pid>/stat start time ticks, process-group ID,
session ID, and /proc/<pid>/exe identity. Rust checks supervisor identity for
accepted/starting records and both supervisor plus child identities for a
running record. Start time prevents PID reuse; group/session and executable
checks prevent a merely matching numeric PID from proving the attempt. Rust
reads stat identity again before acceptance to reduce proc TOCTOU risk. Legacy
version-1 terminal records remain readable and can reconcile through strict
terminal-result validation. Legacy active records lack durable process identity
and remain indeterminate.

Crash windows stay conservative. A terminal Go record durably written before
coordinator aggregate completion persistence is recoverable. An active Go
record is reportable as still running only while exact identity is provable.
Crash before startup state, or after child launch before durable child identity,
is indeterminate. After durable active identity but before terminal state, exact
live identities produce still_running; if identity is gone or mismatched, the
result is indeterminate. Missing state, vanished process, and failed reads are
also indeterminate. Relay never rolls back a running attempt merely because
process or state file is gone, and never retries it.
Coordinator crash during
cancellation is recoverable only from valid persisted cancelled Go result; that
result is never retried.

Aggregate run-state schema remains version 3. Go attempt records use version 2,
with version 1 retained for terminal compatibility. Versions are separate from
plan, output, and process-protocol versions.

### Platform support

Execution and full in-group process cleanup are currently supported on Linux,
using isolated process-group mechanics and full `/proc` inspection in
`process_tree_linux.go`. Other Go targets compile the protocol and fixture
packages, but the runner rejects execution before child launch rather than
advertising process behavior it cannot verify. No platform silently degrades
to shell execution.

### Complete request-to-record example

Request on supervisor stdin:

```json
{
  "protocol_version": 1,
  "run_id": "golden-success",
  "program": "/usr/bin/printf",
  "arguments": ["%s", "literal;$(not-a-shell)"],
  "redact_argument_indexes": [],
  "working_directory": "/var/tmp",
  "environment": [{"name": "LANG", "value": "C"}],
  "timeout_ms": 5000,
  "termination_grace_ms": 250,
  "stdout_limit_bytes": 1024,
  "stderr_limit_bytes": 1024,
  "log_limit_bytes": 4096,
  "state_path": "/var/tmp/relay-runs/golden-success.json"
}
```

The following legacy v1 terminal record remains accepted for compatibility;
current v2 records add state_path and process_identity as shown above.

```json
{
  "run_state_version": 1,
  "supervisor_version": "0.1.0",
  "execution": {
    "run_id": "golden-success",
    "protocol_version": 1,
    "supervisor_version": "0.1.0",
    "program": "/usr/bin/printf",
    "arguments": ["%s", "literal;$(not-a-shell)"],
    "environment": {
      "inherited_allowlist": ["LANG", "LC_ALL", "LC_CTYPE", "PATH", "SYSTEMROOT", "TEMP", "TMP", "TMPDIR", "TZ", "WINDIR"],
      "effective": [
        {"name": "LANG", "source": "user"},
        {"name": "RELAY_PROTOCOL_VERSION", "source": "required"},
        {"name": "RELAY_RUN_ID", "source": "required"},
        {"name": "RELAY_SUPERVISOR_VERSION", "source": "required"}
      ],
      "blocked_inherited_count": 0,
      "duplicate_user_overrides": 0
    },
    "accepted_at": "2026-01-02T03:04:05Z",
    "started_at": "2026-01-02T03:04:05.01Z",
    "completed_at": "2026-01-02T03:04:05.11Z",
    "duration_ms": 100,
    "lifecycle": "completed",
    "termination_reason": "exit_zero",
    "exit_code": 0,
    "failure": null,
    "timeout": {"configured_ms": 5000, "triggered": false},
    "cancellation": {"requested": false, "source": "none"},
    "termination": {"grace_ms": 250, "graceful_attempted": false, "escalated": false},
    "stdout": {
      "text": "literal;$(not-a-shell)",
      "bytes_observed": 22,
      "bytes_retained": 22,
      "truncated": false,
      "first_sequence": 4,
      "last_sequence": 4
    },
    "stderr": {
      "text": "",
      "bytes_observed": 0,
      "bytes_retained": 0,
      "truncated": false,
      "first_sequence": null,
      "last_sequence": null
    },
    "lifecycle_history": [
      {"sequence": 1, "lifecycle": "accepted", "timestamp": "2026-01-02T03:04:05Z"},
      {"sequence": 2, "lifecycle": "starting", "timestamp": "2026-01-02T03:04:05.01Z"},
      {"sequence": 3, "lifecycle": "running", "timestamp": "2026-01-02T03:04:05.02Z"},
      {"sequence": 5, "lifecycle": "completed", "timestamp": "2026-01-02T03:04:05.11Z"}
    ],
    "output_observations": {
      "limit_bytes": 4096,
      "bytes_used": 57,
      "truncated": false,
      "entries": [{"sequence": 4, "stream": "stdout", "bytes": 22}]
    }
  }
}
```

## Security requirements

Protocol v1 and the supervisor require all of the following:

- Invoke both Relay and supervised children with direct argv APIs. Never invoke
  a shell, interpolate a command string, or evaluate placeholder text.
- Validate Relay operation names against the manifest and validate supervisor
  requests, versions, absolute paths, limits, and framing before launch.
- Treat JSON and child output as untrusted; require expected streams, one
  document, negotiated schema versions, and bounded retention.
- Never read or write `.relay` from Go. Rust remains the only owner of project
  state and locking; Go owns only separate per-attempt records outside `.relay`.
- Do not infer an externally advertised orchestrator, queue, workflow runtime,
  authentication mechanism, credential discovery, network service, or telemetry.
  The built-in Rust coordinator owns bounded parallel scheduling; Go is not its
  scheduler.
- Filter the child environment, reject sensitive names, redact declared
  sensitive arguments, and persist metadata rather than environment values.
- Keep stdout, stderr, and observation metadata bounded while continuing to
  drain both pipes.

## Canonical fixtures and evolution

[`protocol/manifest-v1.json`](../protocol/manifest-v1.json) remains the
canonical immutable v1 manifest data object. Its field names, values, operation
order, and templates are contract data; JSON member order and pretty-print
whitespace are not transport semantics. The fixture pins `relay_version` to
`0.2.0`, while a running binary reports its compiled package version.

Language-neutral supervisor fixtures under
[`protocol/fixtures/supervisor-v1`](../protocol/fixtures/supervisor-v1) cover a
golden request and response, malformed input, unsupported version, timeout,
cancellation, truncated streams, start failure, and error envelopes. Go
strictly decodes and deterministically reserializes all corresponding fixtures.
Rust consumes the same fixture set through its closed typed response boundary,
rejecting unknown nested execution fields and inconsistent or request-mismatched
results without claiming ownership of Go execution semantics.

A change to Relay transport, framing, streams, exit semantics,
responsibilities, operation membership/templates, stdin behavior, or manifest
capability meaning requires a new orchestration protocol version and fixture.
Existing fixtures remain unchanged. Output-envelope changes require a new
output schema version. Go run-record changes require a new
`run_state_version`; Rust state changes follow Rust's separate schema and
migration policy.

## Current limitations

The Go component supervises exactly one local process and persists one separate
local attempt record outside `.relay`. The built-in Rust coordinator supplies a
dependency graph and bounded parallel scheduling, but neither it nor `relay
supervise` makes the manifest's externally advertised `orchestrator.implemented`
value true. There is no queue, global retry engine, daemon, remote execution,
authentication, deployment layer, Lua parser, Lua runtime, or Lua sandbox. Execution is currently limited to Linux. Self-detaching descendants
and uncatchable process or machine termination require an external OS
containment boundary.

The configured supervisor is a trusted local executable. Rust's 32 MiB
per-response-stream limit bounds memory, not the runtime of a supervisor that
violates its contract. Standalone `relay supervise` permits `timeout_ms: 0`,
so its requested child may run indefinitely; every built-in `relay run` task
instead requires a finite timeout of at most 24 hours. Go's internal
termination and drain windows remain bounded once cleanup begins.
