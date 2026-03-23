async function render() {
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';

  try {
    await loadAllData();
    renderStatsView(content);
    bindAllHandlers();
  } catch (err) {
    content.innerHTML = `<div class="empty">
      Failed to load analytics.<br>
      <span style="font-size:0.85rem;color:var(--text-muted)">Is budi-daemon running? Try: <code>budi sync</code> first.</span>
    </div>`;
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
      const result = await fetchSessions(DEFAULT_TABLE_ROWS, 0);
      lastSessionData = result.sessions || [];
      sessionTotalCount = result.total_count || 0;
      sessionShowCount = lastSessionData.length;
      $('#sessionsContainer').innerHTML = renderSessionsSection(lastSessionData);
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

render();

// Auto-refresh: poll every 30s to keep dashboard data fresh (skip if tab is hidden)
setInterval(async () => {
  if (document.hidden || !dataLoaded) return;
  await loadStatsData();
  renderStatsView($('#content'));
  bindAllHandlers();
}, 30000);
