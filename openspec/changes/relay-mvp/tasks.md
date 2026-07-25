## 1. Project foundation

- [x] 1.1 Define the CLI surface and dependencies in `Cargo.toml` and `src/cli.rs`
- [x] 1.2 Add typed session, event, checkpoint, and Git snapshot models
- [x] 1.3 Add safe JSON/Markdown replacement and synced JSON Lines append/read support
- [x] 1.4 Add read-only Git discovery and changed-file capture that degrades outside Git

## 2. Session lifecycle

- [x] 2.1 Implement `relay start` with active-session and corrupt-state protection
- [x] 2.2 Implement `relay status` with live Git state, event count, latest checkpoint, and agent context
- [x] 2.3 Implement `relay checkpoint [message]` with persisted artifact and event
- [x] 2.4 Implement `relay finish` with final state capture, summary persistence, event, and completed state

## 3. Agent handoff

- [x] 3.1 Implement `relay handoff <agent>` with repeatable explicit note options
- [x] 3.2 Render, print, and atomically save all required Markdown handoff sections
- [x] 3.3 Persist the most recently used agent and append a structured handoff event

## 4. Verification and documentation

- [x] 4.1 Add integration tests for the full lifecycle in a Git repository
- [x] 4.2 Add tests for non-Git operation, duplicate start, corrupt state, invalid finish, and handoff content/privacy
- [x] 4.3 Add unit tests for storage and Git parsing edge cases where useful
- [x] 4.4 Write a concise README with installation, examples, storage layout, note options, and limitations
- [x] 4.5 Run `cargo fmt --check`, strict `cargo clippy`, `cargo test`, OpenSpec validation, and the requested end-to-end workflow
