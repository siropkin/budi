const $ = (s, el) => (el || document).querySelector(s);
const $$ = (s, el) => [...(el || document).querySelectorAll(s)];
function esc(s) { if (s == null) return ''; const d = document.createElement('div'); d.textContent = String(s); return d.innerHTML; }

let currentPeriod = 'today';
const DEFAULT_TABLE_ROWS = 15;
const DEFAULT_CHART_ROWS = 15;

// Session table state (server-side paginated)
let lastSessionData = [];
let sessionSortCol = 'last_seen';
let sessionSortAsc = false;
let sessionShowCount = DEFAULT_TABLE_ROWS;
let sessionTotalCount = 0;

// Search state
let sessionsSearchTerm = '';

// Provider filter state
let currentProvider = '';
let providersData = [];
let registeredProviders = [];

// Cached data
let dataLoaded = false;
let statsData = null;
let activeSessionsData = null;

// Cached render intermediates for stats view
let cachedSortedModels = [];
let cachedActivityChartTitle = '';

