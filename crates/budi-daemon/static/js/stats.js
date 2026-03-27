function agentBarData() {
  return registeredProviders.map(rp => {
    const stats = providersData.find(p => p.provider === rp.name);
    const cost_cents = stats ? (stats.total_cost_cents != null ? stats.total_cost_cents : stats.estimated_cost * 100) : 0;
    return {
      provider: rp.name,
      display_name: rp.display_name,
      input_tokens: stats ? stats.input_tokens : 0,
      output_tokens: stats ? stats.output_tokens : 0,
      cost_cents,
    };
  }).filter(p => p.cost_cents > 0 || p.input_tokens > 0 || p.output_tokens > 0);
}

/* ===== Render: Summary Cards ===== */
function renderCards(s, cost) {
  const totalTokens = s.total_input_tokens + s.total_output_tokens + s.total_cache_creation_tokens + s.total_cache_read_tokens;
  const totalIn = s.total_input_tokens;
  return `
  <div class="cards">
    <div class="card">
      <div class="label">Est. Cost</div>
      <div class="value cost-value" title="Includes both exact (API/OTEL) and estimated costs">${fmtCost(cost.total_cost)}</div>
      <div class="sub">${fmtCost(cost.input_cost + cost.cache_write_cost + cost.cache_read_cost)} input+cache / ${fmtCost(cost.output_cost)} output</div>
    </div>
    <div class="card">
      <div class="label">Tokens</div>
      <div class="value">${fmtNum(totalTokens)}</div>
      <div class="sub">${fmtNum(totalIn)} input / ${fmtNum(s.total_output_tokens)} output</div>
    </div>
    <div class="card">
      <div class="label">Messages</div>
      <div class="value">${fmtNum(s.total_messages)}</div>
      <div class="sub">${fmtNum(s.total_user_messages)} input / ${fmtNum(s.total_assistant_messages)} output</div>
    </div>
  </div>`;
}

/* ===== Render: Bar Chart ===== */
function barTooltip(item, labelFn, valueFn) {
  const label = labelFn(item, true);
  const cost = (item.cost_cents || 0) / 100;
  const inp = item.input_tokens || 0;
  const outp = item.output_tokens || 0;
  if (inp || outp) return `${label}: ${fmtCost(cost)} — ${fmtNum(inp)} input, ${fmtNum(outp)} output`;
  if (cost > 0) return `${label}: ${fmtCost(cost)}`;
  if (valueFn) return `${label}: ${fmtNum(valueFn(item))} calls`;
  return label;
}

function renderBarChart(items, labelFn, valueFn, colorFn, emptyMsg, formatFn) {
  if (!items.length) return `<div class="empty">${esc(emptyMsg)}</div>`;
  const fmt = formatFn || ((v) => fmtNum(v));
  const max = Math.max(...items.map(valueFn));
  return `<div class="bar-chart">${items.map((item, i) => `
    <div class="bar-row">
      <div class="bar-tooltip">${esc(barTooltip(item, labelFn, valueFn))}</div>
      <div class="bar-label">${esc(labelFn(item, false))}</div>
      <div class="bar-track">
        <div class="bar-fill" style="width:${max > 0 ? (valueFn(item)/max*100) : 0}%;background:${colorFn(item, i)}"></div>
      </div>
      <div class="bar-count">${fmt(valueFn(item), item)}</div>
    </div>`).join('')}
  </div>`;
}

/* ===== Render: Activity Chart (period-aware) ===== */
function localDateStr(d) {
  // Format as YYYY-MM-DD in local timezone (not UTC)
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}

// Activity bucket filling: the server's /analytics/activity endpoint applies
// `tz_offset` (minutes east of UTC) so that returned labels are in the user's
// local time. Hourly labels come back as "HH:00" (e.g. "09:00", "14:00") and
// daily labels as "YYYY-MM-DD". This client-side function must generate
// matching label formats so the dataMap lookup aligns with server data.
function fillActivityBuckets(chartData) {
  const dataMap = {};
  if (chartData) for (const b of chartData) dataMap[b.label] = b;
  const empty = { message_count: 0, input_tokens: 0, output_tokens: 0, cost_cents: 0, tool_call_count: 0 };

  const gran = granularityForPeriod(currentPeriod);
  const now = new Date();
  const y = now.getFullYear(), mo = now.getMonth(), day = now.getDate();
  const buckets = [];

  if (gran === 'hour') {
    for (let h = 0; h < 24; h++) {
      const label = String(h).padStart(2, '0') + ':00';
      buckets.push(dataMap[label] || { label, ...empty });
    }
  } else if (gran === 'day') {
    // Compute start in local time (same logic as dateRange)
    let start, end;
    if (currentPeriod === 'week') {
      const dow = now.getDay();
      const mondayOffset = dow === 0 ? 6 : dow - 1;
      start = new Date(y, mo, day - mondayOffset);
      end = new Date(start);
      end.setDate(end.getDate() + 6); // Monday to Sunday
    } else if (currentPeriod === 'month') {
      start = new Date(y, mo, 1);
      end = new Date(y, mo + 1, 0); // Last day of month
    } else {
      start = new Date(y, mo, day);
      end = now;
    }
    for (let d = new Date(start); d <= end; d.setDate(d.getDate() + 1)) {
      const label = localDateStr(d);
      buckets.push(dataMap[label] || { label, ...empty });
    }
  } else if (gran === 'month') {
    const labels = Object.keys(dataMap).sort();
    if (labels.length) {
      const [sy, sm] = labels[0].split('-').map(Number);
      let cy = sy, cm = sm;
      while (cy < now.getFullYear() || (cy === now.getFullYear() && cm <= now.getMonth() + 1)) {
        const label = String(cy) + '-' + String(cm).padStart(2, '0');
        buckets.push(dataMap[label] || { label, ...empty });
        cm++;
        if (cm > 12) { cm = 1; cy++; }
      }
    }
  }
  return buckets.length ? buckets : (chartData || []);
}

function renderActivityChart(chartData) {
  const filledData = fillActivityBuckets(chartData);
  if (!filledData.length) return `<div class="empty">No activity data yet</div>`;

  const maxTotal = Math.max(...filledData.map(d => (d.input_tokens || 0) + (d.output_tokens || 0)), 1);

  let bars = '';
  let labels = '';
  for (const bucket of filledData) {
    const inp = bucket.input_tokens || 0;
    const outp = bucket.output_tokens || 0;
    const inH = (inp / maxTotal) * 100;
    const outH = (outp / maxTotal) * 100;
    const displayLabel = bucket.label || '';
    const shortLabel = displayLabel.length > 6 ? displayLabel.slice(-5) : displayLabel;
    bars += `<div class="day-bar" style="height:100%">
      <div class="daily-chart-tooltip">${esc(displayLabel)}: ${fmtCost((bucket.cost_cents || 0) / 100)} — ${fmtNum(inp)} input, ${fmtNum(outp)} output</div>
      <div class="bar-msg" style="height:${inH}%"></div>
      <div class="bar-tool" style="height:${outH}%"></div>
    </div>`;
    labels += `<div class="day-label">${esc(shortLabel)}</div>`;
  }

  return `
    <div class="daily-chart">${bars}</div>
    <div class="daily-chart-labels">${labels}</div>
    <div style="display:flex;gap:16px;margin-top:8px;font-size:0.75rem;color:var(--text-muted)">
      <span><span style="display:inline-block;width:10px;height:10px;background:var(--accent);border-radius:2px;vertical-align:middle"></span> Input tokens</span>
      <span><span style="display:inline-block;width:10px;height:10px;background:var(--accent4);border-radius:2px;vertical-align:middle"></span> Output tokens</span>
    </div>`;
}


