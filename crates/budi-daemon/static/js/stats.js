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

/* ===== Render: Summary Cards ===== */
function renderCards(s, cost, gitSummary) {
  const totalTokens = s.total_input_tokens + s.total_output_tokens + s.total_cache_creation_tokens + s.total_cache_read_tokens;
  const totalIn = s.total_input_tokens + s.total_cache_creation_tokens + s.total_cache_read_tokens;
  const git = gitSummary || {};
  const gitCard = `
    <div class="card">
      <div class="label">Git</div>
      <div class="value">${fmtNum(git.total_commits || 0)} commit${(git.total_commits || 0) !== 1 ? 's' : ''}</div>
      <div class="sub"><span style="color:var(--green,#3fb950)">+${fmtNum(git.total_lines_added || 0)} lines</span> / <span style="color:var(--red,#f85149)">-${fmtNum(git.total_lines_removed || 0)} lines</span> / ${fmtNum(git.unique_prs || 0)} PRs</div>
    </div>`;
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
    ${gitCard}
  </div>`;
}

/* ===== Render: Bar Chart ===== */
function barTooltip(item, labelFn, valueFn) {
  const label = labelFn(item, true);
  const cost = (item.cost_cents || 0) / 100;
  const inp = item.input_tokens || 0;
  const outp = item.output_tokens || 0;
  if (inp || outp) return `${label}: ${fmtCost(cost)} — ${fmtNum(inp)} input, ${fmtNum(outp)} output`;
  if (cost > 0) return `${label}: ${fmtCost(cost)}`;
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

