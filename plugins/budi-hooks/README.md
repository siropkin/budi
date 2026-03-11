# budi-hooks Claude Code plugin

`budi-hooks` packages the core `budi` hook automation for Claude Code:

- `SessionStart` -> `budi hook session-start` (injects project map and recently-relevant files)
- `UserPromptSubmit` -> `budi hook user-prompt-submit` (injects relevant code context)
- `PostToolUse` (`Write|Edit`) -> `budi hook post-tool-use` (triggers incremental re-indexing)
- `Stop` -> `budi hook session-end` (logs session stats when debug_io is enabled)

This keeps prompt context injection, post-edit indexing, and session lifecycle behavior
consistent across repos and teams.

## Requirements

- `budi` CLI installed and available in `PATH`
- Claude Code plugin support (v1.0.33+)

## Install

1. Add the marketplace that contains this plugin:

```shell
/plugin marketplace add <owner>/<repo>
```

2. Install the plugin:

```shell
/plugin install budi-hooks@budi-plugins
```

## Verify

- Trigger a normal prompt and confirm `UserPromptSubmit` behavior runs.
- Edit or write a file and confirm `PostToolUse` hook runs.
- Run `budi repo status` to confirm hook/index health.

## Security and behavior notes

- Hooks execute local shell commands and rely on your local `budi` binary.
- The plugin does not call external services by itself.
- Review `hooks/hooks.json` before enabling in shared environments.
