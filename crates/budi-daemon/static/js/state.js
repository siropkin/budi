const $ = (s, el) => (el || document).querySelector(s);
const $$ = (s, el) => [...(el || document).querySelectorAll(s)];
function esc(s) { if (s == null) return ''; return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;'); }

let currentPeriod = localStorage.getItem('budi_period') || 'today';
const VALID_PAGES = ['overview', 'insights', 'sessions', 'settings'];
let currentPage = (function() {
  // Parse from URL path: /dashboard/insights -> insights, /dashboard/sessions/:id -> sessions
  const path = location.pathname.replace(/^\/dashboard\/?/, '');
  if (VALID_PAGES.includes(path)) return path;
  const base = path.split('/')[0];
  if (VALID_PAGES.includes(base)) return base;
  // Fallback to hash for backwards compat
  const hash = location.hash.replace('#', '');
  if (VALID_PAGES.includes(hash)) return hash;
  return 'overview';
})();
const DEFAULT_CHART_ROWS = 15;
const SESSIONS_PAGE_LIMIT = 50;

// Provider data
let providersData = [];
let registeredProviders = [];

// Cached data
let dataLoaded = false;
let statsData = null;
// Cached render intermediates for stats view
let cachedSortedModels = [];
let cachedActivityChartTitle = '';

// Insights page data
let insightsData = null;

// Sessions page data
let sessionsPageData = null;
let sessionsPageTotalCount = 0;
// Parse session ID from URL: /dashboard/sessions/:id
let selectedSessionId = (function() {
  const m = location.pathname.match(/^\/dashboard\/sessions\/(.+)$/);
  try { return m ? decodeURIComponent(m[1]) : null; } catch (_) { return null; }
})();

