# Relay

> Relay — continuous context for terminal agents.

Relay is a small Rust CLI that records one project-local work session, its Git
context, explicit checkpoints, agent handoffs, and a final summary. It does not
run agents, inspect chat transcripts, or replace Git.

## Install or build

Relay requires Rust and the `git` executable for Git-aware snapshots.

```sh
cargo build --release
install -m 755 target/release/relay ~/.local/bin/relay
```

For development, run `cargo run -- <command>` from this repository instead.

## Milestone workflow

Run commands from the exact project directory whose session you want to manage:

```sh
relay start "Improve Fedora installer"
relay status
relay checkpoint "Rust detection fixed"
relay handoff omp \
  --completed-work "Added distro detection" \
  --completed-work "Covered spaces in paths" \
  --decision "Use the Git CLI for snapshots" \
  --failure "Offline package mirror was unavailable" \
  --next-action "Review the installer smoke test"
relay status
relay finish
```

`start` creates an active session. `status` reports its lifecycle state, live
Git state, event count, latest checkpoint, and last agent. `checkpoint` captures
the current Git snapshot with an optional message. `handoff` prints and saves a
targeted Markdown note. `finish` saves a final summary and marks the session
completed. A second active `start` and a second `finish` are errors.

Handoff note flags are optional and repeatable where shown:

- `--completed-work <TEXT>`: completed item; may be supplied more than once
- `--decision <TEXT>`: important decision; may be supplied more than once
- `--failure <TEXT>`: failure or blocked item; may be supplied more than once
- `--next-action <TEXT>`: one suggested next action

Omitted categories render as `Not recorded`; Relay does not invent content.
Output paths such as `.relay/handoffs/<timestamp>-<id>.md` are relative to the
project. `Git clean: no` means Git reported changed or untracked files, while
`unavailable` means the current directory is not in a Git repository or that
particular value (for example, a branch before the first commit) is absent.

## Local data

Relay keeps inspectable, project-local files:

```text
.relay/
├── session.json
├── events.jsonl
├── checkpoints/
│   └── <timestamp>-<id>.json
├── handoffs/
│   └── <timestamp>-<id>.md
└── summary-<session-id>.md
```

The append-only JSON Lines log currently uses these stable event names:
`session.started`, `checkpoint.created`, `handoff.generated`, and
`session.finished`. Every event includes a schema version, event ID, session ID,
UTC timestamp, event type, and structured payload.

This repository ignores `.relay/` so its own acceptance runs do not pollute Git
snapshots. In another project, choose your own version-control policy: ignore
`.relay/` for private/local coordination, or intentionally track appropriate
artifacts if the team wants to share them.

## Git, non-Git, and safety

Inside Git, Relay records the repository root internally plus the branch,
commit, cleanliness, and Git-relative changed paths. Generated handoffs and
final summaries omit the absolute project root. Outside Git, all lifecycle
commands still work and explicitly mark Git information unavailable.

Relay invokes only read-only Git queries. It does not commit, stage, stash,
reset, rewrite history, push, or edit user files. Mutable JSON and Markdown are
written through a synced temporary file and atomically replaced; events are
appended as synced complete lines. Malformed or unsupported session state and
malformed event lines produce errors instead of being silently replaced or
discarded.
Individual state files are protected, but updates spanning several Relay files
are not atomic as a group; an interruption between writes can leave an orphan
artifact or event that requires manual inspection.

The files under `openspec/` describe and validate development intent only.
Relay has no runtime OpenSpec dependency and does not read those files when it
runs.

## MVP limitations

Relay supports one active session per current directory and does not search
parent directories. It has no cross-process locking, daemon, TUI, shell
integration, hosted synchronization, model integration, automatic
summarization, agent execution, or Git mutation. Historical events and
timestamped artifacts remain, but there is no archive/query interface for old
sessions yet. Git paths must be valid UTF-8 in this milestone.
