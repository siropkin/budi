/* ===== Insights Page ===== */

function fmtMcpName(raw) {
  if (!raw) return '--';
  // "mcp__context7__query-docs" -> "context7 / query-docs"
  const parts = raw.replace(/^mcp__/, '').split('__');
  if (parts.length >= 2) return parts[0] + ' / ' + parts.slice(1).join('/');
  return raw;
}

function renderInsightsView(content) {
  const d = insightsData;
  if (!d) { content.innerHTML = '<div class="empty">No insights data</div>'; return; }

  const ce = d.cacheEff;
  const cacheSavings = ce ? (ce.cache_savings_cents / 100) : 0;
  const cacheHitPct = ce ? (ce.cache_hit_rate * 100).toFixed(1) : '0.0';

  // Speed mode
  const speedItems = (d.speedTags || []).filter(t => t.value !== '(untagged)');

  // Cost confidence
  const confLabels = { otel_exact: 'OTEL Exact', exact: 'Exact', exact_cost: 'Exact Cost', estimated: 'Estimated' };
  const confColors = { otel_exact: '#3fb950', exact: '#58a6ff', exact_cost: '#79c0ff', estimated: '#f0883e' };
  const confItems = (d.costConf || []).map(c => ({
    ...c,
    label: confLabels[c.confidence] || c.confidence,
    color: confColors[c.confidence] || '#8b949e',
  }));

  // Session cost curve — split into cost and count
  const curveData = d.sessionCurve || [];
  const costItems = curveData.map(b => ({
    label: b.bucket,
    cost_cents: b.avg_cost_per_message_cents,
    session_count: b.session_count,
    total_cost_cents: b.total_cost_cents,
  }));
  const countItems = curveData.map(b => ({
    label: b.bucket,
    count: b.session_count,
    cost_cents: b.total_cost_cents,
  }));

  // Tools & MCP
  const toolsData = d.tools || [];
  const mcpData = d.mcp || [];

  content.innerHTML = `
    <div class="panel section-mb">
      <h2>Cost Confidence</h2>
      ${renderBarChart(confItems,
        c => c.label,
        c => c.cost_cents,
        (c, i) => confColors[c.confidence] || paletteColor(i),
        'No data',
        fmtCostTokens
      )}
    </div>

    <div class="cards section-mb">
      <div class="card">
        <div class="label">Cache Savings</div>
        <div class="value cost-value">${fmtCost(cacheSavings)}</div>
        <div class="sub">${cacheHitPct}% cache hit rate</div>
      </div>
      <div class="card">
        <div class="label">Cache Read Tokens</div>
        <div class="value">${fmtNum(ce ? ce.total_cache_read_tokens : 0)}</div>
        <div class="sub">${fmtNum(ce ? ce.total_cache_creation_tokens : 0)} cache writes</div>
      </div>
    </div>

    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Avg Cost per Message by Session Length</h2>
        ${renderBarChart(costItems,
          d => d.label + ' msgs',
          d => d.cost_cents,
          (d, i) => paletteColor(i),
          'No session data for this period',
          (_, item) => {
            const avgCost = item.cost_cents / 100;
            return fmtCost(avgCost) + '/msg';
          }
        )}
      </div>
      <div class="panel">
        <h2>Sessions by Length</h2>
        ${renderBarChart(countItems,
          d => d.label + ' msgs',
          d => d.count,
          (d, i) => paletteColor(i),
          'No session data for this period',
          (_, item) => fmtNum(item.count)
        )}
      </div>
    </div>

    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Speed Mode</h2>
        ${renderBarChart(speedItems,
          t => t.value === 'fast' ? 'Fast (6x cost)' : t.value === 'normal' ? 'Normal' : t.value,
          t => t.cost_cents,
          (t, i) => paletteColor(i),
          'No speed data for this period',
          fmtCostTokens
        )}
      </div>
      <div class="panel">
        <h2>Subagent vs Main</h2>
        ${renderBarChart(d.subagent || [],
          s => s.category === 'main' ? 'Main conversation' : 'Subagents',
          s => s.cost_cents,
          (s, i) => paletteColor(i),
          'No subagent data for this period',
          fmtCostTokens
        )}
      </div>
    </div>

    <div class="grid-2 section-mb">
      <div class="panel">
        <h2>Tools</h2>
        ${renderBarChart(toolsData,
          t => t.tool_name,
          t => t.call_count,
          (t, i) => toolColor(t.tool_name),
          'No tool data for this period',
          fmtToolCalls
        )}
      </div>
      <div class="panel">
        <h2>MCP Servers</h2>
        ${renderBarChart(mcpData,
          m => fmtMcpName(m.tool_name || m.mcp_server),
          m => m.call_count,
          (m, i) => paletteColor(i),
          'No MCP data for this period',
          fmtToolCalls
        )}
      </div>
    </div>
  `;
}
