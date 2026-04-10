## Issue #64 review: Cursor extension reliability and health UX

### Findings (ordered by severity)

1. **Stale daemon URL in file-watch refresh path**
   - The session-file watcher captured `daemonUrl` at activation, so updates to `budi.daemonUrl` could keep using an old URL for event-driven refreshes.
   - Impact: status/panel could appear intermittently stale or offline after config updates.
   - Fix: rebuild session watchers on config change and queue an immediate refresh using the latest daemon URL.

2. **No watch attach when session file appears after startup**
   - If `cursor-sessions.json` did not exist during activation, no file watcher was attached.
   - Impact: delayed or missing auto-session updates until polling/manual refresh.
   - Fix: watch both parent directory and file, and attach file watcher as soon as the file appears.

3. **Refresh races across polling/manual/file-watch triggers**
   - Concurrent refresh calls could overlap and apply out-of-order states.
   - Impact: transient status flicker and stale session context.
   - Fix: add serialized refresh queueing so only one refresh runs at a time and newest daemon URL wins.

4. **Inline webview handler interpolation for dynamic IDs/URLs**
   - Session IDs and URLs were directly injected into inline `onclick` handlers.
   - Impact: malformed handlers for unusual identifiers and unnecessary injection risk.
   - Fix: escape dynamic values as JS string literals before interpolation.

### Test coverage improvements

- Added `splitSessionsByDay` unit coverage for day bucketing behavior in extension-facing session lists.

### Documentation updates

- Updated extension README troubleshooting for offline/stale behavior and watcher/polling expectations.
- Updated top-level README troubleshooting with Cursor extension recovery steps.

### Follow-ups (not included in this PR)

- Add focused unit tests for extension lifecycle (`activate`/`deactivate`) and watcher teardown behavior.
- Consider replacing inline webview handlers with delegated event listeners + explicit CSP nonce policy.
