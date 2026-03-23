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

