async function render() {
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';

  try {
    await loadAllData();
    renderCurrentView();
  } catch (err) {
    content.innerHTML = `<div class="empty">
      Failed to load analytics.<br>
      <span style="font-size:0.85rem;color:var(--text-muted)">Is budi-daemon running? Try: <code>budi sync</code> first.</span>
    </div>`;
  }
}

function bindSearchHandler(inputId, setTerm, resetCount, rerender) {
  const el = $('#' + inputId);
  if (el) {
    el.addEventListener('input', (e) => {
      setTerm(e.target.value);
      resetCount();
      rerender();
      bindTableHandlers();
    });
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
  bindSearchHandler('configSearch',
    v => { configSearchTerm = v; },
    () => { configShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#configContainer').innerHTML = renderConfigTable(); }
  );
  bindSearchHandler('projectConfigSearch',
    v => { projectConfigSearchTerm = v; },
    () => { projectConfigShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#projectConfigContainer').innerHTML = renderProjectConfigTable(); }
  );
  bindSearchHandler('pluginsSearch',
    v => { pluginsSearchTerm = v; },
    () => { pluginsShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#pluginsContainer').innerHTML = renderPluginsTable(); }
  );
  bindSearchHandler('permissionsSearch',
    v => { permissionsSearchTerm = v; },
    () => { permissionsShowCount = DEFAULT_TABLE_ROWS; },
    () => { $('#permissionsContainer').innerHTML = renderPermissionsTable(); }
  );
  // Plans search — debounced, server-side
  let plansSearchTimeout = null;
  const plansSearchEl = $('#plansSearch');
  if (plansSearchEl) {
    plansSearchEl.addEventListener('input', (e) => {
      plansSearchTerm = e.target.value;
      clearTimeout(plansSearchTimeout);
      plansSearchTimeout = setTimeout(async () => {
        const result = await fetchPlans(DEFAULT_TABLE_ROWS, 0);
        lastPlansData = result.plans || [];
        plansTotalCount = result.total_count || 0;
        plansShowCount = lastPlansData.length;
        $('#plansContainer').innerHTML = renderPlansTable(lastPlansData);
        bindTableHandlers();
      }, 300);
    });
  }
  // Prompts search — debounced, server-side
  let promptsSearchTimeout = null;
  const promptsSearchEl = $('#promptsSearch');
  if (promptsSearchEl) {
    promptsSearchEl.addEventListener('input', (e) => {
      promptsSearchTerm = e.target.value;
      clearTimeout(promptsSearchTimeout);
      promptsSearchTimeout = setTimeout(async () => {
        const result = await fetchPrompts(DEFAULT_TABLE_ROWS, 0);
        lastHistoryData = result.entries || [];
        promptsTotalCount = result.total_count || 0;
        historyShowCount = lastHistoryData.length;
        $('#historyContainer').innerHTML = renderHistoryTable(lastHistoryData);
        bindTableHandlers();
      }, 300);
    });
  }
}

function bindAllHandlers() {
  bindSearchHandlers();
  bindTableHandlers();
  // Agent tile click-to-filter removed for now
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
  $$('#projectConfigTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (projectConfigSortCol === col) projectConfigSortAsc = !projectConfigSortAsc;
      else { projectConfigSortCol = col; projectConfigSortAsc = col === 'project'; }
      $('#projectConfigContainer').innerHTML = renderProjectConfigTable();
      bindTableHandlers();
    });
  });
  $$('#configTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (configSortCol === col) configSortAsc = !configSortAsc;
      else { configSortCol = col; configSortAsc = col === 'path' || col === 'project' || col === 'file_type'; }
      $('#configContainer').innerHTML = renderConfigTable();
      bindTableHandlers();
    });
  });
  $$('#historyTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (historySortCol === col) historySortAsc = !historySortAsc;
      else { historySortCol = col; historySortAsc = col === 'display' || col === 'project'; }
      // Client-side sort on loaded data (prompts sorted client-side for now)
      $('#historyContainer').innerHTML = renderHistoryTable(lastHistoryData);
      bindTableHandlers();
    });
  });
  $$('#plansTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (plansSortCol === col) plansSortAsc = !plansSortAsc;
      else { plansSortCol = col; plansSortAsc = col === 'name'; }
      // Client-side sort on loaded data (plans sorted client-side for now)
      $('#plansContainer').innerHTML = renderPlansTable(lastPlansData);
      bindTableHandlers();
    });
  });
  $$('#pluginsTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (pluginsSortCol === col) pluginsSortAsc = !pluginsSortAsc;
      else { pluginsSortCol = col; pluginsSortAsc = col === 'name' || col === 'scope'; }
      $('#pluginsContainer').innerHTML = renderPluginsTable();
      bindTableHandlers();
    });
  });
  $$('#permissionsTable th[data-col]').forEach(th => {
    th.addEventListener('click', () => {
      const col = th.dataset.col;
      if (permissionsSortCol === col) permissionsSortAsc = !permissionsSortAsc;
      else { permissionsSortCol = col; permissionsSortAsc = col === 'rule' || col === 'scope'; }
      $('#permissionsContainer').innerHTML = renderPermissionsTable();
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
      } else if (table === 'configTable') {
        configShowCount += DEFAULT_TABLE_ROWS;
        $('#configContainer').innerHTML = renderConfigTable();
      } else if (table === 'projectConfigTable') {
        projectConfigShowCount += DEFAULT_TABLE_ROWS;
        $('#projectConfigContainer').innerHTML = renderProjectConfigTable();
      } else if (table === 'historyTable') {
        const result = await fetchPrompts(DEFAULT_TABLE_ROWS, lastHistoryData.length);
        lastHistoryData = lastHistoryData.concat(result.entries || []);
        promptsTotalCount = result.total_count || promptsTotalCount;
        historyShowCount = lastHistoryData.length;
        $('#historyContainer').innerHTML = renderHistoryTable(lastHistoryData);
      } else if (table === 'plansTable') {
        const result = await fetchPlans(DEFAULT_TABLE_ROWS, lastPlansData.length);
        lastPlansData = lastPlansData.concat(result.plans || []);
        plansTotalCount = result.total_count || plansTotalCount;
        plansShowCount = lastPlansData.length;
        $('#plansContainer').innerHTML = renderPlansTable(lastPlansData);
      } else if (table === 'pluginsTable') {
        pluginsShowCount += DEFAULT_TABLE_ROWS;
        $('#pluginsContainer').innerHTML = renderPluginsTable();
      } else if (table === 'permissionsTable') {
        permissionsShowCount += DEFAULT_TABLE_ROWS;
        $('#permissionsContainer').innerHTML = renderPermissionsTable();
      }
      bindTableHandlers();
    });
  });
}

// Nav tab switching
$$('.nav-tabs a').forEach(a => {
  a.addEventListener('click', e => {
    e.preventDefault();
    history.pushState({}, '', a.href);
    currentView = a.dataset.view;
    if (dataLoaded) {
      renderCurrentView();
    }
  });
});

window.addEventListener('popstate', () => {
  currentView = getCurrentView();
  if (dataLoaded) {
    renderCurrentView();
  }
});

// Request sequencing — cancel in-flight fetches when period/filter changes
let currentAbort = null;

// Shared reload helper — aborts previous in-flight requests
async function switchAndReload() {
  if (currentAbort) currentAbort.abort();
  const abort = new AbortController();
  currentAbort = abort;
  insightsData = null; // Clear insights cache on period/view change
  const content = $('#content');
  content.innerHTML = '<div class="loading">Loading analytics</div>';
  try {
    await loadStatsData(abort.signal);
    if (abort.signal.aborted) return;
    renderCurrentView();
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

// Update nav active state on initial load
$$('.nav-tabs a').forEach(a => a.classList.toggle('active', a.dataset.view === currentView));

// Show/hide period tabs based on initial view
$('#periodBar').style.display = (currentView === 'stats' || currentView === 'insights') ? 'flex' : 'none';

render();

// Auto-refresh: poll every 30s to keep dashboard data fresh (skip if tab is hidden)
setInterval(async () => {
  if (document.hidden || !dataLoaded) return;
  if (currentView === 'stats') {
    await loadStatsData();
  } else if (currentView === 'insights') {
    insightsData = null;
    await loadInsightsData();
  }
  renderCurrentView();
}, 30000);
