import * as vscode from "vscode";
import {
  StatuslineData,
  SessionHealthData,
  fetchStatusline,
  fetchSessionHealth,
} from "./budiClient";

export class CoachPanelProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = "budi.coachPanel";

  private view?: vscode.WebviewView;
  private latestData?: StatuslineData;
  private latestHealth?: SessionHealthData;
  private daemonUrl: string;
  private sessionId?: string;
  private workspacePath?: string;

  constructor(
    private readonly extensionUri: vscode.Uri,
    daemonUrl: string
  ) {
    this.daemonUrl = daemonUrl;
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
      }
    });

    webviewView.onDidChangeVisibility(() => {
      if (webviewView.visible) {
        this.refresh().catch(() => {});
      }
    });

    this.renderHtml();
    this.refresh().catch(() => {});
  }

  updateContext(
    daemonUrl: string,
    sessionId?: string,
    workspacePath?: string
  ): void {
    this.daemonUrl = daemonUrl;
    this.sessionId = sessionId;
    this.workspacePath = workspacePath;
  }

  async refresh(): Promise<void> {
    this.latestData = (await fetchStatusline(
      this.daemonUrl,
      this.sessionId,
      this.workspacePath
    )) ?? undefined;

    this.latestHealth = (await fetchSessionHealth(
      this.daemonUrl,
      this.sessionId
    )) ?? undefined;

    this.renderHtml();
  }

  private renderHtml(): void {
    if (!this.view) {
      return;
    }

    const data = this.latestData;
    const health = this.latestHealth;
    const baseUrl = this.daemonUrl;

    const sessionUrl = this.sessionId
      ? `${baseUrl}/dashboard/sessions/${encodeURIComponent(this.sessionId)}`
      : `${baseUrl}/dashboard`;

    this.view.webview.html = buildHtml(data, health, baseUrl, sessionUrl);
  }
}

function buildHtml(
  data: StatuslineData | undefined,
  health: SessionHealthData | undefined,
  dashboardUrl: string,
  sessionUrl: string
): string {
  if (!data) {
    return `<!DOCTYPE html>
<html>
<head>${styles()}</head>
<body>
  <div class="container">
    <p class="muted">budi daemon offline</p>
    <p class="hint">Run <code>budi init</code> to start the daemon.</p>
  </div>
</body>
</html>`;
  }

  const fmt = (v: number) => {
    if (v >= 1000) return `$${(v / 1000).toFixed(1)}K`;
    if (v >= 100) return `$${Math.round(v)}`;
    if (v > 0) return `$${v.toFixed(2)}`;
    return "$0.00";
  };

  const healthIcon = (state: string) => {
    switch (state) {
      case "red": return "\u{1F534}";
      case "yellow": return "\u{1F7E1}";
      case "gray": return "\u26AA";
      default: return "\u{1F7E2}";
    }
  };

  let healthSection = "";
  if (health) {
    healthSection = `
    <div class="card">
      <div class="card-title">Session Health</div>
      <div class="health-row">
        <span class="health-icon">${healthIcon(health.state)}</span>
        <span>${health.tip || "Not enough data yet"}</span>
      </div>
      <div class="stat-row">
        <span class="label">Messages</span>
        <span class="value">${health.message_count}</span>
      </div>
      <div class="stat-row">
        <span class="label">Session Cost</span>
        <span class="value">${fmt(health.total_cost_cents / 100)}</span>
      </div>
    </div>`;
  } else if (data.health_state) {
    healthSection = `
    <div class="card">
      <div class="card-title">Session Health</div>
      <div class="health-row">
        <span class="health-icon">${healthIcon(data.health_state)}</span>
        <span>${data.health_tip || "Not enough data yet"}</span>
      </div>
    </div>`;
  }

  const costsSection = `
    <div class="card">
      <div class="card-title">Cost Overview</div>
      ${data.session_cost !== undefined ? `<div class="stat-row"><span class="label">Session</span><span class="value">${fmt(data.session_cost)}</span></div>` : ""}
      <div class="stat-row"><span class="label">Today</span><span class="value">${fmt(data.today_cost)}</span></div>
      <div class="stat-row"><span class="label">Week</span><span class="value">${fmt(data.week_cost)}</span></div>
      <div class="stat-row"><span class="label">Month</span><span class="value">${fmt(data.month_cost)}</span></div>
      ${data.branch_cost !== undefined ? `<div class="stat-row"><span class="label">Branch</span><span class="value">${fmt(data.branch_cost)}</span></div>` : ""}
      ${data.project_cost !== undefined ? `<div class="stat-row"><span class="label">Project</span><span class="value">${fmt(data.project_cost)}</span></div>` : ""}
    </div>`;

  const providerSection = data.active_provider
    ? `<div class="card">
        <div class="card-title">Provider</div>
        <div class="stat-row"><span class="value">${data.active_provider}</span></div>
      </div>`
    : "";

  return `<!DOCTYPE html>
<html>
<head>${styles()}</head>
<body>
  <div class="container">
    ${healthSection}
    ${costsSection}
    ${providerSection}
    <div class="links">
      <a href="#" onclick="postMessage('openDashboard', '${dashboardUrl}')">Open Dashboard</a>
      ${data.session_cost !== undefined ? `<a href="#" onclick="postMessage('openDashboard', '${sessionUrl}')">Session Detail</a>` : ""}
    </div>
  </div>
  <script>
    const vscode = acquireVsCodeApi();
    function postMessage(cmd, url) {
      vscode.postMessage({ command: cmd, url: url });
    }
  </script>
</body>
</html>`;
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
    .card-title {
      font-weight: 600;
      font-size: 11px;
      text-transform: uppercase;
      letter-spacing: 0.5px;
      color: var(--vscode-descriptionForeground);
      margin-bottom: 8px;
    }
    .stat-row {
      display: flex;
      justify-content: space-between;
      padding: 3px 0;
    }
    .label { color: var(--vscode-descriptionForeground); }
    .value { font-weight: 600; font-variant-numeric: tabular-nums; }
    .health-row {
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 2px 0 6px;
    }
    .health-icon { font-size: 14px; }
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
    code {
      background: var(--vscode-textCodeBlock-background);
      padding: 1px 4px;
      border-radius: 3px;
      font-size: 12px;
    }
  </style>`;
}
