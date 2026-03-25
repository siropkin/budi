function dateRange(period) {
  const now = new Date();
  const y = now.getFullYear(), m = now.getMonth(), d = now.getDate();
  const toISO = dt => dt.toISOString();
  switch (period) {
    case 'today': return { since: toISO(new Date(y, m, d)) };
    case 'week': {
      const dow = now.getDay();
      const mondayOffset = dow === 0 ? 6 : dow - 1;
      return { since: toISO(new Date(y, m, d - mondayOffset)) };
    }
    case 'month': return { since: toISO(new Date(y, m, 1)) };
    case 'all': return {};
  }
}

function granularityForPeriod(period) {
  switch (period) {
    case 'today': return 'hour';
    case 'week': return 'day';
    case 'month': return 'day';
    case 'all': return 'month';
  }
}

function qs(params) {
  const p = new URLSearchParams();
  for (const [k,v] of Object.entries(params)) if (v != null) p.set(k, v);
  const s = p.toString();
  return s ? '?' + s : '';
}

function fmtNum(n) {
  if (n >= 1_000_000_000) return (n / 1_000_000_000).toFixed(1) + 'B';
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + 'M';
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K';
  return String(n);
}
function fmtCost(n) {
  if (n >= 1000) return '$' + (n / 1000).toFixed(1) + 'K';
  if (n >= 100) return '$' + n.toFixed(0);
  if (n >= 1) return '$' + n.toFixed(2);
  if (n > 0) return '$' + n.toFixed(2);
  return '$0.00';
}
function fmtCostTokens(_, item) {
  const cost = (item.cost_cents || 0) / 100;
  return fmtCost(cost);
}
function fmtDate(iso) {
  if (!iso) return '--';
  const d = new Date(iso), now = new Date();
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const target = new Date(d.getFullYear(), d.getMonth(), d.getDate());
  const diff = Math.floor((today - target) / 86400000);
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (diff === 0) return `Today ${time}`;
  if (diff === 1) return `Yesterday ${time}`;
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + time;
}
function repoName(id) { if (!id) return '--'; return id.split('/').pop() || id; }

function formatModelName(raw) {
  if (!raw || raw === 'unknown') return 'Unknown';
  if (raw === '<synthetic>') return 'Unknown';
  // Handle comma-separated multi-model strings (from Cursor sessions using multiple models)
  if (raw.includes(',')) return raw.split(',').map(m => formatModelName(m.trim())).join(', ');
  const n = raw.toLowerCase().trim();

  // Parse suffixes: -high, -max, -thinking, -codex, -preview
  function parseSuffixes(s) {
    let thinking = false, effort = '', codex = false, preview = false;
    if (s.includes('-thinking')) { thinking = true; s = s.replace('-thinking', ''); }
    if (s.includes('-max')) { effort = 'Max'; s = s.replace('-max', ''); }
    else if (s.includes('-high')) { effort = 'High'; s = s.replace('-high', ''); }
    if (s.includes('-codex')) { codex = true; s = s.replace('-codex', ''); }
    if (s.includes('-preview')) { preview = true; s = s.replace('-preview', ''); }
    let parts = [];
    if (codex) parts.push('Codex');
    if (thinking) parts.push('Thinking');
    if (effort) parts.push(effort + ' Effort');
    if (preview) parts.push('Preview');
    return { base: s, suffix: parts.length ? ' (' + parts.join(', ') + ')' : '' };
  }

  // Claude models — from Claude Code ("claude-opus-4-6") and Cursor ("claude-4.5-opus-high-thinking")
  if (n.includes('claude') || n.includes('opus') || n.includes('sonnet') || n.includes('haiku')) {
    const { base, suffix } = parseSuffixes(n);
    // Extract version: "4.6", "4-6", "4.5", "4" etc.
    const verMatch = base.match(/(\d+)[\._-]?(\d+)?/);
    const ver = verMatch ? verMatch[1] + (verMatch[2] ? '.' + verMatch[2] : '') : '';
    let family = '';
    if (base.includes('opus')) family = 'Opus';
    else if (base.includes('sonnet')) family = 'Sonnet';
    else if (base.includes('haiku')) family = 'Haiku';
    return ('Claude ' + (ver ? ver + ' ' : '') + family + suffix).trim();
  }

  // GPT models — "gpt-5", "gpt-5.2-codex-high"
  if (/^gpt[._-]?\d/.test(n)) {
    const { base, suffix } = parseSuffixes(n);
    const verMatch = base.match(/(\d+[\.\d]*)/);
    const ver = verMatch ? verMatch[1] : '';
    return 'GPT-' + ver + suffix;
  }

  // o-series (o1, o3-mini, etc.)
  if (/^o\d/.test(n)) return raw;

  // Gemini — "gemini-3-pro", "gemini-3-pro-preview"
  if (n.startsWith('gemini')) {
    const { base, suffix } = parseSuffixes(n);
    const rest = base.replace(/^gemini[._-]?/, '').replace(/-/g, ' ').trim();
    const parts = rest.split(' ').map(w => w.charAt(0).toUpperCase() + w.slice(1));
    return 'Gemini ' + parts.join(' ') + suffix;
  }

  // Cursor internal
  if (n === 'default') return 'Auto';
  if (n.startsWith('composer-')) return 'Composer ' + raw.slice(9);

  return raw;
}



function fmtDurationMs(ms) {
  if (ms == null) return '';
  if (ms >= 120000) return Math.floor(ms / 60000) + 'm ' + Math.floor((ms % 60000) / 1000) + 's';
  if (ms >= 1000) return (ms / 1000).toFixed(1) + 's';
  return ms + 'ms';
}
function fmtToolCalls(_, item) {
  const count = item.call_count || 0;
  const avg = item.avg_duration_ms;
  if (avg != null && avg > 0) return fmtNum(count) + ' calls \u00b7 ' + fmtDurationMs(avg) + ' avg';
  return fmtNum(count) + ' calls';
}

const TOOL_COLORS = {
  Read: '#58a6ff', Edit: '#3fb950', Write: '#d2a8ff', Bash: '#f0883e',
  Grep: '#f778ba', Glob: '#79c0ff', Agent: '#ffd33d', default: '#8b949e'
};
function toolColor(name) { return TOOL_COLORS[name] || TOOL_COLORS.default; }

const CHART_PALETTE = ['#58a6ff', '#3fb950', '#d2a8ff', '#f0883e', '#f778ba', '#ffd33d', '#79c0ff', '#a5d6ff', '#7ee787', '#ff9bce'];
function paletteColor(i) { return CHART_PALETTE[i % CHART_PALETTE.length]; }

