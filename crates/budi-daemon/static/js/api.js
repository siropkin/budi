async function fetchSessions(limit, offset) {
  const range = dateRange(currentPeriod);
  const params = { ...range, sort_by: sessionSortCol, sort_asc: sessionSortAsc, limit, offset };
  if (sessionsSearchTerm) params.search = sessionsSearchTerm;
  const result = await fetch('/analytics/sessions' + qs(params)).then(r => r.json()).catch(() => ({sessions:[],total_count:0}));
  return result;
}

async function loadAllData() {
  // Fetch registered providers once (lightweight, doesn't change per period).
  if (registeredProviders.length === 0) {
    registeredProviders = await fetch('/analytics/registered-providers').then(r => r.json()).catch(() => []);
  }
  await loadStatsData();
  dataLoaded = true;
}

async function loadStatsData(signal) {
  const range = dateRange(currentPeriod);
  const q = qs(range);
  const gran = granularityForPeriod(currentPeriod);
  const tzOffset = -new Date().getTimezoneOffset();
  const opts = signal ? { signal } : {};

  const sessionsQ = q + (q ? '&' : '?') + `sort_by=${sessionSortCol}&sort_asc=${sessionSortAsc}&limit=${DEFAULT_TABLE_ROWS}${sessionsSearchTerm ? '&search=' + encodeURIComponent(sessionsSearchTerm) : ''}`;
  const [summary, sessionsResult, cwds, cost, models, activityChart, providers, contextUsage, interactionModes, topTools, mcpTools, branches] = await Promise.all([
    fetch('/analytics/summary' + q, opts).then(r => r.json()),
    fetch('/analytics/sessions' + sessionsQ, opts).then(r => r.json()).catch(() => ({sessions:[],total_count:0})),
    fetch('/analytics/projects' + q + (q ? '&' : '?') + 'limit=' + DEFAULT_CHART_ROWS, opts).then(r => r.json()),
    fetch('/analytics/cost' + q, opts).then(r => r.json()),
    fetch('/analytics/models' + q, opts).then(r => r.json()),
    fetch('/analytics/activity' + q + (q ? '&' : '?') + 'granularity=' + gran + '&tz_offset=' + tzOffset, opts).then(r => r.json()),
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
  lastSessionData = sessions;
  providersData = providers;

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

