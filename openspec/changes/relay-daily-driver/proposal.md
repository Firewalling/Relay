## Why

Relay’s MVP safely records one local session, but routine multi-agent use still risks concurrent corruption, lacks recovery guidance, and requires humans to scrape prose or inspect raw files for history and context. This milestone makes the existing file-based architecture dependable and composable enough for daily coordination among Hermes, OMP, Codex, and other terminal agents without adding a daemon, cloud service, database, or model integration.

## What Changes

- Add fail-fast advisory project locking for every state mutation, with abandoned-lock recovery and actionable ownership details.
- Add `relay doctor` diagnostics for corrupt, unsupported, incomplete, inconsistent, missing, and orphaned state; keep repair explicit, conservative, and limited to operations that do not discard ambiguous data.
- Add a versioned global `--json` output envelope for agent-facing commands while preserving human-readable defaults.
- Preserve immutable per-session records and add `list`, `show`, `history`, and `resume` for historical navigation.
- Add structured categorized notes from an argument, stdin, or `$EDITOR`, and include relevant notes in handoffs, summaries, and generated context.
- Add upward `.relay/` discovery for existing projects while keeping `start` explicitly scoped; distinguish working-directory, Relay project, and Git repository scopes to avoid unrelated monorepo changes.
- Add `relay context` with Markdown/JSON formats and deterministic filters/limits suitable for agent token budgets.
- Add Bash, Zsh, and Fish completions, CI gates, installation guidance, release-build instructions, and integration examples.
- Preserve schema-v1 MVP state through explicit compatibility/import behavior; never silently rewrite, discard, or guess corrupt or unsupported records.

## Capabilities

### New Capabilities

- `cross-process-safety`: Project-level mutation locking, lock diagnostics, and abandoned-lock recovery.
- `recovery-diagnostics`: Read-only state validation plus explicit conservative repair behavior.
- `machine-readable-output`: Stable versioned JSON envelopes for lifecycle, history, diagnostics, and context integrations.
- `session-history`: Immutable inspectable session records, historical lookup, history display, and controlled resume.
- `structured-notes`: Categorized note capture from argument, stdin, or editor and reuse in generated artifacts.
- `project-discovery`: Deliberate Relay-root discovery and bounded Git reporting for nested and monorepo workflows.
- `agent-context`: Concise bounded Markdown or JSON context for Hermes, OMP, Codex, and generic terminal agents.
- `distribution-usability`: Shell completions, automated quality gates, and practical install/release documentation.

### Modified Capabilities

- `session-lifecycle`: Lifecycle commands preserve immutable history, support resume, use locking, honor discovered project scope, and expose JSON output.
- `agent-handoff`: Handoffs incorporate relevant structured notes and support stable JSON output without exposing absolute private project paths.

## Impact

- Extends the existing Rust CLI, application, typed models, storage, and read-only Git modules rather than replacing them.
- Adds small Rust dependencies only for cross-platform advisory locking and shell-completion generation where the standard library is insufficient.
- Evolves the inspectable `.relay/` layout with immutable session records and an operation journal/marker sufficient for diagnostics and conservative recovery; SQLite is not introduced.
- Adds integration and fault-injection coverage, a GitHub Actions workflow, generated completions, and expanded README documentation.
- Existing human-readable commands and schema-v1 MVP data remain usable where valid; unsupported or ambiguous state fails with diagnostics instead of implicit migration.
