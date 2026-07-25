## ADDED Requirements

### Requirement: Emit concise recorded agent context
`relay context` SHALL emit Markdown by default or structured JSON under global `--json`, using only recorded session, note, checkpoint, handoff, lifecycle, and bounded Git facts. It MUST NOT call a model provider, inspect chat transcripts, or invent completed work, decisions, failures, or next actions.

#### Scenario: Markdown context for another agent
- **WHEN** a user requests context for the current session
- **THEN** Relay prints concise Markdown with the goal, lifecycle, relevant notes, recent checkpoints, current scoped Git state, lineage, and explicit truncation status

#### Scenario: JSON context
- **WHEN** a user runs `relay --json context`
- **THEN** Relay returns the same categories as typed arrays and objects in the versioned JSON envelope

### Requirement: Bound and filter context deterministically
Context SHALL support selecting a session, note categories, event kinds, checkpoint/note/event counts, and a maximum character budget. Relay SHALL order items deterministically, truncate only at item boundaries, and report omitted counts or truncation.

#### Scenario: Small context budget
- **WHEN** recorded context exceeds the requested character budget
- **THEN** Relay emits a valid bounded document, identifies truncation, and does not cut JSON or Markdown items into invalid fragments

#### Scenario: Category filter
- **WHEN** a user requests only decisions and next actions
- **THEN** Relay excludes other note categories while preserving the selected notes’ event order

### Requirement: Support terminal-agent composition
Documentation SHALL provide copy-and-pipe examples for Hermes, `hermes-omp`, Codex CLI, and a generic command without requiring provider APIs.

#### Scenario: Integration examples
- **WHEN** a user reads the installation and usage documentation
- **THEN** the user can identify commands that generate bounded Relay context for each named terminal agent
