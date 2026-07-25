## ADDED Requirements

### Requirement: Emit a stable versioned JSON envelope
Global `--json` SHALL cause a supported command to emit exactly one JSON document with `output_schema_version`, `ok`, `command`, and a command-specific `data` object. Output schema version 1 field names and enum values SHALL remain stable and snake_case; additive fields are permitted.

#### Scenario: JSON lifecycle success
- **WHEN** a user runs `start`, `status`, `checkpoint`, `handoff`, or `finish` with global `--json`
- **THEN** stdout contains one valid version-1 success envelope with structured IDs, timestamps, lifecycle state, Git availability, and relative artifact paths applicable to the command

#### Scenario: JSON doctor success
- **WHEN** a user runs `doctor --json`
- **THEN** stdout contains one version-1 envelope whose data includes health, diagnostic counts, and an array of structured diagnostics

### Requirement: Keep machine and human output separate
Human-readable output SHALL remain the default. JSON mode MUST NOT mix headings, progress prose, saved-path prose, or Markdown outside JSON syntax on stdout.

#### Scenario: Default output compatibility
- **WHEN** a user runs an existing MVP command without `--json`
- **THEN** Relay produces human-readable output compatible with the MVP behavior

#### Scenario: JSON handoff content
- **WHEN** a user generates a handoff with `--json`
- **THEN** the Markdown is a string field within structured data and the saved path is project-relative

### Requirement: Emit structured JSON failures
When `--json` is active and command execution fails after parsing, Relay SHALL emit one versioned failure envelope on stderr with a stable error code and message and SHALL return a nonzero exit status.

#### Scenario: Lifecycle failure in JSON mode
- **WHEN** `finish --json` is run without an active session
- **THEN** stderr contains one valid failure envelope and stdout contains no human error prose
