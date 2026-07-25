## ADDED Requirements

### Requirement: Record categorized notes as events
`relay note` SHALL record nonblank text in an active session as a `note.recorded` event with one category from `progress`, `decision`, `blocker`, `observation`, or `next_action`. Relay MUST preserve the supplied text and MUST NOT infer facts beyond it.

#### Scenario: Positional note
- **WHEN** a user runs `relay note decision "Keep JSONL"`
- **THEN** Relay appends a structured decision note containing exactly the supplied nonblank text

#### Scenario: Note from stdin
- **WHEN** a user selects stdin or pipes note text to Relay
- **THEN** Relay reads the complete bounded input, rejects blank content, and records one structured note

#### Scenario: Note from editor
- **WHEN** a user requests editor input and the configured editor exits successfully with nonblank text
- **THEN** Relay records the edited text and removes its temporary editor file

### Requirement: Enforce one explicit input source
Relay SHALL accept exactly one of positional text, stdin, or editor input and SHALL return actionable errors for conflicting sources, unavailable interactive input, editor failure, blank text, or oversized input.

#### Scenario: Conflicting input sources
- **WHEN** positional text and `--stdin` or `--editor` are supplied together
- **THEN** Relay rejects the command before appending an event

### Requirement: Reuse relevant notes in generated context
Handoffs, final summaries, and `relay context` SHALL include relevant current-session notes in event order, map categories to truthful sections, and label missing categories as not recorded rather than inventing content.

#### Scenario: Notes included in handoff
- **WHEN** progress, decision, blocker, and next-action notes exist before a handoff
- **THEN** the generated handoff includes those recorded notes in the corresponding sections in addition to explicit handoff options
