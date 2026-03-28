/* ===== View Renderer ===== */
function renderStatsView(content) {
  const { summary, cwds, cost, models, activityChart, branches, tickets, activityTags } = statsData;
  const activityItems = activityTags || [];
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
          m => m.provider_display + ' / ' + m.model,
          m => m.cost_cents,
          (m, i) => paletteColor(i),
          'No model data for this period',
          fmtCostTokens
        )}
      </div>
    </div>
    <div class="grid-2 section-mb">
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
            const branch = b.git_branch ? b.git_branch.replace(/^refs\/heads\//, '') : '';
            if (branch === '(untagged)' || !branch) return '(untagged)';
            const repo = repoName(b.repo_id);
            return repo + ' / ' + branch;
          },
          b => b.cost_cents,
          (b, i) => paletteColor(i),
          'No branch data for this period',
          fmtCostTokens
        )}
      </div>
    </div>
    <div class="grid-2 section-mb">
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
      <div class="panel">
        <h2>Activity Types</h2>
        ${renderBarChart(activityItems,
          t => t.value,
          t => t.cost_cents,
          (t, i) => paletteColor(i),
          'No activity data for this period',
          fmtCostTokens
        )}
      </div>
    </div>
  `;
}

/* ===== Main Render ===== */
