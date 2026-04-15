# Smoke Test Plan — budi v8.0.0

Structured manual verification for release readiness (issue #199).
Each test has a unique ID, explicit steps, and a pass/fail criterion.
An AI agent running these tests should execute each step, record the output, and report PASS/FAIL with evidence.

> **Note on `budi cloud join`:** This command does not exist. Cloud sync is configured by editing `~/.config/budi/cloud.toml` directly. The issue #199 checklist item about "budi cloud join" was written before this was clarified.

---

## Prerequisites

| Requirement | Why |
|-------------|-----|
| The v8/199-release-readiness branch is merged (or use it directly) | Contains the 8.0.0 version bump |
| Rust toolchain installed (`rustup`) | For source builds |
| Homebrew installed (macOS only) | For Homebrew install path |
| An Anthropic API key OR OpenAI API key | For proxy flow tests |
| Access to `app.getbudi.dev` (Supabase Auth account) | For cloud flow tests |
| A Cursor/VS Code installation | For extension tests |

---

## ST-01: Fresh install from source (macOS)

**Goal:** Verify `scripts/install.sh` builds and installs correctly on macOS.

```bash
# 1. Remove any existing budi installation
pkill -f budi-daemon || true
rm -rf ~/.local/bin/budi ~/.local/bin/budi-daemon
rm -rf ~/.local/share/budi
rm -rf ~/.config/budi

# 2. Clone and install
git clone https://github.com/siropkin/budi.git /tmp/budi-smoke-test
cd /tmp/budi-smoke-test
git checkout v8/199-release-readiness  # or main after merge
./scripts/install.sh

# 3. Verify
budi --version
```

**Pass criteria:**
- Install completes without errors
- `budi --version` prints `budi 8.0.0`
- `budi init` runs automatically as part of install and starts daemon + proxy
- No warnings about missing dashboard or extension build

**Cleanup:**
```bash
rm -rf /tmp/budi-smoke-test
```

---

## ST-02: Fresh install from source (Linux)

**Goal:** Same as ST-01 on a Linux glibc system (Ubuntu/Debian/Fedora).

Same steps as ST-01. Additionally verify:
- `budi autostart status` reports systemd user service status
- Service file exists at `~/.config/systemd/user/budi-daemon.service`

---

## ST-03: Fresh install from source (Windows)

**Goal:** Verify source build on Windows.

```powershell
# 1. Remove any existing budi installation
taskkill /IM budi-daemon.exe /F 2>$null
Remove-Item "$env:LOCALAPPDATA\budi" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item "$env:USERPROFILE\.config\budi" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item "$env:USERPROFILE\.local\share\budi" -Recurse -Force -ErrorAction SilentlyContinue

# 2. Clone and build
git clone https://github.com/siropkin/budi.git $env:TEMP\budi-smoke-test
cd $env:TEMP\budi-smoke-test
cargo build --release --locked
$BinDir = Join-Path $env:LOCALAPPDATA "budi\bin"
New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
Copy-Item .\target\release\budi.exe $BinDir -Force
Copy-Item .\target\release\budi-daemon.exe $BinDir -Force

# 3. Add to PATH for this session and initialize
$env:PATH = "$BinDir;$env:PATH"
budi --version
budi init
```

**Pass criteria:**
- Build completes without errors
- `budi --version` prints `budi 8.0.0`
- `budi init` starts daemon and proxy
- `budi autostart status` reports Task Scheduler status

---

## ST-04: Standalone installer (macOS/Linux)

**Goal:** Verify the curl-pipe-bash standalone installer.

```bash
# 1. Clean slate
pkill -f budi-daemon || true
rm -rf ~/.local/bin/budi ~/.local/bin/budi-daemon
rm -rf ~/.local/share/budi
rm -rf ~/.config/budi

# 2. Install (downloads prebuilt binaries from GitHub Releases)
# Note: this requires a published v8.0.0 release. If not yet published,
# skip this test — it will be verified post-release in issue #158.
curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | bash

# 3. Verify
budi --version
budi doctor
```

**Pass criteria:**
- Installer downloads and extracts binaries
- `budi --version` prints `budi 8.0.0`
- `budi init` runs automatically
- `budi doctor` shows all checks passed

> **Note:** This test can only run after v8.0.0 release assets are published. For pre-release testing, use ST-01 (source build) instead.

---

## ST-05: Standalone installer (Windows)

**Goal:** Verify the PowerShell standalone installer.

```powershell
# Same note as ST-04: requires published release assets.
irm https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.ps1 | iex
budi --version
budi doctor
```

**Pass criteria:** Same as ST-04 but on Windows.

---

## ST-06: `budi init` — agent selection and proxy setup

**Goal:** Verify interactive init configures agents and proxy routing correctly.

```bash
# 1. Start from a clean config (daemon can be running)
rm -f ~/.config/budi/agents.toml

# 2. Run init interactively — select Claude Code and Cursor
budi init

# 3. Verify agent config was written
cat ~/.config/budi/agents.toml

# 4. Verify proxy routing was configured
# For CLI agents: check shell profile for managed env block
grep -A5 "# >>> budi proxy" ~/.zshrc || grep -A5 "# >>> budi proxy" ~/.bashrc

# 5. Verify daemon and proxy are running
budi status
budi doctor
```

**Pass criteria:**
- `agents.toml` contains the selected agents with `enabled = true`
- Shell profile contains a `# >>> budi proxy` block with `ANTHROPIC_BASE_URL=http://127.0.0.1:9878` (if Claude Code enabled) and/or `OPENAI_BASE_URL` (if Codex/Copilot enabled)
- If Cursor was selected: `~/.cursor-server/` or Cursor settings.json contains `openai.baseUrl` pointing to `http://127.0.0.1:9878`
- `budi status` shows daemon running (port 7878) and proxy running (port 9878)
- `budi doctor` shows all checks passed

---

## ST-07: `budi doctor` — full diagnostic

**Goal:** Verify doctor output covers all subsystems.

```bash
budi doctor
budi doctor --deep
```

**Pass criteria for `budi doctor`:**
- Reports git repo status
- Reports config path
- Reports daemon binary path and version (v8.0.0)
- Reports daemon/CLI version match
- Reports daemon running status
- Reports database file readable
- Reports database integrity (quick_check): ok
- Reports disk space
- Reports database schema: v1
- Reports agents enabled (lists configured agents)
- Reports auto-proxy config status
- Reports proxy running status
- Reports autostart status
- Ends with "All checks passed" (or lists specific failures)

**Pass criteria for `budi doctor --deep`:**
- Same as above but database integrity uses `integrity_check` (slower, more thorough)
- No integrity errors

---

## ST-08: `budi stats` / `budi sessions` / `budi health`

**Goal:** Verify analytics CLI commands produce correct output.

```bash
# 1. Summary stats
budi stats
budi stats --format json

# 2. With filters
budi stats --models
budi stats --projects
budi stats --branches
budi stats --provider claude_code

# 3. Sessions
budi sessions
budi sessions --format json

# 4. Session detail (use an ID from the sessions list)
budi sessions <session-id>

# 5. Health
budi health
```

**Pass criteria:**
- `budi stats` shows a formatted table with agent breakdown and total cost line
- `budi stats --format json` returns valid JSON
- `--models` shows model breakdown with provider disambiguation
- `--provider` filters correctly (no error for `codex`, `copilot_cli`, `openai`)
- `budi sessions` lists sessions with: timestamp, session ID, model, repo, cost
- `budi sessions <id>` shows session detail with cost, models, health, tags
- Prefix matching works (first 8 chars of session ID is enough)
- `budi health` shows session vitals (context growth, cache reuse, cost acceleration)
- JSON output is valid parseable JSON in all `--format json` modes

---

## ST-09: `budi import` — historical transcript import

**Goal:** Verify historical data import from all 4 providers.

```bash
# 1. Check help
budi import --help

# 2. Run import (processes whatever transcripts exist on disk)
budi import

# 3. Verify data was imported (if transcripts exist)
budi stats --period all
```

**Pass criteria:**
- `budi import --help` mentions: Claude Code, Codex, Copilot CLI, Cursor
- `budi import` runs without errors (reports files processed and messages ingested)
- If Claude Code transcripts exist at `~/.claude/projects/`: messages appear in stats
- If Codex sessions exist at `~/.codex/sessions/`: messages appear in stats
- If Copilot CLI sessions exist at `~/.copilot/session-state/`: messages appear in stats
- If Cursor is installed: Cursor API data appears in stats

---

## ST-10: Proxy flow — end-to-end cost tracking

**Goal:** Verify: agent → proxy → upstream → budi stats shows the request.

### ST-10a: Claude Code (Anthropic protocol)

```bash
# 1. Ensure proxy is running
budi status

# 2. In a NEW terminal (important — needs the shell profile env vars):
source ~/.zshrc  # or restart terminal
echo $ANTHROPIC_BASE_URL  # should be http://127.0.0.1:9878

# 3. Launch Claude Code and send a short prompt
# Option A: use budi launch
budi launch claude
# Then type: "Say hello in 5 words" and wait for response

# Option B: use claude directly (if env vars are active)
claude
# Then type: "Say hello in 5 words"

# 4. In the original terminal, check stats
budi stats
budi sessions
```

**Pass criteria:**
- `ANTHROPIC_BASE_URL` is set to `http://127.0.0.1:9878`
- Claude Code starts and responds normally (no visible lag)
- `budi stats` shows non-zero cost for Claude Code / claude_code provider
- `budi sessions` shows a new session with cost > $0.00

### ST-10b: Codex CLI (OpenAI protocol)

```bash
# 1. Verify env var
echo $OPENAI_BASE_URL  # should be http://127.0.0.1:9878

# 2. Launch Codex
budi launch codex
# Send a short prompt

# 3. Check stats
budi stats --provider codex
```

**Pass criteria:**
- `OPENAI_BASE_URL` is set to `http://127.0.0.1:9878`
- Codex starts and responds normally
- `budi stats` shows non-zero cost for Codex provider

### ST-10c: Cursor (settings.json patch)

```bash
# 1. Verify Cursor config was patched
budi doctor  # should report "auto-proxy config: ... look good"

# 2. Open Cursor, start a new chat, send a prompt
# 3. Check stats
budi stats --provider cursor
```

**Pass criteria:**
- Cursor's settings.json has `openai.baseUrl` set to the proxy
- Cursor chat works normally
- `budi stats` shows Cursor traffic

---

## ST-11: Cloud sync flow

**Goal:** Verify local daemon syncs data to app.getbudi.dev.

### Prerequisites
- A Supabase Auth account on app.getbudi.dev
- An API key from the Settings page
- Some local cost data (from proxy usage or import)

```bash
# 1. Create cloud config
mkdir -p ~/.config/budi
cat > ~/.config/budi/cloud.toml << 'TOML'
[cloud]
enabled = true
api_key = "budi_YOUR_KEY_HERE"
TOML

# 2. Restart daemon to pick up cloud config
budi init

# 3. Wait for sync (default interval is 300s / 5 min)
# Or check daemon logs for sync activity:
# tail -f ~/.local/share/budi/logs/daemon.log | grep -i cloud

# 4. Verify in the dashboard
# Open https://app.getbudi.dev and check:
# - Overview page shows data
# - Repos/Models/Sessions pages are populated
```

**Pass criteria:**
- Daemon logs show successful cloud sync (HTTP 200 from ingest endpoint)
- Dashboard at app.getbudi.dev shows the synced data (daily rollups, session summaries)
- No prompts, code, or response content appears in the dashboard (privacy contract)
- Only aggregated metrics: token counts, costs, model names, repo hashes, branch names

**Cleanup:**
```bash
# Disable cloud sync after testing
cat > ~/.config/budi/cloud.toml << 'TOML'
[cloud]
enabled = false
TOML
```

---

## ST-12: Autostart — daemon survives reboot

**Goal:** Verify the daemon restarts automatically after a system reboot.

```bash
# 1. Verify autostart is installed
budi autostart status
# Expected: "✓ Autostart: installed and running"

# 2. Note current daemon PID
lsof -i :7878  # macOS/Linux
# or: Get-NetTCPConnection -LocalPort 7878 -State Listen  # Windows

# 3. Reboot the machine
sudo reboot  # macOS/Linux
# or: Restart-Computer  # Windows

# 4. After reboot, open a terminal
budi status
budi doctor
lsof -i :7878  # new PID should be running

# 5. Verify proxy is also up
curl -s http://127.0.0.1:9878/v1/models 2>&1 || true
# Should get a response (502 from proxy is fine — means proxy is listening)
```

**Pass criteria:**
- Before reboot: autostart shows "installed and running"
- After reboot: daemon is running (new PID) without manual intervention
- `budi status` shows daemon and proxy running
- `budi doctor` shows all checks passed
- No need to run `budi init` after reboot

### Platform-specific autostart checks

**macOS (launchd):**
```bash
launchctl list | grep budi
# Should show dev.getbudi.budi-daemon
cat ~/Library/LaunchAgents/dev.getbudi.budi-daemon.plist
```

**Linux (systemd):**
```bash
systemctl --user status budi-daemon
# Should show active (running)
cat ~/.config/systemd/user/budi-daemon.service
```

**Windows (Task Scheduler):**
```powershell
schtasks /Query /TN BudiDaemon
# Should show the task exists and is enabled
```

---

## ST-13: Cursor extension install flow

**Goal:** Verify the Cursor extension installs and connects to the daemon.

```bash
# 1. Install extension during init (or later)
budi integrations install --with cursor-extension

# 2. Open/reload Cursor
# Cmd+Shift+P → "Developer: Reload Window"

# 3. Check status bar
# Should show session health circles: 🟢 X 🟡 Y 🔴 Z

# 4. Click the status bar item to open the side panel
# Should show: session list, vitals, tips

# 5. Verify daemon connectivity
# In Cursor, run: Cmd+Shift+P → "Budi: Refresh Status"
# Should not show "daemon offline" warning
```

**Pass criteria:**
- Extension installs without errors
- Status bar shows session health aggregation
- Side panel opens and displays session data
- Extension connects to daemon on port 7878
- No "incompatible daemon version" warning (api_version check passes)

**Alternative — manual install from budi-cursor repo:**
```bash
git clone https://github.com/siropkin/budi-cursor.git
cd budi-cursor
npm ci && npm run build
npx vsce package --no-dependencies -o cursor-budi.vsix
cursor --install-extension cursor-budi.vsix --force
```

---

## ST-14: `budi update` from prior version

**Goal:** Verify updating from 7.x to 8.0.0 works.

```bash
# 1. Install a 7.x version first
VERSION=v7.6.0 curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install-standalone.sh | bash
budi --version  # should show 7.6.0

# 2. Run update
budi update

# 3. Verify
budi --version  # should show 8.0.0
budi doctor
budi stats
```

**Pass criteria:**
- `budi update` downloads and installs 8.0.0
- Version shows 8.0.0 after update
- Daemon restarts with new version
- `budi doctor` passes all checks
- Database is migrated (schema v1) — old data may be reset due to schema change
- Autostart service is updated with new binary path

> **Note:** This test requires v8.0.0 release assets on GitHub Releases. For pre-release testing, verify the update mechanism works by inspecting the code path.

---

## ST-15: `budi enable` / `budi disable` — toggle agents

**Goal:** Verify per-agent proxy toggle works.

```bash
# 1. Disable an agent
budi disable claude
cat ~/.config/budi/agents.toml  # claude-code should show enabled = false

# 2. Verify proxy config was removed
grep ANTHROPIC_BASE_URL ~/.zshrc || echo "Not found (expected)"

# 3. Re-enable
budi enable claude
cat ~/.config/budi/agents.toml  # claude-code should show enabled = true

# 4. Verify proxy config was restored
grep ANTHROPIC_BASE_URL ~/.zshrc  # should show the env var

# 5. Check shell restart warning
# Enable should print: "Restart your terminal for changes to take effect"
```

**Pass criteria:**
- `budi disable claude` removes the proxy env var from shell profile
- `budi enable claude` adds it back
- `agents.toml` is updated correctly
- Shell restart warning is displayed after enable

---

## ST-16: `budi uninstall` — clean removal

**Goal:** Verify uninstall removes everything except binaries.

```bash
# 1. Run uninstall
budi uninstall --yes

# 2. Verify removal
ls ~/.config/budi/          # should not exist (or be empty)
ls ~/.local/share/budi/     # should not exist
budi autostart status       # should show "not installed"
grep "budi proxy" ~/.zshrc  # should not find the managed block
launchctl list | grep budi  # should not find the service (macOS)

# 3. Binaries should still exist
which budi  # should still be on PATH
```

**Pass criteria:**
- Config directory removed
- Data directory removed
- Autostart service removed
- Shell profile proxy block removed
- Statusline removed
- Binaries are NOT removed (user removes them separately)

---

## Test Matrix Summary

| Test | macOS | Linux | Windows | Requires release assets | Requires API key | Requires cloud account |
|------|-------|-------|---------|------------------------|------------------|----------------------|
| ST-01 | ✅ | — | — | No | No | No |
| ST-02 | — | ✅ | — | No | No | No |
| ST-03 | — | — | ✅ | No | No | No |
| ST-04 | ✅ | ✅ | — | **Yes** | No | No |
| ST-05 | — | — | ✅ | **Yes** | No | No |
| ST-06 | ✅ | ✅ | ✅ | No | No | No |
| ST-07 | ✅ | ✅ | ✅ | No | No | No |
| ST-08 | ✅ | ✅ | ✅ | No | No | No |
| ST-09 | ✅ | ✅ | ✅ | No | No | No |
| ST-10 | ✅ | ✅ | ✅ | No | **Yes** | No |
| ST-11 | ✅ | ✅ | ✅ | No | No | **Yes** |
| ST-12 | ✅ | ✅ | ✅ | No | No | No |
| ST-13 | ✅ | ✅ | ✅ | No | No | No |
| ST-14 | ✅ | ✅ | ✅ | **Yes** | No | No |
| ST-15 | ✅ | ✅ | ✅ | No | No | No |
| ST-16 | ✅ | ✅ | ✅ | No | No | No |

### Minimum viable pre-release test set (no release assets needed)

Run at least these before tagging v8.0.0:

1. **ST-01** or **ST-02** — fresh source install
2. **ST-06** — init + agent selection
3. **ST-07** — doctor
4. **ST-08** — stats/sessions/health
5. **ST-10a** — proxy flow with Claude Code (requires API key)
6. **ST-12** — autostart survives reboot
7. **ST-15** — enable/disable toggle
