## Summary

- Describe what changed and why.
- Link the issue/review scope this PR addresses.

## Review scope

- Area reviewed:
- Key findings:
- Follow-up items intentionally deferred:

## Validation

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] If extension changed: `cd extensions/cursor-budi && npm run lint && npm run format:check && npm run test && npm run build`
- [ ] If dashboard changed: `cd frontend/dashboard && npm ci && npm run build`

Validation evidence:

- [ ] Paste key command output or CI links

## Risk and compatibility

- Risk level: low / medium / high
- Migration or compatibility notes (if any):

## Checklist

- [ ] Docs updated for user-visible behavior
- [ ] Backward compatibility considered
- [ ] Tests added/updated for changed behavior
- [ ] Issue linkage included (`Closes #<issue>`) when applicable
