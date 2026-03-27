function extractTicketId(branch) {
  if (!branch) return '';
  const m = branch.match(/([a-zA-Z]{2,})-(\d+)/);
  return m ? (m[1] + '-' + m[2]).toUpperCase() : '';
}

function renderMessagesSection(messages) {
  const multiProvider = registeredProviders.length > 1;
  const cols = [
    { key: 'timestamp', label: 'Time' },
    { key: 'model', label: 'Model' },
    { key: 'repo_id', label: 'Repo' },
    { key: 'git_branch', label: 'Branch' },
    { key: 'ticket', label: 'Ticket' },
    { key: 'tokens', label: 'Tokens', right: true },
    { key: 'cost', label: 'Cost', right: true },
  ];
  if (!messages.length) return '<div class="empty">No messages for this period</div>';
  const arrow = col => col === sessionSortCol ? `<span class="sort-arrow">${sessionSortAsc ? '\u25b2' : '\u25bc'}</span>` : '';
  const hasMore = sessionTotalCount > messages.length;
  const remaining = sessionTotalCount - messages.length;
  const rowFn = m => {
    const totalTok = m.input_tokens + m.output_tokens;
    const costVal = (m.cost_cents || 0) / 100;
    const isEstimated = m.cost_confidence && m.cost_confidence !== 'exact' && m.cost_confidence !== 'otel_exact';
    const costDisplay = isEstimated ? `~${fmtCost(costVal)}` : fmtCost(costVal);
    const costClass = isEstimated ? 'right muted' : 'right';
    const provDisplay = (registeredProviders.find(rp => rp.name === m.provider) || {}).display_name || m.provider;
    const modelLabel = multiProvider
      ? provDisplay + ' / ' + formatModelName(m.model || 'unknown')
      : formatModelName(m.model || 'unknown');
    const branch = m.git_branch ? m.git_branch.replace(/^refs\/heads\//, '') : '';
    const shortBranch = branch.length > 30 ? branch.slice(0, 27) + '...' : branch;
    const ticket = extractTicketId(branch);
    return `<tr>
      <td>${esc(fmtDate(m.timestamp))}</td>
      <td title="${esc(m.model || '')}">${esc(modelLabel)}</td>
      <td class="dir" title="${esc(m.repo_id || '')}">${esc(repoName(m.repo_id) || '--')}</td>
      <td class="dir" title="${esc(branch)}">${esc(shortBranch || '--')}</td>
      <td>${esc(ticket || '--')}</td>
      <td class="right">${fmtNum(totalTok)}</td>
      <td class="${costClass}">${costDisplay}</td>
    </tr>`;
  };
  return `
  <div class="table-scroll">
  <table class="sortable-table" id="sessionsTable">
    <thead><tr>${cols.map(c =>
      `<th data-col="${c.key}"${c.right ? ' class="right"' : ''}>${c.label}${arrow(c.key)}</th>`
    ).join('')}</tr></thead>
    <tbody>${messages.map(rowFn).join('')}</tbody>
  </table>
  </div>
  ${hasMore ? `<button class="show-more-btn" data-table="sessionsTable">Show more (${remaining} remaining)</button>` : ''}`;
}

/* ===== View Renderer ===== */
function renderStatsView(content) {
  const { summary, sessions, cwds, cost, models, activityChart, branches, tickets } = statsData;
  content.innerHTML = `
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
        <h2>Branches</h2>
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
        <h2>Tickets</h2>
        ${renderBarChart((tickets || []).slice(0, DEFAULT_CHART_ROWS),
          t => t.value,
          t => t.cost_cents,
          (t, i) => paletteColor(i),
          'No ticket data for this period',
          fmtCostTokens
        )}
      </div>
    </div>
    ${(toolsData.length || mcpData.length) ? `<div class="grid-2 section-mb">
      <div class="panel">
        <h2>Tools</h2>
        ${renderBarChart(toolsData.slice(0, DEFAULT_CHART_ROWS),
          t => t.tool_name,
          t => t.call_count,
          (t, i) => toolColor(t.tool_name),
          'No tool data for this period',
          fmtToolCalls
        )}
      </div>
      <div class="panel">
        <h2>MCP Servers</h2>
        ${renderBarChart(mcpData.slice(0, DEFAULT_CHART_ROWS),
          m => m.mcp_server,
          m => m.call_count,
          (m, i) => paletteColor(i),
          'No MCP data for this period',
          fmtToolCalls
        )}
      </div>
    </div>` : ''}
    <div class="panel section-mb">
      <h2>Messages</h2>
      <input type="text" id="sessionsSearch" class="search-input" placeholder="Search messages..." value="${esc(sessionsSearchTerm)}" style="margin-bottom:12px">
      <div id="sessionsContainer">${renderMessagesSection(sessions)}</div>
    </div>
  `;
}

/* ===== Main Render ===== */
