# Agent loop state

## Cycle 1 — bounded context recency

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `a01451c` before this cycle.
- Relay is a synchronous, local-first Rust CLI. `src/cli.rs` defines the command surface, `src/app.rs` owns lifecycle and context behavior, typed schema-v1 records live in `src/model.rs`, `src/storage.rs` provides durable file operations, and Git access is read-only through `src/git.rs`.
- The two existing milestones and all tracked OpenSpec tasks are complete; no tracked `TODO`, `FIXME`, unchecked task, alternate task runner, or additional Rust verification configuration was found.
- The latest milestone made Relay a dependable daily driver with locking, durable history, JSON output, diagnostics, structured notes, bounded context, project discovery, and completions. This cycle strengthens that existing context workflow rather than adding a separate system.

### Canonical verification contract

Tracked CI (`.github/workflows/ci.yml`) is authoritative. From the repository root, every cycle runs these commands in this exact order:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

If the first command reports formatting changes, run `cargo fmt`, repeat `cargo fmt --check`, and then continue in the order above. No separate build gate is tracked.

### Selected improvement and rationale

Make `relay context` limits retain the most recent matching notes, events, and checkpoints while preserving their recorded order. The current implementation keeps the oldest notes and events, silently caps checkpoint consideration at five, and can therefore omit the freshest agent context even when a larger checkpoint limit is requested. That undermines Relay's primary handoff workflow and makes omission metadata inaccurate.

### Focused implementation plan

1. Separate `context` checkpoint limits from the handoff renderer's intentional five-checkpoint window.
2. Apply each `context` count limit to the newest matching items, then render the retained window in chronological event order.
3. Report omitted counts against all matching records and preserve existing filters, JSON fields, character budgeting, and human defaults.
4. Add black-box coverage for recency, limits above five checkpoints, ordering, and omission metadata.
5. Clarify the CLI help and README limit semantics, directly exercise the workflow, and run the canonical suite.

### Cycle outcome

#### Implemented

- `relay context` now applies note, event, and checkpoint count limits after filtering and keeps the newest matching window in chronological event order.
- Checkpoint selection accepts the caller's limit and returns an accurate omitted count. Handoff generation continues to request its intentional five most recent checkpoints, while context may request more than five.
- Event selection retains references instead of cloning every matching event. The change does not alter storage schema, JSON field names, dependencies, filters, character-budget behavior, or human defaults.
- CLI help and the README now define each count as a most-recent limit. A black-box CLI test covers recent-note/event ordering, a six-of-seven checkpoint window, human rendering, and omission metadata.

#### Architectural decisions

- A small generic `retain_latest` helper removes only the obsolete prefix after deterministic filtering, so every retained window remains in append order.
- Checkpoint payloads are deserialized only for the selected window, preserving the handoff path's existing tolerance for malformed records outside its five-item window.
- The implementation stays inside the existing synchronous application layer; no new model, storage format, index, or dependency was warranted.

#### Verification

The focused regression and direct workflow passed:

```text
cargo test --test cli context_limits_keep_latest_items_in_recorded_order -- --exact  PASS (1 test)
cargo build --quiet                                                               PASS
cargo run --quiet -- context --help                                               PASS
```

An isolated non-Git CLI workflow recorded two notes and two checkpoints, then ran:

```sh
relay --scope current --json context --notes 1 --events 1 --checkpoints 1
```

It returned `latest context`, `latest checkpoint`, the final `checkpoint.created` event, and omitted counts of 1 note, 1 checkpoint, and 4 events. The first smoke harness assertion used the Rust field name `event_type` and failed with `KeyError`; the documented JSON contract serializes that field as `type`. Correcting only the harness lookup made the same workflow pass.

The complete canonical suite then passed in the required order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (51 tests)
```

#### Known limitations and remaining technical debt

- Recency is append-order based, not semantic ranking; Relay still makes no inferred relevance claims.
- The character budget may omit items from the already selected recent window. `truncated` reports that condition, while per-category omitted counts continue to describe count-limit exclusions only.
- Human Markdown context identifies selected events by type but does not yet render useful structured payload details such as explicit handoff decisions or next actions.

#### Strongest next-cycle candidates

1. Render concise, type-aware event details in Markdown context, especially recorded handoff payloads, while avoiding duplication with notes and checkpoints.
2. Make typed command results first-class so a completed mutation cannot later be reported as failed because generic output assembly rescans unrelated historical session records.
3. Verify and harden the strict `--max-chars` contract at the minimum budget, including explicit accounting for items omitted by the character budget.
