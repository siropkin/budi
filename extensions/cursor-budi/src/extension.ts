import * as vscode from "vscode";
import {
  fetchStatusline,
  fetchRecentSessions,
  fetchDaemonHealth,
  splitSessionsByDay,
  aggregateHealth,
  formatAggregationStatusText,
  formatAggregationTooltip,
  MIN_API_VERSION,
} from "./budiClient";
import { writeActiveWorkspace, clearActiveWorkspace } from "./sessionStore";
import { HealthPanelProvider } from "./panel";

let statusBarItem: vscode.StatusBarItem;
let dataPollTimer: ReturnType<typeof setInterval> | undefined;
let healthProvider: HealthPanelProvider;
let currentSessionId: string | undefined;
let pinnedSessionId: string | undefined;
let log: vscode.OutputChannel;
let refreshInFlight = false;
let pendingRefreshDaemonUrl: string | undefined;
let onboardingShown = false;

export function activate(context: vscode.ExtensionContext): void {
  log = vscode.window.createOutputChannel("budi");
  context.subscriptions.push(log);
  log.appendLine(`[budi] activated at ${new Date().toISOString()}`);

  const config = vscode.workspace.getConfiguration("budi");
  let daemonUrl: string = config.get("daemonUrl", "http://127.0.0.1:7878");
  let dataPollInterval: number = config.get("pollingIntervalMs", 15000);

  const folders = vscode.workspace.workspaceFolders;
  log.appendLine(
    `[budi] workspaceFolders = ${folders?.map((f) => f.uri.fsPath).join(", ") ?? "none"}`,
  );

  // Write active workspace signal (cursor-sessions.json v1 contract).
  if (folders && folders.length > 0) {
    writeActiveWorkspace(folders[0].uri.fsPath);
  }

  statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, -100);
  statusBarItem.name = "budi";
  statusBarItem.command = "budi.toggleHealthPanel";
  statusBarItem.text = "budi";
  statusBarItem.tooltip = "budi — AI cost tracker\n\nLoading...";
  statusBarItem.show();
  context.subscriptions.push(statusBarItem);

  healthProvider = new HealthPanelProvider(context.extensionUri, daemonUrl);
  healthProvider.setOnSelectSession((sessionId) => {
    pinnedSessionId = sessionId;
    log.appendLine(`[budi] session: pinned to ${sessionId} (from panel)`);
    requestRefresh(daemonUrl);
  });

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(HealthPanelProvider.viewType, healthProvider),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.openDashboard", () => {
      const sid = currentSessionId;
      const url = sid
        ? `${daemonUrl}/dashboard/sessions/${encodeURIComponent(sid)}`
        : `${daemonUrl}/dashboard`;
      vscode.env.openExternal(vscode.Uri.parse(url));
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.refreshStatus", () => {
      log.appendLine(`[budi] manual refresh triggered`);
      requestRefresh(daemonUrl);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.selectSession", async () => {
      const sessions = await fetchRecentSessions(daemonUrl);
      if (!sessions || sessions.length === 0) {
        vscode.window.showInformationMessage(
          "budi: No recent sessions found. Start using Cursor with the proxy to create one.",
        );
        return;
      }

      const autoLabel = "(auto — most recent)";
      const items: vscode.QuickPickItem[] = [
        {
          label: "$(clock) Auto-detect",
          description: autoLabel,
          detail: "Follow the most recently active session",
        },
        ...sessions.map((s) => ({
          label: s.session_id === pinnedSessionId ? `$(pin) ${s.session_id}` : s.session_id,
          description: s.provider ?? "",
          detail: `Started ${s.started_at ?? "unknown"}${s.cost_cents > 0 ? ` · $${(s.cost_cents / 100).toFixed(2)}` : ""}`,
        })),
      ];

      const picked = await vscode.window.showQuickPick(items, {
        placeHolder: "Select which session to display",
      });

      if (!picked) return;

      if (picked.description === autoLabel) {
        pinnedSessionId = undefined;
        log.appendLine("[budi] session: switched to auto-detect");
      } else {
        const label = picked.label.replace("$(pin) ", "");
        const match = sessions.find((s) => s.session_id === label);
        if (match) {
          pinnedSessionId = match.session_id;
          log.appendLine(`[budi] session: pinned to ${pinnedSessionId}`);
        }
      }

      requestRefresh(daemonUrl);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.toggleHealthPanel", () => {
      vscode.commands.executeCommand("budi.healthPanel.focus");
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("budi")) {
        const updated = vscode.workspace.getConfiguration("budi");
        daemonUrl = updated.get("daemonUrl", "http://127.0.0.1:7878");
        dataPollInterval = updated.get("pollingIntervalMs", 15000);
        restartDataPoll(daemonUrl, dataPollInterval);
        requestRefresh(daemonUrl);
      }
    }),
  );

  // Check daemon health and show onboarding if needed.
  void checkDaemonAndOnboard(daemonUrl);

  requestRefresh(daemonUrl);
  startDataPoll(daemonUrl, dataPollInterval);
}

export function deactivate(): void {
  if (dataPollTimer) {
    clearInterval(dataPollTimer);
    dataPollTimer = undefined;
  }
  clearActiveWorkspace();
}

function startDataPoll(daemonUrl: string, intervalMs: number): void {
  dataPollTimer = setInterval(() => {
    requestRefresh(daemonUrl);
  }, intervalMs);
}

function restartDataPoll(daemonUrl: string, intervalMs: number): void {
  if (dataPollTimer) {
    clearInterval(dataPollTimer);
  }
  startDataPoll(daemonUrl, intervalMs);
}

/**
 * Check daemon health on startup. If the daemon is unreachable or has an
 * incompatible API version, show an onboarding notification.
 */
async function checkDaemonAndOnboard(daemonUrl: string): Promise<void> {
  const health = await fetchDaemonHealth(daemonUrl);

  if (!health) {
    log.appendLine("[budi] daemon is not reachable — showing onboarding");
    showOnboardingNotification();
    return;
  }

  log.appendLine(
    `[budi] daemon healthy: version=${health.version}, api_version=${health.api_version}`,
  );

  if (health.api_version < MIN_API_VERSION) {
    vscode.window.showWarningMessage(
      `budi: The daemon (api_version ${health.api_version}) is older than ` +
        `this extension requires (api_version ${MIN_API_VERSION}). ` +
        `Please update budi: curl -fsSL https://raw.githubusercontent.com/siropkin/budi/main/scripts/install.sh | bash`,
    );
  }
}

function showOnboardingNotification(): void {
  if (onboardingShown) return;
  onboardingShown = true;

  void vscode.window
    .showInformationMessage(
      "budi: Daemon not running. Install budi and run `budi init` to start tracking AI costs.",
      "Setup Guide",
      "Dismiss",
    )
    .then((choice) => {
      if (choice === "Setup Guide") {
        void vscode.env.openExternal(
          vscode.Uri.parse("https://github.com/siropkin/budi#quick-start"),
        );
      }
    });
}

function requestRefresh(daemonUrl: string): void {
  pendingRefreshDaemonUrl = daemonUrl;
  if (refreshInFlight) return;

  refreshInFlight = true;
  void (async () => {
    try {
      while (pendingRefreshDaemonUrl) {
        const nextDaemonUrl = pendingRefreshDaemonUrl;
        pendingRefreshDaemonUrl = undefined;
        await refreshData(nextDaemonUrl);
      }
    } catch (err) {
      log.appendLine(`[budi] refresh error: ${err}`);
    } finally {
      refreshInFlight = false;
      if (pendingRefreshDaemonUrl) {
        requestRefresh(pendingRefreshDaemonUrl);
      }
    }
  })();
}

/**
 * Resolve the active session ID. In 8.0, sessions come from the daemon's
 * proxy event tracking — no hook-based cursor-sessions.json dependency.
 * The extension derives the active session from the most recent session
 * in the daemon's API that matches this workspace.
 */
function resolveSessionId(): string | undefined {
  return pinnedSessionId ?? currentSessionId;
}

async function refreshData(daemonUrl: string): Promise<void> {
  const folders = vscode.workspace.workspaceFolders;
  const cwd = folders?.[0]?.uri.fsPath;

  // Update workspace signal on each refresh.
  if (cwd) {
    writeActiveWorkspace(cwd);
  }

  healthProvider.updateContext(daemonUrl, resolveSessionId());

  const [statusline, recentSessions] = await Promise.all([
    fetchStatusline(daemonUrl, resolveSessionId(), cwd).catch(() => null),
    fetchRecentSessions(daemonUrl).catch(() => null),
  ]);

  // Derive the active session from recent sessions if not pinned.
  if (!pinnedSessionId && recentSessions && recentSessions.length > 0) {
    currentSessionId = recentSessions[0].session_id;
  }

  const { today: todaySessions } = recentSessions
    ? splitSessionsByDay(recentSessions)
    : { today: [] as import("./budiClient").SessionListEntry[] };

  if (todaySessions.length > 0) {
    const agg = aggregateHealth(todaySessions);
    const todayCost = statusline?.today_cost ?? 0;
    const text = formatAggregationStatusText(agg);
    const tooltip = formatAggregationTooltip(agg, todayCost);

    log.appendLine(
      `[budi] refreshData: sessions=${agg.total}, green=${agg.green}, yellow=${agg.yellow}, red=${agg.red}, text="${text}"`,
    );
    statusBarItem.text = text;
    statusBarItem.tooltip = tooltip;
  } else if (statusline) {
    statusBarItem.text = `budi · $${statusline.today_cost.toFixed(2)} today`;
    statusBarItem.tooltip = "budi — AI cost tracker\n\nClick to open session health";
  } else {
    log.appendLine(`[budi] refreshData: no data (daemon offline?)`);
    statusBarItem.text = "budi \u00B7 offline";
    statusBarItem.tooltip =
      "budi \u2014 AI cost tracker\n\nDaemon not reachable.\nRun `budi init` to start.";
  }

  healthProvider.refresh().catch(() => {});
}
