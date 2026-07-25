## ADDED Requirements

### Requirement: Generate shell completions
Relay SHALL generate completion scripts for Bash, Zsh, and Fish from the actual CLI definition and write the selected script to stdout without modifying shell configuration.

#### Scenario: Generate each completion
- **WHEN** a user runs `relay completions bash`, `relay completions zsh`, or `relay completions fish`
- **THEN** Relay emits a nonempty script accepted by the corresponding completion mechanism and containing the current commands

### Requirement: Automate project quality gates
The repository SHALL include a basic GitHub Actions workflow that checks formatting, runs strict Clippy with warnings denied, and runs all unit and integration tests on a supported stable Rust toolchain.

#### Scenario: Pull request quality workflow
- **WHEN** GitHub Actions runs for a push or pull request
- **THEN** formatting, strict Clippy, and tests execute as distinct visible gates or steps and any failure fails the workflow

### Requirement: Document practical installation and releases
The README SHALL document prerequisites, source installation, `cargo install --path .`, release build and binary copy steps, shell completion installation examples, storage compatibility, and the supported daily workflow without claiming unsupported packaging.

#### Scenario: Local release build
- **WHEN** a user follows the documented release-build instructions
- **THEN** the user can build `target/release/relay`, install it in a user-local binary directory, and optionally install a completion script
