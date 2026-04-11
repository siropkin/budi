# budi — Cursor Extension

Live AI coding cost analytics in your Cursor status bar and side panel.

## Features

- **Status bar** — session cost + health indicator, updates automatically
- **Health panel** — click the status bar to open; shows active session vitals (context growth, cache reuse, cost acceleration, retry loops), other recent sessions with health at a glance, and cost overview
- **Session switching** — click any session in the health panel to pin it, or use **Budi: Select Session** command
- **Auto-tracking** — the proxy detects session activity and updates automatically

## Prerequisites

- **budi** installed and initialized (`budi init`)
- **budi-daemon** running (starts automatically after `budi init`)
- Cursor's `Override OpenAI Base URL` set to `http://localhost:9878` (Cursor Settings → Models) so LLM traffic routes through the budi proxy

## Install

The extension can be installed during `budi init` and later with:

```bash
budi integrations install --with cursor-extension
```

Run `budi doctor` to verify.

### First-run smoke check

After install/reload, validate in under a minute:

1. Run `budi doctor` and confirm daemon + proxy are healthy
2. Verify `Override OpenAI Base URL` is set to `http://localhost:9878` in Cursor Settings → Models
3. Send one prompt in Cursor chat
4. Click the budi status bar item (`🟢/🟡/🔴`) to open the health panel
5. If no session appears yet, run **Budi: Refresh Status** once

**Manual install** (if auto-install was skipped or you want to rebuild):

```bash
cd extensions/cursor-budi
npm ci
npm run lint
npm run format:check
npm run test
npm run build
npx vsce package --no-dependencies -o cursor-budi.vsix
cursor --install-extension cursor-budi.vsix --force
```

Then reload Cursor: **Cmd+Shift+P** → **Developer: Reload Window**

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

## How it works

1. **Proxy** — LLM traffic from Cursor routes through the budi proxy (port 9878), which captures session activity and updates `cursor-sessions.json` in budi's data directory (`~/.local/share/budi` on Unix, `%LOCALAPPDATA%\budi` on Windows)
2. **File watcher** — the extension watches both the session file and its parent directory, so it can detect active-session changes immediately (including when the file is created after extension startup)
3. **Daemon** — `budi statusline --format json` (or direct HTTP to daemon) returns session cost, health state, and vitals
4. **Health panel** — fetches session health details and lists recent sessions from `/analytics/sessions`

## Limitations

Cursor does not expose the currently focused chat tab to extensions. The extension tracks the most recently active session (via proxy activity). For passive tab switching, use **Budi: Select Session** or click a session in the health panel.

## Troubleshooting

**Status bar says offline / panel shows daemon offline**

1. Run `budi doctor` and confirm daemon health
2. Run `budi init` if the daemon is not running
3. If you changed `budi.daemonUrl`, run **Budi: Refresh Status** (or reload Cursor) to force an immediate reconnect

**Session does not switch quickly after chat activity**

1. Confirm the proxy is running (`budi doctor`)
2. Verify `Override OpenAI Base URL` is set to `http://localhost:9878` in Cursor Settings → Models
3. Send one message in Cursor to create/update `cursor-sessions.json`
4. Use **Budi: Select Session** to pin manually when switching passively between chats

**Panel data is stale**

- The extension updates on both event-driven file changes and periodic polling (`budi.pollingIntervalMs`, default 15s)
- Use **Budi: Refresh Status** for an immediate refresh
