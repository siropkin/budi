import * as vscode from "vscode";
import {
  SessionHealthData,
  SessionListEntry,
  fetchSessionHealth,
  fetchRecentSessions,
  splitSessionsByDay,
} from "./budiClient";

export type SessionSelectCallback = (sessionId: string) => void;

export class HealthPanelProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "budi.healthPanel";

  private view?: vscode.WebviewView;
  private latestHealth?: SessionHealthData;
  private latestSessions?: SessionListEntry[];
  private daemonUrl: string;
  private sessionId?: string;
  private onSelectSession?: SessionSelectCallback;

  constructor(
    private readonly extensionUri: vscode.Uri,
    daemonUrl: string,
  ) {
    this.daemonUrl = daemonUrl;
  }

  setOnSelectSession(cb: SessionSelectCallback): void {
    this.onSelectSession = cb;
  }

  resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;

    webviewView.webview.options = {
      enableScripts: true,
    };

    webviewView.webview.onDidReceiveMessage((msg) => {
      if (msg.command === "openDashboard") {
        const url = msg.url || `${this.daemonUrl}/dashboard`;
        vscode.env.openExternal(vscode.Uri.parse(url));
      } else if (msg.command === "selectSession" && msg.sessionId) {
        this.onSelectSession?.(msg.sessionId);
      }
    });

    webviewView.onDidChangeVisibility(() => {
      if (webviewView.visible) {
        this.refresh().catch(() => {});
      }
    });

    webviewView.onDidDispose(() => {
      this.view = undefined;
    });

    this.renderHtml();
    this.refresh().catch(() => {});
  }

  updateContext(daemonUrl: string, sessionId?: string): void {
    this.daemonUrl = daemonUrl;
    this.sessionId = sessionId;
  }

  async refresh(): Promise<void> {
    const [health, sessions] = await Promise.all([
      fetchSessionHealth(this.daemonUrl, this.sessionId).catch(() => null),
      fetchRecentSessions(this.daemonUrl).catch(() => null),
    ]);

    this.latestHealth = health ?? undefined;
    this.latestSessions = sessions ?? undefined;
    this.renderHtml();
  }

  private renderHtml(): void {
    if (!this.view) return;

    this.view.webview.html = buildHtml(
      this.latestHealth,
      this.latestSessions,
      this.sessionId,
      this.daemonUrl,
    );
  }
}

function buildHtml(
  health: SessionHealthData | undefined,
  sessions: SessionListEntry[] | undefined,
  activeSessionId: string | undefined,
  dashboardUrl: string,
): string {
  if (!sessions && !health) {
    return `<!DOCTYPE html>
<html>
<head>${styles()}</head>
<body>
  <div class="container">
    <div class="card">
      <div class="card-title">Getting Started</div>
      <p class="muted">budi daemon is not running.</p>
      <ol class="setup-steps">
        <li>Install budi and run <code>budi init</code></li>
        <li>In Cursor Settings &rarr; Models, set<br/><strong>Override OpenAI Base URL</strong> to:<br/><code>http://localhost:9878</code></li>
        <li>Restart Cursor</li>
      </ol>
      <p class="hint">All AI requests will be tracked automatically through the local proxy.</p>
    </div>
  </div>
</body>
</html>`;
  }

  const fmtCents = (cents: number) => {
    const d = cents / 100;
    if (d >= 1000) return `$${(d / 1000).toFixed(1)}K`;
    if (d >= 100) return `$${Math.round(d)}`;
    if (d > 0) return `$${d.toFixed(2)}`;
    return "$0.00";
  };

  const icon = (state: string) => {
    switch (state) {
      case "red":
        return "\u{1F534}";
      case "yellow":
        return "\u{1F7E1}";
      default:
        return "\u{1F7E2}";
    }
  };

  const allSessions = sessions ?? [];
  const titleMap = new Map<string, string>();
  for (const s of allSessions) {
    if (s.title) titleMap.set(s.session_id, s.title);
  }
  const sessionName = (id: string) => titleMap.get(id) || id;
  const escapeHtml = (s: string) =>
    s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#039;");
  const jsStringLiteral = (s: string) =>
    JSON.stringify(s).replace(/</g, "\\u003c").replace(/>/g, "\\u003e");

  // Session details
  let detailSection = "";
  if (activeSessionId) {
    const sessionUrl = `${dashboardUrl}/dashboard/sessions/${encodeURIComponent(activeSessionId)}`;
    const title = escapeHtml(sessionName(activeSessionId));

    let healthHtml = "";
    if (health) {
      const vitals = health.vitals;
      const vitalRows = [
        vitals.context_drag
          ? vitalRow(icon(vitals.context_drag.state), "Context Growth", vitals.context_drag.label)
          : "",
        vitals.cache_efficiency
          ? vitalRow(
              icon(vitals.cache_efficiency.state),
              "Cache Reuse",
              vitals.cache_efficiency.label,
            )
          : "",
        vitals.cost_acceleration
          ? vitalRow(
              icon(vitals.cost_acceleration.state),
              "Cost Acceleration",
              vitals.cost_acceleration.label,
            )
          : "",
        vitals.thrashing
          ? vitalRow(icon(vitals.thrashing.state), "Retry Loops", vitals.thrashing.label)
          : "",
      ]
        .filter(Boolean)
        .join("\n");
      const tipHtml = health.tip ? `<div class="tip">${escapeHtml(health.tip)}</div>` : "";

      healthHtml = `
        <div class="health-summary">
          <span class="health-icon-lg">${icon(health.state)}</span>
          <div class="health-meta">
            <span class="health-cost">${fmtCents(health.total_cost_cents)}</span>
            <span class="health-msgs">${health.message_count} messages</span>
          </div>
        </div>
        ${vitalRows ? `<div class="vitals">${vitalRows}</div>` : ""}
        ${tipHtml}`;
    }

    detailSection = `
    <div class="card active-card">
      <div class="card-title">Session Details</div>
      <div class="session-title">${title}</div>
      ${healthHtml}
      <div class="card-links">
        <a href="#" onclick='openDashboard(${jsStringLiteral(sessionUrl)})'>Session Detail \u2197</a>
      </div>
    </div>`;
  }

  // Ensure the active session is in the list (it may not be synced yet)
  let enrichedSessions = allSessions;
  if (activeSessionId && !allSessions.some((s) => s.session_id === activeSessionId)) {
    const stub: SessionListEntry = {
      session_id: activeSessionId,
      started_at: new Date().toISOString(),
      message_count: health?.message_count ?? 0,
      cost_cents: health?.total_cost_cents ?? 0,
      provider: "cursor",
      health_state: health?.state,
      title: titleMap.get(activeSessionId),
    };
    enrichedSessions = [stub, ...allSessions];
  }

  // Sessions grouped by day
  const { today, yesterday } =
    enrichedSessions.length > 0
      ? splitSessionsByDay(enrichedSessions)
      : { today: [] as SessionListEntry[], yesterday: [] as SessionListEntry[] };

  const renderSessionList = (list: SessionListEntry[]) =>
    list
      .map((s) => {
        const isActive = s.session_id === activeSessionId;
        const title = escapeHtml(sessionName(s.session_id));
        return `
        <div class="session-row${isActive ? " session-active" : ""}" onclick='selectSession(${jsStringLiteral(s.session_id)})'>
          <span class="session-health">${icon(s.health_state || "green")}</span>
          <div class="session-info">
            <span class="session-name">${title}</span>
            <span class="session-meta">${fmtCents(s.cost_cents)} · ${s.message_count} msgs</span>
          </div>
        </div>`;
      })
      .join("");

  let sessionsHtml = "";
  if (today.length > 0 || yesterday.length > 0) {
    const todayBlock =
      today.length > 0
        ? `<div class="day-group"><div class="day-label">Today</div>${renderSessionList(today)}</div>`
        : "";
    const yesterdayBlock =
      yesterday.length > 0
        ? `<div class="day-group"><div class="day-label">Yesterday</div>${renderSessionList(yesterday)}</div>`
        : "";

    sessionsHtml = `
    <div class="card">
      <div class="card-title">Sessions</div>
      ${todayBlock}
      ${yesterdayBlock}
    </div>`;
  }

  const linksHtml = `
    <div class="links">
      <a href="#" onclick='openDashboard(${jsStringLiteral(`${dashboardUrl}/dashboard`)})'>Dashboard \u2197</a>
      <a href="#" onclick='openDashboard(${jsStringLiteral(`${dashboardUrl}/dashboard/sessions`)})'>All Sessions \u2197</a>
    </div>`;

  return `<!DOCTYPE html>
<html>
<head>${styles()}</head>
<body>
  <div class="container">
    ${detailSection}
    ${sessionsHtml}
    ${linksHtml}
  </div>
  <script>
    const vscode = acquireVsCodeApi();
    function openDashboard(url) {
      vscode.postMessage({ command: 'openDashboard', url: url });
    }
    function selectSession(id) {
      vscode.postMessage({ command: 'selectSession', sessionId: id });
    }
  </script>
</body>
</html>`;
}

function vitalRow(iconStr: string, label: string, value: string): string {
  return `<div class="vital-row"><span class="vital-icon">${iconStr}</span><span class="vital-label">${label}</span><span class="vital-value">${value}</span></div>`;
}

function styles(): string {
  return `<style>
    body {
      font-family: var(--vscode-font-family);
      font-size: var(--vscode-font-size);
      color: var(--vscode-foreground);
      background: var(--vscode-sideBar-background);
      margin: 0;
      padding: 0;
    }
    .container { padding: 12px; }
    .card {
      background: var(--vscode-editor-background);
      border: 1px solid var(--vscode-widget-border, transparent);
      border-radius: 6px;
      padding: 10px 12px;
      margin-bottom: 10px;
    }
    .active-card {
      border-color: var(--vscode-focusBorder);
    }
    .card-title {
      font-weight: 600;
      font-size: 11px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
      color: var(--vscode-descriptionForeground);
      margin-bottom: 6px;
    }
    .session-title {
      font-size: 13px;
      font-weight: 500;
      margin-bottom: 8px;
      line-height: 1.3;
    }
    .health-summary {
      display: flex;
      align-items: center;
      gap: 10px;
      padding: 4px 0 8px;
    }
    .health-icon-lg { font-size: 20px; }
    .health-meta {
      display: flex;
      flex-direction: column;
      gap: 2px;
    }
    .health-cost { font-weight: 600; font-variant-numeric: tabular-nums; }
    .health-msgs { font-size: 12px; color: var(--vscode-descriptionForeground); }
    .vitals {
      border-top: 1px solid var(--vscode-widget-border, rgba(128,128,128,0.2));
      padding-top: 8px;
      margin-top: 4px;
    }
    .vital-row {
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 3px 0;
      font-size: 12px;
    }
    .vital-icon { font-size: 10px; width: 14px; text-align: center; }
    .vital-label { flex: 1; color: var(--vscode-descriptionForeground); }
    .vital-value { font-variant-numeric: tabular-nums; }
    .tip {
      margin-top: 8px;
      padding: 6px 8px;
      background: var(--vscode-textBlockQuote-background, rgba(128,128,128,0.1));
      border-radius: 4px;
      font-size: 12px;
      line-height: 1.4;
    }
    .card-links {
      margin-top: 8px;
      padding-top: 6px;
      border-top: 1px solid var(--vscode-widget-border, rgba(128,128,128,0.2));
    }
    .card-links a {
      color: var(--vscode-textLink-foreground);
      text-decoration: none;
      font-size: 12px;
    }
    .card-links a:hover { text-decoration: underline; }
    .day-group { margin-bottom: 6px; }
    .day-group:last-child { margin-bottom: 0; }
    .day-label {
      font-size: 11px;
      font-weight: 600;
      color: var(--vscode-descriptionForeground);
      padding: 4px 0 2px;
    }
    .session-row {
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 6px 6px;
      border-radius: 4px;
      cursor: pointer;
    }
    .session-row:hover {
      background: var(--vscode-list-hoverBackground);
    }
    .session-active {
      background: var(--vscode-list-activeSelectionBackground, rgba(128,128,128,0.15));
      color: var(--vscode-list-activeSelectionForeground, inherit);
    }
    .session-health { font-size: 12px; flex-shrink: 0; }
    .session-info {
      display: flex;
      flex-direction: column;
      gap: 1px;
      min-width: 0;
      overflow: hidden;
    }
    .session-name {
      font-size: 12px;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    .session-meta {
      font-size: 11px;
      color: var(--vscode-descriptionForeground);
    }
    .links {
      display: flex;
      gap: 12px;
      margin-top: 6px;
    }
    .links a {
      color: var(--vscode-textLink-foreground);
      text-decoration: none;
      font-size: 12px;
    }
    .links a:hover { text-decoration: underline; }
    .muted { color: var(--vscode-descriptionForeground); }
    .hint {
      font-size: 12px;
      color: var(--vscode-descriptionForeground);
    }
    .setup-steps {
      margin: 8px 0;
      padding-left: 20px;
      font-size: 12px;
      line-height: 1.6;
    }
    .setup-steps li {
      margin-bottom: 6px;
    }
    code {
      background: var(--vscode-textCodeBlock-background);
      padding: 1px 4px;
      border-radius: 3px;
      font-size: 12px;
    }
  </style>`;
}
