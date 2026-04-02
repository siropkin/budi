## Summary

- Describe what changed and why.

## Validation

- [ ] `cargo fmt --all`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] If extension changed: `cd extensions/cursor-budi && npm run lint && npm run format:check && npm run test && npm run build`

## Risk and compatibility

- Risk level: low / medium / high
- Migration or compatibility notes (if any):

## Checklist

- [ ] Docs updated for user-visible behavior
- [ ] Backward compatibility considered
- [ ] Tests added/updated for changed behavior
