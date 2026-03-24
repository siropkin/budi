function renderSessionsSection(sessions) {
  // Sessions are already sorted, filtered, and paginated server-side
  const multiProvider = registeredProviders.length > 1;
  const cols = [
    { key: 'last_seen', label: 'Last Active' },
    { key: 'session_id', label: 'Session' },
    ...(multiProvider ? [{ key: 'provider', label: 'Agent' }] : []),
    { key: 'repo_id', label: 'Repo' },
    { key: 'git_branch', label: 'Branch' },
    { key: 'ticket', label: 'Ticket' },
    { key: 'commit_count', label: 'Commits', right: true },
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
    const branch = (s.git_branch || '').replace(/^refs\/heads\//, '');
    const ticketMatch = branch.match(/[a-zA-Z]{2,}-\d+/);
    const ticket = ticketMatch ? ticketMatch[0].toUpperCase() : '';
    return `<tr>
      <td>${esc(fmtDate(s.last_seen))}</td>
      <td title="${esc(s.session_id)}">${esc(title)}</td>
      ${provCol}
      <td class="dir" title="${esc(s.repo_id || s.project_dir || '')}">${esc(repoName(s.repo_id) || shortenDir(s.project_dir))}</td>
      <td class="dir" title="${esc(s.git_branch || '')}">${esc(branch)}</td>
      <td>${esc(ticket)}</td>
      <td class="right">${s.commit_count || ''}</td>
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

/* ===== View Renderer ===== */
function renderStatsView(content) {
  const { summary, sessions, cwds, cost, models, activityChart, contextUsage, interactionModes, topTools, mcpTools, branches, tickets, gitSummary } = statsData;
  content.innerHTML = `
    ${renderCards(summary, cost, gitSummary)}
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
    <div class="grid-3 section-mb">
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
      <div class="panel">
        <h2>Tickets${ccOnlyLabel()}</h2>
        ${renderBarChart((tickets || []).slice(0, DEFAULT_CHART_ROWS),
          t => t.value,
          t => t.cost_cents,
          (t, i) => paletteColor(i),
          'No ticket data for this period',
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

/* ===== Main Render ===== */
