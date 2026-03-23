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
