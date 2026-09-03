## Why

Relay currently records session notes, checkpoints, Git snapshots, runs, and generated Markdown handoffs separately. A receiving agent must reconstruct their relationship from the event log and filesystem. A bounded structured snapshot can carry recorded state and trusted provenance without making Relay an agent runtime.

## What Changes

- Add version-2 structured handoff records with explicit entry provenance and unavailable-state markers.
- Persist immutable structured records in a bounded append-only JSONL ledger and render generated Markdown from those records.
- Preserve legacy Markdown handoffs and existing `relay handoff <agent>` usage.
- Add handoff create/show/latest/list operations, run references, trusted output/artifact references, and direct context consumption.
- Capture session, checkpoint, note, Git, workflow, and run state using deterministic bounded selection.

## Non-Goals

No model/provider APIs, prompt generation, agent spawning, autonomous loops, daemons, remote agents, networking, authentication, secret store, embeddings, semantic ranking, workflow control language, script evaluation, UI, or artifact upload.

## Impact

Rust remains owner of lifecycle, storage, project scope, Git snapshots, aggregate runs, workflow metadata, and output trust. Handoff records are local snapshots. Generated Markdown is derived and never authoritative. Existing schema-v1 lifecycle state and legacy handoff files remain readable without read-time migration.
