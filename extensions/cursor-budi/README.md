# budi — Cursor Extension

Live AI coding cost analytics in your Cursor status bar and side panel.

## Prerequisites

- **budi** installed and initialized (`budi init`)
- **budi-daemon** running (starts automatically after `budi init`)
- **Proxy configured** — in Cursor Settings > Models, set **Override OpenAI Base URL** to `http://localhost:9878`

## Install

Install from the VS Code Marketplace (search for "budi"), or via CLI:

```bash
budi integrations install --with cursor-extension
```

**Manual install** (for development):

```bash
cd extensions/cursor-budi
npm ci && npm run build
npx vsce package --no-dependencies -o cursor-budi.vsix
cursor --install-extension cursor-budi.vsix --force
```

Then reload Cursor: **Cmd+Shift+P** > **Developer: Reload Window**

## Features

- **Status bar** — today's session cost + health overview, updates automatically
- **Health panel** — click the status bar to open; shows active session vitals, recent sessions with health at a glance
- **Session switching** — click any session in the health panel to pin it, or use **Budi: Select Session** command
- **Onboarding** — guides you through proxy setup when the daemon is not running

## How It Works

In budi 8.0, all AI cost tracking runs through a local proxy:

1. **Proxy** — Cursor sends API requests to `http://localhost:9878` instead of directly to OpenAI. The proxy forwards requests transparently while capturing token usage and cost metadata.
2. **Daemon API** — the extension queries the daemon's HTTP API for statusline data, session health, and recent sessions.
3. **Workspace signal** — the extension writes `~/.local/share/budi/cursor-sessions.json` to indicate which workspace is active (see [Contract](#cursor-sessionsjson-contract) below).

## Commands

| Command                       | Description                                |
| ----------------------------- | ------------------------------------------ |
| **Budi: Toggle Health Panel** | Open/focus the health side panel           |
| **Budi: Select Session**      | Pick which session to display (quick pick) |
| **Budi: Open Dashboard**      | Open the budi web dashboard                |
| **Budi: Refresh Status**      | Force-refresh status bar data              |

## Configuration

| Setting                  | Default                 | Description                      |
| ------------------------ | ----------------------- | -------------------------------- |
| `budi.pollingIntervalMs` | `15000`                 | Status bar refresh interval (ms) |
| `budi.daemonUrl`         | `http://127.0.0.1:7878` | Daemon URL                       |

## cursor-sessions.json Contract

**Version: 1** (ADR-0086 Section 3.4)

The extension writes `~/.local/share/budi/cursor-sessions.json` to signal which Cursor workspace is currently active. The daemon may read this file to correlate proxy events with the active workspace.

```json
{
  "version": 1,
  "active_workspace": "/absolute/path/to/project",
  "updated_at": "2026-04-11T20:00:00.000Z"
}
```

| Field              | Type     | Description                                                       |
| ------------------ | -------- | ----------------------------------------------------------------- |
| `version`          | `number` | Contract version. Currently `1`. Breaking changes require a bump. |
| `active_workspace` | `string` | Absolute path to the active Cursor workspace.                     |
| `updated_at`       | `string` | ISO-8601 timestamp of last update.                                |

The file is written on extension activation and updated on each status refresh. It is deleted on extension deactivation.

## Limitations

- Cursor does not expose the currently focused chat tab to extensions. The extension tracks the most recently active session. For passive tab switching, use **Budi: Select Session** or click a session in the health panel.
- Some built-in Cursor features may bypass the proxy override and use Cursor-managed routes directly (ADR-0082 Section 1).

## Troubleshooting

**Status bar says offline / panel shows daemon offline**

1. Run `budi doctor` and confirm daemon health
2. Run `budi init` if the daemon is not running
3. Check that the proxy is running on port 9878

**No sessions appear after sending prompts**

1. Verify "Override OpenAI Base URL" is set to `http://localhost:9878` in Cursor Settings > Models
2. Restart Cursor after changing the setting
3. Use **Budi: Refresh Status** for an immediate update

**Panel data is stale**

- The extension polls every 15 seconds (configurable via `budi.pollingIntervalMs`)
- Use **Budi: Refresh Status** for an immediate refresh
