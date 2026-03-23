# budi-hooks Claude Code plugin

`budi-hooks` packages the core `budi` hook automation for Claude Code:

- `SessionStart` -> `budi hook session-start` (ensures the budi daemon is running)
- `UserPromptSubmit` -> HTTP `/hook/prompt-submit` (tracks prompt analytics)
- `PostToolUse` (`Write|Edit|Read|Glob`) -> HTTP `/hook/tool-use` (tracks tool usage)
- `SubagentStart` -> `budi hook subagent-start` (subagent lifecycle tracking)
- `Stop` -> `budi hook session-end` (prints session summary)

These hooks feed budi's cost and usage analytics — no context injection or indexing.

## Requirements

- `budi` CLI and `budi-daemon` installed and available in `PATH`
- Claude Code plugin support

## Install

1. Add the marketplace that contains this plugin:

```shell
/plugin marketplace add siropkin/budi
```

2. Install the plugin:

```shell
/plugin install budi-hooks@budi-plugins
```

## Verify

- Trigger a normal prompt and confirm `UserPromptSubmit` hook runs.
- Edit or write a file and confirm `PostToolUse` hook runs.
- Run `budi stats` to confirm data is being tracked.

## Security and behavior notes

- Hooks execute local shell commands and HTTP requests to `127.0.0.1:7878` (local daemon).
- The plugin does not call external services.
- Review `hooks/hooks.json` before enabling in shared environments.
