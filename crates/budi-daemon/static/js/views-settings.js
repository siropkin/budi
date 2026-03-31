/* ===== Settings Page ===== */

let settingsData = null;

async function loadSettingsData(signal) {
  const opts = signal ? { signal } : {};
  const [health, schema, syncStatus, integrations] = await Promise.all([
    fetch('/health', opts).then(fetchOk).catch(() => ({ ok: false })),
    fetch('/admin/schema', opts).then(fetchOk).catch(() => ({ current: '?', target: '?' })),
    fetch('/sync/status', opts).then(fetchOk).catch(() => ({ syncing: false })),
    fetch('/health/integrations', opts).then(fetchOk).catch(() => ({})),
  ]);
  settingsData = { health, schema, syncStatus, integrations };
}

function fmtSyncTime(iso) {
  if (!iso) return 'Never';
  const d = new Date(iso);
  const now = new Date();
  const diffMs = now - d;
  const diffMin = Math.floor(diffMs / 60000);
  if (diffMin < 1) return 'Just now';
  if (diffMin < 60) return diffMin + 'm ago';
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return diffHr + 'h ago';
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function fmtPath(p) {
  if (!p) return '--';
  // Shorten home dir
  const home = p.match(/^(\/Users\/[^/]+|\/home\/[^/]+)/);
  if (home) return '~' + p.slice(home[0].length);
  return p;
}

function renderSettingsView(content) {
  const d = settingsData;
  if (!d) { content.innerHTML = '<div class="empty">Loading settings...</div>'; return; }

  const h = d.health;
  const s = d.schema;
  const needsMigration = s && s.needs_migration;
  const syncing = d.syncStatus.syncing;
  const lastSynced = d.syncStatus.last_synced;
  const ig = d.integrations || {};
  const db = ig.database || {};
  const paths = ig.paths || {};

  content.innerHTML = `
    <div class="panel section-mb" style="font-size:0.82rem;color:var(--text-muted)">
      <h2>Help</h2>
      <div style="display:flex;flex-direction:column;gap:4px">
        <div>Run <code style="background:var(--bg);padding:2px 6px;border-radius:4px">budi init</code> to set up hooks, OTEL, MCP, and statusline</div>
        <div>Run <code style="background:var(--bg);padding:2px 6px;border-radius:4px">budi doctor</code> to diagnose issues</div>
        <div>Run <code style="background:var(--bg);padding:2px 6px;border-radius:4px">budi update</code> to update to the latest version</div>
        <div style="margin-top:4px"><a href="https://github.com/siropkin/budi" target="_blank">Documentation</a> &middot; <a href="https://github.com/siropkin/budi/issues" target="_blank">Report an Issue</a></div>
      </div>
    </div>

    <div class="settings-grid section-mb">
      <div class="panel">
        <h2>Status</h2>
        <div class="settings-item">
          <span class="settings-key">Version</span>
          <span class="settings-val">${esc(h.version || '?')}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Last Sync</span>
          <span class="settings-val ${syncing ? 'warn' : ''}">${syncing ? 'Syncing now...' : fmtSyncTime(lastSynced)}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Providers</span>
          <span class="settings-val">${registeredProviders.map(p => esc(p.display_name)).join(', ') || '--'}</span>
        </div>
      </div>
      <div class="panel">
        <h2>Integrations</h2>
        <div class="settings-item">
          <span class="settings-key">Claude Code Hooks</span>
          <span class="settings-val ${ig.claude_code_hooks ? 'ok' : 'warn'}">${ig.claude_code_hooks ? 'Active' : 'Not set up'}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Cursor Hooks</span>
          <span class="settings-val ${ig.cursor_hooks ? 'ok' : ''}">${ig.cursor_hooks ? 'Active' : 'Not detected'}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">MCP Server</span>
          <span class="settings-val ${ig.mcp_server ? 'ok' : 'warn'}">${ig.mcp_server ? 'Active' : 'Not set up'}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">OTEL (Exact Cost)</span>
          <span class="settings-val ${ig.otel ? 'ok' : 'warn'}">${ig.otel ? 'Active' : 'Not set up'}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Statusline</span>
          <span class="settings-val ${ig.statusline ? 'ok' : ''}">${ig.statusline ? 'Active' : 'Not set up'}</span>
        </div>
      </div>
    </div>

    <div class="settings-grid section-mb">
      <div class="panel">
        <h2>Database</h2>
        <div class="settings-item">
          <span class="settings-key">Schema</span>
          <span class="settings-val ${needsMigration ? 'warn' : ''}">v${esc(String(s.current || '?'))}${needsMigration ? ' (needs v' + esc(String(s.target)) + ')' : ''}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Size</span>
          <span class="settings-val">${db.size_mb != null ? db.size_mb + ' MB' : '--'}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Records</span>
          <span class="settings-val">${db.records != null ? fmtNum(db.records) : '--'}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">First Record</span>
          <span class="settings-val">${db.first_record ? fmtDate(db.first_record) : '--'}</span>
        </div>
      </div>
      <div class="panel">
        <h2>Paths</h2>
        <div class="settings-item">
          <span class="settings-key">Database</span>
          <span class="settings-val" style="font-size:0.75rem" title="${esc(paths.database)}">${esc(fmtPath(paths.database))}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Config</span>
          <span class="settings-val" style="font-size:0.75rem" title="${esc(paths.config)}">${esc(fmtPath(paths.config))}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Claude Settings</span>
          <span class="settings-val" style="font-size:0.75rem" title="${esc(paths.claude_settings)}">${esc(fmtPath(paths.claude_settings))}</span>
        </div>
        <div class="settings-item">
          <span class="settings-key">Cursor Hooks</span>
          <span class="settings-val" style="font-size:0.75rem" title="${esc(paths.cursor_hooks)}">${esc(fmtPath(paths.cursor_hooks))}</span>
        </div>
      </div>
    </div>

    <div style="display:flex;gap:8px;flex-wrap:wrap;margin-bottom:16px">
      <button class="btn btn-secondary" id="syncRecentBtn">Sync Recent Data</button>
      <button class="btn btn-secondary" id="fullResyncBtn">Full Re-sync</button>
      ${needsMigration ? '<button class="btn btn-secondary" id="migrateBtn">Migrate Database</button>' : ''}
      <button class="btn btn-secondary" id="checkUpdateBtn">Check for Updates</button>
    </div>

    <div id="settingsLog" class="panel section-mb" style="display:none">
      <h2>Log</h2>
      <div id="settingsLogContent" style="font-size:0.82rem;color:var(--text-muted);white-space:pre-wrap"></div>
    </div>
  `;

  bindSettingsHandlers();
}

function clearSettingsLog() {
  const log = $('#settingsLog');
  const logContent = $('#settingsLogContent');
  if (log) log.style.display = 'none';
  if (logContent) logContent.textContent = '';
}

function settingsLog(msg) {
  const log = $('#settingsLog');
  const logContent = $('#settingsLogContent');
  if (log && logContent) {
    log.style.display = 'block';
    logContent.textContent += msg + '\n';
  }
}

function setLastSyncDisplay(text, warn) {
  const syncEl = [...$$('.settings-key')].find(e => e.textContent === 'Last Sync');
  if (syncEl) {
    const valEl = syncEl.nextElementSibling;
    if (valEl) {
      valEl.textContent = text;
      valEl.className = 'settings-val' + (warn ? ' warn' : '');
    }
  }
}

async function refreshSettingsStatus() {
  const syncStatus = await fetch('/sync/status').then(fetchOk).catch(() => null);
  if (syncStatus && settingsData) {
    settingsData.syncStatus = syncStatus;
    const syncEl = [...$$('.settings-key')].find(e => e.textContent === 'Last Sync');
    if (syncEl) {
      const valEl = syncEl.nextElementSibling;
      if (valEl) {
        valEl.textContent = syncStatus.syncing ? 'Syncing now...' : fmtSyncTime(syncStatus.last_synced);
        valEl.className = 'settings-val' + (syncStatus.syncing ? ' warn' : '');
      }
    }
  }
}

function bindSettingsHandlers() {
  const syncRecentBtn = $('#syncRecentBtn');
  if (syncRecentBtn) syncRecentBtn.addEventListener('click', async () => {
    clearSettingsLog();
    syncRecentBtn.textContent = 'Syncing...';
    syncRecentBtn.disabled = true;
    setLastSyncDisplay('Syncing now...', true);
    settingsLog('Starting recent sync...');
    try {
      const resp = await fetch('/sync', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"migrate":true}' });
      const r = await resp.json();
      if (resp.status === 409) {
        settingsLog('Sync already in progress. Try again in a moment.');
      } else if (r.files_synced != null) {
        settingsLog('Done: ' + r.files_synced + ' files, ' + (r.messages_ingested || 0) + ' messages');
      } else {
        settingsLog('Done');
      }
    } catch (e) {
      settingsLog('Failed: ' + e.message);
    }
    syncRecentBtn.textContent = 'Sync Recent Data';
    syncRecentBtn.disabled = false;
    await refreshSettingsStatus();
  });

  const fullResyncBtn = $('#fullResyncBtn');
  if (fullResyncBtn) fullResyncBtn.addEventListener('click', async () => {
    clearSettingsLog();
    fullResyncBtn.textContent = 'Re-syncing...';
    fullResyncBtn.disabled = true;
    setLastSyncDisplay('Syncing now...', true);
    settingsLog('Resetting sync state...');
    try {
      const resetResp = await fetch('/sync/reset', { method: 'POST' });
      if (resetResp.status === 409) {
        settingsLog('Sync already in progress. Try again in a moment.');
      } else if (!resetResp.ok) {
        settingsLog('Reset failed: ' + resetResp.statusText);
      } else {
        settingsLog('Re-ingesting all history...');
        const resp = await fetch('/sync/all', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"migrate":true}' });
        const r = await resp.json();
        if (resp.status === 409) {
          settingsLog('Sync already in progress. Try again in a moment.');
        } else {
          settingsLog('Done: ' + (r.files_synced || 0) + ' files, ' + (r.messages_ingested || 0) + ' messages');
        }
      }
    } catch (e) {
      settingsLog('Failed: ' + e.message);
    }
    fullResyncBtn.textContent = 'Full Re-sync';
    fullResyncBtn.disabled = false;
    await refreshSettingsStatus();
  });

  const migrateBtn = $('#migrateBtn');
  if (migrateBtn) migrateBtn.addEventListener('click', async () => {
    clearSettingsLog();
    migrateBtn.textContent = 'Migrating...';
    migrateBtn.disabled = true;
    settingsLog('Running migration...');
    try {
      const resp = await fetch('/admin/migrate', { method: 'POST' });
      const r = await resp.json();
      if (resp.status === 409) {
        settingsLog('Another operation in progress. Try again in a moment.');
      } else if (r.migrated) {
        settingsLog('Migrated from v' + r.from + ' to v' + r.current);
      } else {
        settingsLog('Already up to date (v' + r.current + ')');
      }
    } catch (e) {
      settingsLog('Failed: ' + e.message);
    }
    migrateBtn.textContent = 'Migrate Database';
    migrateBtn.disabled = false;
    await refreshSettingsStatus();
  });

  const checkUpdateBtn = $('#checkUpdateBtn');
  if (checkUpdateBtn) checkUpdateBtn.addEventListener('click', async () => {
    clearSettingsLog();
    checkUpdateBtn.textContent = 'Checking...';
    checkUpdateBtn.disabled = true;
    settingsLog('Checking for updates...');
    try {
      const r = await fetch('/health/check-update').then(fetchOk);
      if (r.error) {
        settingsLog(r.error);
      } else if (r.up_to_date) {
        settingsLog('You are on the latest version (v' + r.current + ')');
      } else if (r.latest) {
        settingsLog('Update available: v' + r.latest + ' (current: v' + r.current + '). Run: budi update');
      } else {
        settingsLog('Could not determine latest version');
      }
    } catch (e) {
      settingsLog('Could not check for updates: ' + e.message);
    }
    checkUpdateBtn.textContent = 'Check for Updates';
    checkUpdateBtn.disabled = false;
  });
}
