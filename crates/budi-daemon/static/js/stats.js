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

