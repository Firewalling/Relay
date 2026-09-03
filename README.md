# Relay

> Continuous, local context for terminal agents.

Relay contains a Rust CLI for recording project-local work sessions,
checkpoints, categorized notes, handoffs, and bounded context for the next
terminal agent. It also provides a Rust-owned, one-shot `relay run` coordinator
for deterministic bounded-parallel DAG plans, reusable project-local workflows,
and `relay supervise`, a Rust launcher for one explicitly requested local child
process through the standalone Go supervisor. Relay does not implement an
agent or model runtime, call model-provider APIs, inspect chat transcripts, or
replace Git.

## Prerequisites and installation

The Rust CLI requires a stable Rust toolchain with Cargo. Building and running
the optional one-child supervisor requires Linux and Go 1.26 or newer. Install
`git` too for Git-aware snapshots; Relay also works outside a Git repository.

Install the checked-out source directly:

```sh
cargo install --path .
relay --version
```

For a local release build and an explicit user-local install:

```sh
cargo build --release
mkdir -p "$HOME/.local/bin"
cp target/release/relay "$HOME/.local/bin/relay"
chmod 755 "$HOME/.local/bin/relay"
```

Build the standalone supervisor without installing it:

```sh
go build -o relay-supervisor ./cmd/relay-supervisor
```

`relay supervise` discovers that executable only by an explicit absolute
`--supervisor PATH`, then an absolute `RELAY_SUPERVISOR` path, then a
`relay-supervisor` sibling of the running `relay` executable. It never searches
`PATH` for the supervisor.
Ensure `$HOME/.local/bin` is on `PATH` if that is where you install Relay;
Relay does not currently claim prebuilt packages or a crates.io release.

## Run a bounded-parallel task plan

```sh
relay run [--jobs <N>] [--supervisor <ABSOLUTE_PATH>] [--state-path <ABSOLUTE_PATH>] <PLAN>
relay run [--jobs <N>] [--supervisor <ABSOLUTE_PATH>] --resume <ABSOLUTE_STATE_PATH> <PLAN>
```

`PLAN` is a JSON file. A new run accepts an optional absolute aggregate
`--state-path`; `--state-path` and `--resume` are mutually exclusive. Supervisor
discovery follows the same rules as `relay supervise`. Programs and working
directories in the plan must be absolute. Arguments are passed directly as
distinct argv elements: Relay never invokes or evaluates a shell. On the
initial run, a task without `working_directory` uses Relay's invocation
directory; that resolved absolute directory is persisted and reused on resume.

Plan schema v1 is closed, so unknown fields are errors. Plan schema v2 is also
closed and adds optional per-task retries; a `retry` field is invalid in v1. Plan
schema v3 adds typed task outputs and explicit dependency inputs; v3 remains
closed. This complete two-task plan creates a marker and verifies it:

```json
{
  "schema_version": 1,
  "tasks": [
    {
      "id": "prepare",
      "depends_on": [],
      "program": "/usr/bin/touch",
      "arguments": ["/absolute/path/to/project/build.ready"],
      "working_directory": "/absolute/path/to/project",
      "timeout_ms": 10000,
      "termination_grace_ms": 2000,
      "stdout_limit_bytes": 1048576,
      "stderr_limit_bytes": 1048576,
      "log_limit_bytes": 65536,
      "redact_argument_indexes": []
    },
    {
      "id": "verify",
      "depends_on": ["prepare"],
      "program": "/usr/bin/test",
      "arguments": ["-f", "/absolute/path/to/project/build.ready"],
      "working_directory": "/absolute/path/to/project",
      "timeout_ms": 10000,
      "termination_grace_ms": 2000,
      "stdout_limit_bytes": 1048576,
      "stderr_limit_bytes": 1048576,
      "log_limit_bytes": 65536,
      "redact_argument_indexes": []
    }
  ]
}
```

Plans contain 1–64 tasks in stable input order. IDs must be unique and match
`[A-Za-z0-9][A-Za-z0-9._-]{0,47}`. `depends_on` defaults to `[]`; every named
dependency must exist, may appear only once, cannot name the task itself, and
the graph must be acyclic. `arguments` and `redact_argument_indexes` also
default to `[]`. `timeout_ms` is required, finite, and between 1 ms and 24
hours. The optional limits shown above are their defaults; termination grace
may be 1–60,000 ms, stdout and stderr retention 1 byte–8 MiB each, and log
retention 1 byte–4 MiB. Redaction indexes are zero-based indexes into
`arguments`. In schema v2, `retry.max_attempts` is 1–10 and includes the
initial attempt; `retry.delay_ms` is 0–86,400,000. Missing `retry` means no
retry.

Execution is deterministic at scheduler selection and defaults to one active
task. Use `--jobs N` to bound active tasks; `--jobs 1` remains sequential.
Independent ready tasks overlap only within that cap and are selected in stable
plan order. Only typed `completed`/`exit_zero` with exit code `0` succeeds.

Schema-v2 tasks may retry known non-cancellation failures: nonzero exit, start
failure, timeout, or definite non-cancellation forced cleanup. Relay never
retries supervisor transport/protocol errors, indeterminate outcomes, or
cancellation. A retry waits as pending, consumes no job slot, and keeps
descendants unready; unrelated ready work continues. Exhausted failures block
descendants. Aggregate run-state schema v5 stores retry history and output
provenance; valid v1/v2/v3/v4 records migrate to v5 on safe resume.
Reconciliation may safely promote a strictly validated terminal Go result without creating another attempt.

### Structured task outputs

Schema-v3 tasks may declare up to four named outputs. Each declaration uses
`{"name": "summary", "type": "json", "path": "summary.json"}` or
`type: "file"`. `path` is relative, traversal-free, and resolved inside a
fresh project/run/attempt directory. Relay passes its absolute path to the
program through explicit `RELAY_OUTPUT_MANIFEST` JSON map from names to paths; it never
scrapes stdout or expands shell syntax. JSON files are parsed and retained up to
2 KiB. File outputs are retained as bounded metadata and SHA-256 identity after
hashing files up to 64 MiB.

Output records include task, aggregate run ID, attempt number, attempt run ID,
type, path, size, and identity. Relay publishes records only after a typed
successful terminal attempt and validates them again on resume and consumption.
Failed, cancelled, timed-out, indeterminate, stale, malformed, symlinked, and
hardlinked outputs are not trusted. Retries get new output directories.

Consumers use typed `inputs`, for example:

```json
{"from_task":"inspect","output":"summary","as":"--summary"}
```

Only successful direct or transitive dependencies may supply inputs. JSON values
become one deterministic argv value; files become their validated absolute path.
Human output lists output names concisely; `--json` includes bounded values and
metadata. Relay does not provide an output blob store, remote uploads, workflow
expressions, or shell interpolation.

## Reusable workflows

Store version-controlled workflow definitions as JSON files in
relay-workflows/. Each file defines one named, closed DAG. Inputs are only
scalar string, integer, or boolean values. Task arguments use literal strings
or explicit typed references; Relay never evaluates shell syntax or performs
environment expansion.
Workflow task timeout defaults to 60,000 ms; remaining task defaults match
plan-v3. Programs and explicit working directories remain absolute paths.

~~~json
{
  "schema_version": 1,
  "name": "verify",
  "inputs": {
    "profile": {"type": "string", "default": "dev"}
  },
  "tasks": [
    {
      "id": "inspect",
      "program": "/usr/bin/cargo",
      "arguments": ["metadata", "--format-version", "1"]
    },
    {
      "id": "test",
      "depends_on": ["inspect"],
      "program": "/usr/bin/cargo",
      "arguments": ["test", "--profile", {"workflow_input": "profile"}]
    }
  ]
}
~~~

Inspect or validate before execution, then run repeatedly:

~~~sh
relay workflow list
relay workflow show verify
relay workflow validate verify --input profile=release
relay workflow run verify --input profile=release
relay workflow run verify --resume /absolute/path/to/run.json
~~~

Workflow resolution compiles into the existing plan-v3 model, so DAG
scheduling, bounded parallelism, retries, cancellation, structured outputs,
artifacts, and crash reconciliation remain unchanged. Fresh workflow runs
persist definition SHA-256, resolved inputs, and sanitized resolved plan in
aggregate state. Resume uses that persisted plan even if live definition is
changed, deleted, or malformed; a valid changed definition is reported without
changing execution. Persisted inputs are not a secret store.
Workflow definitions execute local commands and are not a sandbox or trust boundary. The definition hash identifies normalized content; it is not protection against a malicious local user.

Workflow limits are 1 MiB per file, 64 definitions per project, 48-byte names,
16 inputs, 4 KiB per input string, 64 tasks, and 16 KiB persisted metadata.
Resolved plan encoding remains capped at 1 MiB after input expansion and working-directory resolution.
Workflow-level output aliases, imports, composition, expressions, conditionals,
loops, scripting, remote execution, daemon mode, and model/provider integrations
are not supported.

Rust atomically owns aggregate record. Its default path is
`<project>/.relay/runs/<run-uuid>.json`, where `<project>` is selected by
global scope rules; explicit new-run paths must be absolute. Aggregate run-state
v5 contains run status, timestamps, ordered task records, retry deadlines, and
bounded `attempts` history. Each history entry retains attempt number, fresh
UUID, Go state path, terminal status, reason, bounded execution summary, and
optional output records with exact producer and aggregate/attempt provenance.
Aggregate state uses compact JSON. Retained failure codes are capped at 64 bytes
and messages at 256 bytes; attempt reasons keep only termination classifications
so maximum 64-task, ten-attempt retry history stays below 4 MiB.
The plan copy is sanitized: argument values named by
`redact_argument_indexes` become `<redacted>`. Declared structured output values
are intentionally persisted for consumers; do not declare secrets as outputs.
Review other plan fields and captured process output for secrets.

Each attempt has a fresh UUID and separate Go-owned record under
`${TMPDIR}/relay/supervisor/<attempt-uuid>.json`; validation guarantees that
path is outside Rust-owned `.relay`, but `TMPDIR` may point inside project.
Go attempt records use schema v2 with exact state_path and durable process
identity. Linux identity binds PID to proc start time, process group, session,
and executable. Legacy v1 terminal records remain readable; legacy active
records cannot prove process identity and remain indeterminate.
Rust records every attempt path and never overwrites earlier history. Pending
retry state clears current-attempt fields and stores `retry_ready_at`; resume
can continue it after validating its failed history. Resume validates persisted
paths without requiring same `TMPDIR`.

Human output prints the run ID and status, aggregate state path, and each
task's status, reason, and Go state path. Shell exit is zero only when the
aggregate status is `completed`. `failed` (including blocked descendants) and
`cancelled` runs exit nonzero but still return the normal `ok: true`
output-schema-v1 envelope on stdout, with the full persisted state under
`data.run`; this keeps task outcome distinct from coordinator failure while
making `relay run ... && deploy` safe. An indeterminate supervisor boundary
failure instead exits nonzero and returns `ok: false` on stderr with
`error.code: "run_supervisor"` plus the full persisted state under `data.run`
and its canonical path under `data.state_path`.

Resume always requires the original plan:

```sh
relay run --resume /absolute/path/to/run.json plan.json
```

Relay validates record and compares supplied sanitized plan with persisted
sanitized plan. Any plan containing `redact_argument_indexes` is non-resumable
because raw argument values are not persisted. It clears prior run-level
cancellation intent, never reruns terminal or blocked tasks, and continues only
safe pending work. A pending task with `retry_ready_at` resumes its explicitly
configured next attempt after its prior typed failure; a future deadline is
waited without occupying a job slot.

Resume reconciles persisted `running` attempts before scheduling. A strictly
validated terminal Go result updates that existing attempt in place; its
summary, reason, dependency transition, and retry policy match live completion.
A verified active attempt blocks resume with no duplicate launch. Missing,
malformed, mismatched, stale, or unverifiable work becomes `indeterminate`.
JSON output includes structured reconciliation classifications and reasons.

## Supervise one process

```sh
relay supervise [options] -- <ABSOLUTE_PROGRAM> [ARG...]
```

The literal `--` separates launcher options from the child argv. The program
must be an absolute path; every following argument remains a distinct argv
element and is never shell-evaluated. The launcher options are
`--supervisor ABSOLUTE_PATH`, `--run-id ID`, `--state-path PATH`,
`--working-directory PATH`, `--timeout-ms N`, `--termination-grace-ms N`,
`--stdout-limit-bytes N`, `--stderr-limit-bytes N`, `--log-limit-bytes N`, and
repeatable `--redact-argument-index N`. `--run-id` defaults to a UUID;
`--state-path` defaults to `<temporary-directory>/relay-runs/<run-id>.json`.
The Go defaults are a disabled timeout (`0`), 2,000 ms termination grace, 1 MiB
each for retained stdout and stderr, and 64 KiB for output observations.

The launcher sends one JSON request and closes the supervisor's stdin, then
waits for one response while bounding each response stream to 32 MiB. On Linux,
it forwards `SIGINT` and `SIGTERM` to the supervisor while it waits; Go owns the
child-process-group cleanup. The standalone relay supervise command does not read existing Go records or
reconcile them; the Go supervisor writes the requested `state_path` record for
its attempt. Relay run reads those records only during crash reconciliation.
Human output summarizes lifecycle, reason, exit code, duration, state path, and retained stdout/stderr byte counts;
`--json` returns an outer object with `state_path` and `execution`. A valid supervisor response exits zero even when the child lifecycle is `failed`,
`timed_out`, or `cancelled`; inspect `execution.lifecycle` and `termination_reason` for the child result. Protocol and request failures exit nonzero.

This command is project-independent: it neither discovers nor reads `.relay`.
It launches one local child, not a scheduler, queue, daemon, remote executor,
or Lua workflow runtime.

## Shell completions

`relay completions bash|zsh|fish` generates a script from the current CLI and
writes it to stdout. It never edits shell configuration.

```sh
# Bash: load now
source <(relay completions bash)

# Zsh: install in a directory already on $fpath, then restart the shell
mkdir -p "$HOME/.zfunc"
relay completions zsh > "$HOME/.zfunc/_relay"

# Fish: install for the current user
mkdir -p "$HOME/.config/fish/completions"
relay completions fish > "$HOME/.config/fish/completions/relay.fish"
```

For the Zsh example, add `fpath=("$HOME/.zfunc" $fpath)` before `compinit` in
`.zshrc` if that directory is not already on `fpath`.

## Daily workflow

```sh
relay start "Improve the installer"
relay note decision "Use the Git CLI for snapshots"
relay note progress "Added distro detection"
relay checkpoint "Detection and tests pass"
relay handoff codex \
  --completed-work "Added distro detection" \
  --decision "Use the Git CLI for snapshots" \
  --failure "Offline mirror was unavailable" \
  --next-action "Review the installer smoke test"
relay search "installer" --category decision --reverse
relay context --max-chars 8000
relay finish
```

Relay supports these commands:

- `start <GOAL>` starts the only active session.
- `status` shows the current session and live scoped Git state.
- `checkpoint [MESSAGE]` saves a timestamped Git snapshot.
- `handoff <AGENT>` prints and saves Markdown. `--completed-work`,
  `--decision`, and `--failure` are repeatable; `--next-action` accepts one
  value. Every detail is optional, and missing facts are reported as not
  recorded.
- `finish` saves a final Markdown summary and completes the active session.
- `list` lists all known sessions.
- `show <SESSION>` shows a session selected by full UUID, an unambiguous UUID
  prefix, or `latest`.
- `history [--session <SESSION>] [--type <EVENT_TYPE>] [--since <RFC3339>]
  [--until <RFC3339>] [--reverse] [--limit <N>]` filters append-only events,
  then displays the earliest matches by default or newest matches with
  `--reverse`; both time bounds are inclusive and the limit follows that order.
- `search <QUERY> [--session <SESSION>] [--type <EVENT_TYPE>]
  [--category <CATEGORY>] [--since <RFC3339>] [--until <RFC3339>] [--reverse]
  [--limit <N>]` searches stored event types and payload strings for an exact,
  case-sensitive plain substring. Filters run before matching, results retain
  append order by default, and `--reverse` selects newest matches before the
  limit.
- `resume <SESSION>` starts a new active continuation of a completed session,
  preserving its goal and recording lineage. It accepts the same selectors,
  including `latest`, does not modify the source, and fails if a session is
  already active.
- `note <CATEGORY> [TEXT]` records `progress`, `decision`, `blocker`,
  `observation`, or `next_action`.
- `context` emits bounded, recorded agent context; its filters are described
  below.
- `run [--jobs <N>] [--supervisor <ABSOLUTE_PATH>] [--state-path <ABSOLUTE_PATH>] <PLAN>`
  executes a new bounded-parallel plan; replace `--state-path` with
  `--resume <ABSOLUTE_STATE_PATH>` to reconcile running attempts and continue
  only proven terminal results or safe pending work.
- `supervise [OPTIONS] -- <ABSOLUTE_PROGRAM> [ARG...]` launches one local
  process through the external Go supervisor.
- `doctor [--repair]` diagnoses storage consistency.
- `completions <SHELL>` prints a Bash, Zsh, or Fish completion script.
- `protocol` reports the project-independent, read-only orchestration process
  contract compiled into Relay.

See the [orchestration process protocol](docs/orchestration-protocol.md) for
the exact versioned manifest, one-child execution request/result, lifecycle,
environment policy, limits, persistence, platform support, and security
requirements. That immutable protocol-v1 manifest continues to report
`orchestrator.owner: "go"` and `orchestrator.implemented: false` for a future
external orchestrator capability. It advertises neither the Rust-owned built-in
`relay run` coordinator nor the `relay supervise` CLI surface.
Allowed Git-aware Rust operations may invoke `git`; existing note editor
behavior remains outside protocol v1.

The supervisor reaps its direct child independently of concurrent stdout/stderr
draining. After the configured graceful period, forced reaping and group
confirmation are bounded to 250 ms. Drains get 250 ms to reach EOF, then their
readers are closed and given a final 250 ms to stop; unconfirmed cleanup or
drain shutdown produces a protocol failure rather than an invented completed
result. Cleanup covers only the original process group. A child that detaches
with `setsid` or self-regroups requires a cgroup or service boundary, and a
successful result cannot prove that no such detached process exists.

`--json` and `--scope auto|current|repository` are global options and work
before or after a subcommand. Human output remains the default. An exact global
`--json` anywhere before a literal `--` requests JSON error framing even when
Clap rejects the arguments: Relay writes one compact output-schema-v1 error
envelope to stderr, writes nothing to stdout, and retains Clap's nonzero exit
code. That envelope uses `command: "cli"`, `error.code: "argument_error"`, and
an actionable normalized Clap message without a redundant leading `error: `.
Without that request, parser failures remain ordinary Clap stderr text.

After parsing, a failure to read the current directory uses the same JSON error
envelope shape when requested, with `error.code: "current_directory"` and the
parsed command name; human output remains unchanged. When JSON was requested,
application handling likewise emits one versioned success document on stdout
or one versioned error document on stderr. Successful help and version requests
remain ordinary shell-facing Clap text, including with `--json`, and
successful completion-script generation remains shell text. If the operating
system cannot start the Relay process, Relay
cannot emit an envelope; the caller must handle that launch failure.
`output_schema_version` is separate from the on-disk schema version.

For mutating commands, human and JSON output are rendered from the operation's
typed result; Relay does not reconstruct a completed mutation by rescanning
unrelated historical records.

## Notes

Supply note text in exactly one of these ways:

```sh
relay note decision "Keep JSON Lines append-only"
printf '%s\n' "Waiting for review" | relay note blocker --stdin
relay note next_action --editor
```

With neither text nor `--stdin`, Relay reads redirected stdin when present;
otherwise it opens `$VISUAL`, then `$EDITOR`, falling back to `vi`. Text must be
nonblank valid UTF-8 and is limited to 64 KiB. Explicitly combining positional
text, `--stdin`, and `--editor` is rejected.

Recorded notes are facts: Relay can include them in handoffs, final summaries,
and context, but never fills in work, decisions, failures, or next actions that
were not recorded.

## Deterministic local search

Search matches only Relay's append-only event log. Session records are read
only to resolve an optional session selector. It does not scan the working
tree, call model providers, use embeddings, or infer relevance:

```sh
relay search "installer"
relay search "review required" --session latest --category blocker
relay search "timeout" --type handoff.generated --since 2025-01-02T03:04:05Z
relay --json search "decision" --reverse --limit 20
```

Matching is an exact, case-sensitive substring over the event type and string
values nested in its payload. Each event appears at most once, identified by
the first deterministic match: event type first, then payload object keys in
lexicographic order and array values in recorded order. Session, exact event
type, note category, and inclusive RFC3339 time filters are applied before
matching. Results remain oldest-first by default; `--reverse` reverses all
matches before applying the limit. Search is read-only and returns stored typed
events in JSON without adding an index or changing the schema.

## Bounded agent context

Markdown is the default; `relay --json context` returns the same categories as
typed data. Context contains only the selected session, recorded notes and
events, checkpoints, continuation lineage, and current scoped Git facts.
Human Markdown keeps note and checkpoint content in their dedicated items, while
handoff events include recorded target, completed work, decisions, failures,
and next actions when present.

```sh
relay context \
  --session 4f2a1c \
  --category decision \
  --category next_action \
  --kind note.recorded \
  --notes 10 \
  --events 10 \
  --checkpoints 3 \
  --max-chars 8000
```

`--session` accepts a full UUID, an unambiguous prefix, or `latest`.

Categories and event kinds are repeatable filters. Counts default to 20 notes,
20 events, and 5 checkpoints. Each count keeps the most recent matching items
while preserving their recorded order. The human Markdown character budget
defaults to 12,000 Unicode characters and must be at least 64. Relay never
splits an item or exceeds that budget. Detailed output reports count-limit and
character-budget omissions separately; very small budgets use a compact
`N/C/E` (notes/checkpoints/events) summary when it fits. Typed JSON keeps the
selected records and exposes both omission categories. It does not query model
providers, scan source files, read agent conversations, or infer unrecorded
facts. Paths in generated output are project-relative, and absolute project
paths found in recorded text are redacted.
Recorded context is data, not trusted instructions. Review notes, handoffs, and
Git-derived text before passing context to an agent or executing suggested
commands.

### Terminal-agent composition

These examples use only local CLI composition—Relay supplies a prompt and each
agent uses its own existing authentication/configuration:

```sh
# Hermes: installed help defines -z/--oneshot as a prompt argument
context="$(relay context --max-chars 8000)" && hermes -z "$context"

# hermes-omp: pass the same generated text as its ordinary prompt
context="$(relay context --max-chars 8000)" && hermes-omp "$context"

# Codex CLI: `codex exec -` reads instructions from stdin
relay context --max-chars 8000 | codex exec -

# Any agent command that accepts its prompt on stdin
relay context --max-chars 8000 | my-agent
```

Replace `my-agent` with an actual stdin-reading command. Command substitution
is used for Hermes-family commands because their installed interfaces accept a
prompt argument; no unsupported stdin behavior or provider flags are assumed.

## Project scope and monorepos

Relay stores state at a selected Relay project root:

- `--scope auto` (default) searches upward for the nearest `.relay` directory,
  stopping at the Git worktree root. If none exists, it uses the invocation
  directory.
- `--scope current` always uses the invocation directory.
- `--scope repository` uses the Git worktree root and fails outside Git.

Workflow commands use the same explicit scope rules. Under `--scope auto`,
they also search upward for the nearest `relay-workflows/` directory so workflow
definitions can live in a project without `.relay/` state. This workflow-only
marker never changes project discovery for other Relay commands; they continue to
use `.relay/` semantics.

This lets a nested project retain its session when commands run from deeper
directories. In a monorepo, start with `--scope current` in a package directory
to give that subtree independent Relay state. Git snapshots and changed paths
are then limited to that subtree; sibling package changes do not affect its
cleanliness. Use `--scope repository` when the whole worktree is one Relay
project.
When repository scope coexists with nested current-scoped projects, those
nested `.relay/` directories are outside the repository root's excluded
path and appear as changed files in repository Git snapshots.

`relay run` uses this selected project only to place its default aggregate
record under `.relay/runs/`; task working directories still default to the
invocation directory. An explicit aggregate state path may be elsewhere.
`relay supervise` remains project-independent and neither discovers nor reads
`.relay`.

## Files, compatibility, migration, and locking

Relay's inspectable project-local layout is:

```text
.relay/
├── session.json
├── sessions/
│   └── <session-id>.json
├── events.jsonl
├── checkpoints/
│   └── <timestamp>-<id>.json
├── handoffs/
│   └── <timestamp>-<id>.md
├── operations/
│   └── <operation-id>.json
├── runs/
│   ├── <run-id>.json
│   └── outputs/<attempt-run-id>/<declared-path>
├── lock
└── summary-<session-id>.md
```

Session JSON records and JSON Lines events use on-disk `schema_version: 1`;
the crate version is independent. Optional continuation fields are
backward-compatible. Rust-owned run aggregates use separate closed
schema_version: 5 with bounded retry history, successful-attempt output provenance,
and optional reusable-workflow identity metadata. Aggregate v1 records omit
jobs and imply jobs: 1; aggregate v2 records store jobs but no retry history.
Clean v1/v2/v3/v4 records migrate to v5 on safe resume. Running or
indeterminate records remain non-resumable. Unsupported versions, unknown
fields, and malformed JSON/JSONL are reported, never silently upgraded,
discarded, or overwritten.
Back up `.relay/` before manual migration or repair.

Mutating commands take a nonblocking advisory project lock. Contention fails
with the recorded owner PID and command when available; the kernel releases
the lock if a process exits. Read-only commands do not take the mutation lock.
Multi-file mutations use durable operation markers that record completed
effects. Individual JSON/Markdown replacements are synced and atomic, and
events are appended as synced, newline-terminated records. An interruption can
leave an operation marker or durable partial effects; Relay preserves them for
diagnosis rather than guessing how to roll them back.

Run aggregates use their own adjacent advisory lock and atomic replacement;
they do not use the session mutation lock. The referenced per-attempt
supervisor records are under `${TMPDIR}/relay/supervisor/`. Validation
guarantees that they remain outside Rust-owned `.relay`, but `TMPDIR` itself
may point inside the selected project. Callers handling sensitive captured
output should choose a private, safe temporary directory.

Choose your own version-control policy for `.relay/`. Ignore it for private
local coordination, or intentionally share selected artifacts after reviewing
their contents. Goals, notes, handoffs, summaries, branch names, commit IDs,
changed paths, plans, and process summaries can contain sensitive information.
Relay provides no encryption, hosted sync, or access control. Run-plan
redaction covers only explicitly indexed argument values in the sanitized
aggregate plan; it is not a general redaction policy and does not sanitize
other fields or captured output.

## Git behavior and operations

Inside Git, Relay records the branch, commit, cleanliness, and project-scoped
changed paths while excluding its own `.relay/` directory. Non-UTF-8 path bytes
on Unix are rendered deterministically. Outside Git, lifecycle commands work
and report Git fields as unavailable.

Relay invokes read-only Git queries only. It never commits, stages, stashes,
checks out, resets, rewrites history, or pushes. Session commands mutate the
selected `.relay/` storage, a run may write its explicit aggregate path and
Go-owned temporary records, and `relay run`/`relay supervise` execute the
requested child programs with the caller's filesystem permissions.

## Doctor and recovery

`relay doctor` performs byte-preserving diagnostics by default. It reports
stable codes, severity, relative affected paths, guidance, and health totals
for malformed or partial JSONL, malformed/unsupported records and artifacts,
invalid lifecycle combinations, missing references, orphaned artifacts,
current/history disagreement, and incomplete operation markers.
The diagnostic command exits zero after producing a valid report even when
`data.healthy` is false; inspect health totals or JSON `data.healthy` instead
of using process status as the health signal.

`relay doctor --repair` is deliberately conservative. It takes the project
lock and removes only stale operation markers whose listed effects are all
provably complete. If any ambiguous, incomplete, or potentially destructive
problem exists, repair is refused without changing it. Inspect diagnostics,
back up `.relay/`, and resolve such damage manually.

## Structured agent handoffs

Structured handoffs are immutable Relay snapshots for passing recorded state from one terminal agent to another. They do not inspect transcripts, summarize source, execute next actions, or start agents.

The existing positional form remains valid and preserves the target text exactly. The action words `show`, `latest`, `list`, and `create` are reserved for inspection or explicit creation; use `relay handoff --target <name>` when an older positional target has one of those names:

    relay handoff codex --completed-work "implemented parser" --decision "keep JSONL" --failure "review pending" --next-action "run integration"

Use explicit action form when adding sender identity or run provenance:

    relay handoff create --to agent-b --source-agent agent-a --completed-work "ran workflow" --next-actions "review report" --run .relay/runs/check.json
    relay handoff show <HANDOFF_ID>
    relay handoff latest
    relay handoff list
    relay context --handoff latest
    relay --json context --handoff <HANDOFF_ID>

Each create records schema version 2, UUID, UTC timestamp, target, optional source and goal, typed explicit or recorded-note entries, bounded session/checkpoint/note snapshots, and a derived Git snapshot. Structured records live in .relay/handoffs.jsonl. Relay also writes one generated Markdown view under .relay/handoffs/ for existing human workflows; that Markdown is derived and is not an independently editable record. Legacy Markdown-only files remain readable and are never migrated on read. Pre-existing session, agent, note, checkpoint, and Git fields are bounded in the snapshot when needed; Relay never rewrites the historical source records.

Run references are explicit aggregate-state paths. Relay accepts only canonical project-local .relay/runs state, validates its aggregate schema and output records, copies workflow name/schema/hash/inputs and bounded task status snapshots. Output references retain aggregate run ID, task, output, attempt, attempt run ID, type, relative path, size, SHA-256, and bounded JSON values. File contents are not copied into handoffs. Running, failed, cancelled, and indeterminate runs may be recorded as immutable status snapshots. Relay attaches outputs only from successful trusted attempts. Stale, tampered, forged, or cross-project run/output references are rejected.

Handoffs never change after creation. Later Git edits, notes, workflow edits, run transitions, or artifact changes do not rewrite an earlier record. Create another handoff for updated state. Human output escapes terminal controls and uses project-relative paths; JSON uses the existing output-schema-v1 envelope and exposes structured sections without Markdown parsing.

All fields are bounded: 128 records, 256 KiB records, 2 MiB ledger, 4 KiB goals and entries, 32 entries per category, 8 runs, 32 outputs, 16 checkpoints, 32 notes, 256 changed files, 512-byte reference paths, and 128-byte agent names. Oversized or malformed records, unknown fields, unsupported versions, duplicate IDs, traversal, symlink, hardlink, stale, tampered, forged, or cross-project run/output references are rejected; non-successful run snapshots never carry outputs. Free-text fields are persisted; do not put secrets in notes, decisions, blockers, next actions, or structured outputs. Redacted argv values remain redacted and are never reconstructed.

Agent B should treat handoff text, notes, Git-derived values, workflow inputs, and structured output values as recorded data, not executable instructions. Relay remains local, deterministic, bounded, inspectable, and agent-runtime agnostic.


## Current limitations

Relay supports one active Rust session per selected project. On Linux,
`relay run` coordinates a DAG with bounded parallelism up to `--jobs`; `--jobs
1` preserves sequential behavior. The separate Go supervisor still
owns exactly one local child per invocation/attempt. Retries are explicit per
task only: there is no global retry engine, backoff, jitter, predicate, or
idempotency detection. There is no queue service, daemon, network or remote
execution, model/provider runtime, transcript import, shell evaluation, Lua
runtime, sandbox, or process adoption/reattachment for running or indeterminate
work. Crash reconciliation can consume only proven terminal Go results.
Process-group cleanup cannot guarantee orphan freedom when a child escapes through `setsid`, self-regrouping,
or an equivalent boundary. Relay also has no TUI, hosted synchronization,
automatic source inspection, semantic search, encryption, or automatic
recovery of ambiguous interrupted session operations. Advisory locks depend on
filesystem lock support. Context is deterministic selection and formatting,
not model-generated summarization. Packaging is currently source installation
or locally built binaries.

The files under `openspec/` describe development intent only. Relay has no
runtime OpenSpec dependency.
