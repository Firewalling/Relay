## 1. Compatibility foundation and project scope

- [x] 1.1 Add CLI/global output and scope types without changing existing human command defaults
- [x] 1.2 Add Relay project discovery for auto, current, and repository scopes with nested-directory tests
- [x] 1.3 Bound Git status to the selected Relay project subtree and add monorepo sibling-change tests
- [x] 1.4 Render non-UTF-8 Git path bytes deterministically on Unix and preserve existing UTF-8 behavior
- [x] 1.5 Add regression assertions that all lifecycle commands preserve Git HEAD, index, worktree contents, and stash state

## 2. Cross-process mutation safety

- [x] 2.1 Add a small cross-platform advisory-lock dependency and typed lock owner metadata
- [x] 2.2 Implement a non-blocking project lock guard with actionable busy errors and automatic drop release
- [x] 2.3 Acquire the guard around every mutating command before state validation or mutation
- [x] 2.4 Add two-process contention, error-release, normal-release, and abandoned-process recovery tests

## 3. Durable multi-file operation tracking

- [x] 3.1 Add typed operation-marker models and atomic create/advance/complete storage helpers
- [x] 3.2 Wrap checkpoint, handoff, finish, start/resume, note, and history materialization mutations with operation markers where multiple durable effects occur
- [x] 3.3 Add deterministic fault-injection hooks for interruption after artifact, event, and session-record steps
- [x] 3.4 Add tests proving partial operations preserve durable data and remain diagnosable

## 4. Session records and history

- [x] 4.1 Extend the session model compatibly with optional continuation lineage and add per-session paths
- [x] 4.2 Materialize valid MVP current state into a per-session record only during a locked mutation
- [x] 4.3 Write and update current plus per-session records while preserving all earlier sessions
- [x] 4.4 Implement exact/unambiguous session selectors and `relay list`
- [x] 4.5 Implement `relay show <session>` and filtered/limited `relay history`
- [x] 4.6 Implement `relay resume <session>` as a new active continuation of a completed source
- [x] 4.7 Add multi-session lookup, valid resume, invalid resume, legacy compatibility, and ambiguity tests

## 5. Versioned machine-readable output

- [x] 5.1 Add typed version-1 success and error envelopes separated from storage schema versions
- [x] 5.2 Return typed command results while retaining compatible human renderers
- [x] 5.3 Add global `--json` output for start, status, checkpoint, handoff, finish, and doctor
- [x] 5.4 Extend consistent JSON output to list, show, history, resume, note, and context
- [x] 5.5 Add black-box tests for one-document output, schema version, typed fields, relative paths, and structured failures

## 6. Doctor diagnostics and conservative repair

- [x] 6.1 Define stable diagnostic codes, severities, affected relative paths, and health summary models
- [x] 6.2 Diagnose malformed/partial JSONL, malformed state/artifacts, unsupported schemas, and invalid lifecycle combinations
- [x] 6.3 Diagnose missing references, orphaned artifacts, current/history disagreement, and incomplete operation markers
- [x] 6.4 Implement `relay doctor` as byte-preserving diagnostics by default
- [x] 6.5 Implement locked `doctor --repair` only for fully completed stale marker cleanup and refuse ambiguous/destructive repairs
- [x] 6.6 Add damaged-state detection, no-default-mutation, safe-cleanup, and unsafe-repair-refusal tests

## 7. Structured notes and generated summaries

- [x] 7.1 Add note categories and CLI validation for positional, stdin, and editor sources
- [x] 7.2 Record bounded nonblank notes as `note.recorded` events under the mutation lock
- [x] 7.3 Include categorized notes truthfully in handoffs and final summaries without inventing absent content
- [x] 7.4 Add argument, piped stdin, editor, conflict, blank, and note-inclusion tests

## 8. Agent context surface

- [x] 8.1 Implement typed context selection from supported session events, notes, checkpoints, lineage, and scoped Git state
- [x] 8.2 Render concise Markdown and JSON context using only recorded facts and relative paths
- [x] 8.3 Add deterministic session/category/kind/count filters and item-boundary character limiting with truncation metadata
- [x] 8.4 Add privacy, filter, deterministic ordering, and small-budget tests

## 9. Distribution and documentation

- [x] 9.1 Add `clap_complete` and `relay completions` for Bash, Zsh, and Fish with smoke tests
- [x] 9.2 Add GitHub Actions formatting, strict Clippy, and all-test gates
- [x] 9.3 Update README installation, release build, completion setup, storage/migration, scope, doctor, JSON, history, note, and context documentation
- [x] 9.4 Add concrete Hermes, `hermes-omp`, Codex, and generic terminal integration examples

## 10. Release verification

- [x] 10.1 Run `cargo fmt --check` with a matching Rust formatter
- [x] 10.2 Run strict `cargo clippy --all-targets --all-features -- -D warnings`
- [x] 10.3 Run all unit and integration tests including compatibility and Git immutability coverage
- [x] 10.4 Run strict OpenSpec validation for `relay-daily-driver`
- [x] 10.5 Exercise and document a real end-to-end workflow in a temporary Git repository
- [x] 10.6 Exercise and document a real end-to-end workflow in a temporary non-Git directory
