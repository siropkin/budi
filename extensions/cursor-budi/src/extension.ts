import * as vscode from "vscode";
import * as fs from "fs";
import {
  fetchStatusline,
  fetchRecentSessions,
  splitSessionsByDay,
  aggregateHealth,
  formatAggregationStatusText,
  formatAggregationTooltip,
} from "./budiClient";
import {
  getActiveSessionFromFile,
  getAllActiveSessions,
  SESSION_FILE,
} from "./sessionStore";
import { HealthPanelProvider } from "./panel";

let statusBarItem: vscode.StatusBarItem;
let dataPollTimer: ReturnType<typeof setInterval> | undefined;
let sessionFileWatcher: fs.FSWatcher | undefined;
let healthProvider: HealthPanelProvider;
let currentSessionId: string | undefined;
let pinnedSessionId: string | undefined;
let log: vscode.OutputChannel;

export function activate(context: vscode.ExtensionContext): void {
  log = vscode.window.createOutputChannel("budi");
  context.subscriptions.push(log);
  log.appendLine(`[budi] activated at ${new Date().toISOString()}`);

  const config = vscode.workspace.getConfiguration("budi");
  let daemonUrl: string = config.get("daemonUrl", "http://127.0.0.1:7878");
  let dataPollInterval: number = config.get("pollingIntervalMs", 15000);

  const folders = vscode.workspace.workspaceFolders;
  log.appendLine(
    `[budi] workspaceFolders = ${folders?.map((f) => f.uri.fsPath).join(", ") ?? "none"}`
  );

  statusBarItem = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left,
    -100
  );
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
    refreshData(daemonUrl);
  });

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(
      HealthPanelProvider.viewType,
      healthProvider
    )
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.openDashboard", () => {
      const sid = resolveSessionId();
      const url = sid
        ? `${daemonUrl}/dashboard/sessions/${encodeURIComponent(sid)}`
        : `${daemonUrl}/dashboard`;
      vscode.env.openExternal(vscode.Uri.parse(url));
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.refreshStatus", () => {
      log.appendLine(`[budi] manual refresh triggered`);
      refreshData(daemonUrl);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.selectSession", async () => {
      const workspacePath = folders?.[0]?.uri.fsPath;
      if (!workspacePath) {
        vscode.window.showWarningMessage("budi: No workspace open.");
        return;
      }

      const sessions = getAllActiveSessions(workspacePath);
      if (sessions.length === 0) {
        vscode.window.showInformationMessage(
          "budi: No active sessions found. Start a chat to create one."
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
          description: s.composer_mode ?? "",
          detail: `Started ${s.started_at}${s.last_active_at ? ` · Active ${s.last_active_at}` : ""}`,
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

      refreshData(daemonUrl);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("budi.toggleHealthPanel", () => {
      vscode.commands.executeCommand("budi.healthPanel.focus");
    })
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("budi")) {
        const updated = vscode.workspace.getConfiguration("budi");
        daemonUrl = updated.get("daemonUrl", "http://127.0.0.1:7878");
        dataPollInterval = updated.get("pollingIntervalMs", 15000);
        restartDataPoll(daemonUrl, dataPollInterval);
      }
    })
  );

  watchSessionFile(daemonUrl);

  refreshData(daemonUrl);
  startDataPoll(daemonUrl, dataPollInterval);
}

export function deactivate(): void {
  if (dataPollTimer) {
    clearInterval(dataPollTimer);
    dataPollTimer = undefined;
  }
  if (sessionFileWatcher) {
    sessionFileWatcher.close();
    sessionFileWatcher = undefined;
  }
}

function startDataPoll(daemonUrl: string, intervalMs: number): void {
  dataPollTimer = setInterval(() => {
    refreshData(daemonUrl).catch((err) => {
      log.appendLine(`[budi] poll error: ${err}`);
    });
  }, intervalMs);
}

function restartDataPoll(daemonUrl: string, intervalMs: number): void {
  if (dataPollTimer) {
    clearInterval(dataPollTimer);
  }
  startDataPoll(daemonUrl, intervalMs);
}

/**
 * Watch cursor-sessions.json for changes. When a hook fires (user sends a
 * message, tool completes, etc.), the file updates and we refresh both the
 * statusline and panel — giving near-instant updates on interaction.
 */
function watchSessionFile(daemonUrl: string): void {
  try {
    if (!fs.existsSync(SESSION_FILE)) return;

    let debounce: ReturnType<typeof setTimeout> | undefined;
    sessionFileWatcher = fs.watch(SESSION_FILE, () => {
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(() => {
        const newId = resolveSessionId();
        if (newId !== currentSessionId) {
          log.appendLine(
            `[budi] session file changed: ${currentSessionId ?? "none"} → ${newId ?? "none"}`
          );
        }
        refreshData(daemonUrl).catch(() => {});
      }, 500);
    });
  } catch {
    // File may not exist yet — will be created by first hook.
  }
}

function resolveSessionId(): string | undefined {
  if (pinnedSessionId) return pinnedSessionId;

  const folders = vscode.workspace.workspaceFolders;
  if (!folders || folders.length === 0) return undefined;

  const session = getActiveSessionFromFile(folders[0].uri.fsPath);
  return session?.session_id;
}

async function refreshData(daemonUrl: string): Promise<void> {
  const folders = vscode.workspace.workspaceFolders;
  const cwd = folders?.[0]?.uri.fsPath;
  const sessionId = resolveSessionId();
  currentSessionId = sessionId;

  log.appendLine(
    `[budi] refreshData: session=${sessionId ?? "none"}, cwd=${cwd ?? "none"}`
  );

  healthProvider.updateContext(daemonUrl, sessionId);

  const [statusline, recentSessions] = await Promise.all([
    fetchStatusline(daemonUrl, sessionId, cwd).catch(() => null),
    fetchRecentSessions(daemonUrl).catch(() => null),
  ]);

  const { today: todaySessions } = recentSessions
    ? splitSessionsByDay(recentSessions)
    : { today: [] as import("./budiClient").SessionListEntry[] };

  if (todaySessions.length > 0) {
    const agg = aggregateHealth(todaySessions);
    const todayCost = statusline?.today_cost ?? 0;
    const text = formatAggregationStatusText(agg);
    const tooltip = formatAggregationTooltip(agg, todayCost);

    log.appendLine(
      `[budi] refreshData: sessions=${agg.total}, green=${agg.green}, yellow=${agg.yellow}, red=${agg.red}, text="${text}"`
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
