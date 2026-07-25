## MODIFIED Requirements

### Requirement: Generate a targeted handoff
Relay SHALL generate a concise Markdown handoff for a required target-agent name and SHALL include Goal, Completed work, Important decisions, Failures or blocked work, Current scoped Git state, Changed files, Relevant checkpoints, Suggested next action, and Target agent sections. It SHALL combine explicit handoff options with relevant categorized notes recorded for the active session, persist an inspectable artifact, append `handoff.generated`, and support the stable JSON envelope.

#### Scenario: Generate handoff with explicit notes
- **WHEN** a user runs `relay handoff omp` with completed-work, decision, failure, or next-action options during an active session
- **THEN** Relay renders those explicit notes with recorded session, note, checkpoint, and scoped Git context; prints or returns the handoff; saves it under `.relay/handoffs/`; and appends `handoff.generated`

#### Scenario: Generate handoff from recorded notes
- **WHEN** categorized notes exist and one or more explicit handoff categories are omitted
- **THEN** Relay includes applicable recorded notes in event order and labels only truly absent categories as not recorded

#### Scenario: Generate handoff without notes
- **WHEN** no explicit or recorded note exists for a category
- **THEN** Relay labels that category as not recorded rather than inferring or fabricating content

### Requirement: Remember agent context
After a successful locked handoff, Relay SHALL persist the target as the most recently used agent in both current and per-session records so that subsequent status, handoff, context, and final-summary output can identify it.

#### Scenario: Status after handoff
- **WHEN** a handoff to `omp` succeeds and the user runs `relay status`
- **THEN** Relay reports `omp` as the most recently used agent

### Requirement: Avoid leaking project root paths
Generated handoffs, final summaries, agent context, and JSON output MUST NOT include the absolute project-root path. Changed files SHALL be represented using Relay-project-relative escaped paths when Git provides them.

#### Scenario: Handoff content privacy
- **WHEN** Relay writes or prints a handoff inside a project under a private home-directory path
- **THEN** the handoff includes relevant Git and changed-file context without including the absolute project-root path
