const $ = (s, el) => (el || document).querySelector(s);
const $$ = (s, el) => [...(el || document).querySelectorAll(s)];
function esc(s) { if (s == null) return ''; const d = document.createElement('div'); d.textContent = String(s); return d.innerHTML; }

let currentPeriod = localStorage.getItem('budi_period') || 'today';
const VALID_PAGES = ['overview', 'insights', 'sessions', 'settings'];
let currentPage = (function() {
  // Parse from URL path: /dashboard/insights -> insights
  const path = location.pathname.replace(/^\/dashboard\/?/, '');
  if (VALID_PAGES.includes(path)) return path;
  // Fallback to hash for backwards compat
  const hash = location.hash.replace('#', '');
  if (VALID_PAGES.includes(hash)) return hash;
  return 'overview';
})();
const DEFAULT_CHART_ROWS = 15;

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
  return m ? decodeURIComponent(m[1]) : null;
})();

