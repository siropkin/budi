async function render() {
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';

  try {
    await loadAllData();
    renderStatsView(content);
    bindAllHandlers();
  } catch (err) {
    content.innerHTML = renderError(err);
    bindErrorHandlers();
  }
}

function renderError(err) {
  const detail = err && err.message ? err.message : '';
  return `<div class="error-state">
    <div class="error-icon">!</div>
    <div class="error-title">Failed to load analytics</div>
    <div class="error-detail">${detail ? esc(detail) : 'Could not connect to budi-daemon.'}</div>
    <div class="error-actions">
      <button class="btn btn-primary" id="retryBtn">Retry</button>
      <button class="btn btn-secondary" id="syncBtn">Sync Data</button>
    </div>
    <div class="error-hint">Or run <code>budi sync</code> in your terminal</div>
  </div>`;
}

function bindErrorHandlers() {
  const retryBtn = $('#retryBtn');
  if (retryBtn) retryBtn.addEventListener('click', () => render());
  const syncBtn = $('#syncBtn');
  if (syncBtn) syncBtn.addEventListener('click', async () => {
    syncBtn.textContent = 'Syncing...';
    syncBtn.disabled = true;
    try {
      await fetch('/sync', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"migrate":true}' });
      render();
    } catch (_) {
      syncBtn.textContent = 'Sync failed';
      setTimeout(() => { syncBtn.textContent = 'Sync Data'; syncBtn.disabled = false; }, 2000);
    }
  });
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
        const result = await fetchMessages(DEFAULT_TABLE_ROWS, 0);
        lastSessionData = result.messages || [];
        sessionTotalCount = result.total_count || 0;
        $('#sessionsContainer').innerHTML = renderMessagesSection(lastSessionData);
        bindTableHandlers();
      }, 300);
    });
  }
}

function bindAllHandlers() {
  bindSearchHandlers();
  bindTableHandlers();
}

function bindTableHandlers() {
  // Sessions table sort — re-fetch from server
  $$('#sessionsTable th[data-col]').forEach(th => {
    th.addEventListener('click', async () => {
      const col = th.dataset.col;
      if (sessionSortCol === col) sessionSortAsc = !sessionSortAsc;
      else { sessionSortCol = col; sessionSortAsc = col === 'session_id' || col === 'repo_id'; }
      const result = await fetchMessages(DEFAULT_TABLE_ROWS, 0);
      lastSessionData = result.messages || [];
      sessionTotalCount = result.total_count || 0;
      $('#sessionsContainer').innerHTML = renderMessagesSection(lastSessionData);
      bindTableHandlers();
    });
  });
  $$('.show-more-btn').forEach(btn => {
    btn.addEventListener('click', async () => {
      const table = btn.dataset.table;
      if (table === 'sessionsTable') {
        // Fetch next page from server and append
        const result = await fetchMessages(DEFAULT_TABLE_ROWS, lastSessionData.length);
        lastSessionData = lastSessionData.concat(result.messages || []);
        sessionTotalCount = result.total_count || sessionTotalCount;
        $('#sessionsContainer').innerHTML = renderMessagesSection(lastSessionData);
      }
      bindTableHandlers();
    });
  });
}

// Request sequencing — cancel in-flight fetches when period changes
let currentAbort = null;

// Shared reload helper — aborts previous in-flight requests
async function switchAndReload() {
  if (currentAbort) currentAbort.abort();
  const abort = new AbortController();
  currentAbort = abort;
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  try {
    await loadStatsData(abort.signal);
    if (abort.signal.aborted) return;
    renderStatsView(content);
    bindAllHandlers();
  } catch (err) {
    if (abort.signal.aborted) return;
    content.innerHTML = renderError(err);
    bindErrorHandlers();
  }
}

// Period tab switching — restore saved selection
$$('.period-tabs button').forEach(btn => {
  if (btn.dataset.period === currentPeriod) {
    $$('.period-tabs button').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
  }
  btn.addEventListener('click', () => {
    $$('.period-tabs button').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    currentPeriod = btn.dataset.period;
    localStorage.setItem('budi_period', currentPeriod);
    switchAndReload();
  });
});

render();

// Auto-refresh: poll every 30s to keep dashboard data fresh (skip if tab is hidden)
setInterval(async () => {
  if (document.hidden || !dataLoaded) return;
  await loadStatsData();
  renderStatsView($('#content'));
  bindAllHandlers();
}, 30000);
