const $ = (s, el) => (el || document).querySelector(s);
const $$ = (s, el) => [...(el || document).querySelectorAll(s)];
function esc(s) { if (s == null) return ''; const d = document.createElement('div'); d.textContent = String(s); return d.innerHTML; }

function getCurrentView() {
  const path = window.location.pathname;
  if (path.includes('/insights')) return 'insights';
  if (path.includes('/plans')) return 'plans';
  if (path.includes('/prompts')) return 'prompts';
  if (path.includes('/setup')) return 'setup';
  return 'stats';
}

let currentPeriod = 'today';
let currentView = getCurrentView();
const DEFAULT_TABLE_ROWS = 15;
const DEFAULT_CHART_ROWS = 15;

// Session table state (server-side paginated)
let lastSessionData = [];
let sessionSortCol = 'last_seen';
let sessionSortAsc = false;
let sessionShowCount = DEFAULT_TABLE_ROWS;
let sessionTotalCount = 0;

// Config table state
let lastConfigData = [];
let configSortCol = 'est_tokens';
let configSortAsc = false;
let configShowCount = DEFAULT_TABLE_ROWS;

// Project config table state
let lastProjectConfigData = [];
let projectConfigSortCol = 'tokens';
let projectConfigSortAsc = false;
let projectConfigShowCount = DEFAULT_TABLE_ROWS;

// History table state
let lastHistoryData = [];
let historySortCol = 'timestamp';
let historySortAsc = false;
let historyShowCount = DEFAULT_TABLE_ROWS;

// Plans table state
let lastPlansData = [];
let plansSortCol = 'modified';
let plansSortAsc = false;
let plansShowCount = DEFAULT_TABLE_ROWS;

// Plugins table state
let lastPluginsData = [];
let pluginsSortCol = 'name';
let pluginsSortAsc = true;
let pluginsShowCount = DEFAULT_TABLE_ROWS;

// Permissions table state
let lastPermissionsData = [];
let permissionsSortCol = 'scope';
let permissionsSortAsc = true;
let permissionsShowCount = DEFAULT_TABLE_ROWS;

// Search state
let sessionsSearchTerm = '';
let configSearchTerm = '';
let projectConfigSearchTerm = '';
let pluginsSearchTerm = '';
let permissionsSearchTerm = '';
let plansSearchTerm = '';
let promptsSearchTerm = '';

// Provider filter state
let currentProvider = '';
let providersData = [];
let registeredProviders = [];

// Cached data
let dataLoaded = false;
let statsData = null;
let setupData = null;
let plansData = null;
let promptsData = null;
let activityData = null;
let activeSessionsData = null;

// Cached render intermediates for stats view
let cachedSortedModels = [];
let cachedActivityChartTitle = '';
let cachedMergedConfigFiles = [];

