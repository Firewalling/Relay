## Implementation

- [x] Add schema-v3 output and input declarations.
- [x] Add explicit attempt output environment paths.
- [x] Add bounded JSON and artifact finalization.
- [x] Persist output records with provenance.
- [x] Integrate retry and reconciliation transitions.
- [x] Append trusted outputs to dependency argv.
- [x] Expose bounded outputs in structured JSON.
- [x] Keep human output concise.
- [x] Migrate aggregate state versions to v4.

## Verification

- [x] Validate names, fields, types, paths, and bounds.
- [x] Reject failed and malformed output promotion.
- [x] Reject symlink and hardlink artifact aliases.
- [x] Reject tampered cross-run output metadata.
- [x] Cover retry isolation and exact provenance.
- [x] Cover real Go success and resume.
- [x] Cover real Go crash reconciliation.
- [x] Cover aggregate metadata state-size bound.
- [x] Run full required Rust and Go CI matrix.

## Documentation and smoke

- [x] Document output trust and limits.
- [x] Document retry and reconciliation ordering.
- [x] Document explicit dependency inputs.
- [x] Record real repository smoke command results.
- [ ] Open draft pull request after verification.
