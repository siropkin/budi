async function render() {
  if (currentAbort) currentAbort.abort();
  const abort = new AbortController();
  currentAbort = abort;
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  const periodBar = $('.period-tabs');
  if (periodBar) periodBar.style.display = currentPage === 'settings' ? 'none' : '';
  try {
    await ensureProvidersLoaded(abort.signal);
    if (abort.signal.aborted) return;
    await renderCurrentPage(content, abort.signal);
    dataLoaded = true;
  } catch (err) {
    if (abort.signal.aborted) return;
    content.innerHTML = renderError(err);
    bindErrorHandlers();
  }
}

async function renderCurrentPage(content, signal) {
  if (currentPage === 'insights') {
    if (!insightsData) await loadInsightsData(signal);
    if (signal && signal.aborted) return;
    renderInsightsView(content);
  } else if (currentPage === 'sessions') {
    if (selectedSessionId) {
      await renderSessionDetail(selectedSessionId, content, signal);
    } else {
      if (!sessionsPageData) await loadSessionsPageData(signal);
      if (signal && signal.aborted) return;
      renderSessionsView(content);
      bindSessionsHandlers(content);
    }
  } else if (currentPage === 'settings') {
    if (!settingsData) await loadSettingsData(signal);
    if (signal && signal.aborted) return;
    renderSettingsView(content);
  } else {
    if (!statsData) await loadStatsData(signal);
    if (signal && signal.aborted) return;
    renderStatsView(content);
  }
}

function renderError(err) {
  const detail = err && err.message ? err.message : '';
  const isConnectionError = !detail || detail.includes('Failed to fetch') || detail.includes('NetworkError');
  const title = isConnectionError ? 'Cannot reach budi-daemon' : 'Failed to load analytics';
  const hint = isConnectionError
    ? 'Run <code>budi init</code> to start the daemon'
    : 'Run <code>budi init</code> to set up, or <code>budi sync</code> to refresh data';
  return `<div class="error-state">
    <div class="error-icon">!</div>
    <div class="error-title">${title}</div>
    <div class="error-detail">${detail ? esc(detail) : ''}</div>
    <div class="error-actions">
      <button class="btn btn-primary" id="retryBtn">Retry</button>
      <button class="btn btn-secondary" id="syncBtn">Sync Data</button>
    </div>
    <div class="error-hint">${hint}</div>
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

let currentAbort = null;

async function switchAndReload() {
  if (currentAbort) currentAbort.abort();
  const abort = new AbortController();
  currentAbort = abort;
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  insightsData = null;
  sessionsPageData = null;
  settingsData = null;
  statsData = null;
  lastStatsHash = '';
  const sessionUrlMatch = location.pathname.match(/^\/dashboard\/sessions\/(.+)$/);
  try { selectedSessionId = sessionUrlMatch ? decodeURIComponent(sessionUrlMatch[1]) : null; } catch (_) { selectedSessionId = null; }
  const periodBar = $('.period-tabs');
  if (periodBar) periodBar.style.display = currentPage === 'settings' ? 'none' : '';
  try {
    await renderCurrentPage(content, abort.signal);
  } catch (err) {
    if (abort.signal.aborted) return;
    content.innerHTML = renderError(err);
    bindErrorHandlers();
  }
}

// Restore saved period selection
const periodButtons = $$('.period-tabs button');
periodButtons.forEach(btn => {
  if (btn.dataset.period === currentPeriod) {
    periodButtons.forEach(b => { b.classList.remove('active'); b.setAttribute('aria-selected', 'false'); });
    btn.classList.add('active');
    btn.setAttribute('aria-selected', 'true');
  }
  btn.addEventListener('click', () => {
    periodButtons.forEach(b => { b.classList.remove('active'); b.setAttribute('aria-selected', 'false'); });
    btn.classList.add('active');
    btn.setAttribute('aria-selected', 'true');
    currentPeriod = btn.dataset.period;
    localStorage.setItem('budi_period', currentPeriod);
    switchAndReload();
  });
});

// Page tab switching — use real URL paths
const pageButtons = $$('.page-tabs button');
pageButtons.forEach(btn => {
  if (btn.dataset.page === currentPage) {
    pageButtons.forEach(b => { b.classList.remove('active'); b.setAttribute('aria-selected', 'false'); });
    btn.classList.add('active');
    btn.setAttribute('aria-selected', 'true');
  }
  btn.addEventListener('click', () => {
    pageButtons.forEach(b => { b.classList.remove('active'); b.setAttribute('aria-selected', 'false'); });
    btn.classList.add('active');
    btn.setAttribute('aria-selected', 'true');
    currentPage = btn.dataset.page;
    const url = currentPage === 'overview' ? '/dashboard' : '/dashboard/' + currentPage;
    history.pushState(null, '', url);
    selectedSessionId = null;
    switchAndReload();
  });
});

window.addEventListener('popstate', () => {
  const path = location.pathname.replace(/^\/dashboard\/?/, '');
  const sessionMatch = path.match(/^sessions\/(.+)$/);
  if (sessionMatch) {
    currentPage = 'sessions';
    try { selectedSessionId = decodeURIComponent(sessionMatch[1]); } catch (_) { selectedSessionId = null; }
  } else {
    const newPage = VALID_PAGES.includes(path) ? path : 'overview';
    currentPage = newPage;
    selectedSessionId = null;
  }
  pageButtons.forEach(b => {
    const isActive = b.dataset.page === currentPage;
    b.classList.toggle('active', isActive);
    b.setAttribute('aria-selected', String(isActive));
  });
  switchAndReload();
});

render();

let overviewRefreshing = false;
let lastStatsHash = '';
setInterval(async () => {
  if (document.hidden || !dataLoaded || overviewRefreshing) return;
  if (currentPage === 'overview') {
    overviewRefreshing = true;
    try {
      await loadStatsData();
      const s = statsData.summary, c = statsData.cost;
      const hash = `${s.total_messages}|${s.total_input_tokens}|${c.total_cost}`;
      if (currentPage === 'overview' && hash !== lastStatsHash) {
        lastStatsHash = hash;
        renderStatsView($('#content'));
      }
    } catch (_) { /* poll failure is non-fatal */ }
    overviewRefreshing = false;
  }
}, 30000);

setInterval(async () => {
  if (document.hidden || currentPage !== 'settings') return;
  await refreshSettingsStatus();
}, 5000);
