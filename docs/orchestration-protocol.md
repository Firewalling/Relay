# Relay orchestration process protocol v1

Relay exposes a negotiated, versioned process boundary for a possible future
orchestrator. The read-only `relay protocol` command reports compiled
capabilities without discovering a project, reading or validating `.relay`, or
creating files. The canonical v1 manifest is
[`protocol/manifest-v1.json`](../protocol/manifest-v1.json).

This document defines a process protocol only. Relay does not currently include
a Go orchestrator, workflow scheduler, run-file format, Lua runtime, Lua
sandbox, or agent/workflow process launcher. Allowed Git-aware operations may
invoke `git`; existing note editor behavior is outside protocol v1.

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

Each invocation attempts to start one process. For a successfully parsed
invocation that reaches a started application operation, application-response
framing is exactly one JSON document; there is no streaming session, JSON Lines
framing, multiplexing, or persistent process. On success, the process exits
with code 0, writes exactly one output-schema-v1 success document to stdout,
and writes nothing to stderr. When such an application operation fails, it
exits nonzero, writes exactly one output-schema-v1 error document to stderr,
and does not write a success document to stdout. The manifest's `one_document`
field describes this application-response framing.

Clap argument-parsing failures and failures before envelope-producing
application startup may instead emit plain text or another non-document
diagnostic. They remain nonzero protocol failures. Consumers must also treat a
mismatched exit status, stream, application-response document count, or schema
version as a protocol failure.

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

- **Rust** owns all `.relay` state, state validation, locking, command behavior,
  JSON envelopes, and protocol manifest reporting. Schema-v1 state remains the
  existing source of truth.
- **Go** is reserved as the owner of a future orchestrator. It is explicitly
  `implemented: false`; no Go scheduler or process launcher exists in Relay.
  A future implementation must use the declared Relay commands rather than
  reading or writing `.relay` directly.
- **Lua** is reserved as the owner of a future workflow layer. It is explicitly
  `implemented: false`; no workflow language, runtime, parser, or sandbox exists
  in Relay.

The v1 boundary adds no persistence field or migration. Existing commands and
schema-v1 state remain compatible. A future orchestration component must not
invent state outside the Rust-owned command behavior or treat the output schema
as the state schema.

## Security requirements

Protocol v1 requires all of the following:

- Invoke Relay with a direct argv API. Never invoke a shell, interpolate a
  command string, or evaluate placeholder text.
- Validate operation names and arguments against the allowed subset before
  launch.
- Treat JSON from the expected stream as untrusted input and require the
  negotiated protocol and output schema versions.
- Do not infer permission to launch an agent or workflow process. The current
  Rust implementation provides no such launcher, and the manifest declares no
  implemented orchestrator or workflow runtime. Allowed Git-aware operations
  may invoke `git`; the existing interactive note editor is outside protocol
  v1, whose `note` template requires `--stdin`.
- Do not add implicit authentication, credential discovery, network access, or
  telemetry. None is part of v1.
- Any future child-process environment must be deliberately filtered rather
  than inheriting ambient secrets or credentials by default.
- Any future captured logs must have explicit byte and record bounds, with
  truncation handled as data rather than allowing unbounded memory or storage.

## Canonical fixture and evolution

[`protocol/manifest-v1.json`](../protocol/manifest-v1.json) is the canonical v1
data object. Its field names, values, operation order, and operation templates
are contract data; JSON object member ordering and pretty-print whitespace are
not transport semantics. The fixture pins `relay_version` to `0.2.0`, while a
running binary reports its compiled package version.

A change to transport, framing, streams, exit semantics, responsibilities,
operation membership or templates, stdin behavior, or capability meaning
requires a new orchestration protocol version and a new versioned fixture.
Existing versioned fixtures remain unchanged. Output-envelope changes require a
new output schema version; state-record changes follow the separate state
schema compatibility and migration policy. Documentation may clarify v1 only
when it does not change observable contract data or semantics.

## Current limitations

Only the Rust CLI and its existing project-local operations are implemented.
The protocol manifest is capability negotiation, not orchestration execution.
There is no Go implementation, scheduler, run-file format, persisted run state,
agent launcher, retry system, workflow parser, Lua runtime, or Lua sandbox.
Relay performs no implicit authentication, network access, or telemetry.
