async function render() {
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  // Hide period tabs on settings page
  const periodBar = $('.period-tabs');
  if (periodBar) periodBar.style.display = currentPage === 'settings' ? 'none' : '';

  try {
    await loadAllData();
    await renderCurrentPage(content);
  } catch (err) {
    content.innerHTML = renderError(err);
    bindErrorHandlers();
  }
}

async function renderCurrentPage(content) {
  if (currentPage === 'insights') {
    if (!insightsData) await loadInsightsData();
    renderInsightsView(content);
  } else if (currentPage === 'sessions') {
    if (selectedSessionId) {
      await renderSessionDetail(selectedSessionId, content);
    } else {
      if (!sessionsPageData) await loadSessionsPageData();
      renderSessionsView(content);
      bindSessionsHandlers(content);
    }
  } else if (currentPage === 'settings') {
    if (!settingsData) await loadSettingsData();
    renderSettingsView(content);
  } else {
    if (!statsData) await loadStatsData();
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


// Request sequencing — cancel in-flight fetches when period changes
let currentAbort = null;

// Shared reload helper — aborts previous in-flight requests
async function switchAndReload() {
  if (currentAbort) currentAbort.abort();
  const abort = new AbortController();
  currentAbort = abort;
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  // Clear cached data for pages that need refresh
  insightsData = null;
  sessionsPageData = null;
  settingsData = null;
  selectedSessionId = null;
  // Hide period tabs on settings page
  const periodBar = $('.period-tabs');
  if (periodBar) periodBar.style.display = currentPage === 'settings' ? 'none' : '';
  try {
    if (currentPage === 'insights') {
      await loadInsightsData(abort.signal);
      if (abort.signal.aborted) return;
      renderInsightsView(content);
    } else if (currentPage === 'sessions') {
      await loadSessionsPageData(abort.signal);
      if (abort.signal.aborted) return;
      renderSessionsView(content);
      bindSessionsHandlers(content);
    } else if (currentPage === 'settings') {
      await loadSettingsData();
      if (abort.signal.aborted) return;
      renderSettingsView(content);
    } else {
      await loadStatsData(abort.signal);
      if (abort.signal.aborted) return;
      renderStatsView(content);
      // Overview has no interactive handlers
    }
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

// Page tab switching — use real URL paths
$$('.page-tabs button').forEach(btn => {
  if (btn.dataset.page === currentPage) {
    $$('.page-tabs button').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
  }
  btn.addEventListener('click', () => {
    $$('.page-tabs button').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    currentPage = btn.dataset.page;
    const url = currentPage === 'overview' ? '/dashboard' : '/dashboard/' + currentPage;
    history.pushState(null, '', url);
    selectedSessionId = null;
    switchAndReload();
  });
});

// Handle browser back/forward
window.addEventListener('popstate', () => {
  const path = location.pathname.replace(/^\/dashboard\/?/, '');
  // Check for session detail URL: sessions/:id
  const sessionMatch = path.match(/^sessions\/(.+)$/);
  if (sessionMatch) {
    currentPage = 'sessions';
    selectedSessionId = decodeURIComponent(sessionMatch[1]);
  } else {
    const newPage = VALID_PAGES.includes(path) ? path : 'overview';
    currentPage = newPage;
    selectedSessionId = null;
  }
  $$('.page-tabs button').forEach(b => {
    b.classList.toggle('active', b.dataset.page === currentPage);
  });
  switchAndReload();
});

render();

// Auto-refresh: poll every 30s for overview, every 5s for settings sync status
setInterval(async () => {
  if (document.hidden || !dataLoaded) return;
  if (currentPage === 'overview') {
    try {
      await loadStatsData();
      renderStatsView($('#content'));
    } catch (_) { /* poll failure is non-fatal */ }
  }
}, 30000);

setInterval(async () => {
  if (document.hidden || currentPage !== 'settings') return;
  await refreshSettingsStatus();
}, 5000);
