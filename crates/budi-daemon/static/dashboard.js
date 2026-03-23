const $ = (s, el) => (el || document).querySelector(s);
const $$ = (s, el) => [...(el || document).querySelectorAll(s)];
function esc(s) { if (s == null) return ''; const d = document.createElement('div'); d.textContent = String(s); return d.innerHTML; }

function getCurrentView() {
  const path = window.location.pathname;
  if (path.includes('/insights')) return 'insights';
  if (path.includes('/plans')) return 'plans';
  if (path.includes('/prompts')) return 'prompts';
  if (path.includes('/setup')) return 'setup';
  return 'stats';
}

let currentPeriod = 'today';
let currentView = getCurrentView();
const DEFAULT_TABLE_ROWS = 15;
const DEFAULT_CHART_ROWS = 15;

// Session table state (server-side paginated)
let lastSessionData = [];
let sessionSortCol = 'last_seen';
let sessionSortAsc = false;
let sessionShowCount = DEFAULT_TABLE_ROWS;
let sessionTotalCount = 0;

// Config table state
let lastConfigData = [];
let configSortCol = 'est_tokens';
let configSortAsc = false;
let configShowCount = DEFAULT_TABLE_ROWS;

// Project config table state
let lastProjectConfigData = [];
let projectConfigSortCol = 'tokens';
let projectConfigSortAsc = false;
let projectConfigShowCount = DEFAULT_TABLE_ROWS;

// History table state
let lastHistoryData = [];
let historySortCol = 'timestamp';
let historySortAsc = false;
let historyShowCount = DEFAULT_TABLE_ROWS;

// Plans table state
let lastPlansData = [];
let plansSortCol = 'modified';
let plansSortAsc = false;
let plansShowCount = DEFAULT_TABLE_ROWS;

// Plugins table state
let lastPluginsData = [];
let pluginsSortCol = 'name';
let pluginsSortAsc = true;
let pluginsShowCount = DEFAULT_TABLE_ROWS;

// Permissions table state
let lastPermissionsData = [];
let permissionsSortCol = 'scope';
let permissionsSortAsc = true;
let permissionsShowCount = DEFAULT_TABLE_ROWS;

// Search state
let sessionsSearchTerm = '';
let configSearchTerm = '';
let projectConfigSearchTerm = '';
let pluginsSearchTerm = '';
let permissionsSearchTerm = '';
let plansSearchTerm = '';
let promptsSearchTerm = '';

// Provider filter state
let currentProvider = '';
let providersData = [];
let registeredProviders = [];

// Cached data
let dataLoaded = false;
let statsData = null;
let setupData = null;
let plansData = null;
let promptsData = null;
let activityData = null;
let activeSessionsData = null;

// Cached render intermediates for stats view
let cachedSortedModels = [];
let cachedActivityChartTitle = '';
let cachedMergedConfigFiles = [];

function dateRange(period) {
  const now = new Date();
  const y = now.getFullYear(), m = now.getMonth(), d = now.getDate();
  const toISO = dt => dt.toISOString();
  switch (period) {
    case 'today': return { since: toISO(new Date(y, m, d)) };
    case 'week': {
      const dow = now.getDay();
      const mondayOffset = dow === 0 ? 6 : dow - 1;
      return { since: toISO(new Date(y, m, d - mondayOffset)) };
    }
    case 'month': return { since: toISO(new Date(y, m, 1)) };
    case 'all': return {};
  }
}

function granularityForPeriod(period) {
  switch (period) {
    case 'today': return 'hour';
    case 'week': return 'day';
    case 'month': return 'day';
    case 'all': return 'month';
  }
}

function qs(params) {
  const p = new URLSearchParams();
  for (const [k,v] of Object.entries(params)) if (v != null) p.set(k, v);
  const s = p.toString();
  return s ? '?' + s : '';
}

function fmtNum(n) {
  if (n >= 1_000_000_000) return (n / 1_000_000_000).toFixed(1) + 'B';
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K';
  return String(n);
}
function fmtTokensHtml(tokens) {
  const str = tokens >= 1000 ? (tokens / 1000).toFixed(1) + 'K' : String(tokens);
  return tokens > 2000 ? `<span style="color:var(--accent4)">${str}</span>` : str;
}
function fmtCost(n) {
  if (n >= 1000) return '$' + (n / 1000).toFixed(1) + 'K';
  if (n >= 100) return '$' + n.toFixed(0);
  if (n >= 1) return '$' + n.toFixed(2);
  if (n > 0) return '$' + n.toFixed(3);
  return '$0.00';
}
function fmtCostTokens(_, item) {
  const cost = (item.cost_cents || 0) / 100;
  return fmtCost(cost);
}
function fmtDuration(firstSeen, lastSeen) {
  if (!firstSeen || !lastSeen) return '--';
  const ms = new Date(lastSeen) - new Date(firstSeen);
  if (ms < 0) return '--';
  const mins = Math.floor(ms / 60000);
  if (mins < 1) return '<1m';
  if (mins < 60) return mins + 'm';
  return Math.floor(mins / 60) + 'h ' + (mins % 60) + 'm';
}
function durationMs(a, b) { if (!a || !b) return 0; return Math.max(0, new Date(b) - new Date(a)); }
function fmtDate(iso) {
  if (!iso) return '--';
  const d = new Date(iso), now = new Date();
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const target = new Date(d.getFullYear(), d.getMonth(), d.getDate());
  const diff = Math.floor((today - target) / 86400000);
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (diff === 0) return `Today ${time}`;
  if (diff === 1) return `Yesterday ${time}`;
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + time;
}
function fmtDateFromMs(ms) {
  if (!ms) return '--';
  return fmtDate(new Date(ms).toISOString());
}
function filterBySearch(data, term, fieldsFn) {
  if (!term) return data;
  const lower = term.toLowerCase();
  return data.filter(item => fieldsFn(item).some(f => (f || '').toLowerCase().includes(lower)));
}

function shortenDir(dir) { if (!dir) return '--'; return dir.replace(/^\/Users\/[^/]+/, '~').replace(/^\/home\/[^/]+/, '~'); }
function projectName(dir) { if (!dir) return '--'; const s = shortenDir(dir); return s.split('/').pop() || s; }
function repoName(id) { if (!id) return '--'; return id.split('/').pop() || id; }

function formatModelName(raw) {
  if (!raw || raw === 'unknown') return 'Unknown';
  if (raw === '<synthetic>') return 'Unknown';
  // Handle comma-separated multi-model strings (from Cursor sessions using multiple models)
  if (raw.includes(',')) return raw.split(',').map(m => formatModelName(m.trim())).join(', ');
  const n = raw.toLowerCase().trim();

  // Parse suffixes: -high, -max, -thinking, -codex, -preview
  function parseSuffixes(s) {
    let thinking = false, effort = '', codex = false, preview = false;
    if (s.includes('-thinking')) { thinking = true; s = s.replace('-thinking', ''); }
    if (s.includes('-max')) { effort = 'Max'; s = s.replace('-max', ''); }
    else if (s.includes('-high')) { effort = 'High'; s = s.replace('-high', ''); }
    if (s.includes('-codex')) { codex = true; s = s.replace('-codex', ''); }
    if (s.includes('-preview')) { preview = true; s = s.replace('-preview', ''); }
    let parts = [];
    if (codex) parts.push('Codex');
    if (thinking) parts.push('Thinking');
    if (effort) parts.push(effort + ' Effort');
    if (preview) parts.push('Preview');
    return { base: s, suffix: parts.length ? ' (' + parts.join(', ') + ')' : '' };
  }

  // Claude models — from Claude Code ("claude-opus-4-6") and Cursor ("claude-4.5-opus-high-thinking")
  if (n.includes('claude') || n.includes('opus') || n.includes('sonnet') || n.includes('haiku')) {
    const { base, suffix } = parseSuffixes(n);
    // Extract version: "4.6", "4-6", "4.5", "4" etc.
    const verMatch = base.match(/(\d+)[\._-]?(\d+)?/);
    const ver = verMatch ? verMatch[1] + (verMatch[2] ? '.' + verMatch[2] : '') : '';
    let family = '';
    if (base.includes('opus')) family = 'Opus';
    else if (base.includes('sonnet')) family = 'Sonnet';
    else if (base.includes('haiku')) family = 'Haiku';
    return ('Claude ' + (ver ? ver + ' ' : '') + family + suffix).trim();
  }

  // GPT models — "gpt-5", "gpt-5.2-codex-high"
  if (/^gpt[._-]?\d/.test(n)) {
    const { base, suffix } = parseSuffixes(n);
    const verMatch = base.match(/(\d+[\.\d]*)/);
    const ver = verMatch ? verMatch[1] : '';
    return 'GPT-' + ver + suffix;
  }

  // o-series (o1, o3-mini, etc.)
  if (/^o\d/.test(n)) return raw;

  // Gemini — "gemini-3-pro", "gemini-3-pro-preview"
  if (n.startsWith('gemini')) {
    const { base, suffix } = parseSuffixes(n);
    const rest = base.replace(/^gemini[._-]?/, '').replace(/-/g, ' ').trim();
    const parts = rest.split(' ').map(w => w.charAt(0).toUpperCase() + w.slice(1));
    return 'Gemini ' + parts.join(' ') + suffix;
  }

  // Cursor internal
  if (n === 'default') return 'Auto';
  if (n.startsWith('composer-')) return 'Composer ' + raw.slice(9);

  return raw;
}



const TOOL_COLORS = {
  Read: '#58a6ff', Edit: '#3fb950', Write: '#d2a8ff', Bash: '#f0883e',
  Grep: '#f778ba', Glob: '#79c0ff', Agent: '#ffd33d', default: '#8b949e'
};
function toolColor(name) { return TOOL_COLORS[name] || TOOL_COLORS.default; }

const CHART_PALETTE = ['#58a6ff', '#3fb950', '#d2a8ff', '#f0883e', '#f778ba', '#ffd33d', '#79c0ff', '#a5d6ff', '#7ee787', '#ff9bce'];
function paletteColor(i) { return CHART_PALETTE[i % CHART_PALETTE.length]; }

function modelColor(name) {
  const n = (name || '').toLowerCase();
  if (n.includes('opus')) return '#d2a8ff';
  if (n.includes('sonnet')) return '#58a6ff';
  if (n.includes('haiku')) return '#3fb950';
  if (n.includes('gpt')) return '#f0883e';
  if (n.includes('gemini')) return '#ffd33d';
  if (n.includes('o3') || n.includes('o1')) return '#f778ba';
  if (n.includes('auto') || n.includes('composer')) return '#79c0ff';
  return '#8b949e';
}

async function fetchSessions(limit, offset) {
  const range = dateRange(currentPeriod);
  const params = { ...range, sort_by: sessionSortCol, sort_asc: sessionSortAsc, limit, offset };
  if (sessionsSearchTerm) params.search = sessionsSearchTerm;
  const result = await fetch('/analytics/sessions' + qs(params)).then(r => r.json()).catch(() => ({sessions:[],total_count:0}));
  return result;
}

// Track which page data has been loaded to avoid re-fetching.
let loadedPages = {};

async function loadAllData() {
  // Fetch registered providers once (lightweight, doesn't change per period).
  if (registeredProviders.length === 0) {
    registeredProviders = await fetch('/analytics/registered-providers').then(r => r.json()).catch(() => []);
  }
  // Only fetch what the current view needs. Other pages lazy-load on navigation.
  await loadStatsData();
  dataLoaded = true;
}

async function loadStatsData(signal) {
  const range = dateRange(currentPeriod);
  // Provider filter removed — always show all agents
  const q = qs(range);
  const gran = granularityForPeriod(currentPeriod);
  const tzOffset = -new Date().getTimezoneOffset();
  const opts = signal ? { signal } : {};

  const sessionsQ = q + (q ? '&' : '?') + `sort_by=${sessionSortCol}&sort_asc=${sessionSortAsc}&limit=${DEFAULT_TABLE_ROWS}${sessionsSearchTerm ? '&search=' + encodeURIComponent(sessionsSearchTerm) : ''}`;
  const [summary, sessionsResult, cwds, cost, models, activityChart, activeSessions, providers, contextUsage, interactionModes, topTools, mcpTools, branches] = await Promise.all([
    fetch('/analytics/summary' + q, opts).then(r => r.json()),
    fetch('/analytics/sessions' + sessionsQ, opts).then(r => r.json()).catch(() => ({sessions:[],total_count:0})),
    fetch('/analytics/projects' + q + (q ? '&' : '?') + 'limit=' + DEFAULT_CHART_ROWS, opts).then(r => r.json()),
    fetch('/analytics/cost' + q, opts).then(r => r.json()),
    fetch('/analytics/models' + q, opts).then(r => r.json()),
    fetch('/analytics/activity' + q + (q ? '&' : '?') + 'granularity=' + gran + '&tz_offset=' + tzOffset, opts).then(r => r.json()),
    fetch('/analytics/active-sessions', opts).then(r => r.json()).catch(() => []),
    fetch('/analytics/providers' + qs(dateRange(currentPeriod)), opts).then(r => r.json()).catch(() => []),
    fetch('/analytics/context-usage' + q, opts).then(r => r.json()).catch(() => ({avg_usage_pct:0,max_usage_pct:0,sessions_over_80_pct:0,total_sessions_with_data:0})),
    fetch('/analytics/interaction-modes' + q, opts).then(r => r.json()).catch(() => []),
    fetch('/analytics/top-tools' + q, opts).then(r => r.json()).catch(() => []),
    fetch('/analytics/mcp-tools' + q, opts).then(r => r.json()).catch(() => []),
    fetch('/analytics/branches' + q, opts).then(r => r.json()).catch(() => []),
  ]);

  const sessions = sessionsResult.sessions || [];
  sessionTotalCount = sessionsResult.total_count || 0;
  statsData = { summary, sessions, cwds, cost, models, activityChart, contextUsage, interactionModes, topTools, mcpTools, branches };
  activeSessionsData = activeSessions;
  lastSessionData = sessions;
  providersData = providers;
  updateProviderFilter();

  // Merge models with same normalized display name per provider
  const modelMap = {};
  for (const m of models) {
    const modelName = formatModelName(m.model);
    const prov = m.provider || 'claude_code';
    const key = prov + '/' + modelName;
    const provDisplay = (registeredProviders.find(rp => rp.name === prov) || {}).display_name || prov;
    if (!modelMap[key]) {
      modelMap[key] = { model: modelName, provider: prov, provider_display: provDisplay, message_count: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0, cost_cents: 0 };
    }
    modelMap[key].message_count += m.message_count;
    modelMap[key].input_tokens += m.input_tokens;
    modelMap[key].output_tokens += m.output_tokens;
    modelMap[key].cache_read_tokens += m.cache_read_tokens;
    modelMap[key].cache_creation_tokens += m.cache_creation_tokens;
    modelMap[key].cost_cents += m.cost_cents || 0;
  }
  let sortedModels = Object.values(modelMap);
  sortedModels.sort((a, b) => (b.cost_cents || 0) - (a.cost_cents || 0));
  cachedSortedModels = sortedModels.slice(0, DEFAULT_CHART_ROWS);
  cachedActivityChartTitle = currentPeriod === 'today' ? 'Activity (Hourly)'
    : currentPeriod === 'week' ? 'Activity (Daily)'
    : currentPeriod === 'month' ? 'Activity (Daily)'
    : 'Activity (Monthly)';
  sessionShowCount = DEFAULT_TABLE_ROWS;

}

async function loadSetupData() {
  if (setupData) return; // already loaded
  const [configFiles, memory, plugins, permissions, integrations] = await Promise.all([
    fetch('/analytics/config-files').then(r => r.json()).catch(() => []),
    fetch('/analytics/memory').then(r => r.json()).catch(() => []),
    fetch('/analytics/plugins').then(r => r.json()).catch(() => []),
    fetch('/analytics/permissions').then(r => r.json()).catch(() => ({default_mode:'default',rules:[]})),
    fetch('/system/integrations').then(r => r.json()).catch(() => ({})),
  ]);
  setupData = { configFiles, memory, plugins, permissions, integrations };
  const memoryAsConfig = memory.map(m => ({
    path: m.path || '', project: m.project || '', file_type: 'memory',
    size_bytes: m.size_bytes || 0, est_tokens: m.est_tokens || 0,
  }));
  cachedMergedConfigFiles = [...configFiles, ...memoryAsConfig];
  lastConfigData = cachedMergedConfigFiles;
  const byProject = {};
  for (const f of cachedMergedConfigFiles) {
    const p = projectName(f.project);
    if (!byProject[p]) byProject[p] = { tokens: 0, count: 0, types: new Set() };
    byProject[p].tokens += f.est_tokens;
    byProject[p].count++;
    byProject[p].types.add(f.file_type);
  }
  lastProjectConfigData = Object.entries(byProject).map(([proj, info]) => ({
    project: proj, tokens: info.tokens, count: info.count, typeList: [...info.types]
  }));
  lastPluginsData = plugins;
  lastPermissionsData = (permissions.rules || []);
  configShowCount = DEFAULT_TABLE_ROWS;
  projectConfigShowCount = DEFAULT_TABLE_ROWS;
  pluginsShowCount = DEFAULT_TABLE_ROWS;
  permissionsShowCount = DEFAULT_TABLE_ROWS;
}

let plansTotalCount = 0;
let promptsTotalCount = 0;

async function fetchPlans(limit, offset) {
  const params = { limit, offset };
  if (plansSearchTerm) params.search = plansSearchTerm;
  return fetch('/analytics/plans' + qs(params)).then(r => r.json()).catch(() => ({plans:[],total_count:0}));
}

async function fetchPrompts(limit, offset) {
  const params = { limit, offset };
  if (promptsSearchTerm) params.search = promptsSearchTerm;
  return fetch('/analytics/prompts' + qs(params)).then(r => r.json()).catch(() => ({total_count:0,entries:[]}));
}

async function loadPlansData() {
  const result = await fetchPlans(DEFAULT_TABLE_ROWS, 0);
  lastPlansData = result.plans || [];
  plansTotalCount = result.total_count || 0;
  plansShowCount = lastPlansData.length;
}

async function loadPromptsData() {
  const result = await fetchPrompts(DEFAULT_TABLE_ROWS, 0);
  lastHistoryData = result.entries || [];
  promptsTotalCount = result.total_count || 0;
  historyShowCount = lastHistoryData.length;
}

/* ===== Provider Filter ===== */
function updateProviderFilter() {
  const filter = $('#providerFilter');
  const select = $('#providerSelect');
  if (registeredProviders.length > 1) {
    filter.style.display = '';
    const currentVal = select.value;
    select.innerHTML = '<option value="">All Agents</option>';
    // Use registered providers for the options, show all even if no data
    registeredProviders.forEach(rp => {
      const opt = document.createElement('option');
      opt.value = rp.name;
      opt.textContent = rp.display_name;
      select.appendChild(opt);
    });
    select.value = currentVal || '';
  } else {
    filter.style.display = 'none';
  }
}

function hasProvider(name) {
  return providersData.some(p => p.provider === name);
}
function ccOnlyLabel() {
  return registeredProviders.length > 1 ? ' <span style="font-size:0.7rem;font-weight:normal;color:var(--text-muted)">(Claude Code only)</span>' : '';
}

const providerColors = {
  claude_code: '#f0883e',
  cursor: '#58a6ff',
};

function agentBarData() {
  return registeredProviders.map(rp => {
    const stats = providersData.find(p => p.provider === rp.name);
    const cost_cents = stats ? (stats.total_cost_cents > 0 ? stats.total_cost_cents : stats.estimated_cost * 100) : 0;
    return {
      provider: rp.name,
      display_name: rp.display_name,
      input_tokens: stats ? stats.input_tokens : 0,
      output_tokens: stats ? stats.output_tokens : 0,
      cost_cents,
    };
  }).filter(p => p.cost_cents > 0 || p.input_tokens > 0 || p.output_tokens > 0);
}

/* ===== Render: Agents Tile ===== */
function renderAgentsTile() {
  if (registeredProviders.length <= 1) return '';
  // Build display data: merge registered providers with stats data
  const agentData = registeredProviders.map(rp => {
    const stats = providersData.find(p => p.provider === rp.name);
    return stats || { provider: rp.name, display_name: rp.display_name, session_count: 0, input_tokens: 0, output_tokens: 0, cache_creation_tokens: 0, cache_read_tokens: 0, total_cost_cents: 0, estimated_cost: 0, total_lines_added: 0, total_lines_removed: 0 };
  });
  const total = agentData.reduce((s, p) => s + p.input_tokens + p.output_tokens + p.cache_creation_tokens + p.cache_read_tokens, 0);
  return `<div class="panel section-mb">
    <h2>Agents</h2>
    <div style="display:flex;flex-direction:column;gap:8px">
      ${agentData.map(p => {
        const tokens = p.input_tokens + p.output_tokens + p.cache_creation_tokens + p.cache_read_tokens;
        const pct = total > 0 ? (tokens / total * 100) : 0;
        const color = providerColors[p.provider] || '#8b949e';
        return `<div style="display:flex;align-items:center;gap:10px;">
          <div style="width:10px;height:10px;border-radius:50%;background:${color};flex-shrink:0"></div>
          <div style="flex:1;min-width:0">
            <div style="display:flex;justify-content:space-between;font-size:0.85rem">
              <span style="font-weight:500">${esc(p.display_name)}</span>
              <span style="color:var(--text-muted)">${fmtNum(p.session_count)} sessions</span>
            </div>
            <div style="height:6px;background:var(--border);border-radius:3px;margin-top:3px">
              <div style="height:100%;width:${pct}%;background:${color};border-radius:3px"></div>
            </div>
            <div style="display:flex;justify-content:space-between;font-size:0.75rem;color:var(--text-muted);margin-top:2px">
              <span>${fmtNum(tokens)} tokens</span>
              <span>${fmtCost(p.total_cost_cents > 0 ? p.total_cost_cents / 100 : p.estimated_cost)}</span>
            </div>
          </div>
        </div>`;
      }).join('')}
    </div>
  </div>`;
}

/* ===== Render: New Analytics Cards ===== */
function renderContextUsageCard(ctx) {
  const pct = ctx && ctx.total_sessions_with_data > 0 ? Math.round(ctx.avg_usage_pct) : 0;
  const color = pct >= 80 ? 'var(--danger)' : pct >= 60 ? 'var(--accent4)' : 'var(--accent2)';
  const over80 = ctx ? ctx.sessions_over_80_pct : 0;
  return `<div class="card">
    <div class="label">Context Window</div>
    <div class="value" style="color:${color}">${pct}%</div>
    <div class="sub">${over80} sessions &gt;80%</div>
  </div>`;
}

function renderLinesChangedCard(providers) {
  const totalAdded = providers.reduce((s, p) => s + (p.total_lines_added || 0), 0);
  const totalRemoved = providers.reduce((s, p) => s + (p.total_lines_removed || 0), 0);
  const total = totalAdded + totalRemoved;
  return `<div class="card">
    <div class="label">Lines Changed</div>
    <div class="value">${fmtNum(total)}</div>
    <div class="sub"><span style="color:var(--accent2)">+${fmtNum(totalAdded)}</span> / <span style="color:var(--danger)">-${fmtNum(totalRemoved)}</span> (Cursor only)</div>
  </div>`;
}

function renderInteractionModesCard(modes) {
  const count = modes ? modes.length : 0;
  const items = count > 0 ? modes.map(([mode, cnt]) => `${mode}: ${fmtNum(cnt)}`).join(', ') : 'No data';
  return `<div class="card">
    <div class="label">Interaction Modes</div>
    <div class="value">${fmtNum(count)}</div>
    <div class="sub">${items}</div>
  </div>`;
}

/* ===== Render: Active Sessions ===== */
function renderActiveSessions(activeSessions) {
  const ccOnly = ccOnlyLabel();
  const alive = activeSessions.filter(s => s.is_alive);
  if (alive.length > 0) {
    const counts = {};
    alive.forEach(s => { const n = projectName(s.cwd); counts[n] = (counts[n] || 0) + 1; });
    const projects = Object.entries(counts).map(([n, c]) => c > 1 ? `${n} (${c})` : n).join(', ');
    return `<div class="active-sessions">
      <span class="active-dot green"></span>
      <span class="active-label">${alive.length} active session${alive.length > 1 ? 's' : ''}${ccOnly}</span>
      <span class="active-projects">${esc(projects)}</span>
    </div>`;
  }
  return `<div class="active-sessions">
    <span class="active-dot gray"></span>
    <span class="active-label">No active sessions${ccOnly}</span>
  </div>`;
}

/* ===== Render: Summary Cards ===== */
function renderCards(s, cost) {
  const totalTokens = s.total_input_tokens + s.total_output_tokens + s.total_cache_creation_tokens + s.total_cache_read_tokens;
  const totalIn = s.total_input_tokens + s.total_cache_creation_tokens + s.total_cache_read_tokens;
  return `
  <div class="cards">
    <div class="card">
      <div class="label">Est. Cost</div>
      <div class="value cost-value">${fmtCost(cost.total_cost)}</div>
      <div class="sub">${fmtCost(cost.input_cost + cost.cache_write_cost + cost.cache_read_cost)} input / ${fmtCost(cost.output_cost)} output</div>
    </div>
    <div class="card">
      <div class="label">Total Tokens</div>
      <div class="value">${fmtNum(totalTokens)}</div>
      <div class="sub">${fmtNum(totalIn)} input / ${fmtNum(s.total_output_tokens)} output</div>
    </div>
    <div class="card">
      <div class="label">Sessions</div>
      <div class="value">${fmtNum(s.total_sessions)}</div>
      <div class="sub">${fmtNum(s.total_user_messages)} prompts / ${fmtNum(s.total_assistant_messages)} responses</div>
    </div>
  </div>`;
}

/* ===== Render: Bar Chart ===== */
function barTooltip(item, labelFn, valueFn) {
  const label = labelFn(item, true);
  const cost = (item.cost_cents || 0) / 100;
  const inp = item.input_tokens || 0;
  const outp = item.output_tokens || 0;
  if (inp || outp) return `${label}: ${fmtCost(cost)} — ${fmtNum(inp)} input, ${fmtNum(outp)} output`;
  if (valueFn) return `${label}: ${fmtNum(valueFn(item))} calls`;
  return label;
}

function renderBarChart(items, labelFn, valueFn, colorFn, emptyMsg, formatFn) {
  if (!items.length) return `<div class="empty">${esc(emptyMsg)}</div>`;
  const fmt = formatFn || ((v) => fmtNum(v));
  const max = Math.max(...items.map(valueFn));
  return `<div class="bar-chart">${items.map((item, i) => `
    <div class="bar-row">
      <div class="bar-tooltip">${esc(barTooltip(item, labelFn, valueFn))}</div>
      <div class="bar-label">${esc(labelFn(item, false))}</div>
      <div class="bar-track">
        <div class="bar-fill" style="width:${max > 0 ? (valueFn(item)/max*100) : 0}%;background:${colorFn(item, i)}"></div>
      </div>
      <div class="bar-count">${fmt(valueFn(item), item)}</div>
    </div>`).join('')}
  </div>`;
}

/* ===== Render: Activity Chart (period-aware) ===== */
function renderActivityChart(chartData) {
  if (!chartData || !chartData.length) return `<div class="empty">No activity data yet</div>`;

  const maxTotal = Math.max(...chartData.map(d => (d.input_tokens || 0) + (d.output_tokens || 0)), 1);

  let bars = '';
  let labels = '';
  for (const bucket of chartData) {
    const inp = bucket.input_tokens || 0;
    const outp = bucket.output_tokens || 0;
    const inH = (inp / maxTotal) * 100;
    const outH = (outp / maxTotal) * 100;
    const displayLabel = bucket.label || '';
    const shortLabel = displayLabel.length > 6 ? displayLabel.slice(-5) : displayLabel;
    bars += `<div class="day-bar" style="height:100%">
      <div class="daily-chart-tooltip">${esc(displayLabel)}: ${fmtCost((bucket.cost_cents || 0) / 100)} — ${fmtNum(inp)} input, ${fmtNum(outp)} output</div>
      <div class="bar-msg" style="height:${inH}%"></div>
      <div class="bar-tool" style="height:${outH}%"></div>
    </div>`;
    labels += `<div class="day-label">${esc(shortLabel)}</div>`;
  }

  return `
    <div class="daily-chart">${bars}</div>
    <div class="daily-chart-labels">${labels}</div>
    <div style="display:flex;gap:16px;margin-top:8px;font-size:0.75rem;color:var(--text-muted)">
      <span><span style="display:inline-block;width:10px;height:10px;background:var(--accent);border-radius:2px;vertical-align:middle"></span> Input tokens</span>
      <span><span style="display:inline-block;width:10px;height:10px;background:var(--accent4);border-radius:2px;vertical-align:middle"></span> Output tokens</span>
    </div>`;
}

/* ===== Generic Sort ===== */
function genericSort(items, col, asc, getters) {
  return [...items].sort((a, b) => {
    const g = getters[col];
    if (!g) return 0;
    const va = g(a), vb = g(b);
    if (typeof va === 'string') {
      const cmp = va.localeCompare(vb, undefined, { sensitivity: 'base' });
      return asc ? cmp : -cmp;
    }
    return asc ? va - vb : vb - va;
  });
}

/* ===== Render: Sortable Table ===== */
function renderSortableTable(id, cols, data, limit, sortCol, sortAsc, rowFn) {
  if (!data.length) return `<div class="empty">No data for this period</div>`;
  const arrow = col => col === sortCol ? `<span class="sort-arrow">${sortAsc ? '\u25b2' : '\u25bc'}</span>` : '';
  const showing = data.slice(0, limit);
  const hasMore = data.length > limit;
  return `
  <table class="sortable-table" id="${id}">
    <thead><tr>${cols.map(c =>
      `<th data-col="${c.key}"${c.right ? ' class="right"' : ''}>${c.label}${arrow(c.key)}</th>`
    ).join('')}</tr></thead>
    <tbody>${showing.map(rowFn).join('')}</tbody>
  </table>
  ${hasMore ? `<button class="show-more-btn" data-table="${id}">Show more (${data.length - limit} remaining)</button>` : ''}`;
}

/* ===== Sessions ===== */
const sessionGetters = {
  session_id: s => s.session_id,
  repo_id: s => s.repo_id || '',
  last_seen: s => s.last_seen || '',
  duration: s => durationMs(s.first_seen, s.last_seen),
  message_count: s => s.message_count,
  tokens: s => s.input_tokens + s.output_tokens,
  cost: s => (s.cost_cents || 0) / 100,
};

function renderSessionsSection(sessions) {
  // Sessions are already sorted, filtered, and paginated server-side
  const multiProvider = registeredProviders.length > 1;
  const cols = [
    { key: 'session_id', label: 'Session' },
    ...(multiProvider ? [{ key: 'provider', label: 'Agent' }] : []),
    { key: 'repo_id', label: 'Repo' },
    { key: 'last_seen', label: 'Last Active' },
    { key: 'duration', label: 'Duration', right: true },
    { key: 'message_count', label: 'Messages', right: true },
    { key: 'tokens', label: 'Tokens', right: true },
    { key: 'cost', label: 'Cost', right: true },
  ];
  if (!sessions.length) return '<div class="empty">No sessions for this period</div>';
  const arrow = col => col === sessionSortCol ? `<span class="sort-arrow">${sessionSortAsc ? '\u25b2' : '\u25bc'}</span>` : '';
  const hasMore = sessionTotalCount > sessions.length;
  const remaining = sessionTotalCount - sessions.length;
  const rowFn = s => {
    const totalTok = s.input_tokens + s.output_tokens;
    const title = s.session_title || s.session_id.slice(0, 8);
    const costVal = (s.cost_cents || 0) / 100;
    const provDisplay = (registeredProviders.find(rp => rp.name === s.provider) || {}).display_name || s.provider;
    const provCol = multiProvider ? `<td>${esc(provDisplay)}</td>` : '';
    return `<tr>
      <td title="${esc(s.session_id)}">${esc(title)}</td>
      ${provCol}
      <td class="dir" title="${esc(s.repo_id || s.project_dir || '')}">${esc(repoName(s.repo_id) || shortenDir(s.project_dir))}</td>
      <td>${esc(fmtDate(s.last_seen))}</td>
      <td class="right">${fmtDuration(s.first_seen, s.last_seen)}</td>
      <td class="right">${fmtNum(s.message_count)}</td>
      <td class="right">${fmtNum(totalTok)}</td>
      <td class="right">${fmtCost(costVal)}</td>
    </tr>`;
  };
  return `
  <table class="sortable-table" id="sessionsTable">
    <thead><tr>${cols.map(c =>
      `<th data-col="${c.key}"${c.right ? ' class="right"' : ''}>${c.label}${arrow(c.key)}</th>`
    ).join('')}</tr></thead>
    <tbody>${sessions.map(rowFn).join('')}</tbody>
  </table>
  ${hasMore ? `<button class="show-more-btn" data-table="sessionsTable">Show more (${remaining} remaining)</button>` : ''}`;
}

/* ===== Config Files ===== */
const configGetters = {
  file_type: f => f.file_type,
  project: f => f.project || '',
  path: f => f.path,
  size_bytes: f => f.size_bytes,
  est_tokens: f => f.est_tokens,
};

function badgeClass(t) {
  if (t === 'claude-md') return 'claude-md';
  if (t === 'rule') return 'rule';
  if (t === 'skill') return 'skill';
  if (t === 'memory') return 'memory';
  return 'settings';
}
function badgeLabel(t) {
  return { 'claude-md': 'CLAUDE.md', 'rule': 'Rule', 'skill': 'Skill', 'settings': 'Settings', 'settings-local': 'Local Settings', 'memory': 'Memory' }[t] || t;
}

function renderConfigRow(f) {
  const sizeStr = f.size_bytes >= 1024 ? (f.size_bytes / 1024).toFixed(1) + ' KB' : f.size_bytes + ' B';
  const tokStr = f.est_tokens >= 1000 ? (f.est_tokens / 1000).toFixed(1) + 'K' : String(f.est_tokens);
  const warn = f.est_tokens > 2000 ? ' style="color:var(--accent4)"' : '';
  const shortPath = shortenDir(f.path);
  return `<tr>
    <td><span class="type-badge ${badgeClass(f.file_type)}">${badgeLabel(f.file_type)}</span></td>
    <td class="dir" title="${esc(f.project)}">${esc(projectName(f.project))}</td>
    <td class="dir" title="${esc(f.path)}" style="max-width:400px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(shortPath)}</td>
    <td class="right">${sizeStr}</td>
    <td class="right"${warn}>${tokStr}</td>
  </tr>`;
}

const configCols = [
  { key: 'file_type', label: 'Type' },
  { key: 'project', label: 'Project' },
  { key: 'path', label: 'File' },
  { key: 'size_bytes', label: 'Size', right: true },
  { key: 'est_tokens', label: 'Est. Tokens', right: true },
];

function renderConfigTable() {
  const filtered = filterBySearch(lastConfigData, configSearchTerm, f => [f.file_type, f.project, f.path]);
  const sorted = genericSort(filtered, configSortCol, configSortAsc, configGetters);
  return renderSortableTable('configTable', configCols, sorted, configShowCount, configSortCol, configSortAsc, renderConfigRow);
}

const projectConfigGetters = {
  project: p => p.project,
  count: p => p.count,
  tokens: p => p.tokens,
};
const projectConfigCols = [
  { key: 'project', label: 'Project' },
  { key: 'types', label: 'File Types' },
  { key: 'count', label: 'Files', right: true },
  { key: 'tokens', label: 'Est. Tokens', right: true },
];

function renderProjectConfigRow(p) {
  const types = p.typeList.map(t => `<span class="type-badge ${badgeClass(t)}">${badgeLabel(t)}</span>`).join(' ');
  return `<tr>
    <td title="${esc(p.project)}" class="dir">${esc(projectName(p.project))}</td>
    <td>${types}</td>
    <td class="right">${p.count}</td>
    <td class="right">${fmtTokensHtml(p.tokens)}</td>
  </tr>`;
}

function renderProjectConfigTable() {
  const filtered = filterBySearch(lastProjectConfigData, projectConfigSearchTerm, p => [p.project]);
  const sorted = genericSort(filtered, projectConfigSortCol, projectConfigSortAsc, projectConfigGetters);
  return renderSortableTable('projectConfigTable', projectConfigCols, sorted, projectConfigShowCount, projectConfigSortCol, projectConfigSortAsc, renderProjectConfigRow);
}

function renderConfigSection(configFiles) {
  const projectCount = lastProjectConfigData.length;
  return `
  <div class="panel section-mb">
    <h2>Config by Project</h2>
    <input type="text" id="projectConfigSearch" class="search-input" placeholder="Search projects..." value="${esc(projectConfigSearchTerm)}" style="margin-bottom:12px">
    <div id="projectConfigContainer">${renderProjectConfigTable()}</div>
  </div>
  <div class="panel section-mb">
    <h2>Config Files</h2>
    <input type="text" id="configSearch" class="search-input" placeholder="Search config files..." value="${esc(configSearchTerm)}" style="margin-bottom:12px">
    <div id="configContainer">${renderConfigTable()}</div>
  </div>`;
}

/* ===== Insights ===== */
function renderInsights(ins) {
  let cards = '';

  const se = ins.search_efficiency;
  const pct = se.total_calls > 0 ? (se.ratio * 100).toFixed(0) : '0';
  const seCls = se.total_calls > 0 && se.ratio > 0.40 ? 'warn' : 'good';
  cards += `<div class="insight-card">
    <div class="insight-header"><span class="icon">&#x1f50d;</span> Search Efficiency</div>
    <div class="insight-desc">How much time Claude spends searching (Grep/Glob) vs doing actual work. Lower is better -- add file paths to CLAUDE.md to help Claude find code faster.</div>
    ${se.total_calls > 0 ? `
      <div class="insight-metric">${pct}% search calls</div>
      <div style="font-size:0.85rem;color:var(--text-muted)">${fmtNum(se.search_calls)} Grep+Glob / ${fmtNum(se.total_calls)} total tool calls</div>
      ${se.recommendation ? `<div class="insight-rec ${seCls}">${esc(se.recommendation)}</div>` : ''}
    ` : '<div style="color:var(--text-muted);font-size:0.85rem">No tool usage data for this period</div>'}
  </div>`;

  const ce = ins.cache_efficiency;
  const ceCls = ce.total_input_tokens > 0 && ce.hit_rate < 0.30 ? 'warn' : 'good';
  cards += `<div class="insight-card">
    <div class="insight-header"><span class="icon">&#x26a1;</span> Cache Efficiency</div>
    <div class="insight-desc">How well <a href="https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching" target="_blank" rel="noopener">prompt caching</a> is working. Higher = more tokens served from cache = lower cost. Longer focused sessions improve cache reuse.</div>
    ${ce.total_input_tokens > 0 ? `
      <div class="insight-metric">${(ce.hit_rate * 100).toFixed(0)}% cache hit rate</div>
      <div style="font-size:0.85rem;color:var(--text-muted)">${fmtNum(ce.total_cache_read_tokens)} cached / ${fmtNum(ce.total_input_tokens)} total input</div>
      ${ce.recommendation ? `<div class="insight-rec ${ceCls}">${esc(ce.recommendation)}</div>` : ''}
    ` : '<div style="color:var(--text-muted);font-size:0.85rem">No token data for this period</div>'}
  </div>`;

  cards += `<div class="insight-card">
    <div class="insight-header"><span class="icon">&#x1f4a1;</span> Token-Heavy Sessions</div>
    <div class="insight-desc">Sessions where input is 5x+ the output -- lots of context sent, little generated back. Consider splitting large tasks into smaller, focused sessions.</div>
    ${ins.token_heavy_sessions.length > 0 ? `
      <div class="insight-list">${ins.token_heavy_sessions.slice(0, 5).map(s =>
        `<div class="insight-row">
          <span class="mono" style="color:var(--accent)">${esc(s.session_id.slice(0,8))}</span>
          <span>${fmtNum(s.input_tokens)} input / ${fmtNum(s.output_tokens)} output (${s.ratio.toFixed(0)}x)</span>
        </div>`
      ).join('')}</div>
    ` : '<div style="color:var(--text-muted);font-size:0.85rem">No data for this period</div>'}
  </div>`;

  return `<div class="panel section-mb">
    <h2>Insights</h2>
    <div class="insight-cards">${cards}</div>
  </div>`;
}

/* ===== Insights V2 Page ===== */
let insightsData = null;

async function loadInsightsData() {
  if (insightsData) return;
  const range = dateRange(currentPeriod);
  const q = qs(range);
  const tzOffset = -new Date().getTimezoneOffset();
  insightsData = await fetch('/analytics/insights' + q + (q ? '&' : '?') + 'tz_offset=' + tzOffset).then(r => r.json()).catch(() => null);
}

function renderInsightsPageView(content) {
  const ins = insightsData;
  if (!ins) {
    content.innerHTML = '<div class="loading">Loading insights</div>';
    loadInsightsData().then(() => { if (currentView === 'insights') renderInsightsPageView(content); });
    return;
  }

  let html = '';

  // Recommendations banner
  if (ins.recommendations && ins.recommendations.length > 0) {
    html += '<div class="panel section-mb"><h2>Recommendations</h2>';
    html += '<div class="insight-cards">';
    for (const rec of ins.recommendations) {
      const icon = rec.severity === 'warn' ? '&#x26a0;' : rec.severity === 'good' ? '&#x2705;' : '&#x1f4a1;';
      const cls = rec.severity === 'warn' ? 'warn' : 'good';
      html += `<div class="insight-card">
        <div class="insight-header"><span class="icon">${icon}</span> ${esc(rec.title)}</div>
        <div class="insight-rec ${cls}">${esc(rec.detail)}</div>
      </div>`;
    }
    html += '</div></div>';
  }

  // Session patterns + Tool diversity
  const sp = ins.session_patterns;
  const td = ins.tool_diversity;
  html += '<div class="grid-2 section-mb">';
  html += '<div class="panel"><h2>Session Patterns</h2>';
  if (sp.total_sessions > 0) {
    html += `<div class="insight-cards" style="grid-template-columns:1fr">
      <div class="insight-card">
        <div class="insight-metric">${sp.total_sessions}</div>
        <div class="insight-desc">sessions in this period</div>
      </div>
      <div class="insight-card">
        <div class="insight-metric">${sp.avg_duration_mins} min</div>
        <div class="insight-desc">average session duration</div>
      </div>
      <div class="insight-card">
        <div class="insight-metric">${sp.avg_messages_per_session}</div>
        <div class="insight-desc">average messages per session</div>
      </div>
      <div class="insight-card">
        <div class="insight-metric">${fmtCost(sp.avg_cost_per_session)}</div>
        <div class="insight-desc">average cost per session</div>
      </div>`;
    if (sp.busiest_hour != null) {
      const h = sp.busiest_hour;
      const label = h === 0 ? '12 AM' : h < 12 ? h + ' AM' : h === 12 ? '12 PM' : (h - 12) + ' PM';
      html += `<div class="insight-card">
        <div class="insight-metric">${label}</div>
        <div class="insight-desc">busiest hour (${sp.busiest_hour_sessions} sessions)</div>
      </div>`;
    }
    html += '</div>';
  } else {
    html += '<div class="empty">No session data for this period</div>';
  }
  html += '</div>';

  html += '<div class="panel"><h2>Tool Diversity</h2>';
  if (td.total_calls > 0) {
    html += `<div class="insight-cards" style="grid-template-columns:1fr">
      <div class="insight-card">
        <div class="insight-metric">${td.unique_tools}</div>
        <div class="insight-desc">unique tools used</div>
      </div>
      <div class="insight-card">
        <div class="insight-metric">${fmtNum(td.total_calls)}</div>
        <div class="insight-desc">total tool calls</div>
      </div>`;
    if (td.top_tool) {
      html += `<div class="insight-card">
        <div class="insight-metric">${esc(td.top_tool)}</div>
        <div class="insight-desc">most used tool (${td.top_tool_pct}% of calls)</div>
      </div>`;
    }
    html += '</div>';
  } else {
    html += '<div class="empty">No tool usage data for this period</div>';
  }
  html += '</div></div>';

  // Search & Cache efficiency (detailed)
  const se = ins.search_efficiency;
  const ce = ins.cache_efficiency;
  html += '<div class="grid-2 section-mb">';
  html += `<div class="panel"><h2>Search Efficiency${ccOnlyLabel()}</h2>`;
  if (se.total_calls > 0) {
    const pct = (se.ratio * 100).toFixed(0);
    const cls = se.ratio > 0.40 ? 'warn' : 'good';
    html += `<div class="insight-metric">${pct}% search calls</div>
      <div style="font-size:0.85rem;color:var(--text-muted);margin-bottom:8px">${fmtNum(se.search_calls)} Grep+Glob / ${fmtNum(se.total_calls)} total tool calls</div>
      <div class="cache-bar" style="height:12px;margin-bottom:8px">
        <div class="segment" style="width:${pct}%;background:${cls === 'warn' ? 'var(--accent4)' : 'var(--accent2)'}"></div>
        <div class="segment" style="width:${100-pct}%;background:var(--border)"></div>
      </div>
      ${se.recommendation ? `<div class="insight-rec ${cls}">${esc(se.recommendation)}</div>` : ''}`;
  } else {
    html += '<div class="empty">No tool usage data</div>';
  }
  html += '</div>';

  html += `<div class="panel"><h2>Cache Efficiency${ccOnlyLabel()}</h2>`;
  if (ce.total_input_tokens > 0) {
    const hitPct = (ce.hit_rate * 100).toFixed(0);
    const cls = ce.hit_rate < 0.30 ? 'warn' : 'good';
    html += `<div class="insight-metric">${hitPct}% hit rate</div>
      <div style="font-size:0.85rem;color:var(--text-muted);margin-bottom:8px">${fmtNum(ce.total_cache_read_tokens)} cached / ${fmtNum(ce.total_input_tokens)} total input</div>
      <div class="cache-bar" style="height:12px;margin-bottom:8px">
        <div class="segment" style="width:${hitPct}%;background:var(--accent2)"></div>
        <div class="segment" style="width:${100-hitPct}%;background:var(--border)"></div>
      </div>
      ${ce.recommendation ? `<div class="insight-rec ${cls}">${esc(ce.recommendation)}</div>` : ''}`;
  } else {
    html += '<div class="empty">No token data</div>';
  }
  html += '</div></div>';

  // Context Window usage
  const ctx = statsData ? statsData.contextUsage : null;
  if (ctx && ctx.total_sessions_with_data > 0) {
    const pct = Math.round(ctx.avg_usage_pct);
    const maxPct = Math.round(ctx.max_usage_pct);
    const color = pct >= 80 ? 'var(--danger)' : pct >= 60 ? 'var(--accent4)' : 'var(--accent2)';
    const cls = pct >= 80 ? 'warn' : 'good';
    html += '<div class="panel section-mb"><h2>Context Window Usage</h2>';
    html += `<div class="insight-metric" style="color:${color}">${pct}% avg</div>
      <div style="font-size:0.85rem;color:var(--text-muted);margin-bottom:8px">${ctx.sessions_over_80_pct} sessions &gt;80% · max ${maxPct}% · ${ctx.total_sessions_with_data} sessions with data</div>
      <div class="cache-bar" style="height:12px;margin-bottom:8px">
        <div class="segment" style="width:${pct}%;background:${color}"></div>
        <div class="segment" style="width:${100-pct}%;background:var(--border)"></div>
      </div>
      <div class="insight-rec ${cls}">Average context window usage across sessions. High usage (&gt;80%) may cause context compression and degrade quality.</div>`;
    html += '</div>';
  }

  // Daily cost trend chart
  if (ins.daily_cost && ins.daily_cost.length > 0) {
    const maxCost = Math.max(...ins.daily_cost.map(d => d.cost), 0.01);
    html += '<div class="panel section-mb"><h2>Daily Cost Trend</h2>';
    html += '<div class="daily-chart">';
    for (const d of ins.daily_cost) {
      const h = (d.cost / maxCost) * 100;
      html += `<div class="day-bar" style="height:100%">
        <div class="daily-chart-tooltip">${esc(d.date)}: ${fmtCost(d.cost)} (${d.sessions} sessions)</div>
        <div class="bar-msg" style="height:${h}%;background:var(--accent2)"></div>
      </div>`;
    }
    html += '</div><div class="daily-chart-labels">';
    for (const d of ins.daily_cost) {
      const short = d.date.length > 6 ? d.date.slice(5) : d.date;
      html += `<div class="day-label">${esc(short)}</div>`;
    }
    html += '</div></div>';
  }

  // Config health
  const ch = ins.config_health;
  if (ch && ch.total_config_files > 0) {
    html += `<div class="panel section-mb"><h2>Config Health${ccOnlyLabel()}</h2>`;
    html += '<div class="insight-cards">';
    html += `<div class="insight-card">
      <div class="insight-metric">${ch.total_config_files}</div>
      <div class="insight-desc">config files across all projects</div>
    </div>
    <div class="insight-card">
      <div class="insight-metric">${fmtNum(ch.total_tokens)}</div>
      <div class="insight-desc">total config tokens loaded per session</div>
    </div>`;
    if (ch.heaviest_project) {
      html += `<div class="insight-card">
        <div class="insight-metric">${esc(projectName(ch.heaviest_project))}</div>
        <div class="insight-desc">heaviest project (${fmtNum(ch.heaviest_tokens)} tokens)</div>
      </div>`;
    }
    html += '</div>';
    // CLAUDE.md warnings
    if (ins.claude_md_files && ins.claude_md_files.length > 0) {
      html += '<div style="margin-top:16px"><h2 style="margin-bottom:8px">CLAUDE.md Files</h2>';
      html += '<div class="insight-list">';
      for (const f of ins.claude_md_files) {
        const tokStr = f.est_tokens >= 1000 ? (f.est_tokens / 1000).toFixed(1) + 'K' : String(f.est_tokens);
        const warn = f.est_tokens > 2000;
        html += `<div class="insight-row">
          <span style="color:var(--text-muted);max-width:60%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap" title="${esc(f.path)}">${esc(f.path.split('/').slice(-3).join('/'))}</span>
          <span${warn ? ' style="color:var(--accent4)"' : ''}>${tokStr} tokens</span>
        </div>`;
      }
      html += '</div>';
      if (ins.claude_md_files.some(f => f.recommendation)) {
        const recs = ins.claude_md_files.filter(f => f.recommendation);
        html += `<div class="insight-rec warn" style="margin-top:8px">${esc(recs[0].recommendation)}</div>`;
      }
      html += '</div>';
    }
    html += '</div>';
  }

  // Token-heavy sessions
  if (ins.token_heavy_sessions && ins.token_heavy_sessions.length > 0) {
    html += '<div class="panel section-mb"><h2>Token-Heavy Sessions</h2>';
    html += '<div class="insight-desc" style="margin-bottom:12px">Sessions where input is 5x+ the output -- lots of context sent, little generated back.</div>';
    html += '<div class="insight-list">';
    for (const s of ins.token_heavy_sessions.slice(0, 10)) {
      html += `<div class="insight-row">
        <span>
          <span class="mono" style="color:var(--accent)">${esc(s.session_id.slice(0,8))}</span>
          ${s.repo_id ? `<span style="color:var(--text-muted);margin-left:8px">${esc(repoName(s.repo_id))}</span>` : ''}
        </span>
        <span>${fmtNum(s.input_tokens)} input / ${fmtNum(s.output_tokens)} output (${s.ratio.toFixed(1)}x)</span>
      </div>`;
    }
    html += '</div></div>';
  }

  content.innerHTML = html;
}

/* ===== Prompts ===== */
const historyGetters = {
  display: e => e.display || '',
  project: e => e.project || '',
  timestamp: e => e.timestamp || 0,
};
const historyCols = [
  { key: 'display', label: 'Prompt' },
  { key: 'project', label: 'Project' },
  { key: 'timestamp', label: 'Time', right: true },
];

function renderHistoryRow(e) {
  const display = e.display || '';
  const truncated = display.length > 80 ? display.slice(0, 80) + '...' : display;
  return `<tr>
    <td class="prompt-cell" title="${esc(display)}">${esc(truncated)}</td>
    <td class="dir" title="${esc(e.project || '')}">${esc(projectName(e.project))}</td>
    <td class="right">${fmtDateFromMs(e.timestamp)}</td>
  </tr>`;
}

function renderHistoryTable(data) {
  const items = data || lastHistoryData;
  const sorted = genericSort(items, historySortCol, historySortAsc, historyGetters);
  let html = renderSortableTable('historyTable', historyCols, sorted, sorted.length, historySortCol, historySortAsc, renderHistoryRow);
  if (promptsTotalCount > items.length) {
    html += `<button class="show-more-btn" data-table="historyTable">Show more (${promptsTotalCount - items.length} remaining)</button>`;
  }
  return html;
}

function renderHistorySection() {
  return `<div class="panel section-mb">
    <h2>Prompts</h2>
    <input type="text" id="promptsSearch" class="search-input" placeholder="Search prompts..." value="${esc(promptsSearchTerm)}" style="margin-bottom:12px">
    <div id="historyContainer">${renderHistoryTable(lastHistoryData)}</div>
  </div>`;
}

/* ===== Plans ===== */
const plansGetters = {
  title: p => p.title || p.name || '',
  path: p => p.path || '',
  size_bytes: p => p.size_bytes || 0,
  est_tokens: p => p.est_tokens || 0,
  modified: p => p.modified || '',
};
const plansCols = [
  { key: 'title', label: 'Title' },
  { key: 'path', label: 'File' },
  { key: 'size_bytes', label: 'Size', right: true },
  { key: 'est_tokens', label: 'Est. Tokens', right: true },
  { key: 'modified', label: 'Modified', right: true },
];

function renderPlansRow(p) {
  const sizeStr = p.size_bytes >= 1024 ? (p.size_bytes / 1024).toFixed(1) + ' KB' : (p.size_bytes || 0) + ' B';
  const displayTitle = p.title || p.name || '--';
  const shortPath = p.path ? shortenDir(p.path) : '--';
  return `<tr>
    <td style="max-width:300px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(displayTitle)}</td>
    <td class="dir" title="${esc(p.path || '')}" style="max-width:300px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(shortPath)}</td>
    <td class="right">${sizeStr}</td>
    <td class="right">${fmtTokensHtml(p.est_tokens || 0)}</td>
    <td class="right">${fmtDate(p.modified)}</td>
  </tr>`;
}

function renderPlansTable(data) {
  const items = data || lastPlansData;
  const sorted = genericSort(items, plansSortCol, plansSortAsc, plansGetters);
  let html = renderSortableTable('plansTable', plansCols, sorted, sorted.length, plansSortCol, plansSortAsc, renderPlansRow);
  if (plansTotalCount > items.length) {
    html += `<button class="show-more-btn" data-table="plansTable">Show more (${plansTotalCount - items.length} remaining)</button>`;
  }
  return html;
}

function renderPlansSection() {
  return `<div class="panel section-mb">
    <h2>Plans</h2>
    <input type="text" id="plansSearch" class="search-input" placeholder="Search plans..." value="${esc(plansSearchTerm)}" style="margin-bottom:12px">
    <div id="plansContainer">${renderPlansTable(lastPlansData)}</div>
  </div>`;
}

/* ===== Plugins ===== */
const pluginsGetters = {
  name: p => p.name || '',
  description: p => p.description || '',
  version: p => p.version || '',
  scope: p => p.scope || '',
  installed_at: p => p.installed_at || '',
  last_updated: p => p.last_updated || '',
};
const pluginsCols = [
  { key: 'name', label: 'Name' },
  { key: 'description', label: 'Description' },
  { key: 'version', label: 'Version' },
  { key: 'scope', label: 'Scope' },
  { key: 'installed_at', label: 'Installed', right: true },
  { key: 'last_updated', label: 'Last Updated', right: true },
];

function renderPluginsRow(p) {
  const desc = p.description || '';
  const truncDesc = desc.length > 60 ? desc.slice(0, 60) + '...' : desc;
  return `<tr>
    <td>${esc(p.name || '--')}</td>
    <td class="dir" title="${esc(desc)}" style="max-width:250px">${esc(truncDesc || '--')}</td>
    <td>${esc(p.version || '--')}</td>
    <td>${esc(p.scope || '--')}</td>
    <td class="right">${fmtDate(p.installed_at)}</td>
    <td class="right">${fmtDate(p.last_updated)}</td>
  </tr>`;
}

function renderPluginsTable() {
  const filtered = filterBySearch(lastPluginsData, pluginsSearchTerm, p => [p.name, p.description]);
  const sorted = genericSort(filtered, pluginsSortCol, pluginsSortAsc, pluginsGetters);
  return renderSortableTable('pluginsTable', pluginsCols, sorted, pluginsShowCount, pluginsSortCol, pluginsSortAsc, renderPluginsRow);
}

function renderPluginsSection(plugins) {
  if (!plugins.length) {
    return `<div class="panel section-mb">
      <h2>Plugins</h2>
      <div class="empty">No plugins installed</div>
    </div>`;
  }
  return `<div class="panel section-mb">
    <h2>Plugins</h2>
    <input type="text" id="pluginsSearch" class="search-input" placeholder="Search plugins..." value="${esc(pluginsSearchTerm)}" style="margin-bottom:12px">
    <div id="pluginsContainer">${renderPluginsTable()}</div>
  </div>`;
}

/* ===== Permissions ===== */
const permissionsGetters = {
  rule: r => r.rule || '',
  action: r => r.action || '',
  scope: r => r.scope || '',
};
const permissionsCols = [
  { key: 'rule', label: 'Rule' },
  { key: 'action', label: 'Action' },
  { key: 'scope', label: 'Scope' },
];

function scopeBadgeClass(scope) {
  if (scope === 'global') return 'scope-global';
  if (scope === 'local') return 'scope-local';
  return 'scope-project';
}

function renderPermissionsRow(r) {
  return `<tr>
    <td class="dir" style="max-width:400px" title="${esc(r.rule)}">${esc(r.rule)}</td>
    <td><span class="action-badge ${r.action === 'allow' ? 'allow' : 'deny'}">${esc(r.action)}</span></td>
    <td><span class="scope-badge ${scopeBadgeClass(r.scope)}">${esc(r.scope)}</span></td>
  </tr>`;
}

function renderPermissionsTable() {
  const filtered = filterBySearch(lastPermissionsData, permissionsSearchTerm, r => [r.rule]);
  const sorted = genericSort(filtered, permissionsSortCol, permissionsSortAsc, permissionsGetters);
  return renderSortableTable('permissionsTable', permissionsCols, sorted, permissionsShowCount, permissionsSortCol, permissionsSortAsc, renderPermissionsRow);
}

function renderPermissionsSection(permissions) {
  const mode = permissions.default_mode || 'default';
  const rules = permissions.rules || [];
  return `<div class="panel section-mb">
    <h2>Permissions</h2>
    <input type="text" id="permissionsSearch" class="search-input" placeholder="Search permissions..." value="${esc(permissionsSearchTerm)}" style="margin-bottom:12px">
    <div id="permissionsContainer">${renderPermissionsTable()}</div>
  </div>`;
}

/* ===== View Renderers ===== */
function renderStatsView(content) {
  const { summary, sessions, cwds, cost, models, activityChart, contextUsage, interactionModes, topTools, mcpTools, branches } = statsData;
  content.innerHTML = `
    ${renderActiveSessions(activeSessionsData)}
    ${renderCards(summary, cost)}
    <div class="panel section-mb">
      <h2>${cachedActivityChartTitle}</h2>
      ${renderActivityChart(activityChart)}
    </div>
    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Agents</h2>
        ${renderBarChart(agentBarData(),
          p => p.display_name,
          p => p.cost_cents,
          (p, i) => paletteColor(i),
          'No agent data for this period',
          fmtCostTokens
        )}
      </div>
      <div class="panel">
        <h2>Models</h2>
        ${renderBarChart(cachedSortedModels,
          (m, full) => {
            const label = m.provider_display + ' / ' + m.model;
            return full ? label : label;
          },
          m => m.cost_cents,
          (m, i) => paletteColor(i),
          'No model data for this period',
          fmtCostTokens
        )}
      </div>
    </div>
    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Projects</h2>
        ${renderBarChart(cwds,
          (c, full) => full ? (c.repo_id || '--') : repoName(c.repo_id),
          c => c.cost_cents,
          (c, i) => paletteColor(i),
          'No project data for this period',
          fmtCostTokens
        )}
      </div>
      <div class="panel">
        <h2>Branches${ccOnlyLabel()}</h2>
        ${renderBarChart((branches || []).slice(0, DEFAULT_CHART_ROWS),
          (b, full) => {
            const branch = b.git_branch.replace(/^refs\/heads\//, '');
            const repo = repoName(b.repo_id);
            return repo + ' / ' + branch;
          },
          b => b.cost_cents,
          (b, i) => paletteColor(i),
          'No branch data for this period',
          fmtCostTokens
        )}
      </div>
    </div>
    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Tools${ccOnlyLabel()}</h2>
        ${renderBarChart((topTools || []).filter(t => !t[0].startsWith('mcp__')).slice(0, DEFAULT_CHART_ROWS),
          (t) => t[0],
          t => t[1],
          (t, i) => paletteColor(i),
          'No tool usage data for this period'
        )}
      </div>
      <div class="panel">
        <h2>MCP${ccOnlyLabel()}</h2>
        ${renderBarChart((mcpTools || []).slice(0, DEFAULT_CHART_ROWS),
          (m, full) => full ? m.tool : m.tool.replace(/^mcp__/, ''),
          m => m.call_count,
          (m, i) => paletteColor(i),
          'No MCP tools used in this period'
        )}
      </div>
    </div>
    <div class="panel section-mb">
      <h2>Sessions</h2>
      <input type="text" id="sessionsSearch" class="search-input" placeholder="Search sessions..." value="${esc(sessionsSearchTerm)}" style="margin-bottom:12px">
      <div id="sessionsContainer">${renderSessionsSection(sessions)}</div>
    </div>
  `;
}

function renderSetupView(content) {
  content.innerHTML = `
    ${renderIntegrationsSection(setupData.integrations)}
    ${renderConfigSection(cachedMergedConfigFiles)}
    ${renderPluginsSection(lastPluginsData)}
    ${renderPermissionsSection(setupData.permissions)}
  `;
}

function renderIntegrationsSection(integrations) {
  if (!integrations) return '';
  const starship = integrations.starship || {};
  const claudeOk = integrations.claude_code_statusline;

  const starshipSnippet = `# Budi — AI code analytics
[custom.budi]
command = "budi statusline --format=starship"
when = "command -v budi-daemon"
format = "[$output]($style) "
style = "cyan"
shell = ["sh"]`;

  let starshipStatus;
  if (starship.configured) {
    starshipStatus = '<span style="color:var(--green)">✓ configured</span>';
  } else if (starship.installed) {
    starshipStatus = '<span style="color:var(--yellow)">installed but not configured</span>';
  } else {
    starshipStatus = '<span style="color:var(--text-dim)">not installed</span>';
  }

  return `<div class="panel section-mb">
    <h2>Integrations</h2>
    <table class="sortable-table">
      <thead><tr><th>Integration</th><th>Status</th></tr></thead>
      <tbody>
        <tr><td>Claude Code Statusline</td><td>${claudeOk ? '<span style="color:var(--green)">✓ active</span>' : '<span style="color:var(--yellow)">not configured</span> — run <code>budi statusline --install</code>'}</td></tr>
        <tr><td>Starship Shell Prompt</td><td>${starshipStatus}</td></tr>
      </tbody>
    </table>
    ${starship.installed && !starship.configured ? `
    <div style="margin-top:12px">
      <p style="margin:0 0 8px;color:var(--text-muted)">Run <code>budi init</code> to auto-configure, or add this to <code>~/.config/starship.toml</code>:</p>
      <pre style="background:var(--surface);border:1px solid var(--border);border-radius:var(--radius);padding:12px;font-size:0.8rem;overflow-x:auto">${starshipSnippet}</pre>
    </div>` : ''}
  </div>`;
}

function renderPlansView(content) {
  content.innerHTML = renderPlansSection();
}

function renderPromptsView(content) {
  content.innerHTML = renderHistorySection();
}

/* ===== renderCurrentView ===== */
function renderCurrentView() {
  // Update nav active state
  $$('.nav-tabs a').forEach(a => a.classList.toggle('active', a.dataset.view === currentView));

  // Show/hide period tabs and provider filter
  const showPeriod = currentView === 'stats' || currentView === 'insights';
  $('#periodBar').style.display = showPeriod ? 'flex' : 'none';
  $('#providerFilter').style.display = 'none';

  const content = $('#content');
  switch (currentView) {
    case 'stats': renderStatsView(content); break;
    case 'setup':
      if (!setupData) {
        content.innerHTML = '<div class="loading">Loading setup data</div>';
        loadSetupData().then(() => { renderSetupView(content); bindAllHandlers(); });
        return;
      }
      break;
    case 'plans':
      if (lastPlansData.length === 0) {
        content.innerHTML = '<div class="loading">Loading plans</div>';
        loadPlansData().then(() => { renderPlansView(content); bindAllHandlers(); });
        return;
      }
      break;
    case 'prompts':
      if (lastHistoryData.length === 0) {
        content.innerHTML = '<div class="loading">Loading prompts</div>';
        loadPromptsData().then(() => { renderPromptsView(content); bindAllHandlers(); });
        return;
      }
      break;
  }
  // Default rendering for already-loaded pages
  switch (currentView) {
    case 'stats': break; // already handled above
    case 'insights': renderInsightsPageView(content); break;
    case 'setup': renderSetupView(content); break;
    case 'plans': renderPlansView(content); break;
    case 'prompts': renderPromptsView(content); break;
  }
  bindAllHandlers();
}

/* ===== Main Render ===== */
async function render() {
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';

  try {
    await loadAllData();
    renderCurrentView();
  } catch (err) {
    content.innerHTML = `<div class="empty">
      Failed to load analytics.<br>
      <span style="font-size:0.85rem;color:var(--text-muted)">Is budi-daemon running? Try: <code>budi sync</code> first.</span>
    </div>`;
  }
}

function bindSearchHandler(inputId, setTerm, resetCount, rerender) {
  const el = $('#' + inputId);
  if (el) {
    el.addEventListener('input', (e) => {
      setTerm(e.target.value);
      resetCount();
      rerender();
      bindTableHandlers();
    });
  }
}

function bindSearchHandlers() {
  // Sessions search — debounced, server-side
  let sessionsSearchTimeout = null;
  const sessionsSearchEl = $('#sessionsSearch');
  if (sessionsSearchEl) {
    sessionsSearchEl.addEventListener('input', (e) => {
      sessionsSearchTerm = e.target.value;
      clearTimeout(sessionsSearchTimeout);
      sessionsSearchTimeout = setTimeout(async () => {
        const result = await fetchSessions(DEFAULT_TABLE_ROWS, 0);
        lastSessionData = result.sessions || [];
        sessionTotalCount = result.total_count || 0;
        sessionShowCount = lastSessionData.length;
        $('#sessionsContainer').innerHTML = renderSessionsSection(lastSessionData);
        bindTableHandlers();
      }, 300);
    });
  }
  bindSearchHandler('configSearch',
    v => { configSearchTerm = v; },
    () => { configShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#configContainer').innerHTML = renderConfigTable(); }
  );
  bindSearchHandler('projectConfigSearch',
    v => { projectConfigSearchTerm = v; },
    () => { projectConfigShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#projectConfigContainer').innerHTML = renderProjectConfigTable(); }
  );
  bindSearchHandler('pluginsSearch',
    v => { pluginsSearchTerm = v; },
    () => { pluginsShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#pluginsContainer').innerHTML = renderPluginsTable(); }
  );
  bindSearchHandler('permissionsSearch',
    v => { permissionsSearchTerm = v; },
    () => { permissionsShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#permissionsContainer').innerHTML = renderPermissionsTable(); }
  );
  // Plans search — debounced, server-side
  let plansSearchTimeout = null;
  const plansSearchEl = $('#plansSearch');
  if (plansSearchEl) {
    plansSearchEl.addEventListener('input', (e) => {
      plansSearchTerm = e.target.value;
      clearTimeout(plansSearchTimeout);
      plansSearchTimeout = setTimeout(async () => {
        const result = await fetchPlans(DEFAULT_TABLE_ROWS, 0);
        lastPlansData = result.plans || [];
        plansTotalCount = result.total_count || 0;
        plansShowCount = lastPlansData.length;
        $('#plansContainer').innerHTML = renderPlansTable(lastPlansData);
        bindTableHandlers();
      }, 300);
    });
  }
  // Prompts search — debounced, server-side
  let promptsSearchTimeout = null;
  const promptsSearchEl = $('#promptsSearch');
  if (promptsSearchEl) {
    promptsSearchEl.addEventListener('input', (e) => {
      promptsSearchTerm = e.target.value;
      clearTimeout(promptsSearchTimeout);
      promptsSearchTimeout = setTimeout(async () => {
        const result = await fetchPrompts(DEFAULT_TABLE_ROWS, 0);
        lastHistoryData = result.entries || [];
        promptsTotalCount = result.total_count || 0;
        historyShowCount = lastHistoryData.length;
        $('#historyContainer').innerHTML = renderHistoryTable(lastHistoryData);
        bindTableHandlers();
      }, 300);
    });
  }
}

function bindAllHandlers() {
  bindSearchHandlers();
  bindTableHandlers();
  // Agent tile click-to-filter removed for now
}

function bindTableHandlers() {
  // Sessions table sort — re-fetch from server
  $$('#sessionsTable th[data-col]').forEach(th => {
    th.addEventListener('click', async () => {
      const col = th.dataset.col;
      if (sessionSortCol === col) sessionSortAsc = !sessionSortAsc;
      else { sessionSortCol = col; sessionSortAsc = col === 'session_id' || col === 'repo_id'; }
      const result = await fetchSessions(DEFAULT_TABLE_ROWS, 0);
      lastSessionData = result.sessions || [];
      sessionTotalCount = result.total_count || 0;
      sessionShowCount = lastSessionData.length;
      $('#sessionsContainer').innerHTML = renderSessionsSection(lastSessionData);
      bindTableHandlers();
    });
  });
  $$('#projectConfigTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (projectConfigSortCol === col) projectConfigSortAsc = !projectConfigSortAsc;
      else { projectConfigSortCol = col; projectConfigSortAsc = col === 'project'; }
      $('#projectConfigContainer').innerHTML = renderProjectConfigTable();
      bindTableHandlers();
    });
  });
  $$('#configTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (configSortCol === col) configSortAsc = !configSortAsc;
      else { configSortCol = col; configSortAsc = col === 'path' || col === 'project' || col === 'file_type'; }
      $('#configContainer').innerHTML = renderConfigTable();
      bindTableHandlers();
    });
  });
  $$('#historyTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (historySortCol === col) historySortAsc = !historySortAsc;
      else { historySortCol = col; historySortAsc = col === 'display' || col === 'project'; }
      // Client-side sort on loaded data (prompts sorted client-side for now)
      $('#historyContainer').innerHTML = renderHistoryTable(lastHistoryData);
      bindTableHandlers();
    });
  });
  $$('#plansTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (plansSortCol === col) plansSortAsc = !plansSortAsc;
      else { plansSortCol = col; plansSortAsc = col === 'name'; }
      // Client-side sort on loaded data (plans sorted client-side for now)
      $('#plansContainer').innerHTML = renderPlansTable(lastPlansData);
      bindTableHandlers();
    });
  });
  $$('#pluginsTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (pluginsSortCol === col) pluginsSortAsc = !pluginsSortAsc;
      else { pluginsSortCol = col; pluginsSortAsc = col === 'name' || col === 'scope'; }
      $('#pluginsContainer').innerHTML = renderPluginsTable();
      bindTableHandlers();
    });
  });
  $$('#permissionsTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (permissionsSortCol === col) permissionsSortAsc = !permissionsSortAsc;
      else { permissionsSortCol = col; permissionsSortAsc = col === 'rule' || col === 'scope'; }
      $('#permissionsContainer').innerHTML = renderPermissionsTable();
      bindTableHandlers();
    });
  });
  $$('.show-more-btn').forEach(btn => {
    btn.addEventListener('click', async () => {
      const table = btn.dataset.table;
      if (table === 'sessionsTable') {
        // Fetch next page from server and append
        const result = await fetchSessions(DEFAULT_TABLE_ROWS, lastSessionData.length);
        lastSessionData = lastSessionData.concat(result.sessions || []);
        sessionTotalCount = result.total_count || sessionTotalCount;
        sessionShowCount = lastSessionData.length;
        $('#sessionsContainer').innerHTML = renderSessionsSection(lastSessionData);
      } else if (table === 'configTable') {
        configShowCount += DEFAULT_TABLE_ROWS;
        $('#configContainer').innerHTML = renderConfigTable();
      } else if (table === 'projectConfigTable') {
        projectConfigShowCount += DEFAULT_TABLE_ROWS;
        $('#projectConfigContainer').innerHTML = renderProjectConfigTable();
      } else if (table === 'historyTable') {
        const result = await fetchPrompts(DEFAULT_TABLE_ROWS, lastHistoryData.length);
        lastHistoryData = lastHistoryData.concat(result.entries || []);
        promptsTotalCount = result.total_count || promptsTotalCount;
        historyShowCount = lastHistoryData.length;
        $('#historyContainer').innerHTML = renderHistoryTable(lastHistoryData);
      } else if (table === 'plansTable') {
        const result = await fetchPlans(DEFAULT_TABLE_ROWS, lastPlansData.length);
        lastPlansData = lastPlansData.concat(result.plans || []);
        plansTotalCount = result.total_count || plansTotalCount;
        plansShowCount = lastPlansData.length;
        $('#plansContainer').innerHTML = renderPlansTable(lastPlansData);
      } else if (table === 'pluginsTable') {
        pluginsShowCount += DEFAULT_TABLE_ROWS;
        $('#pluginsContainer').innerHTML = renderPluginsTable();
      } else if (table === 'permissionsTable') {
        permissionsShowCount += DEFAULT_TABLE_ROWS;
        $('#permissionsContainer').innerHTML = renderPermissionsTable();
      }
      bindTableHandlers();
    });
  });
}

// Nav tab switching
$$('.nav-tabs a').forEach(a => {
  a.addEventListener('click', e => {
    e.preventDefault();
    history.pushState({}, '', a.href);
    currentView = a.dataset.view;
    if (dataLoaded) {
      renderCurrentView();
    }
  });
});

window.addEventListener('popstate', () => {
  currentView = getCurrentView();
  if (dataLoaded) {
    renderCurrentView();
  }
});

// Request sequencing — cancel in-flight fetches when period/filter changes
let currentAbort = null;

// Shared reload helper — aborts previous in-flight requests
async function switchAndReload() {
  if (currentAbort) currentAbort.abort();
  const abort = new AbortController();
  currentAbort = abort;
  insightsData = null; // Clear insights cache on period/view change
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  try {
    await loadStatsData(abort.signal);
    if (abort.signal.aborted) return;
    renderCurrentView();
  } catch (err) {
    if (abort.signal.aborted) return;
    content.innerHTML = `<div class="empty">
      Failed to load analytics.<br>
      <span style="font-size:0.85rem;color:var(--text-muted)">Is budi-daemon running? Try: <code>budi sync</code> first.</span>
    </div>`;
  }
}

// Period tab switching
$$('.period-tabs button').forEach(btn => {
  btn.addEventListener('click', () => {
    $$('.period-tabs button').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    currentPeriod = btn.dataset.period;
    switchAndReload();
  });
});

// Update nav active state on initial load
$$('.nav-tabs a').forEach(a => a.classList.toggle('active', a.dataset.view === currentView));

// Show/hide period tabs based on initial view
$('#periodBar').style.display = (currentView === 'stats' || currentView === 'insights') ? 'flex' : 'none';

render();

// Auto-refresh: poll every 30s to keep dashboard data fresh (skip if tab is hidden)
setInterval(async () => {
  if (document.hidden || !dataLoaded) return;
  if (currentView === 'stats') {
    await loadStatsData();
  } else if (currentView === 'insights') {
    insightsData = null;
    await loadInsightsData();
  }
  renderCurrentView();
}, 30000);
