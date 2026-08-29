# Relay feature audit

Evidence: real runs against hexyl, bat, Tokio, and nested local Git repositories;
full Rust/Go suites; crash, retry, corruption, and scope scenarios. Classifications:
`core`, `useful`, `niche`, `redundant`, `unclear`, `remove_candidate`.

| Capability | Classification | Recommendation | Evidence |
| --- | --- | --- | --- |
| Session lifecycle | core | keep | Goal, current state, completion, and lineage survived real handoffs. |
| Checkpoints | useful | keep | Durable Git boundary used by handoff/context. |
| Categorized notes | core | keep | Decisions, blockers, and next actions survived agent change. |
| Handoffs and summaries | core | keep | Real handoff was concise and actionable. |
| Bounded context | core | keep | Fresh agent got prior facts without repeated topology inspection. |
| History and navigation | useful | keep | UUID prefixes, `latest`, history, and lineage worked. |
| Search | useful | simplify | Useful event search; case-sensitive no-match behavior adds friction. |
| Project scoping | core | keep | Nested, package, repository, and outside-Git boundaries held. |
| Git integration | core | keep | Branch, commit, cleanliness, and scoped paths explained work. |
| Doctor diagnostics | useful | keep | Corruption was explained without rewriting bytes. |
| Doctor repair | niche | keep | Only provably complete marker was repaired. |
| Mutation locking | useful | keep | Contention returned actionable owner metadata. |
| JSON envelopes | core | keep | Machine-readable success/error framing worked across commands. |
| Shell completions | useful | keep | Bash/Zsh/Fish scripts generated current commands. |
| Standalone supervisor | useful | document | Real timeout, cancel, output, and redaction behavior worked; child outcomes exit through transport-success semantics. |
| Go one-child supervisor | core | keep | Typed per-attempt state underpins safe orchestration. |
| Protocol/versioning | unclear | deprecate | Stable manifest has no observed external consumer and advertises unimplemented Go/Lua components. |
| Plan validation/direct argv | core | keep | Unknown fields and unsafe inputs failed before launch. |
| DAG orchestration | useful | simplify | Real six-task graph worked; JSON authoring is verbose for simple builds. |
| Bounded parallelism | useful | keep | `--jobs 2` overlapped work and peaked at two tasks. |
| Cancellation | core | keep | SIGINT stopped work, blocked descendants, and suppressed retry. |
| Explicit retries | useful | document | Known failure retried exactly when configured; uncertainty never retried. |
| Aggregate run state | core | keep | Durable task/attempt state enabled crash and next-day recovery. |
| Run resume | core | keep | Pending work resumed without rerunning trusted terminal work. |
| Crash reconciliation | core | keep | Terminal evidence reconciled; uncertain work stayed uncertain. |
| Process identity and cleanup | useful | keep | Stale/mismatched identities blocked unsafe continuation. |
| Output/resource/secret controls | useful | keep | Retention capped, args redacted, environment restricted. |
| OpenSpec artifacts | unclear | document | Maintainer planning only; not runtime capability. |
| Future external orchestrator/Lua advertisement | remove_candidate | deprecate | `relay protocol` reports both unimplemented; no real workflow used them. |

## Decisions

### Keep

Session lifecycle, notes, handoffs, context, project scope, Git snapshots, JSON,
plan validation, aggregate state, resume, cancellation, crash reconciliation, and
the one-child supervisor boundary materially improve cross-agent or recoverable
local work.

### Simplify or document

Search, DAG authoring, retries, standalone supervisor outcomes, and completions
need low-friction expectations. Native Cargo/Make/just/Task remains simpler for
ordinary builds; Relay earns its cost when durable state, handoff, or recovery
matters.

### Remove

No implemented feature met evidence threshold for immediate removal. Deprecate
only future unimplemented protocol advertisements after checking compatibility
consumers.
