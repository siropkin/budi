/* ===== Sessions Page ===== */

const CHART_MAX_BUCKETS = 40;
const BRANCH_TRUNCATE_LEN = 30;
const SEARCH_DEBOUNCE_MS = 300;

// Sessions page sort state
let sessionsPageSortCol = 'started_at';
let sessionsPageSortAsc = false;
let sessionsPageSearchTerm = '';

function renderSessionsList(sessions) {
  const multiProvider = registeredProviders.length > 1;
  const cols = [
    { key: 'started_at', label: 'Time' },
    { key: 'duration', label: 'Duration' },
    ...(multiProvider ? [{ key: 'provider', label: 'Agent' }] : []),
    { key: 'model', label: 'Model' },
    { key: 'repo_id', label: 'Repo' },
    { key: 'git_branch', label: 'Branch' },
    { key: 'tokens', label: 'Tokens', right: true },
    { key: 'cost', label: 'Cost', right: true },
  ];

  if (!sessions || !sessions.length) return '<div class="empty">No sessions for this period</div>';
  const arrow = col => col === sessionsPageSortCol ? `<span class="sort-arrow">${sessionsPageSortAsc ? '\u25b2' : '\u25bc'}</span>` : '';
  const hasMore = sessionsPageTotalCount > sessions.length;
  const remaining = sessionsPageTotalCount - sessions.length;

  const rowFn = s => {
    const cost = (s.cost_cents || 0) / 100;
    const branch = s.git_branch ? s.git_branch.replace(/^refs\/heads\//, '') : '';
    const shortBranch = branch.length > BRANCH_TRUNCATE_LEN ? branch.slice(0, BRANCH_TRUNCATE_LEN - 3) + '...' : branch;
    const models = (s.model || '').split(',').map(m => m.trim()).filter(Boolean);
    const model = models.length ? formatModelName(models[0]) + (models.length > 1 ? ' +' + (models.length - 1) : '') : '--';
    const totalTok = (s.input_tokens || 0) + (s.output_tokens || 0);
    const duration = fmtSessionDuration(s);
    const provDisplay = multiProvider
      ? ((registeredProviders.find(rp => rp.name === s.provider) || {}).display_name || s.provider || '--')
      : '';

    return `<tr class="session-row" data-session-id="${esc(s.session_id)}" style="cursor:pointer">
      <td>${esc(fmtDate(s.started_at))}</td>
      <td>${esc(duration)}</td>
      ${multiProvider ? `<td>${esc(provDisplay)}</td>` : ''}
      <td title="${esc(s.model || '')}">${esc(model)}</td>
      <td class="dir" title="${esc(s.repo_id || '')}">${esc(repoName(s.repo_id) || '--')}</td>
      <td class="dir" title="${esc(branch)}">${esc(shortBranch || '--')}</td>
      <td class="right">${fmtNum(totalTok)}</td>
      <td class="right">${fmtCost(cost)}</td>
    </tr>`;
  };

  return `
  <div class="table-scroll">
  <table class="sortable-table" id="sessionsPageTable">
    <thead><tr>${cols.map(c =>
      `<th data-col="${c.key}"${c.right ? ' class="right"' : ''}>${c.label}${arrow(c.key)}</th>`
    ).join('')}</tr></thead>
    <tbody>${sessions.map(rowFn).join('')}</tbody>
  </table>
  </div>
  ${hasMore ? `<button class="show-more-btn" data-table="sessionsPageTable">Show more (${remaining} remaining)</button>` : ''}`;
}

// Session detail state
let sessionDetailMessages = [];
let sessionDetailSortCol = 'timestamp';
let sessionDetailSortAsc = false;
let sessionDetailSearch = '';

function renderSessionDetailMessages(messages) {
  const multiProvider = registeredProviders.length > 1;
  // Client-side search
  let filtered = messages;
  if (sessionDetailSearch) {
    const q = sessionDetailSearch.toLowerCase();
    filtered = messages.filter(m =>
      (m.model || '').toLowerCase().includes(q) ||
      (m.provider || '').toLowerCase().includes(q) ||
      (m.cost_confidence || '').toLowerCase().includes(q)
    );
  }
  // Client-side sort
  const col = sessionDetailSortCol;
  const asc = sessionDetailSortAsc;
  const sorted = [...filtered];
  sorted.sort((a, b) => {
    let va, vb;
    if (col === 'tokens') { va = (a.input_tokens || 0) + (a.output_tokens || 0); vb = (b.input_tokens || 0) + (b.output_tokens || 0); }
    else if (col === 'cost') { va = a.cost_cents || 0; vb = b.cost_cents || 0; }
    else if (col === 'model') { va = a.model || ''; vb = b.model || ''; }
    else if (col === 'provider') { va = a.provider || ''; vb = b.provider || ''; }
    else { va = a.timestamp || ''; vb = b.timestamp || ''; }
    if (va < vb) return asc ? -1 : 1;
    if (va > vb) return asc ? 1 : -1;
    return 0;
  });

  const arrow = c => c === sessionDetailSortCol ? `<span class="sort-arrow">${sessionDetailSortAsc ? '\u25b2' : '\u25bc'}</span>` : '';
  const cols = [
    { key: 'timestamp', label: 'Time' },
    ...(multiProvider ? [{ key: 'provider', label: 'Agent' }] : []),
    { key: 'model', label: 'Model' },
    { key: 'tokens', label: 'Tokens', right: true },
    { key: 'cost', label: 'Cost', right: true },
  ];

  const rows = sorted.map(m => {
    const totalTok = (m.input_tokens || 0) + (m.output_tokens || 0);
    const costVal = (m.cost_cents || 0) / 100;
    const isExact = !m.cost_confidence || m.cost_confidence === 'exact' || m.cost_confidence === 'exact_cost' || m.cost_confidence === 'otel_exact';
    const costDisplay = isExact ? fmtCost(costVal) : `~${fmtCost(costVal)}`;
    const costClass = isExact ? 'right' : 'right muted';
    const provDisplay = (registeredProviders.find(rp => rp.name === m.provider) || {}).display_name || m.provider;
    return `<tr>
      <td>${esc(fmtDate(m.timestamp))}</td>
      ${multiProvider ? `<td>${esc(provDisplay)}</td>` : ''}
      <td title="${esc(m.model || '')}">${esc(formatModelName(m.model || 'unknown'))}</td>
      <td class="right">${fmtNum(totalTok)}</td>
      <td class="${costClass}" title="${esc(m.cost_confidence || 'n/a')}">${costDisplay}</td>
    </tr>`;
  }).join('');

  return `
  <div class="table-scroll">
  <table class="sortable-table" id="sessionDetailTable">
    <thead><tr>${cols.map(c =>
      `<th data-col="${c.key}"${c.right ? ' class="right"' : ''}>${c.label}${arrow(c.key)}</th>`
    ).join('')}</tr></thead>
    <tbody>${rows}</tbody>
  </table>
  </div>`;
}

// Group messages into buckets for input token growth chart
function groupMessagesForChart(messages, maxBuckets) {
  if (messages.length <= maxBuckets) return messages;
  const bucketSize = Math.ceil(messages.length / maxBuckets);
  const buckets = [];
  for (let i = 0; i < messages.length; i += bucketSize) {
    const slice = messages.slice(i, i + bucketSize);
    const last = slice[slice.length - 1];
    buckets.push({
      input_tokens: Math.max(...slice.map(m => m.input_tokens)),
      cost_cents: slice.reduce((s, m) => s + (m.cost_cents || 0), 0),
      label: 'Msgs ' + (i + 1) + '-' + Math.min(i + bucketSize, messages.length),
      timestamp: last.timestamp,
    });
  }
  return buckets;
}

function healthIcon(state) {
  switch (state) {
    case 'red': return '🔴';
    case 'yellow': return '🟡';
    case 'gray': return '⚪';
    default: return '🟢';
  }
}

function renderHealthPanel(health) {
  if (!health) return '';

  const vitalNames = {
    context_drag: 'Context Growth',
    cache_efficiency: 'Cache Reuse',
    thrashing: 'Retry Loops',
    cost_acceleration: 'Cost Per Turn',
  };

  const vitals = health.vitals || {};
  const vitalKeys = ['context_drag', 'cache_efficiency', 'thrashing', 'cost_acceleration'];

  const cards = vitalKeys.map(key => {
    const v = vitals[key];
    if (!v) return `<div class="vital-card">
      <div class="vital-header"><span class="vital-name">${vitalNames[key]}</span><span class="vital-state">⚪</span></div>
      <div class="vital-label" style="color:var(--text-muted)">Not enough data</div>
    </div>`;

    return `<div class="vital-card">
      <div class="vital-header"><span class="vital-name">${vitalNames[key]}</span><span class="vital-state">${healthIcon(v.state)}</span></div>
      <div class="vital-label">${esc(v.label)}</div>
    </div>`;
  }).join('');

  const tips = (health.details || []).map(d => {
    const name = vitalNames[d.vital] || d.vital;
    const tipIcon = healthIcon(d.state);
    const actions = (d.actions || []).length
      ? `<ul class="health-tip-actions">${d.actions.map(action => `<li>${esc(action)}</li>`).join('')}</ul>`
      : '';
    return `<div class="health-tip-card">
      <div class="health-tip-body">
        ${tipIcon} <strong>${esc(name)}:</strong> ${esc(d.tip)}
        ${actions}
      </div>
    </div>`;
  }).join('');

  return `<div class="panel section-mb" id="health">
    <h2>Health</h2>
    <div class="vitals-grid">${cards}</div>
    ${tips ? `<div class="health-tips"><h3 class="health-tips-title">Tips</h3>${tips}</div>` : ''}
  </div>`;
}

async function renderSessionDetail(sessionId, content, signal) {
  content.innerHTML = '<div class="loading">Loading session</div>';

  const sessionsListPromise = sessionsPageData ? Promise.resolve() : loadSessionsPageData(signal);
  const [msgs, tags, health] = await Promise.all([
    loadSessionMessages(sessionId, signal),
    loadSessionTags(sessionId, signal),
    loadSessionHealth(sessionId, signal),
  ]);
  if (signal && signal.aborted) return;
  await sessionsListPromise;
  sessionDetailMessages = msgs;
  sessionDetailSortCol = 'timestamp';
  sessionDetailSortAsc = false;
  sessionDetailSearch = '';
  const session = (sessionsPageData || []).find(s => s.session_id === sessionId);

  const totalCost = sessionDetailMessages.reduce((s, m) => s + (m.cost_cents || 0), 0) / 100;
  const totalTokens = sessionDetailMessages.reduce((s, m) => s + (m.input_tokens || 0) + (m.output_tokens || 0), 0);
  const duration = session ? fmtSessionDuration(session) : '--';

  // Filter tags to show only interesting ones (skip redundant keys)
  const skipTagKeys = new Set(['provider', 'model', 'repo', 'machine', 'cost_confidence']);
  const displayTags = (tags || []).filter(t => !skipTagKeys.has(t.key));
  const tagsHtml = displayTags.length
    ? displayTags.map(t => `<span class="tag-chip" title="${esc(t.key)}">${esc(t.key)}: ${esc(t.value)}</span>`).join(' ')
    : '';

  const meta = session ? `
    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Session</h2>
        <div class="session-meta" style="flex-direction:column">
          <div class="meta-item"><span class="meta-label">Model: </span><span class="meta-value">${esc((session.model || '').split(',').map(m => formatModelName(m.trim())).filter(Boolean).join(', ') || '--')}</span></div>
          <div class="meta-item"><span class="meta-label">Repo: </span><span class="meta-value">${esc(repoName(session.repo_id) || '--')}</span></div>
          <div class="meta-item"><span class="meta-label">Branch: </span><span class="meta-value">${esc((session.git_branch || '').replace(/^refs\/heads\//, '') || '--')}</span></div>
          <div class="meta-item"><span class="meta-label">Duration: </span><span class="meta-value">${esc(duration)}</span></div>
        </div>
      </div>
      <div class="panel">
        <h2>Usage</h2>
        <div class="session-meta" style="flex-direction:column">
          <div class="meta-item"><span class="meta-label">Messages: </span><span class="meta-value">${sessionDetailMessages.length}</span></div>
          <div class="meta-item"><span class="meta-label">Tokens: </span><span class="meta-value">${fmtNum(totalTokens)}</span></div>
          <div class="meta-item"><span class="meta-label">Cost: </span><span class="meta-value">${fmtCost(totalCost)}</span></div>
          ${health ? `<div class="meta-item"><span class="meta-label">Health: </span><span class="meta-value">${healthIcon(health.state)} ${esc(health.tip || '')}</span></div>` : ''}
        </div>
      </div>
    </div>
    ${tagsHtml ? `<div class="panel section-mb">
      <h2>Tags</h2>
      <div style="display:flex;flex-wrap:wrap;gap:6px">${tagsHtml}</div>
    </div>` : ''}` : '';

  // Input token growth chart — group into max buckets
  let bloatChart = '';
  if (sessionDetailMessages.length >= 2) {
    const chartData = groupMessagesForChart(sessionDetailMessages, CHART_MAX_BUCKETS);
    const maxInput = Math.max(...chartData.map(m => m.input_tokens), 1);
    bloatChart = `
    <div class="panel section-mb">
      <h2>Input Token Growth</h2>
      <div class="daily-chart" style="height:100px">${chartData.map((m, i) => {
        const h = (m.input_tokens / maxInput) * 100;
        const tip = m.label || ('Msg ' + (i + 1));
        return `<div class="day-bar" style="height:100%">
          <div class="daily-chart-tooltip">${esc(tip)}: ${fmtNum(m.input_tokens)} input, ${fmtCost((m.cost_cents || 0) / 100)}</div>
          <div class="bar-msg" style="height:${h}%"></div>
        </div>`;
      }).join('')}</div>
    </div>`;
  }

  const healthPanel = renderHealthPanel(health);

  content.innerHTML = `
    <button class="btn btn-secondary" id="backToSessions" style="margin-bottom:12px">Back to sessions</button>
    ${meta}
    ${healthPanel}
    ${bloatChart}
    <div class="panel section-mb">
      <h2>Messages</h2>
      <input type="text" id="sessionDetailSearch" class="search-input" placeholder="Search messages..." style="margin-bottom:12px">
      <div id="sessionDetailContainer">${renderSessionDetailMessages(sessionDetailMessages)}</div>
    </div>
  `;

  bindSessionDetailHandlers(content);

  if (location.hash) {
    const target = document.querySelector(location.hash);
    if (target) target.scrollIntoView({ behavior: 'smooth' });
  }
}

function bindSessionDetailHandlers(content) {
  const backBtn = $('#backToSessions');
  if (backBtn) backBtn.addEventListener('click', () => {
    selectedSessionId = null;
    history.pushState(null, '', '/dashboard/sessions');
    renderSessionsView(content);
    bindSessionsHandlers(content);
  });

  const container = $('#sessionDetailContainer');
  if (container) {
    container.onclick = (e) => {
      const th = e.target.closest('#sessionDetailTable th[data-col]');
      if (th) {
        const col = th.dataset.col;
        if (sessionDetailSortCol === col) sessionDetailSortAsc = !sessionDetailSortAsc;
        else { sessionDetailSortCol = col; sessionDetailSortAsc = false; }
        container.innerHTML = renderSessionDetailMessages(sessionDetailMessages);
      }
    };
  }

  const searchEl = $('#sessionDetailSearch');
  let searchTimeout = null;
  if (searchEl) {
    searchEl.addEventListener('input', (e) => {
      sessionDetailSearch = e.target.value;
      clearTimeout(searchTimeout);
      searchTimeout = setTimeout(() => {
        if (container) container.innerHTML = renderSessionDetailMessages(sessionDetailMessages);
      }, SEARCH_DEBOUNCE_MS);
    });
  }
}

function renderSessionsView(content) {
  content.innerHTML = `
    <div class="panel section-mb">
      <h2>Sessions</h2>
      <input type="text" id="sessionsPageSearch" class="search-input" placeholder="Search sessions..." value="${esc(sessionsPageSearchTerm)}" style="margin-bottom:12px">
      <div id="sessionsPageContainer">${renderSessionsList(sessionsPageData)}</div>
    </div>
  `;
}

function bindSessionsHandlers(content) {
  const container = $('#sessionsPageContainer');
  if (!container) return;

  container.onclick = async (e) => {
    const row = e.target.closest('.session-row');
    if (row) {
      selectedSessionId = row.dataset.sessionId;
      history.pushState(null, '', '/dashboard/sessions/' + encodeURIComponent(selectedSessionId));
      renderSessionDetail(selectedSessionId, content).catch(err => {
        content.innerHTML = renderError(err);
        bindErrorHandlers();
      });
      return;
    }

    const th = e.target.closest('#sessionsPageTable th[data-col]');
    if (th) {
      const col = th.dataset.col;
      if (sessionsPageSortCol === col) sessionsPageSortAsc = !sessionsPageSortAsc;
      else { sessionsPageSortCol = col; sessionsPageSortAsc = false; }
      await reloadSessionsPage(content);
      return;
    }

    const moreBtn = e.target.closest('[data-table="sessionsPageTable"].show-more-btn');
    if (moreBtn) {
      moreBtn.textContent = 'Loading...';
      moreBtn.disabled = true;
      const extra = { limit: SESSIONS_PAGE_LIMIT, offset: sessionsPageData.length, sort_by: sessionsPageSortCol, sort_asc: sessionsPageSortAsc };
      if (sessionsPageSearchTerm) extra.search = sessionsPageSearchTerm;
      const result = await fetch(buildUrl('/analytics/sessions', extra)).then(fetchOk).catch(() => ({ sessions: [], total_count: 0 }));
      sessionsPageData = sessionsPageData.concat(result.sessions || []);
      sessionsPageTotalCount = result.total_count || sessionsPageTotalCount;
      container.innerHTML = renderSessionsList(sessionsPageData);
      return;
    }
  };

  let searchTimeout = null;
  const searchEl = $('#sessionsPageSearch');
  if (searchEl && !searchEl._bound) {
    searchEl._bound = true;
    searchEl.addEventListener('input', (e) => {
      sessionsPageSearchTerm = e.target.value;
      clearTimeout(searchTimeout);
      searchTimeout = setTimeout(async () => {
        await reloadSessionsPage(content);
      }, SEARCH_DEBOUNCE_MS);
    });
  }
}

async function reloadSessionsPage(content) {
  const extra = { limit: SESSIONS_PAGE_LIMIT, sort_by: sessionsPageSortCol, sort_asc: sessionsPageSortAsc };
  if (sessionsPageSearchTerm) extra.search = sessionsPageSearchTerm;
  const result = await fetch(buildUrl('/analytics/sessions', extra)).then(fetchOk).catch(() => ({ sessions: [], total_count: 0 }));
  sessionsPageData = result.sessions || [];
  sessionsPageTotalCount = result.total_count || 0;
  const container = $('#sessionsPageContainer');
  if (!container) return;
  container.innerHTML = renderSessionsList(sessionsPageData);
  bindSessionsHandlers(content);
}
