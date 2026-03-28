async function loadAllData() {
  // Fetch registered providers once (lightweight, doesn't change per period).
  if (registeredProviders.length === 0) {
    registeredProviders = await fetch('/admin/providers').then(r => r.json()).catch(() => []);
  }
  if (currentPage === 'overview') await loadStatsData();
  dataLoaded = true;
}

async function loadStatsData(signal) {
  const gran = granularityForPeriod(currentPeriod);
  const tzOffset = -new Date().getTimezoneOffset();
  const opts = signal ? { signal } : {};

  const ok = r => { if (!r.ok) throw new Error(`${r.url}: ${r.status}`); return r.json(); };
  const [summary, cwds, cost, models, activityChart, providers, branches, tickets, activityTags] = await Promise.all([
    fetch(buildUrl('/analytics/summary'), opts).then(ok),
    fetch(buildUrl('/analytics/projects', { limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/cost'), opts).then(ok),
    fetch(buildUrl('/analytics/models', { limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/activity', { granularity: gran, tz_offset: tzOffset }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/providers'), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/branches', { limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/tags', { key: 'ticket_id', limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/tags', { key: 'activity', limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
  ]);

  statsData = { summary, cwds, cost, models, activityChart, branches, tickets, activityTags };
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

}

async function loadInsightsData(signal) {
  const opts = signal ? { signal } : {};
  const ok = r => { if (!r.ok) throw new Error(`${r.url}: ${r.status}`); return r.json(); };

  const [cacheEff, sessionCurve, costConf, subagent, speedTags, tools, mcp] = await Promise.all([
    fetch(buildUrl('/analytics/cache-efficiency'), opts).then(ok).catch(() => null),
    fetch(buildUrl('/analytics/session-cost-curve'), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/cost-confidence'), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/subagent-cost'), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/tags', { key: 'speed', limit: 10 }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/tools', { limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
    fetch(buildUrl('/analytics/mcp', { limit: DEFAULT_CHART_ROWS }), opts).then(ok).catch(() => []),
  ]);

  insightsData = { cacheEff, sessionCurve, costConf, subagent, speedTags, tools, mcp };
}

async function loadSessionsPageData(signal) {
  const opts = signal ? { signal } : {};
  const ok = r => { if (!r.ok) throw new Error(`${r.url}: ${r.status}`); return r.json(); };

  const extra = { limit: 50, sort_by: sessionsPageSortCol, sort_asc: sessionsPageSortAsc };
  if (sessionsPageSearchTerm) extra.search = sessionsPageSearchTerm;
  const result = await fetch(buildUrl('/analytics/sessions', extra), opts).then(ok).catch(() => ({ sessions: [], total_count: 0 }));
  sessionsPageData = result.sessions || [];
  sessionsPageTotalCount = result.total_count || 0;
}

async function loadSessionMessages(sessionId) {
  const ok = r => { if (!r.ok) throw new Error(`${r.url}: ${r.status}`); return r.json(); };
  return fetch('/analytics/sessions/' + encodeURIComponent(sessionId) + '/messages').then(ok).catch(() => []);
}

async function loadSessionTags(sessionId) {
  const ok = r => { if (!r.ok) throw new Error(`${r.url}: ${r.status}`); return r.json(); };
  return fetch('/analytics/sessions/' + encodeURIComponent(sessionId) + '/tags').then(ok).catch(() => []);
}
