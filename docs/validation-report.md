# Relay real-world validation report

## Scope and evidence

This is an audit of the checked-out post-PR14 candidate. Local `origin/main`
was still `e596bf1` (PR13); `feat/crash-reconciliation` was at
`11edab6282fb842dd3351272ffa024c46a4dd090` (two commits after PR13). `git fetch
--prune origin` and `git pull --ff-only` could not verify GitHub because the
configured remote rejected credentials. Per the requested post-PR14 premise,
local `main` and `audit/real-world-validation` were advanced to that candidate.
This remote-merge limitation remains explicit; no upstream branch was changed.

Validation used Relay `0.2.0`, Go `1.26.0`, a locally built
`relay-supervisor`, Linux process identity checks, and dedicated repositories
under `/tmp`. No upstream clone received a commit or push.

Measurements are manual, coarse audit measurements, not product telemetry.
`<1 s` means command-level wall time below one second. “Repeated” means an
unnecessary repetition observed in the scripted takeover, not every command a
human might choose to repeat. State probes and deliberate corruption setup are
listed separately from normal developer work.

## 1. Feature inventory

| Feature | Problem, primary user, audience | Dependencies | Failure modes | Duplication | Complexity, coverage, real-world use |
| --- | --- | --- | --- | --- | --- |
| Session lifecycle (`start`, `status`, `finish`) | Preserves goal, current state, completion, and final state for agents and developers; both-facing. | Project discovery, storage, Git snapshot, lock. | Active-session conflict, corrupt state, Git or storage failure. | `finish` overlaps summary; distinct from continuation `resume`. | Medium; `src/app.rs`, CLI tests, real takeover. Clear use: yes. |
| Checkpoints | Records durable progress plus a Git snapshot before handoff; both-facing. | Active session, Git, atomic artifact/event writes. | Git unavailable, storage failure, interrupted multi-file operation. | Overlaps notes and final summary, but uniquely captures repository state. | Medium; CLI tests and real bat workflow. Clear use: yes. |
| Categorized notes | Captures progress, decisions, blockers, observations, and next actions with low ceremony; both-facing. | Active session, event log, optional stdin/editor. | Blank input, editor failure, malformed history, lock/storage failure. | Handoff imports notes; context renders them, but neither replaces note capture. | Low; CLI tests, A/B takeover. Clear use: yes. |
| Handoffs and summaries | Produces inspectable Markdown for another agent or a completed session; agent-primary, human-readable. | Session, notes, checkpoints, Git, atomic artifacts, event log. | Incomplete operation marker, artifact/event disagreement, unsafe Markdown input. | Handoff and `finish` both render status; handoff transfers work, summary closes session. | Medium; CLI tests plus real 39-line handoff. Clear use: yes. |
| Bounded context | Gives an agent a deterministic, size-limited prompt without rescanning the repository; agent-primary, human-readable. | Session/event history, filters, Git snapshot, budget accounting. | Corrupt events, missing checkpoint payload, count/budget omission, untrusted text. | Subsumes selected handoff facts but remains a generated view, not storage. | High; context tests and real takeover. Clear use: yes. |
| History and navigation | Makes lineage and append-only decisions inspectable; both-facing. | Event log, durable sessions, selectors (`latest`/UUID prefix). | Ambiguous selector, malformed event, unsupported schema. | `show`/`list` expose sessions; `search` filters payloads. | Low; CLI tests and real history queries. Clear use: yes. |
| Search | Finds stored decisions and failures without scanning source; both-facing. | Event log, session selector, event filters. | Case-sensitive exact substring, no source search, no semantic relevance, corrupt log. | Overlaps `history`; search adds payload matching and filters. | Low; CLI tests and real query. Clear use: yes, with discoverability friction. |
| Project scoping (`auto`, `current`, `repository`) | Prevents state crossing nested projects and supports monorepos; both-facing. | Git root detection, `.relay` discovery, storage paths. | Outside-Git repository scope, nearest-state surprises, nested state visible to root Git. | None; scope controls all other project-local features. | Medium; project tests and Tokio/nested-repo validation. Clear use: yes. |
| Git integration | Anchors sessions/checkpoints/handoffs to branch, commit, cleanliness, and changed paths; both-facing. | Git executable and selected project root. | Outside Git, command failure, non-UTF-8 paths, scope-relative path errors. | Checkpoints also contain snapshots; live status remains distinct. | Medium; Git unit/CLI tests and monorepo runs. Clear use: yes. |
| Doctor diagnostics | Explains malformed, partial, inconsistent, or orphaned Relay state without rewriting it; human-primary. | All local state files, event parsing, project scope. | Reports errors while exiting zero; ambiguous damage remains manual. | Similar role to `fsck`, but domain-specific and byte-preserving. | Medium; CLI tests and corrupted-state runs. Clear use: yes. |
| Doctor repair | Removes only provably complete operation markers; human-primary safety tool. | Doctor diagnostics, project lock, event evidence. | Refuses incomplete or destructive repairs; never invents history. | No safe equivalent in plain shell history. | Low; CLI tests and real injected interruption. Clear use: niche but real. |
| Project mutation lock | Prevents concurrent state writers and exposes owner PID/command; both-facing safety. | Filesystem advisory locking and lock metadata. | Busy state, stale/unreadable owner metadata, filesystem lock limitations. | None; distinct from run aggregate lock. | Low; lock test plus real contention. Clear use: yes. |
| JSON output envelopes | Gives scripts and agents one typed success/error document with stable schema; agent/tooling-primary. | CLI parser, output schema, command-specific typed data. | Human help remains text; errors use stderr; malformed requests exit nonzero. | Human output is a separate view, not a replacement. | Low; CLI tests and real protocol/run/supervisor calls. Clear use: yes. |
| Shell completions | Makes the broad CLI discoverable at the shell; human-primary. | Clap command model and `clap_complete`. | Bash/Zsh/Fish only; installation remains user-managed. | Shell help already exists but is less discoverable. | Low; CLI tests and generated scripts (874/542/202 lines). Clear use: yes, not differentiating. |
| Standalone `supervise` | Runs one absolute child with timeout, output limits, redaction, and persisted Go observation; advanced human/agent use. | Rust launcher, Go supervisor, Linux process mechanics. | Valid child failure/timeout/cancel is transport success; protocol/startup failures are errors; process escape limits. | Overlaps `timeout`/shell, adds durable typed lifecycle and cleanup. | High; supervisor tests and real success/failure/timeout/cancel/redaction runs. Clear use: yes when state/safety matters. |
| Go one-child supervisor | Isolates child lifecycle, process-group cleanup, output capture, and durable attempt state; internal plus advanced users. | Go binary, protocol v1, filesystem and `/proc` on Linux. | Readiness/protocol failure, stale process identity, detached process groups, bounded output. | It is intentionally not a scheduler; `run` composes it. | High; Go race tests, Rust integration, real DAG. Clear use: yes as foundation. |
| Protocol and versioning | Prevents Rust/Go schema drift and lets tooling negotiate framing and ownership; internal/agent-facing. | Manifest v1, output schema v1, execution protocol v1. | Unsupported versions, closed fields, no external orchestrator consumer. | Manifest repeats parts of docs and advertises unimplemented components. | Medium; protocol tests and `relay protocol`. Real user value unclear. |
| Plan validation and direct argv | Rejects malformed/unsafe plans and avoids shell injection; both-facing safety. | Closed plan schemas, absolute paths, supervisor preflight. | Unknown fields, relative paths, invalid IDs/limits/dependencies, redacted plans non-resumable. | Replaces shell evaluation rather than duplicating it. | Medium; run tests, invalid-plan smoke test. Clear use: yes. |
| DAG orchestration | Runs dependency-aware multi-step repository work with persisted outcomes; agent/developer-facing. | Plan validation, coordinator, supervisor, aggregate state. | Failed ancestors block descendants; indeterminate supervisor boundary blocks resume. | Overlaps Make/just/Task for ordering, but adds outcome state and reconciliation. | High; 76 run tests and six-task bat DAG. Clear use: yes for crashable multi-step work, not trivial commands. |
| Bounded parallelism | Reduces wall time while capping active tasks and preserving input-order selection; both-facing. | DAG scheduler, `--jobs`, per-task supervisor. | Invalid jobs, scheduler cancellation races, output interleaving is represented only in state summaries. | Make/just can parallelize, but not with Relay's attempt state. | Medium/high; run tests and observed peak two tasks with `--jobs 2`. Clear use: yes for multi-step runs. |
| Cancellation | Stops active work, blocks descendants, and prevents retry after cancellation; both-facing safety. | Signal handling, coordinator, Go process groups, persisted intent. | Signal timing, detached process escape, cancelled run requires explicit interpretation. | Shell signals exist but do not persist dependency state. | High; run/supervisor tests and real SIGINT. Clear use: yes. |
| Explicit retries | Repeats only configured, definite non-cancellation failures; agent/developer-facing. | Plan schema v2, attempt history, coordinator deadlines. | No retry for indeterminate/cancelled work; no predicate/idempotency detection; JSON setup is verbose. | Shell loops are simpler but cannot safely distinguish uncertainty. | Medium; run tests and controlled flaky task. Clear use: niche but real. |
| Persisted aggregate run state | Gives one durable run record with task status, attempts, summaries, and canonical path; both-facing. | Atomic storage, DAG, supervisor records, locking. | Truncated/malformed/unsupported state, canonical-path mismatch, size limits. | Go attempt records are per-attempt evidence, not an aggregate duplicate. | High; 76 run tests and real states. Clear use: yes. |
| Run resume | Continues safe pending work after returning later and preserves original plan identity; both-facing. | Aggregate state, original plan, reconciliation, supervisor paths. | Plan mismatch, redactions make resume unavailable, terminal/blocked tasks never rerun. | Session `resume` creates a new context session; run `--resume` continues a run. | Medium/high; migration, crash, and next-day scenarios. Clear use: yes. |
| Crash reconciliation and process identity | Prevents duplicate execution after coordinator loss; both-facing safety. | Persisted attempt identity, Go record, PID/start time/process group/executable. | Missing, malformed, stale, mismatched, still-running, or unverifiable attempts remain indeterminate/refused. | No plain shell/Make equivalent with this safety boundary. | High; current post-PR14 code, integration tests, seven crash subcases. Clear use: yes and strongest differentiator. |
| Resource and secret controls | Bounds retained output/logs, redacts declared args, filters environment, and uses direct argv; both-facing safety. | Supervisor protocol, plan validation, process runner. | Redaction is caller-declared, detached processes can escape, output is intentionally truncated. | OS `timeout`/`env` can cover fragments only. | Medium/high; unit/integration tests and real redaction/limit run. Clear use: yes as safety mechanics. |
| OpenSpec artifacts | Records development intent for maintainers; maintainer-facing, not runtime. | None at runtime. | Can become stale or be mistaken for implemented behavior. | Overlaps current docs only as planning history. | Low; no runtime tests. No obvious end-user use; keep clearly marked as intent. |

## 2. Value audit

### Evidence-based classifications

| Feature | Classification | Evidence and disappearance test | Recommendation |
| --- | --- | --- | --- |
| Session lifecycle | core | A/B could resume a goal, commit, and current state without a separate note; disappearance removes the durable workflow. | keep |
| Checkpoints | useful | Real checkpoint captured repository facts and gave handoff a stable recent boundary. | keep |
| Notes | core | Decision/blocker/next-action facts survived agent change; plain files would require a convention and selector. | keep |
| Handoffs | core | Real Markdown handoff was 39 lines, signal-dense, and immediately actionable. | keep |
| Bounded context | core | Fresh takeover got goal, decision, blocker, next action, Git state, and lineage before rereading source. | keep |
| History/navigation | useful | `history`, `show`, `list`, `latest`, and UUID prefixes made lineage inspectable. | keep |
| Search | useful | Found stored event payloads without source scan; case sensitivity caused one expected no-match surprise. | simplify |
| Project scoping | core | Tokio package/root and nested Git tests showed independent state and boundary enforcement. | keep |
| Git integration | core | Commit/branch/cleanliness and scoped changed files materially explained work state. | keep |
| Doctor diagnostics | useful | Corruption runs gave stable explanations and did not rewrite damaged bytes. | keep |
| Doctor repair | niche | Injected completed marker was removed safely; incomplete marker repair was refused. | keep |
| Locking | useful | Concurrent checkpoint/note produced actionable owner metadata instead of data races. | keep |
| JSON output | core | Run, protocol, context, and supervisor envelopes were parseable and stream-separated. | keep |
| Shell completions | useful | Scripts generated correctly and contained current commands; useful standard polish, not Relay differentiation. | keep |
| Standalone supervisor | useful | Real lifecycle, timeout, cancellation, redaction, and retention behavior worked; direct shell tools remain simpler for one-offs. | document |
| Go one-child supervisor | core internal | Every real run had one typed attempt record; it is the safety foundation, not a competing scheduler. | keep |
| Protocol/versioning | unclear | `protocol` is stable and tested, but its manifest reports future Go/Lua components as unimplemented and has no observed external consumer. | deprecate |
| Plan validation/direct argv | core | Unknown fields and unsafe plan inputs failed before state creation or launch. | keep |
| DAG orchestration | useful | Six real bat tasks completed with dependency gating and durable state; plan authoring was much heavier than Cargo commands. | simplify |
| Bounded parallelism | useful | `--jobs 2` overlapped lint/unit and never exceeded two active tasks. | keep |
| Cancellation | core | SIGINT terminated work, blocked descendants, and created no retry. | keep |
| Explicit retries | useful | Controlled failure retried exactly once when configured; no-retry and uncertainty cases did not retry. | document |
| Aggregate run state | core | Crash and next-day state survived coordinator death and preserved attempt identity. | keep |
| Run resume | core | Pending work resumed in under one second without rerunning trusted terminal work. | keep |
| Crash reconciliation | core | Success/failure reconciled in place; running, missing, stale, malformed, and mismatched work stayed safe/uncertain. | keep |
| Resource/secret controls | useful | Output capped at four retained bytes, arguments redacted, and child environment restricted. | keep |
| OpenSpec artifacts | unclear | Useful to maintainers only; not a product capability. | document |
### Findings table

| Feature | real_world_value | complexity | friction | evidence | recommendation |
| --- | --- | --- | --- | --- | --- |
| Session lifecycle | high | medium | low | Real hexyl/bat sessions preserved goal and completion. | keep |
| Checkpoints | medium | medium | low | Bat checkpoint captured Git boundary used by handoff. | keep |
| Notes | high | low | low | Decision, blocker, and next-action notes survived takeover. | keep |
| Handoffs | high | medium | low | 39-line bat artifact gave B actionable state. | keep |
| Bounded context | high | high | low after setup | A/B context avoided repeated topology reconstruction. | keep |
| History/navigation | medium | low | low | `latest`, UUID prefixes, `show`, and filtered history worked. | keep |
| Search | medium | low | medium | Event payload search worked; case-sensitive miss was easy to make. | simplify |
| Project scoping | high | medium | low | Tokio and nested Git boundaries held. | keep |
| Git integration | high | medium | low | Scoped commit, branch, cleanliness, and paths were useful. | keep |
| Doctor diagnostics | medium | medium | low | Corrupt state produced stable, non-destructive guidance. | keep |
| Doctor repair | low/medium | low | medium | Completed marker repaired; incomplete marker refused. | keep |
| Mutation locking | medium | low | low | Real contention exposed PID and command. | keep |
| JSON envelopes | high for tools | low | low | Protocol, run, context, and supervisor JSON parsed cleanly. | keep |
| Completions | low/medium | low | low | Three shell scripts contained current commands. | keep |
| Standalone supervisor | medium | high | medium | Real child failure, timeout, cancel, limits, and redaction worked. | document |
| One-child Go supervisor | high internal | high | medium | Every run created typed attempt evidence. | keep |
| Protocol/versioning | unclear | medium | medium | Stable manifest has no observed external consumer. | deprecate |
| Plan validation/direct argv | high | medium | medium | Unknown-field plan failed before state creation. | keep |
| DAG orchestration | medium/high | high | high | Six-task bat graph completed with persisted outcomes. | simplify |
| Bounded parallelism | medium/high | high | medium | `--jobs 2` peaked at two active tasks and overlapped work. | keep |
| Cancellation | high | high | low | SIGINT stopped task, blocked descendant, and suppressed retry. | keep |
| Explicit retries | medium | medium | medium | Controlled failure retried only with schema-v2 policy. | document |
| Aggregate run state | high | high | low after setup | State retained task and attempt identity across crashes. | keep |
| Run resume | high | medium/high | low | Pending test resumed in under one second without rerun. | keep |
| Crash reconciliation | high | high | low | Terminal evidence reconciled; uncertainty stayed refused. | keep |
| Resource/secret controls | medium/high | medium/high | medium | Four-byte retention, redaction, and environment limits held. | keep |
| OpenSpec artifacts | low product value | low | low | Maintainer intent only; no runtime consumer. | document |
| Future Go/Lua advertisement | none observed | low | medium | Protocol output explicitly reports both unimplemented. | deprecate |

`real_world_value` is a workflow judgment, not usage telemetry. Complexity is
implementation/operational burden; friction is setup or repeated-use burden.

### Questions answered by evidence

- **Would a developer notice disappearance?** Yes for lifecycle, context, handoffs,
  Git snapshots, persisted run state, resume, cancellation, and crash
  reconciliation. Notes and history would be missed soon after.
- **Does Relay reduce manual work or context loss?** Yes for handoffs and
  next-day run resume. It does not inspect source or infer facts, so repository
  understanding still requires normal tools.
- **Does it prevent duplicate work?** Crash reconciliation and process identity
  did: all tested resumes kept one attempt for trusted terminal outcomes and
  refused unsafe active/uncertain work.
- **Is it easier than existing tools?** Session/context/handoff are easier than
  inventing a local convention. DAG authoring and standalone supervision are
  harder than direct Cargo, `make`, `just`, or `timeout` for simple work.
- **Is UX discoverable?** Top-level help is strong and completions work. The
  broad feature surface, JSON plan schema, case-sensitive search, and
  transport-success child failures need explicit reading.
- **Is failure understandable?** Run failure/blocked/indeterminate output was
  clear. Standalone `supervise` and `doctor` intentionally return zero for a
  valid diagnostic/transport response even when contained work is failed or
  unhealthy; documentation now states this explicitly.
- **Is anything only synthetic?** The future external orchestrator/Lua manifest
  fields have no observed real consumer. Six-task DAG, retries, crash states,
  and scope behavior were exercised against real repositories and local
  processes, not only fixtures.

## 3. Test repositories

| Repository | Shape and purpose | Revision and local handling | Real commands/results |
| --- | --- | --- | --- |
| `sharkdp/hexyl` | Small Rust repository and unfamiliar takeover target. | Shallow clone `/tmp/relay-audit-small`, `6ecc29b`; no upstream changes. | `cargo metadata` identified four targets. `cargo test` ran 10, 5, and 41-test groups; 10 and 5 passed, while 3 of 41 color snapshot tests failed due terminal-visible spacing differences. |
| `sharkdp/bat` | Medium Rust repository for handoff, DAG, retry, supervisor, and control work. | Shallow clone `/tmp/relay-audit-medium`, `b671e53`; local branch `relay-local-handoff`; no upstream changes. | `cargo fmt --check` passed. `cargo test --all-targets --quiet` passed all targets (440 tests including ignored groups). |
| `tokio-rs/tokio` | Multi-package Rust workspace/monorepo. | Shallow clone `/tmp/relay-audit-monorepo`, `ea91b33`; no upstream changes. | Workspace has five public/internal package members plus tests/examples. Relay ran `cargo check -p tokio-util --quiet` successfully in 7 seconds. |
| Nested local Git repositories | Boundary target for nested repository behavior. | Dedicated `/tmp/relay-audit-scope-boundary`; outer and inner repositories created locally. | `auto` and `repository` stayed on inner state; outer `auto` returned outer state. Outside-Git `repository` scope failed as documented. |

A no-Relay hexyl control clone at `/tmp/relay-audit-small-control` performed
status, README/CONTRIBUTING reads, and `cargo test --lib` in five seconds. This
is not a randomized user study: it shows that Relay did not make basic source
orientation faster by itself, while the Relay handoff made prior agent state
available without reconstructing it.

## 4. Scenario methodology and results

### Scenario A — unfamiliar repository takeover

Agent A started a Relay session in fresh hexyl, recorded a decision and progress
note, mapped Cargo targets, ran tests, recorded the controlled test failure, made
a checkpoint, and generated a handoff. Agent B first ran `relay context`, then
read the suggested files, confirmed library tests, checkpointed, and finished.

The context contained the goal, commit, branch, prior work, decision, blocker,
next action, checkpoints, and event lineage. B had zero unanswered handoff
questions and did not repeat A's repository topology or metadata commands.

### Scenario B — multi-agent handoff

Agent A performed bat formatter and Cargo-target orientation on an isolated local
branch, then handed off. Agent B consumed the persisted context, ran
`cargo test --all-targets`, recorded progress, checkpointed, and finished. The
saved handoff was 39 lines and contained goal, two completed facts, two decisions,
Git identity, one checkpoint, and one next action—no event-log dump or unrelated
source text.

### Scenario C — real build/test DAG

The bat plan contained `format`, `lint`, `unit`, `integration`, `build`, and
`package`, with dependencies between stages and `--jobs 2`. It invoked Cargo and
`tar` directly, produced `/tmp/relay-audit-bat.tar.gz`, and persisted a 10,492-byte
schema-v3 aggregate. All six tasks succeeded. Recorded intervals showed `lint`
and `unit` overlap, `integration` overlap with still-running `lint`, and a peak
of two active tasks. Input order remained `format`, `lint`, `unit`,
`integration`, `build`, `package`.

A separate plan proved a failed task blocks its descendant while unrelated work
still succeeds: `fail` was `failed (exit_nonzero)`, `blocked` was
`blocked (dependency fail did not succeed)`, and `unrelated` succeeded.

Compared with Make/just/Task, Relay's plan is verbose but adds per-task attempt
state, bounded concurrency, failure propagation, cancellation, and crash-safe
resume. Bat had no Makefile, justfile, or Taskfile; Cargo commands were the
simpler alternative for ordinary local builds.

### Scenario D — retry behavior

A controlled `/usr/bin/sh` task failed once and then succeeded. With schema-v2
`retry.max_attempts: 2`, Relay recorded attempts `(1, failed, exit_nonzero)` and
`(2, succeeded)`; the dependent task ran once. With no retry field, the same
controlled failure had one attempt and failed. A SIGINT during a sleeping task
produced one `cancelled (cancelled_terminated)` attempt and a blocked descendant.
Using `/usr/bin/false` as the supervisor caused `run indeterminate` with one
indeterminate attempt and no retry.

Retry policy is usable for a known flaky command, but JSON authoring is high
friction and there is no idempotency or retry predicate. No automatic retry of
uncertain work was observed.

### Scenario E — crash recovery

The debug-only pause hooks stopped the Rust coordinator after state persistence at
several boundaries. The coordinator was killed with `SIGKILL`; Go state and
aggregate state were then inspected before resume.

| State at coordinator death | Resume result | Duplicate/unsafe rerun |
| --- | --- | --- |
| Go terminal success, aggregate task still running | Trusted `completed`; one attempt remained | None |
| Go terminal nonzero failure, aggregate task still running | Trusted `failed`; one attempt remained | None |
| Child/supervisor still running | Resume refused with `attempt still active`; state stayed running | None |
| Missing Go state | Indeterminate with `supervisor attempt state is missing` | None |
| Malformed Go state | Indeterminate with parse reason | None |
| Stale dead supervisor/child | Indeterminate with unverified process identity | None |
| Active state with changed process identity | Indeterminate with `supervisor process identity could not be verified` | None |
| Active state with mismatched execution run ID | Indeterminate with `run_id did not match attempt` | None |
| Parallel fast-terminal plus active task | Fast task reconciled in memory; active branch caused safe refusal | None |

No unsafe rerun or duplicate work event occurred. Still-running and uncertain
cases correctly require manual recovery rather than pretending the task is safe.

### Scenario F — next-day-style resume

The coordinator was killed after `prepare` had a trusted Go terminal result but
before aggregate completion; `test` remained pending. The next process ran only
with the persisted state and original plan. Resume reconciled `prepare`, launched
`test` once, and completed in under one second at command granularity. No terminal
attempt was rerun.

### Scenario G — corrupted/stale state

The following were exercised on dedicated copies or state paths:

- truncated aggregate JSON;
- unsupported aggregate schema `99`;
- inconsistent aggregate status;
- mismatched aggregate/supervisor identity/path;
- active supervisor run-ID mismatch;
- supported v1 and v2 aggregate records, both migrated to v3;
- missing, malformed, and stale supervisor records;
- event-state symlink and aggregate-state symlink substitution;
- incomplete and provably complete operation markers.

Errors were explicit and non-destructive. Supported v1/v2 copies resumed and were
rewritten as schema 3. Unsupported/truncated/inconsistent state failed before
launch. Symlinked paths were rejected. Doctor repaired only the completed marker
and refused the incomplete marker. Corruption setup required deliberate Python
state edits; this is test harness work, not normal developer intervention.

### Scenario H — monorepo and project scoping

Tokio root state was created with `--scope repository`; `tokio/` and
`tokio-util/` received independent `--scope current` sessions. A deep `tokio/src`
command with `auto` selected the nearest package state; `repository` selected root;
`current` required local state and did not cross upward. A sibling package with no
current state selected root under `auto`, then became independent after its own
current state was created. Nested Git repositories stopped discovery at the
nearest Git worktree. Outside Git, repository scope failed.

One concrete friction appeared: repository-scoped Git snapshots exclude only the
selected root `.relay`, so nested current-scoped `.relay` directories appear as
changed files in the root snapshot. Package-scoped snapshots stayed clean. README
now documents this boundary instead of implying all nested state is excluded.

## 5. Measurements

The counts below are manually counted from scenario command blocks. They measure
this audit run, not general product averages.

| Scenario | time_to_resume | repeated_commands | repeated_repo_reads | manual_interventions | duplicate_work_events | unsafe_rerun_events | failed_recoveries | context_gaps | relay_commands_used | relay_commands_bypassed | usefulness / clarity / trust / discoverability / friction |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| A takeover | `<1 s` context; 28 s B work | 0 observed | 0 observed | 1 harness quoting retry | 0 | 0 | 0 | 0 unanswered | 20 | 12 raw repo/control commands | 5 / 4 / 4 / 4 / 3 |
| B handoff | `<1 s` context | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 8 | 7 Git/Cargo commands | 4 / 4 / 4 / 3 / 3 |
| C DAG | N/A | 0 | 0 | 0 | 0 | 0 | 0 | N/A | 2 runs | 0 inside plan; 6 child commands | 4 / 4 / 5 / 3 / 4 |
| D retries | N/A | 0 | 0 | 0 | 1 intended retry | 0 | 0 | N/A | 4 runs | 0 | 4 / 4 / 5 / 3 / 3 |
| E crash recovery | `<1 s` terminal reconcile; refusal immediate | 0 | 0 | 6 deliberate teardown/state setup actions | 0 | 0 | 0 incorrect recoveries; 6 safe refusals | N/A | 18 run invocations | 9 Python/process-control probes | 5 / 5 / 5 / 3 / 3 |
| F next-day resume | `<1 s` | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 runs | 2 Python state probes | 5 / 4 / 5 / 3 / 2 |
| G corruption | `<1 s` error/refusal | 0 | 0 | 7 deliberate corruption edits | 0 | 0 | 0 incorrect recoveries | N/A | 8+ | 10 state/symlink setup commands | 4 / 4 / 5 / 3 / 4 |
| H scoping | N/A | 0 | 0 | 0 | 0 | 0 | 0 | 0 boundary crossings observed | 14+ | 8 Git/init/status commands | 5 / 4 / 5 / 4 / 3 |

The only normal-work manual intervention was a malformed audit-side Python
pipeline during hexyl metadata collection; it was corrected without changing
Relay or the repository. Other interventions above are intentional crash or
corruption controls.

## 6. Failures found

### Relay/documentation failures

1. README claimed standalone `relay supervise` never writes Go records, but real
   standalone runs created the requested Go state file. README and protocol docs
   now say Rust does not reconcile existing records while Go writes each attempt
   record.
2. README did not explain that a valid child failure, timeout, or cancellation is
   a transport-success response and therefore exits zero. It now points callers
   to `execution.lifecycle` and `termination_reason`.
3. README did not explain that `doctor` can exit zero with `data.healthy: false`.
   It now identifies health fields as authoritative.
4. README did not warn that recorded context is untrusted data. It now tells
   agents and developers to review notes, handoffs, and Git-derived text before
   execution.
5. README did not describe nested `.relay` directories appearing in a
   repository-scoped Git snapshot. It now states the observed boundary.

### External/repository observations

- Hexyl's three color snapshot tests failed in this terminal because expected
  and actual output differed only in visible spacing, while the child exited
  zero. No Relay behavior caused this failure and no upstream fix was attempted.
- The control and Relay runs show Relay is not a replacement for repository
  inspection or native build tools. Its value appears after facts exist: durable
  handoff, bounded context, safe orchestration, and recovery.

No Relay correctness or unsafe-rerun bug was found in exercised paths. No source
code fix was justified by this audit.

## 7. Alternative comparison

| Use case | Simplest alternative | Relay advantage | Alternative wins when | Verdict |
| --- | --- | --- | --- | --- |
| Goal/progress handoff | README/TODO or manual text | Typed session, Git facts, event history, bounded context, lineage. | One person, one short task, no handoff. | Relay worth using for recurring agent work. |
| Finding prior decisions | `git log`, shell history, `grep` | Searches one durable event log with session/type/time/category filters. | Facts already live in commits or search needs source files. | Useful, not a universal search replacement. |
| Multi-step build | Cargo commands, Make, just, Task | Dependencies, per-attempt state, cancellation, retry policy, crash reconciliation. | Repeatable simple build with no interrupted-agent risk. | Use native runner for routine builds; Relay for recoverable agent runs. |
| One child with timeout | `timeout`, shell, direct command | Typed lifecycle, process identity, output limits, redaction, persisted evidence. | One-off command where durable record is unnecessary. | Advanced/niche; keep as run foundation. |
| Retry | Shell loop | Explicit attempt history and no retry after uncertainty/cancel. | Idempotent command with obvious local failure. | Keep, document limits. |
| Machine integration | Raw JSON or scripts | Versioned envelopes, fixed stdout/stderr contract, protocol checks. | Private script with no compatibility need. | Keep for agents/tools; protocol manifest itself needs pruning decision. |
| Session memory | Agent-native memory | Repo-local, inspectable, cross-agent, independent of provider. | Same agent/session remains alive. | Relay wins for handoff and next-day return. |

## 8. Documentation review and changes

README and protocol docs were checked against command output, source dispatch, and
real state files. Setup, first-run lifecycle, notes, context, handoff, project
scope, DAG schema, retries, resume, crash reconciliation, JSON framing, and
completions matched behavior. The following documentation-only fixes were made:

- corrected Go record ownership wording;
- documented standalone transport-success/child-outcome exit semantics;
- documented doctor health semantics;
- added context prompt-injection warning;
- documented nested project-state visibility in repository Git snapshots.

`docs/agent-loop-state.md` remains historical cycle evidence, not a current usage
guide; its old branch/revision baselines are archival claims and should not be
read as current implementation status.

## 9. Remaining limitations

- Remote PR14 merge status could not be independently fetched because GitHub
  authentication failed; audit base evidence is local candidate history plus the
  user premise.
- Context and search are deterministic, not semantic; search is case-sensitive.
- Handoff quality depends on agents recording facts; Relay does not inspect source,
  transcripts, or model memory.
- DAG JSON is verbose for small workflows; no native Make/just/task import exists.
- Retry policy has no predicate, idempotency model, jitter, or global policy.
- Running, uncertain, or detached work is not automatically adopted or retried.
- Process-group cleanup cannot prove freedom from a child that self-detaches.
- Protocol manifest still exposes future Go/Lua components as unimplemented.
- Hexyl snapshot failures show terminal-sensitive repository tests can remain
  environment-dependent even when Relay correctly records the result.

## 10. Features with clear value

Keep exactly: session lifecycle, notes, handoffs, bounded context, project scope,
Git snapshots, JSON envelopes, plan validation, aggregate state, resume,
cancellation, crash reconciliation, and the one-child supervisor boundary.

Keep with normal scope: checkpoints, history/navigation, search, doctor/repair,
locking, completions, bounded parallelism, retries, output/resource controls.
They solve real problems but are not all daily differentiators.

## 11. Features without clear standalone value

The public `protocol` command's future external-orchestrator/Lua advertisement
has no observed consumer and is confusing when read as product functionality.
Deprecate that unimplemented advertisement or reduce it to implemented contract
metadata in a later compatibility decision. Do not remove it in this audit: the
manifest is versioned and no consumer inventory is available.

OpenSpec files are maintainer artifacts, not user features. No replacement feature
was added.

## 12. Bugs fixed and UX issues fixed

| Type | Fix |
| --- | --- |
| Correctness | None; tested paths had no Relay correctness or unsafe-rerun defect. |
| Safety | None; missing/stale/malformed/mismatched state already refused safely. |
| UX/documentation | Clarified supervise result/exit semantics, doctor health signal, Go record ownership, untrusted context, and nested scope snapshots. |

## 13. Recommended next milestone

Do not start a new major feature milestone. First decide whether the unused
future protocol advertisement should be deprecated, then tighten the existing
daily-driver path: one short handoff/context example, one real run/resume example,
and clearer search/retry expectations. Keep native Cargo/Make/just/Task as the
simpler choice for ordinary builds. Add no remote execution, daemon, workflow
language, UI, model integration, or structured-output subsystem based on this
audit.

## Conclusion

Relay proved useful in real workflows, but narrowly. It materially improves
cross-agent context transfer and recovery of multi-step local work. It does not
replace repository tools, source inspection, or native task runners. The strongest
keep decision is the durable session/context/handoff plus crash-safe run state;
the strongest simplify/deprecate decisions are verbose DAG authoring and the
unconsumed future protocol advertisement.

## 14. Reusable workflows milestone

This section supersedes the historical recommendation above for the current
workflow milestone. Workflow definitions live in version-controlled
relay-workflows/ JSON files. The Rust resolver validates bounded scalar inputs,
explicit argv references, task DAGs, retries, task outputs, and artifact inputs,
then calls the existing plan-v3 coordinator.

Real Go integration used a locally built relay-supervisor and an isolated
project with actual local task programs. Workflow verify ran inspect, format, test, and report
tasks with a required profile input, structured JSON outputs, file artifacts,
and downstream output consumption. It completed twice as fresh runs. An
interrupted run was resumed after changing its workflow file; persisted plan
arguments and exact output attempt provenance remained unchanged, with no
duplicate successful inspect attempt. The same resume path also passed after
definition deletion and malformed replacement in the Rust integration suite.

Workflow-level output aliases, composition, imports, Lua, scripting, dynamic
control flow, remote execution, daemon mode, secret management, and provider
integrations remain deliberately deferred.

### Review answers

1. Resolution is deterministic: sorted JSON files, BTreeMap inputs, ordered tasks, and canonical serialization.
2. Malformed definitions and inputs launch zero child processes; validation precedes preflight.
3. String inputs stay single literal argv elements; no shell or interpolation exists.
4. Workflow execution calls existing plan-v3 DAG machinery; no second scheduler exists.
5. A changed definition cannot alter an existing interrupted run; persisted plan wins.
6. Deleting or malforming live definition does not block safe workflow resume.
7. State stores workflow name, schema version, SHA-256, resolved inputs, and sanitized plan.
8. Output and artifact trust checks remain in existing Rust finalization and consumption paths.
9. File, count, name, input, task, parser-depth, metadata, plan, output, and aggregate bounds are enforced.
10. The schema has no expressions, branching, loops, imports, functions, or dynamic task generation.
11. Direct relay run remains covered by the existing full Rust and real Go suites.
12. Small workflow definitions remove repeated plan boilerplate while retaining explicit task configuration.
