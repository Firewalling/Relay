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

## Cycle 3 — strict context character budgets

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `136635e` before this cycle. The local branch was one verified cycle ahead of `origin/feat/relay-daily-driver`.
- Cycle 2 left `--max-chars` using a magic 64-character footer reservation. Its minimum-budget fallback could exceed the accepted limit, Unicode/footer boundaries were not proved, and `truncated` did not distinguish count-limit omissions from character-budget omissions.
- This cycle implements only the strongest recorded candidate: harden the existing bounded-context contract. It does not begin typed mutation results, navigation, search, Go orchestration, or Lua workflows.

### Selected improvement and rationale

Make human `relay context --max-chars` deterministic and strictly bounded for every accepted value, including the minimum 64 Unicode characters. Agents need a reliable prompt-size boundary, and operators need to know whether records were excluded by explicit count limits or only from the rendered Markdown because of the character budget.

### Focused implementation plan

1. Replace approximate footer reservation with exact Unicode-scalar accounting for the session header, indivisible items, omission metadata, and the CLI's final transport newline.
2. Track count-limit and character-budget omissions independently for notes, checkpoints, and events.
3. Preserve selected typed records in JSON while adding explicit budget omission metadata.
4. Provide deterministic compact and minimal human fallbacks when the full session header and detailed footer cannot fit.
5. Cover minimum and exact budgets, Unicode, long content, whole-item behavior, repeated execution, footer accounting, and human/JSON reporting.

### Cycle outcome

#### Implemented

- Context rendering now represents selected notes, checkpoints, and events as typed indivisible items with cached Unicode character counts. Inclusion preserves presentation order and continues trying later items when an earlier one is too large.
- Detailed footer length is computed from its exact labels, boolean, and decimal counts. A monotone inclusion pass accounts for count-width changes, so the rendered Markdown and CLI-added newline never exceed the requested budget.
- When the full session header cannot fit, Relay reports a compact `N/C/E` summary for notes, checkpoints, and events. If unusually wide counts make that summary too large, a fixed minimal truncated response remains safely below the accepted minimum.
- Existing JSON `omitted` counts remain count-limit-only. Additive `budget_omitted` counts and `budget_truncated` identify human character-budget exclusions; selected note, checkpoint, and event arrays remain unchanged.
- CLI help and the README now scope `--max-chars` to human Markdown Unicode characters, document indivisible items and both omission causes, and clarify that the JSON envelope retains typed selected records rather than being character-bounded.
- The change adds no dependency, persistence field, schema migration, or storage reinterpretation.

#### Architectural decisions

- Rust remains the sole owner of bounded context selection and rendering. No process boundary or new component is warranted for deterministic local formatting.
- The application reserves one explicit character for the existing `println!` transport newline instead of changing global output emission behavior for unrelated commands.
- Budget omission metadata describes presentation only. Count-limited typed records remain available to JSON consumers, preserving the existing machine-readable contract while adding diagnostic fields.

#### Verification

The focused regression passed:

```text
cargo test --test cli context_strict_character_budget_is_deterministic_and_reports_omission_causes -- --exact  PASS (1 test)
```

The test covers a long Unicode goal, long Unicode notes and checkpoints, deterministic repeated minimum output, exact-fit and one-character-short budgets, whole-item rendering, detailed and compact footers, count versus budget omissions, and unchanged selected JSON records. Its first run compared a normalized Markdown item with the fixture's raw trailing whitespace and falsely reported a split item; correcting the assertion to use the documented whitespace normalization made the behavior test pass.

`cargo build --quiet` passed. A direct isolated non-Git workflow created two notes and two checkpoints under a long Unicode goal, then ran human and JSON context with one selected item per category at `--max-chars 64`. The repeated human output was identical at 63 of 64 Unicode characters, reported count omissions `1/1/4` and budget omissions `1/1/1`, and JSON reported the same separate causes while retaining one selected typed record per category.

The first canonical Clippy run found one equivalent test comparison written as `count <= budget - 1`; the isolated correction used `count < budget`. The complete canonical suite then passed in order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (53 tests)
```

#### Known limitations and remaining technical debt

- `--max-chars` bounds human Markdown/stdout; the versioned JSON envelope intentionally remains unbounded and returns the selected typed records plus budget metadata.
- Extremely wide omission counts may not fit the compact minimum response. Relay then emits only the bounded minimal truncated marker; full counts remain available in JSON.
- Character budgeting counts Unicode scalar values, matching Rust `chars()`, rather than terminal display columns or encoded bytes.
- Mutation output assembly can still rescan unrelated historical records after a durable write instead of rendering a typed result returned directly by that operation.
- This is not the final completion cycle, so the branch is committed locally but not pushed again and no draft pull request is created in this cycle.

#### Strongest next-cycle candidates

1. Make mutating commands return first-class typed results so successful durable writes cannot be reported as failures because unrelated historical records are corrupt.
2. Add an inspected deterministic subset of context/history navigation, such as latest-session selection, reverse display, or time bounds, without changing existing defaults.
3. Add deterministic local search across recorded sessions, event types, note categories, times, and plain substrings without hosted services or inferred relevance.

## Cycle 4 — typed mutation results

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `61909b2` before this cycle. The local branch was two verified cycles ahead of `origin/feat/relay-daily-driver`.
- Mutating functions returned only human strings. After durable effects completed, generic `command_data` rescanned every session and event, recaptured Git, assumed the last event belonged to the command, and parsed the handoff path from a human suffix.
- An unrelated malformed historical session record could therefore make a successfully appended note or completed artifact mutation exit as a failure during output reconstruction.
- This cycle implements only typed mutation results. It does not begin navigation, search, Go orchestration, or Lua workflows.

### Selected improvement and rationale

Make each mutation return its own typed result after durable effects complete, then render both human and versioned JSON output directly from that result. A command that has safely completed its write must not be reclassified as failed because unrelated historical records cannot be rescanned or because generic reconstruction guesses the wrong latest event.

### Focused implementation plan

1. Define private typed results for start, checkpoint, handoff, finish, resume, note, and doctor.
2. Preserve existing human wording and JSON field names while rendering from operation-owned session, Git, artifact, path, Markdown, and note values.
3. Remove generic post-dispatch history rescans, latest-event assumptions, handoff suffix parsing, and second Git captures.
4. Assemble status directly from one coherent read set so the generic reconstruction function can be deleted.
5. Prove durable mutation output survives unrelated historical corruption and corresponds exactly to current artifacts, events, sessions, and additive-compatible envelopes.

### Cycle outcome

#### Implemented

- `start`, `checkpoint`, `handoff`, `finish`, `resume`, `note`, and `doctor` now return command-specific typed result objects. A small infallible conversion layer renders each result into the existing human text and JSON object.
- Start and resume reuse the Git snapshot already stored in their new session and report the deterministic single creation event. Checkpoint, handoff, finish, and note return their direct artifact/session/payload values rather than inspecting the event-log tail.
- Handoff JSON receives the exact saved Markdown and relative path without parsing `Saved:` from human text. Finish returns its own summary path, and note returns its exact category/text payload.
- Doctor uses a typed diagnostic result for both inspection and repair; its post-repair state inspection remains intentional command behavior rather than generic output reconstruction.
- Status now builds human and JSON output from one session/event/Git read set. The generic `command_data` function and every post-dispatch reconstruction call were deleted.
- Existing human wording, JSON field names and types, output schema version, storage schema, operation ordering, locking, and recovery behavior remain unchanged.
- The README now documents that mutating human and JSON output come from the operation's typed result rather than an unrelated history rescan.

#### Architectural decisions

- Typed command results remain private to the Rust application layer. They are transient output values, not new persisted models, so schema-v1 state and compatibility require no migration.
- Result rendering is infallible and performs no filesystem or Git access after durable completion. Any command-specific validation or history access still occurs before mutation or as an explicit part of the operation.
- Output-v1 tests require established fields but permit additive fields, preserving the documented backward-compatible envelope policy.

#### Verification

The focused regressions passed:

```text
cargo test --test cli mutation_results_do_not_rescan_corrupt_unrelated_session_records -- --exact                 PASS (1 test)
cargo test --test cli json_envelopes_are_single_typed_documents_and_failures_stay_on_stderr -- --exact           PASS (1 test)
```

The corruption regression finishes an older session, starts a valid current session, corrupts only the older durable session record, and then successfully records a human note plus JSON checkpoint, handoff, and finish results. It verifies the note event, checkpoint artifact/event/path, handoff bytes/path/current last agent, finish event/current summary path/completed session, single envelopes, relative paths, and unchanged corrupt bytes.

`cargo build --quiet` passed. A direct isolated non-Git workflow repeated the same historical-corruption scenario and confirmed that note, checkpoint, handoff, and finish output remained successful and matched their current durable artifacts.

The first canonical Clippy compilation exposed missing `Debug` derives required by existing in-file `unwrap_err` tests after their success types became typed results. An isolated correction added consistent derives without runtime changes. Independent test review also corrected two coverage defects: required-key assertions now permit additive output fields, and finish output is tied to the latest current-session `session.finished` event rather than merely any existing summary file.

The complete canonical suite then passed in order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (54 tests)
```

#### Known limitations and remaining technical debt

- Commands that semantically select or validate historical state, such as start and resume, still reject malformed records before mutation. Typed results prevent only post-completion output reconstruction from introducing a new failure.
- Doctor intentionally inspects state after a repair to report the resulting diagnostics; that inspection is part of the repair contract and not generic reconstruction.
- Typed results are private Rust application values rather than a cross-process protocol. Relay still has no Go orchestrator or Lua workflow layer.
- Deterministic context/history navigation and local search remain unimplemented.
- This is not the final completion cycle, so the branch is committed locally but not pushed again and no draft pull request is created in this cycle.

#### Strongest next-cycle candidates

1. Add an inspected deterministic subset of context/history navigation, such as latest-session selection, reverse display, or time bounds, without changing existing defaults.
2. Add deterministic local search across sessions, event types, note categories, times, and plain substrings without embeddings, hosted services, or inferred relevance.
3. Define the first versioned Rust/Go orchestration protocol boundary before implementing any external-process scheduler or Lua workflow runtime.

## Cycle 5 — deterministic history navigation

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `50cc5b7` before this cycle. The local branch was three verified cycles ahead of `origin/feat/relay-daily-driver`.
- Session selectors accepted only full UUIDs or unambiguous prefixes. History always displayed earliest matching events first, had no time bounds, and applied its limit only to that fixed order.
- This cycle implements one inspected navigation subset: a shared latest-session alias plus reversible, inclusive time-bounded history. It does not add search, cursors, persistence, Go orchestration, or Lua workflows.

### Selected improvement and rationale

Add deterministic terminal navigation without changing existing defaults. Operators and agents need to address the newest session without copying UUIDs, retrieve newest history under a bound, and select an exact recorded time window while retaining Relay's append-order semantics.

### Focused implementation plan

1. Extend central session selection with a case-insensitive `latest` alias based on Relay's existing `started_at`/UUID deterministic sort.
2. Add `history --reverse`, inclusive RFC3339 `--since`, and inclusive RFC3339 `--until` arguments with UTC normalization and actionable parsing.
3. Apply session, type, and time filters before order and limit; preserve oldest-first defaults while making reverse limits select newest matches.
4. Retain existing history fields and human lines while adding order/bound/limit metadata to JSON.
5. Prove default compatibility, fixed-time boundary inclusion, reverse truncation, invalid-range safety, UUID prefixes, and shared latest selection.

### Cycle outcome

#### Implemented

- Full UUIDs and unambiguous UUID prefixes remain supported. The reserved case-insensitive `latest` selector now resolves the first session from the existing deterministic latest-first sort and is shared by show, resume, history, and context.
- History accepts `--reverse`, `--since <RFC3339>`, and `--until <RFC3339>`. Offset timestamps normalize to UTC, both bounds are inclusive, and malformed values fail during argument parsing with RFC3339 guidance.
- A since value after until fails with an actionable lifecycle error before state access or mutation.
- History applies session, event-type, and time filters first. It reverses the complete matching sequence only when requested, computes truncation against that ordered sequence, and then applies the limit. Existing default oldest-first and earliest-limit behavior is unchanged.
- Human event lines and the final `Truncated` line remain unchanged. JSON retains `events` and `truncated` and adds `order`, normalized `since`, normalized `until`, and `limit`.
- README command guidance documents the shared latest alias, inclusive time bounds, and order-before-limit behavior.
- No persistence field, storage schema, dependency, migration, or mutation behavior changed.

#### Architectural decisions

- `latest` is resolved centrally instead of separately in commands, preventing selector drift. Its tie-break remains the existing descending UUID after descending start time.
- Time navigation is based only on stored event timestamps and explicit inclusive comparisons. Relay does not infer relative recency or semantic relevance.
- Reverse ordering is a presentation/selection option over the matching append-only records; no event bytes or durable order are changed.

#### Verification

The focused regression passed:

```text
cargo test --test cli navigation_is_deterministic_across_history_and_shared_session_selectors -- --exact  PASS (1 test)
```

The test records two sessions and ten events, rewrites only top-level event timestamps to fixed increasing valid RFC3339 values, and confirms every other field plus newline-terminated JSONL remains unchanged. It covers default earliest selection, reverse newest-two selection, inclusive note boundaries, post-filter truncation, invalid-range byte preservation, additive JSON metadata, mixed-case latest across resume/show/context/history, and UUID-prefix compatibility.

`cargo build --quiet` passed. A direct isolated workflow created a completed session and a latest resumed session, then confirmed latest selection, default and reverse ordering, newest-two truncation, inclusive note bounds, and invalid-range rejection.

The first canonical Clippy run found one needless borrow in the new timestamp-preservation assertion. An isolated correction passed the copyable timestamp directly without changing coverage. Independent core review found the selector, bounds, ordering, truncation, help, and metadata contracts correct.

The complete canonical suite then passed in order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (58 tests)
```

#### Known limitations and remaining technical debt

- Navigation has no cursor or pagination token. Large histories still use an explicit numeric limit over deterministic order.
- Bounds are absolute RFC3339 instants; Relay does not parse relative phrases such as `2h ago`.
- `latest` means the newest started session, whether active or completed. Resume still applies its existing lifecycle rules after selection.
- Deterministic local search remains unimplemented.
- Relay still has no versioned Rust/Go orchestration protocol, external-process scheduler, or sandboxed Lua workflow language.
- This is not the final completion cycle, so the branch is committed locally but not pushed again and no draft pull request is created in this cycle.

#### Strongest next-cycle candidates

1. Add deterministic local search across sessions, event types, note categories, times, and plain substrings without embeddings, hosted services, or inferred relevance.
2. Define the first versioned Rust/Go orchestration protocol boundary before implementing any external-process scheduler or Lua workflow runtime.
3. Design the sandboxed Lua workflow surface against that versioned protocol without adding execution until validation and security boundaries are explicit.

## Cycle 6 — deterministic local event search

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `0b5f947` before this cycle. The local branch was four verified cycles ahead of `origin/feat/relay-daily-driver`.
- Relay could navigate stored sessions and time-bounded event history, but finding a recorded fact still required reading complete history or context output. It had no local query command.
- This cycle implements only deterministic, read-only search over stored event types and payload strings. It does not add an index, fuzzy or semantic ranking, source-tree scanning, provider access, persistence, orchestration, or Lua.

### Selected improvement and rationale

Add an inspectable `relay search` command so operators and agents can locate recorded goals, notes, checkpoint messages, handoff details, and event kinds with explicit session/type/category/time filters. Exact matching and stable traversal keep results reproducible and explainable without introducing external services or inferred relevance.

### Focused implementation plan

1. Add a required nonblank search query plus shared session selection, exact event type, note category, inclusive RFC3339 time, reverse-order, and numeric-limit arguments.
2. Match event type first, then recursively nested payload strings using lexicographically sorted object keys and recorded array order.
3. Return at most one first match per event with an unambiguous RFC6901 pointer, deterministic order, explicit truncation, safe human rows, and typed JSON events.
4. Keep search read-only, bound retained matches to the requested limit plus one truncation sentinel, and reject nested session-record symlinks during selector resolution.
5. Prove filtering, ordering, exact case sensitivity, traversal, privacy, limit behavior, read-only state, and invalid-input safety.

### Cycle outcome

#### Implemented

- Added `search <QUERY>` with `--session`, exact `--type`, `--category`, inclusive `--since`/`--until`, `--reverse`, and `--limit`. The query is a required, case-sensitive, nonblank plain substring.
- Search filters append-only events before matching. Note categories accept the existing typed categories and match only `note.recorded` payloads; session selectors retain full UUID, unambiguous prefix, and case-insensitive `latest` behavior.
- Each event produces at most one result. Event type has precedence; payload object keys are traversed lexicographically, arrays in recorded order, and nested fields use RFC6901 escaping under `/payload`.
- Default results preserve oldest-first event order. Reverse changes traversal to newest-first before the limit. Search retains at most `limit + 1` owned matches, uses the extra result only to determine truncation, and correctly handles a zero limit.
- Human rows contain timestamp, session, event type, match field, and matched text. All recorded string columns are project-root-redacted, collapsed to one line, and encode remaining control characters rather than emitting terminal controls. JSON retains raw typed events and matched text plus explicit query/filter/order/limit metadata.
- Invalid time ranges fail before state validation or reads. Successful and failed searches do not acquire the project lock or modify Relay state.
- Session-filtered search reuses its already loaded event snapshot while resolving stored sessions. Session enumeration now rejects nested `.relay/sessions/*.json` symlinks without following them.
- README command guidance and examples describe exact matching, stable first-match traversal, filter/order semantics, local-only behavior, and the limited session-record read used for selector resolution.
- No storage schema, dependency, migration, event representation, or existing command output changed.

#### Architectural decisions

- Search scans the canonical append-only JSONL event log rather than creating a second persisted index whose consistency would need recovery and migration.
- Matching is deliberately case-sensitive `str::contains`. Relay does not tokenize, normalize, score, fuzzy-match, embed, or infer relevance.
- Results preserve complete typed events in JSON while match metadata identifies the exact first field and raw matched string. Human-only sanitization never rewrites stored data.
- The existing session selector remains authoritative. A preloaded-event helper avoids rescanning history during selector resolution while retaining legacy session reconstruction compatibility.

#### Verification

The focused regression passed:

```text
cargo test --test cli search_is_deterministic_filterable_and_read_only -- --exact  PASS (1 test)
```

The regression creates two completed/continued sessions, fixes session and event times deterministically, and covers exact output schemas, repeated-byte determinism, event-type precedence, sorted object and recorded array traversal, one result per event, full/latest session selection, exact type and category filters, inclusive bounds, reverse-before-limit behavior, zero-limit truncation, case sensitivity, no matches, path/newline/control sanitization, invalid-range byte preservation, lock immutability, artifact/session/event immutability, and refusal to follow a nested session-record symlink.

`cargo build --quiet` passed. A direct isolated workflow recorded a note and checkpoint, then confirmed inclusive bounds, latest/category filtering, reverse truncation, case sensitivity, path/control sanitization, invalid-range rejection, and byte-preserved event state.

Parallel implementation used isolated CLI, engine, and black-box coverage writers. Independent core, test-strength, and security reviewers found and drove corrections for dynamic-column/control sanitization, redact-before-normalize ordering, bounded match retention, duplicate event-log reads, nested session symlinks, lock-file assertions, deterministic latest fixtures, exact type discrimination, and recorded array-order coverage. A mistaken intermediate test expectation briefly allowed two hits for one event; the focused test failed and the assertion was corrected to preserve the one-hit contract. A later selector optimization initially returned a session reference instead of its UUID; compilation failed and the typed ID mapping was restored before verification.

The complete canonical suite then passed in order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (63 tests)
```

#### Known limitations and remaining technical debt

- Search is a linear scan of the event log and loads the typed log before matching. It bounds retained matches but has no cursor or persisted index.
- Matching covers event types and string-valued payload leaves only. Numeric, Boolean, null, field-name, source-file, and unrecorded content are intentionally excluded.
- Matching is exact and case-sensitive; Relay has no case-folded, regular-expression, fuzzy, semantic, or ranked mode.
- A supplied session selector validates recorded sessions and can therefore reject an unrelated malformed session record, consistent with existing shared selector behavior.
- Relay still has no versioned Rust/Go orchestration protocol, external-process scheduler, or sandboxed Lua workflow language.
- This is not the final completion cycle, so the branch is committed locally but not pushed again and no draft pull request is created in this cycle.

#### Strongest next-cycle candidates

1. Define the first versioned Rust/Go orchestration protocol boundary before implementing any external-process scheduler or Lua workflow runtime.
2. Implement the minimal Rust-side orchestration request/status boundary with explicit schema validation and no implicit process launch.
3. Design the sandboxed Lua workflow surface against the accepted protocol before adding execution.

## Cycle 7 — versioned orchestration process boundary

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `349b2bb` before this cycle. The local branch was five verified cycles ahead of `origin/feat/relay-daily-driver`.
- Relay already emitted output-schema-v1 JSON per CLI invocation, but no machine-readable contract identified the safe orchestration operation subset, framing, independent protocol/state/output versions, or Rust/Go/Lua ownership boundary.
- The workstation had no `go` executable and the repository had no Go module or files. This cycle therefore defines and fully verifies the prerequisite Rust-side process contract without adding unverifiable Go code, speculative CI, a scheduler, run files, or Lua.

### Selected improvement and rationale

Define orchestration protocol v1 as an inspectable capability manifest over Relay's existing direct-argv JSON CLI. A future Go process supervisor can negotiate exact versions, operations, streams, and ownership before invoking Rust state operations, while the manifest makes the current non-implementation of Go and Lua explicit.

### Focused implementation plan

1. Add independent output-schema and orchestration-protocol constants plus strict typed manifest structures.
2. Add a project-independent, read-only `relay protocol` command that emits the manifest through the existing human/JSON output path.
3. Define exact direct-argv transport, application-response framing, six allowed operations, stdin behavior, and Rust/Go/Lua ownership.
4. Pin a canonical v1 data fixture and prove exact runtime agreement without coupling future crate-version bumps to protocol-version changes.
5. Document security, compatibility, evolution, and current non-implementation boundaries without adding process execution.

### Cycle outcome

#### Implemented

- Added `relay protocol`, a unit command that reports compiled capabilities without querying the current directory, discovering a project, reading or validating `.relay`, invoking Git, acquiring a lock, or creating files.
- Added public, independent `OUTPUT_SCHEMA_VERSION` and `ORCHESTRATION_PROTOCOL_VERSION` constants. Existing success and error envelopes now use the output constant instead of duplicate literals.
- Added strict serde manifest types for transport, Rust-owned state, future component ownership, operations, and stdin behavior. Object types reject unknown fields.
- Protocol v1 declares direct argv, no shell, one process per operation, application-response JSON framing, stdout success, stderr application errors, output schema v1, state schema v1, and a fixed `--json --scope current` prefix.
- The allowed ordered subset is status, context, note via UTF-8 stdin, checkpoint, handoff, and finish. Each manifest entry declares an argv template and whether it mutates Rust-owned state.
- Go is declared the unimplemented future orchestration owner; Lua is declared the unimplemented future workflow owner. Relay provides no agent/workflow process launcher. Existing Git-aware commands may still invoke `git`, and interactive note editing remains outside protocol v1.
- Added `protocol/manifest-v1.json` as the canonical v1 data fixture. It pins the release that introduced the protocol while a running binary reports its current crate version.
- Added `docs/orchestration-protocol.md` with negotiation, framing, allowed operations, direct-argv rules, stream/exit behavior, placeholders, ownership, security requirements, version evolution, compatibility, and explicit current limitations.
- README now lists and links the protocol and accurately scopes JSON envelope guarantees to parsed operations that reach application handling.
- No dependency, Go file/module, Lua file/runtime, CI workflow, storage schema, persisted record, migration, or existing operation behavior changed.

#### Architectural decisions

- Protocol v1 formalizes existing one-shot CLI processes instead of introducing FFI, a daemon, or a second generic command-dispatch protocol.
- The Rust command special-cases capability negotiation before current-directory and project discovery because the manifest is compiled metadata and must remain safe even beside a symlinked `.relay`.
- Protocol, output, state, and Relay package versions evolve independently. Contract changes require a new fixture/version; package releases alone do not.
- `one_document` describes successfully parsed application-response framing. Clap parse failures and failures before envelope-producing startup remain nonzero protocol failures but may be plain text.
- Argument-template placeholders are documentation only. A future supervisor must validate and substitute separate argv elements and must never invoke a shell.

#### Verification

Focused protocol coverage passed:

```text
cargo test --test cli protocol_manifest_is_versioned_project_independent_and_stable -- --exact  PASS (1 test)
cargo test cli::tests::protocol -- --nocapture                                      PASS (3 tests)
```

The black-box regression verifies exact fixture data and envelope keys, strict unknown-field rejection, historical fixture/runtime version separation, exact human output, repeatable bytes, global options on either side, forced repository-discovery bypass, no `.relay` creation, and success without following or mutating a symlinked `.relay` target. Parser tests verify the argument-free command, name, global options, and unexpected-argument rejection.

`cargo build --quiet` passed. A direct isolated negotiation used repository scope with a Git ceiling and a symlinked `.relay`, then confirmed exact fixture output, ordered operations, unimplemented Go/Lua components, no target mutation, and the scoped no-agent/workflow-launch guarantee.

Parallel isolated writers implemented typed models/CLI, runtime/main dispatch, and fixtures/tests/documentation. Independent core review found the implementation contract-correct. Security review found four contract defects: an overbroad no-subprocess claim despite Git/editor subprocesses, package-version coupling in fixture equality, an overbroad JSON-error guarantee for parse/startup failures, and a repository-bypass test without a forced-failure precondition. All were corrected and focused verification rerun.

The complete canonical suite then passed in order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (67 tests)
```

#### Known limitations and remaining technical debt

- This is capability negotiation only. There is no Go protocol client, scheduler, process supervisor, run-file schema, status/log stream, cancellation, timeout, retry, or resume implementation.
- There is no Lua parser, validator, graph preview, sandbox, workflow runtime, or policy API.
- Protocol v1 exposes only six operations and one-shot application responses. It has no long-lived stream, multiplexing, or orchestration request file.
- Parse and pre-envelope startup failures are nonzero protocol failures but are not guaranteed to be JSON envelopes.
- The current workstation has no Go toolchain, so no Go code or Go verification was attempted or claimed.
- This is not the final completion cycle, so the branch is committed locally but not pushed again and no draft pull request is created in this cycle.

#### Strongest next-cycle candidates

1. With a verified Go toolchain available, implement the strict protocol-v1 decoder, a minimal versioned Go-owned run-file schema, and canonical Go CI gates before scheduling.
2. Add bounded dependency-aware Go scheduling, direct-argv supervision, cancellation/timeouts, explicit retries, filtered environments, and bounded logs against that schema.
3. Define and validate the sandboxed Lua workflow surface only after the Go run-state and execution-safety boundaries are executable and tested.

## Cycle 8 — complete JSON error envelopes

### Repository baseline

- The working tree was clean on `feat/relay-daily-driver` at `2c7ccb6` before this cycle. The local branch was six verified cycles ahead of `origin/feat/relay-daily-driver`.
- Protocol v1 required the `--json --scope current` prefix and defined one-document application responses, but Clap argument failures still escaped as plain text even when `--json` was present. A future process supervisor would therefore need two incompatible error decoders at the same process boundary.
- Failures after successful parsing already used typed application error envelopes except for failure to read the invocation directory. Clap's own help/version output and successful completion-script generation remained intentionally shell-facing text.
- This is the eighth and final permitted development cycle. The workstation still has no `go` or `gofmt` executable, and the repository still has no Go module, Go source, Lua source, or embedded Lua runtime.

### Selected improvement and rationale

Make JSON error framing begin at argument parsing rather than application dispatch. This closes the remaining Rust-side process-boundary ambiguity without changing protocol v1 data, command semantics, project storage, or the unimplemented Go/Lua ownership boundary.

### Focused implementation plan

1. Collect OS-native argv and detect an exact global `--json` only before a literal `--`.
2. Replace infallible Clap parsing with explicit error handling that preserves Clap's streams and exit codes.
3. Emit output-schema-v1 `argument_error` envelopes for JSON-mode parser failures and `current_directory` envelopes for parsed invocation-directory failures.
4. Preserve successful help/version and completion-script output as shell text, and preserve ordinary Clap diagnostics when JSON was not requested.
5. Prove exact envelope shape, placement, delimiter semantics, no state mutation, and closed-stream exit behavior in black-box coverage.
6. Update README and protocol framing documentation without changing the manifest or claiming orchestration implementation.

### Cycle outcome

#### Implemented

- `main` now collects `args_os` and uses `Cli::try_parse_from`, preserving OS-native argument values, including non-UTF-8 data, rather than converting argv lossily.
- An exact `--json` token before a literal `--` requests parser-stage JSON errors. Clap failures now emit one compact output-schema-v1 error envelope on stderr with `command: "cli"`, `error.code: "argument_error"`, a normalized actionable Clap message, no stdout, and Clap's original nonzero exit code.
- Without the JSON request, parser failures retain Clap's normal text and stream. Successful help/version requests retain normal Clap text even with `--json`; successful completion-script generation remains shell text.
- After parsing, inability to read the current directory now uses `error.code: "current_directory"` and the parsed command name through the same output-mode-aware error renderer.
- Diagnostic writes are fallible and ignored after the intended status is selected. Closed stderr can no longer turn a parser or application failure into a panic, and closed stdout cannot turn Clap help into a panic status.
- Focused black-box coverage checks JSON placement before and after a subcommand, invalid typed values, unknown commands, missing arguments, exact envelope keys, streams, one trailing newline, exit code 2, normalized messages, human fallback, help behavior, literal-`--` handling, absence of `.relay` mutation, and Unix closed-stream behavior.
- README and `docs/orchestration-protocol.md` now distinguish parser failures, parsed startup failures, application responses, successful help/version/completion output, and OS process-launch failure.
- No protocol/output/state schema version, manifest fixture, operation, dependency, storage record, migration, Go/Lua artifact, or CI workflow changed.

#### Architectural decisions

- Parser errors use the stable synthetic command name `cli` because malformed argv may not identify any valid Relay command. Parsed startup and application errors continue to use the actual command name.
- JSON intent is detected from exact OS-native argv only before `--`; a positional value spelled `--json` cannot silently change framing.
- Help and version are successful shell inspection requests rather than protocol application responses. Failure to start the Relay executable is outside Relay's process and cannot produce a Relay envelope.
- Error output deliberately ignores broken-stream writes after selecting the semantic exit code, matching Clap's prior non-panicking behavior and avoiding panic-status substitution.

#### Verification

Focused and direct verification passed:

```text
cargo test --test cli parser_failures_honor_json_mode_without_mutating_project_state -- --exact  PASS (1 test)
direct malformed-invocation smoke                                                        PASS
LSP diagnostics: src/main.rs                                                            OK
LSP diagnostics: tests/cli.rs                                                           OK
git diff --check                                                                         PASS
```

The direct smoke covered JSON before and after the command, unknown-command errors, human fallback, JSON-mode help, literal-`--` handling, and absence of project-state mutation.

Parallel isolated writers implemented runtime handling, black-box coverage, and documentation. Independent core and contract reviews found two concrete defects: diagnostic write failures could replace Clap's intended status with a panic, and README overgeneralized completion output despite malformed completion invocations using parser envelopes. Both were corrected and focused verification rerun.

The complete available Rust suite passed in order:

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                   PASS (68 tests)
```

#### Stop condition and remaining milestone work

- The complete orchestration milestone is not successful: no Go protocol client, run-state model, scheduler, process supervisor, status/log stream, cancellation, timeout, retry, resume, Lua parser, workflow validator, graph preview, sandbox, workflow runtime, or orchestration end-to-end test exists.
- `go version`, `go test ./...`, and `go vet ./...` fail with exit 127 because `go` is unavailable. `gofmt -l .` likewise fails with exit 127 because `gofmt` is unavailable. There are no Go or Lua files to verify.
- The mandatory Rust/Go/Lua verification contract therefore cannot pass. Pushing this incomplete branch or creating the required draft pull request would misrepresent the milestone, so neither action is performed.
- The eight-cycle cap is reached with this coherent Rust cycle committed locally. Per the milestone stop conditions, no ninth cycle may begin in this run.
- Schema-v1 projects remain compatible: this cycle changes only process-error rendering before project access and does not read, write, reinterpret, or migrate durable state.

#### Recommended continuation

1. In a new authorized run on a workstation with a verified Go toolchain, implement the strict protocol-v1 Go decoder, minimal versioned Go-owned run-state schema, and canonical Go CI gates.
2. Add bounded dependency-aware direct-argv supervision with filtered environments, bounded secret-safe logs, cancellation/timeouts, explicit retries, child cleanup, and deterministic status/resume.
3. Only after that execution boundary is verified, add the embedded sandboxed Lua workflow validator/runtime and the required Rust/Go/Lua end-to-end test.

## Current architecture note — reusable workflows

The historical cycle log above predates the reusable-workflows milestone. Relay
now also supports version-controlled JSON definitions in
relay-workflows/, typed scalar inputs, explicit argv references, and
workflow list/show/validate/run commands. Rust resolves each definition into
the existing plan-v3 scheduler; no Lua or workflow runtime was added.

Aggregate run-state schema v5 optionally stores workflow name, workflow schema
version, canonical definition SHA-256, resolved inputs, and the sanitized
resolved plan. Workflow resume uses that persisted plan after live definition
changes, deletion, or malformed contents. Existing direct plan runs and older
aggregate state schemas remain supported through explicit migration.
## Cycle 9 — agent-native handoffs

This cycle extends Relay continuity with schema-versioned handoff snapshots. Relay remains a synchronous local Rust coordinator and does not become an agent runtime.

Structured records use schema version 2 and UUID identity. Each record captures creation time, target agent, optional explicit source agent, session snapshot, goal, completed/decision/blocker/next-action entries, entry provenance, unavailable markers, bounded Git state, checkpoint/note references, and optional run/workflow/output references. Explicit statements are labeled explicit; imported note facts are labeled recorded_note; Git and run/output data remain separate derived/trusted sections.

Records append to .relay/handoffs.jsonl. One generated Markdown view remains under .relay/handoffs/ for human use. Renderer reads structured data; Markdown is derivative. Legacy Markdown-only artifacts remain readable without read-time migration. Unsupported versions, malformed records, duplicate IDs, oversized state, and unsafe paths fail without rewriting source bytes.

Run references use canonical project-local aggregate state. A lock-free nofollow reader allows a live run status snapshot without competing with its coordinator lock. Existing Rust aggregate validation remains authoritative. Only successful task outputs with matching attempt, attempt-run, hash, size, type, path, and task provenance are exposed. File contents stay outside handoff records; bounded structured JSON values may be copied because aggregate output semantics already persist them. Workflow name/schema/hash/inputs are copied as a historical snapshot.

Agent B can consume relay context --handoff latest or an exact ID. Human output is concise, deterministic, control-character-safe, and bounded. JSON returns the handoff plus session, Git, notes, checkpoints, runs, outputs, unavailable sections, and truncation metadata in the existing output-schema-v1 envelope. Later Git edits, notes, workflow edits, run transitions, and artifact changes do not mutate an existing record; a new handoff captures new state.

Security review covers project-local run paths, traversal, symlink and hardlink rejection, malformed legacy input, duplicate IDs, redacted argv preservation, bounded fields, JSON string encoding, and untrusted persisted text. Free-text fields remain persisted data, not a secret store or executable instruction source.

Deferred: provider APIs, prompt generation, agent spawning, autonomous loops, daemons, networking, remote handoffs, authentication, uploads, semantic ranking, embeddings, UI/TUI, workflow conditions, scripting, and secret management.
