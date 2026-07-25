## Why

Terminal-based AI work loses essential context when work moves between agents or sessions, leaving goals, Git state, decisions, and next actions trapped in transcripts. Relay provides a small, local-first coordination record that makes active work understandable, transferable, recoverable, and inspectable without replacing existing agents or shells.

## What Changes

- Add a Rust `relay` CLI with `start`, `status`, `checkpoint`, `handoff`, and `finish` commands.
- Store the active/completed session in an inspectable `.relay/` directory using JSON, JSON Lines, and Markdown.
- Capture Git repository, branch, commit, cleanliness, and changed-file context when available while remaining useful outside Git.
- Generate concise persisted handoffs and final summaries from explicit session information and recorded checkpoints.
- Use safe state-file replacement and surface actionable errors for corrupt state and invalid lifecycle transitions.

## Capabilities

### New Capabilities

- `session-lifecycle`: Start, inspect, checkpoint, and finish one local Relay session with optional Git context and append-only events.
- `agent-handoff`: Generate and persist a concise Markdown handoff for a named target agent using explicit notes and recorded session context.

### Modified Capabilities

None.

## Impact

- Adds a single Rust binary and modular core implementation.
- Adds `.relay/` project-local runtime state; Relay has no daemon, hosted component, shell integration, or model-provider dependency.
- Adds Rust dependencies only where they directly support CLI parsing, serialization, timestamps, errors, and atomic temporary files.
- Adds integration tests and a concise README documenting the milestone workflow and limitations.
