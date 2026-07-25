## ADDED Requirements

### Requirement: Distinguish invocation, Relay, and Git roots
Relay SHALL model the invocation directory, selected Relay project root, and containing Git repository root as distinct scopes and MUST keep generated artifacts and JSON free of absolute private project paths.

#### Scenario: Relay subproject in monorepo
- **WHEN** `.relay/` belongs to a subdirectory of a larger Git repository
- **THEN** Relay uses the subdirectory as its project root while obtaining branch and commit metadata read-only from the containing repository

### Requirement: Discover the nearest Relay project deliberately
Default auto discovery for existing state SHALL walk upward to the nearest `.relay/` and SHALL NOT cross the current Git worktree root. `--scope current` SHALL disable upward discovery, and `--scope repository` SHALL select the Git root or fail outside Git.

#### Scenario: Command from nested directory
- **WHEN** a user runs status from a nested directory below a project containing `.relay/`
- **THEN** Relay selects the nearest ancestor project and reports the same session as at that project root

#### Scenario: Nested Relay projects
- **WHEN** both a subproject and an ancestor contain `.relay/`
- **THEN** auto discovery selects the nearest subproject

#### Scenario: Current scope opt-out
- **WHEN** a nested-directory user supplies `--scope current`
- **THEN** Relay does not mutate or report an ancestor Relay project

### Requirement: Create new state at an explicit scope
In auto mode, `start` SHALL reuse a discovered Relay project; when none is discovered it SHALL create `.relay/` in the invocation directory. Repository scope SHALL create or use state at the Git worktree root.

#### Scenario: Start subdirectory session
- **WHEN** a user starts in a monorepo subdirectory with no discoverable Relay state and default scope
- **THEN** Relay creates subproject-local state rather than silently selecting the repository root

### Requirement: Bound Git changes to Relay project scope
Git cleanliness and changed-file capture SHALL use a pathspec bounded to the Relay project subtree. A project at Git root sees the full worktree; a subproject MUST NOT report unrelated sibling changes.

#### Scenario: Unrelated sibling change
- **WHEN** a monorepo sibling outside the selected Relay project is dirty
- **THEN** Relay does not list that path or mark the Relay project dirty solely because of the sibling change

### Requirement: Represent non-UTF-8 Git paths deterministically
On platforms that support non-UTF-8 paths, Relay SHALL capture them without lossy replacement or failure by rendering invalid bytes with deterministic escapes and identifying that encoding in structured output.

#### Scenario: Non-UTF-8 changed path
- **WHEN** Git reports a changed path containing invalid UTF-8 bytes within the Relay scope
- **THEN** Relay succeeds, emits an escaped unambiguous relative path, and performs no Git or worktree mutation
