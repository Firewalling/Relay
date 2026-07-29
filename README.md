# Relay

> Continuous, local context for terminal agents.

Relay contains a Rust CLI for recording project-local work sessions,
checkpoints, categorized notes, handoffs, and bounded context for the next
terminal agent, plus a standalone Go supervisor for one explicitly requested
local child process. The Rust CLI does not run agents, call model-provider APIs,
inspect chat transcripts, or replace Git; the Go component is not integrated
with the Rust command surface or a scheduler.

## Prerequisites and installation

The Rust CLI requires a stable Rust toolchain with Cargo. Building and running
the optional one-child supervisor requires Linux and Go 1.26 or newer. Install
`git` too for Git-aware snapshots; Relay also works outside a Git repository.

Install the checked-out source directly:

```sh
cargo install --path .
relay --version
```

For a local release build and an explicit user-local install:

```sh
cargo build --release
mkdir -p "$HOME/.local/bin"
cp target/release/relay "$HOME/.local/bin/relay"
chmod 755 "$HOME/.local/bin/relay"
```

Build the standalone supervisor without installing it:

```sh
go build ./cmd/relay-supervisor
```

Ensure `$HOME/.local/bin` is on `PATH`. Relay does not currently claim
prebuilt packages or a crates.io release.

## Shell completions

`relay completions bash|zsh|fish` generates a script from the current CLI and
writes it to stdout. It never edits shell configuration.

```sh
# Bash: load now
source <(relay completions bash)

# Zsh: install in a directory already on $fpath, then restart the shell
mkdir -p "$HOME/.zfunc"
relay completions zsh > "$HOME/.zfunc/_relay"

# Fish: install for the current user
mkdir -p "$HOME/.config/fish/completions"
relay completions fish > "$HOME/.config/fish/completions/relay.fish"
```

For the Zsh example, add `fpath=("$HOME/.zfunc" $fpath)` before `compinit` in
`.zshrc` if that directory is not already on `fpath`.

## Daily workflow

```sh
relay start "Improve the installer"
relay note decision "Use the Git CLI for snapshots"
relay note progress "Added distro detection"
relay checkpoint "Detection and tests pass"
relay handoff codex \
  --completed-work "Added distro detection" \
  --decision "Use the Git CLI for snapshots" \
  --failure "Offline mirror was unavailable" \
  --next-action "Review the installer smoke test"
relay search "installer" --category decision --reverse
relay context --max-chars 8000
relay finish
```

Relay supports these commands:

- `start <GOAL>` starts the only active session.
- `status` shows the current session and live scoped Git state.
- `checkpoint [MESSAGE]` saves a timestamped Git snapshot.
- `handoff <AGENT>` prints and saves Markdown. `--completed-work`,
  `--decision`, and `--failure` are repeatable; `--next-action` accepts one
  value. Every detail is optional, and missing facts are reported as not
  recorded.
- `finish` saves a final Markdown summary and completes the active session.
- `list` lists all known sessions.
- `show <SESSION>` shows a session selected by full UUID, an unambiguous UUID
  prefix, or `latest`.
- `history [--session <SESSION>] [--type <EVENT_TYPE>] [--since <RFC3339>]
  [--until <RFC3339>] [--reverse] [--limit <N>]` filters append-only events,
  then displays the earliest matches by default or newest matches with
  `--reverse`; both time bounds are inclusive and the limit follows that order.
- `search <QUERY> [--session <SESSION>] [--type <EVENT_TYPE>]
  [--category <CATEGORY>] [--since <RFC3339>] [--until <RFC3339>] [--reverse]
  [--limit <N>]` searches stored event types and payload strings for an exact,
  case-sensitive plain substring. Filters run before matching, results retain
  append order by default, and `--reverse` selects newest matches before the
  limit.
- `resume <SESSION>` starts a new active continuation of a completed session,
  preserving its goal and recording lineage. It accepts the same selectors,
  including `latest`, does not modify the source, and fails if a session is
  already active.
- `note <CATEGORY> [TEXT]` records `progress`, `decision`, `blocker`,
  `observation`, or `next_action`.
- `context` emits bounded, recorded agent context; its filters are described
  below.
- `doctor [--repair]` diagnoses storage consistency.
- `completions <SHELL>` prints a Bash, Zsh, or Fish completion script.
- `protocol` reports the project-independent, read-only orchestration process
  contract compiled into Relay.

See the [orchestration process protocol](docs/orchestration-protocol.md) for
the exact versioned manifest, one-child execution request/result, lifecycle,
environment policy, limits, persistence, platform support, and security
requirements. The standalone supervisor is deliberately not a Go orchestrator:
Relay still has no scheduler, queue, Rust launcher command, or Lua workflow
runtime. Allowed Git-aware Rust operations may invoke `git`; existing note
editor behavior remains outside protocol v1.

`--json` and `--scope auto|current|repository` are global options and work
before or after a subcommand. Human output remains the default. An exact global
`--json` anywhere before a literal `--` requests JSON error framing even when
Clap rejects the arguments: Relay writes one compact output-schema-v1 error
envelope to stderr, writes nothing to stdout, and retains Clap's nonzero exit
code. That envelope uses `command: "cli"`, `error.code: "argument_error"`, and
an actionable normalized Clap message without a redundant leading `error: `.
Without that request, parser failures remain ordinary Clap stderr text.

After parsing, a failure to read the current directory uses the same JSON error
envelope shape when requested, with `error.code: "current_directory"` and the
parsed command name; human output remains unchanged. When JSON was requested,
application handling likewise emits one versioned success document on stdout
or one versioned error document on stderr. Successful help and version requests
remain ordinary shell-facing Clap text, including with `--json`, and
successful completion-script generation remains shell text. If the operating
system cannot start the Relay process, Relay
cannot emit an envelope; the caller must handle that launch failure.
`output_schema_version` is separate from the on-disk schema version.

For mutating commands, human and JSON output are rendered from the operation's
typed result; Relay does not reconstruct a completed mutation by rescanning
unrelated historical records.

## Notes

Supply note text in exactly one of these ways:

```sh
relay note decision "Keep JSON Lines append-only"
printf '%s\n' "Waiting for review" | relay note blocker --stdin
relay note next_action --editor
```

With neither text nor `--stdin`, Relay reads redirected stdin when present;
otherwise it opens `$VISUAL`, then `$EDITOR`, falling back to `vi`. Text must be
nonblank valid UTF-8 and is limited to 64 KiB. Explicitly combining positional
text, `--stdin`, and `--editor` is rejected.

Recorded notes are facts: Relay can include them in handoffs, final summaries,
and context, but never fills in work, decisions, failures, or next actions that
were not recorded.

## Deterministic local search

Search matches only Relay's append-only event log. Session records are read
only to resolve an optional session selector. It does not scan the working
tree, call model providers, use embeddings, or infer relevance:

```sh
relay search "installer"
relay search "review required" --session latest --category blocker
relay search "timeout" --type handoff.generated --since 2025-01-02T03:04:05Z
relay --json search "decision" --reverse --limit 20
```

Matching is an exact, case-sensitive substring over the event type and string
values nested in its payload. Each event appears at most once, identified by
the first deterministic match: event type first, then payload object keys in
lexicographic order and array values in recorded order. Session, exact event
type, note category, and inclusive RFC3339 time filters are applied before
matching. Results remain oldest-first by default; `--reverse` reverses all
matches before applying the limit. Search is read-only and returns stored typed
events in JSON without adding an index or changing the schema.

## Bounded agent context

Markdown is the default; `relay --json context` returns the same categories as
typed data. Context contains only the selected session, recorded notes and
events, checkpoints, continuation lineage, and current scoped Git facts.
Human Markdown keeps note and checkpoint content in their dedicated items, while
handoff events include recorded target, completed work, decisions, failures,
and next actions when present.

```sh
relay context \
  --session 4f2a1c \
  --category decision \
  --category next_action \
  --kind note.recorded \
  --notes 10 \
  --events 10 \
  --checkpoints 3 \
  --max-chars 8000
```

`--session` accepts a full UUID, an unambiguous prefix, or `latest`.

Categories and event kinds are repeatable filters. Counts default to 20 notes,
20 events, and 5 checkpoints. Each count keeps the most recent matching items
while preserving their recorded order. The human Markdown character budget
defaults to 12,000 Unicode characters and must be at least 64. Relay never
splits an item or exceeds that budget. Detailed output reports count-limit and
character-budget omissions separately; very small budgets use a compact
`N/C/E` (notes/checkpoints/events) summary when it fits. Typed JSON keeps the
selected records and exposes both omission categories. It does not query model
providers, scan source files, read agent conversations, or infer unrecorded
facts. Paths in generated output are project-relative, and absolute project
paths found in recorded text are redacted.

### Terminal-agent composition

These examples use only local CLI composition—Relay supplies a prompt and each
agent uses its own existing authentication/configuration:

```sh
# Hermes: installed help defines -z/--oneshot as a prompt argument
context="$(relay context --max-chars 8000)" && hermes -z "$context"

# hermes-omp: pass the same generated text as its ordinary prompt
context="$(relay context --max-chars 8000)" && hermes-omp "$context"

# Codex CLI: `codex exec -` reads instructions from stdin
relay context --max-chars 8000 | codex exec -

# Any agent command that accepts its prompt on stdin
relay context --max-chars 8000 | my-agent
```

Replace `my-agent` with an actual stdin-reading command. Command substitution
is used for Hermes-family commands because their installed interfaces accept a
prompt argument; no unsupported stdin behavior or provider flags are assumed.

## Project scope and monorepos

Relay stores state at a selected Relay project root:

- `--scope auto` (default) searches upward for the nearest `.relay` directory,
  stopping at the Git worktree root. If none exists, it uses the invocation
  directory.
- `--scope current` always uses the invocation directory.
- `--scope repository` uses the Git worktree root and fails outside Git.

This lets a nested project retain its session when commands run from deeper
directories. In a monorepo, start with `--scope current` in a package directory
to give that subtree independent Relay state. Git snapshots and changed paths
are then limited to that subtree; sibling package changes do not affect its
cleanliness. Use `--scope repository` when the whole worktree is one Relay
project.

## Files, compatibility, migration, and locking

Relay's inspectable project-local layout is:

```text
.relay/
├── session.json
├── sessions/
│   └── <session-id>.json
├── events.jsonl
├── checkpoints/
│   └── <timestamp>-<id>.json
├── handoffs/
│   └── <timestamp>-<id>.md
├── operations/
│   └── <operation-id>.json
├── lock
└── summary-<session-id>.md
```

JSON records and JSON Lines events currently use on-disk `schema_version: 1`;
the crate version is independent. Optional continuation fields are
backward-compatible. A valid earlier single-current-session layout remains
readable without mutation. On the next normal locked mutation, Relay
materializes the missing `sessions/<id>.json` record while preserving the
existing current record and history. Unsupported schema versions and malformed
JSON/JSONL are reported, never silently upgraded, discarded, or overwritten.
Back up `.relay/` before manual migration or repair.

Mutating commands take a nonblocking advisory project lock. Contention fails
with the recorded owner PID and command when available; the kernel releases
the lock if a process exits. Read-only commands do not take the mutation lock.
Multi-file mutations use durable operation markers that record completed
effects. Individual JSON/Markdown replacements are synced and atomic, and
events are appended as synced, newline-terminated records. An interruption can
leave an operation marker or durable partial effects; Relay preserves them for
diagnosis rather than guessing how to roll them back.

Choose your own version-control policy for `.relay/`. Ignore it for private
local coordination, or intentionally share selected artifacts after reviewing
their contents. Goals, notes, handoffs, summaries, branch names, commit IDs, and
changed paths can contain sensitive information. Relay provides no encryption,
redaction policy beyond context-path privacy, hosted sync, or access control.

## Git behavior and operations

Inside Git, Relay records the branch, commit, cleanliness, and project-scoped
changed paths while excluding its own `.relay/` directory. Non-UTF-8 path bytes
on Unix are rendered deterministically. Outside Git, lifecycle commands work
and report Git fields as unavailable.

Relay invokes read-only Git queries only. It never commits, stages, stashes,
checks out, resets, rewrites history, pushes, or edits working files. Relay
mutates only its selected `.relay/` storage and user-requested completion
redirection targets.

## Doctor and recovery

`relay doctor` performs byte-preserving diagnostics by default. It reports
stable codes, severity, relative affected paths, guidance, and health totals
for malformed or partial JSONL, malformed/unsupported records and artifacts,
invalid lifecycle combinations, missing references, orphaned artifacts,
current/history disagreement, and incomplete operation markers.

`relay doctor --repair` is deliberately conservative. It takes the project
lock and removes only stale operation markers whose listed effects are all
provably complete. If any ambiguous, incomplete, or potentially destructive
problem exists, repair is refused without changing it. Inspect diagnostics,
back up `.relay/`, and resolve such damage manually.

## Current limitations

Relay supports one active Rust session per selected project. The Go component
supervises one local child per invocation on Linux; it has no queue, retries,
remote execution, or Rust command-surface integration. Relay
has no daemon, TUI, hosted synchronization, provider integration, transcript
import, automatic source inspection, semantic search, encryption, Lua runtime,
or automatic recovery of ambiguous interrupted operations. Advisory locks
depend on filesystem lock support. Context is deterministic selection and
formatting, not model-generated summarization. Packaging is currently source
installation or locally built binaries.

The files under `openspec/` describe development intent only. Relay has no
runtime OpenSpec dependency.
