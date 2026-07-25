## ADDED Requirements

### Requirement: Generate a targeted handoff
Relay SHALL generate a concise Markdown handoff for a required target-agent name and SHALL include Goal, Completed work, Important decisions, Failures or blocked work, Current Git state, Changed files, Relevant checkpoints, Suggested next action, and Target agent sections.

#### Scenario: Generate handoff with explicit notes
- **WHEN** a user runs `relay handoff omp` with completed-work, decision, failure, or next-action options during an active session
- **THEN** Relay renders those explicit notes with current session, checkpoint, and Git context; prints the handoff; saves it under `.relay/handoffs/`; and appends `handoff.generated`

#### Scenario: Generate handoff without notes
- **WHEN** a user omits one or more optional note categories
- **THEN** Relay labels each missing category as not recorded rather than inferring or fabricating content

### Requirement: Remember agent context
After a successful handoff, Relay SHALL persist the target as the most recently used agent so that subsequent status, handoff, and final-summary output can identify it.

#### Scenario: Status after handoff
- **WHEN** a handoff to `omp` succeeds and the user runs `relay status`
- **THEN** Relay reports `omp` as the most recently used agent

### Requirement: Avoid leaking project root paths
Generated handoffs and final summaries MUST NOT include the absolute project-root path. Changed files SHALL be represented using Git-relative paths when Git provides them.

#### Scenario: Handoff content privacy
- **WHEN** Relay writes or prints a handoff inside a project under a private home-directory path
- **THEN** the handoff includes relevant Git and changed-file context without including the absolute project-root path
