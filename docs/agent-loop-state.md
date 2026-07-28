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

## Cycle 2 — structured context event details

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `61f73a9` before this cycle, with the branch aligned to `origin/feat/relay-daily-driver`.
- Cycle 1's canonical Rust verification contract remains authoritative. No Go or Lua component exists yet, so this bounded Rust context improvement does not add or change a cross-process protocol, persistence schema, dependency, or migration.
- The strongest recorded next-cycle candidate was to make selected events useful in human context without duplicating notes and checkpoints. This cycle implements only that candidate.

### Selected improvement and rationale

Render the structured payload of `handoff.generated` events in human `relay context` output. Type-only event lines hid the handoff's target, completed work, decisions, failures, and next action even though Relay had durably recorded those fields. Surfacing that data makes bounded context materially more useful to the next terminal agent while preserving deterministic selection and typed JSON.

### Focused implementation plan

1. Add a private, type-aware renderer for selected context events.
2. Render known handoff fields in a stable semantic order with existing whitespace normalization and project-root redaction.
3. Keep notes and checkpoints in their dedicated context items, leave unknown event kinds type-only, and preserve each event as one character-budget item.
4. Prove human details, privacy, non-duplication, and unchanged structured JSON through a black-box CLI test and direct workflow.
5. Document the human rendering contract and run the canonical suite.

### Cycle outcome

#### Implemented

- Human context keeps the existing timestamp and event-type prefix, then renders present handoff fields under `Target agent`, `Completed work`, `Decisions`, `Failures`, and `Next actions`.
- Missing, malformed, non-string, and whitespace-only handoff values are omitted safely. Recorded array order is preserved, and every rendered value uses the existing Markdown normalization and project-root redaction helper.
- Note text and checkpoint messages remain rendered only by their dedicated items. Other event kinds remain type-only, preventing accidental payload dumping or duplication.
- Typed JSON events and their schema-v1 payloads are unchanged. Event filtering, recency, ordering, count omissions, and item-boundary character budgeting are unchanged.
- Parallel writers worked in separate detached worktrees on `src/app.rs` and `tests/cli.rs`; a separate renderer review found no further correctness or maintainability defect. The primary checkout integrated both patches and the README clarification.

#### Verification

The focused regression passed:

```text
cargo test --test cli context_renders_structured_handoff_details_without_duplicating_separate_items -- --exact  PASS (1 test)
```

`cargo build --quiet` passed. A direct isolated CLI workflow recorded a note, checkpoint, and handoff with every structured field, then exercised human and JSON context. It confirmed ordered labeled details, one occurrence each for the note and checkpoint text, project-root redaction in Markdown, and the unchanged scalar `next_action` plus array-valued handoff fields in JSON. The first smoke assertion expected the next action on the label line, but the renderer intentionally emits it as a nested list item; correcting only that harness assertion made the workflow pass.

The complete canonical suite passed in the required order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (52 tests)
```

#### Known limitations and remaining technical debt

- Structured human details are intentionally limited to handoff events. Session lifecycle events remain type-only because their useful current facts already appear in the session section.
- Character-budget truncation still reports only a boolean; count-limit omissions are not distinguished from items excluded by the character budget.
- Mutation output assembly can still rescan unrelated historical records after a durable write instead of rendering a typed result returned directly by that operation.
- This is not the final completion cycle: the branch is committed locally after verification but is not pushed again and no draft pull request is created in this cycle.

#### Strongest next-cycle candidates

1. Harden the strict `--max-chars` contract at the minimum accepted budget, including Unicode and separate character-budget omission accounting.
2. Make mutating commands return first-class typed results so output cannot fail because unrelated historical records are corrupt.
3. Add an inspected, deterministic subset of context/history navigation such as latest-session selection, reverse display, or time bounds without changing existing defaults.
