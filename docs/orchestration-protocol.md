# Relay orchestration process protocol v1

Relay exposes a negotiated, versioned process boundary for a possible future
orchestrator. The read-only `relay protocol` command reports compiled
capabilities without discovering a project, reading or validating `.relay`, or
creating files. The canonical v1 manifest is
[`protocol/manifest-v1.json`](../protocol/manifest-v1.json).

Relay now includes a standalone Go one-child supervisor at
`cmd/relay-supervisor`. It implements the low-level execution safety boundary
described below, but it is not invoked or advertised by the Rust CLI and is not
a scheduler, queue, daemon, remote executor, or Lua runtime. Allowed Git-aware
Rust operations may invoke `git`; existing note editor behavior remains outside
protocol v1.

## Negotiation and versions

A consumer first runs `relay --json --scope current protocol` by direct argument
vector and inspects the success envelope's `data` object. It must accept only a
`protocol_version` it implements. Protocol version, Relay crate version, JSON
output schema version, and on-disk state schema version are independent:

- `protocol_version: 1` identifies this orchestration contract.
- `relay_version` identifies the Relay binary that reported it.
- `output_schema_version: 1` identifies Relay's success and error envelopes.
- `state.schema_version: 1` identifies the Rust-owned `.relay` state schema.

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
  behavior, JSON envelopes, and protocol manifest reporting. Schema-v1 state
  remains the source of truth. Rust does not read or write Go run records.
- **Go** owns strict decoding of the canonical manifest, one versioned
  execution request, direct process launch, filtered child environments,
  bounded output metadata, cancellation and timeout handling, process-tree
  cleanup, and versioned run records outside `.relay`.
- **Orchestration and scheduling remain unimplemented.** The immutable
  manifest still reports `orchestrator.implemented: false` because no Rust
  command invokes the supervisor and there is no queue, dependency graph,
  retry engine, daemon, or long-running service.
- **Lua** remains the owner of a future workflow layer and remains
  `implemented: false`; there is no workflow language, parser, runtime, or
  sandbox.

The Go records are a separately versioned, subordinate execution contract, not
an extension to Rust state or to `manifest-v1.json`. Existing Rust commands,
schema-v1 projects, and protocol-v1 consumers therefore remain compatible. A
future Rust command that advertises or launches the supervisor would require a
new capability manifest version.

## Go one-child supervisor

`go run ./cmd/relay-supervisor` (or the built `relay-supervisor` executable)
reads exactly one JSON execution request from stdin, up to 1 MiB. A valid
request produces one output-schema-v1 success envelope on stdout with
`command: "supervise"` and the execution result in `data`. Child nonzero exits,
start failures, timeouts, cancellations, and forced kills are execution data,
so the supervisor process still exits zero when it reports them successfully.
Malformed or unsupported requests and supervisor infrastructure failures
produce one output-schema-v1 error envelope on stderr and a nonzero supervisor
exit. There is no JSON Lines stream or second child.

The supervisor installs catchable termination-signal handling only after the
complete stdin request has been read and decoded. Signals received while request
framing is still incomplete are not interpreted as cancellation of a child,
because no child has been launched.

Before decoding requests, Go strictly decodes the same embedded
[`manifest-v1.json`](../protocol/manifest-v1.json) used by Rust. It requires the
v1 direct-argv, no-shell, stream, ownership, and ordered-operation contract.
The execution request reuses `protocol_version: 1` and the established envelope
fields; it does not add fields to the immutable manifest.

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

Every accepted execution records `accepted`, then `starting`; a successful OS
start records `running`. The final lifecycle is one of:

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
run_state_version: 1
supervisor_version: 0.1.0
execution: <the complete execution result>
```

It is written at accepted, starting, running, and final transitions with mode
`0600`. Each replacement is encoded with a 140 MiB ceiling, written to a
same-directory temporary file, synced, closed, atomically renamed, and followed
by a directory sync. On Linux, persistence pins the validated parent directory
by file descriptor, verifies its identity, and performs target open/rename
relative to that handle so a path swap cannot redirect the operation. Failed
replacement leaves the prior valid record untouched and removes the temporary
file. Reload rejects unknown fields, multiple documents, oversized records,
and unsupported versions. State files and parent directories may not be
symbolic links, and neither the lexical nor resolved path may enter Rust-owned
`.relay`.

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

After the direct child exits, `state_path` contains this complete record (fixed
timestamps make the example reproducible):

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
  state and locking.
- Do not infer an implemented scheduler, workflow runtime, authentication
  mechanism, credential discovery, network service, or telemetry.
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
Rust consumes the same fixture set to verify the existing
output-schema-v1 envelope boundary without claiming ownership of Go execution
semantics.

A change to Relay transport, framing, streams, exit semantics,
responsibilities, operation membership/templates, stdin behavior, or manifest
capability meaning requires a new orchestration protocol version and fixture.
Existing fixtures remain unchanged. Output-envelope changes require a new
output schema version. Go run-record changes require a new
`run_state_version`; Rust state changes follow Rust's separate schema and
migration policy.

## Current limitations

The Go component supervises exactly one local process and persists one local
run record. No Rust command invokes it yet, so `orchestrator.implemented`
correctly remains false. There is no scheduler, queue, dependency graph,
retry/resume engine, daemon, remote execution, authentication, deployment
layer, Lua parser, Lua runtime, or Lua sandbox. Execution is currently limited
to Linux. Self-detaching descendants and uncatchable process or machine
termination require an external OS containment boundary.
