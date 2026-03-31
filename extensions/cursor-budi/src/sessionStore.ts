import * as fs from "fs";
import * as path from "path";
import * as os from "os";

interface SessionEntry {
  session_id: string;
  workspace_path: string;
  started_at: string;
  composer_mode?: string;
  active: boolean;
  last_active_at?: string;
}

interface SessionState {
  sessions: SessionEntry[];
}

const STATE_DIR = path.join(os.homedir(), ".local", "share", "budi");
export const SESSION_FILE = path.join(STATE_DIR, "cursor-sessions.json");

export function getActiveSessionFromFile(
  workspacePath: string
): { session_id: string } | null {
  const matches = getActiveSessions(workspacePath);
  return matches[0] ? { session_id: matches[0].session_id } : null;
}

export function getAllActiveSessions(
  workspacePath: string
): SessionEntry[] {
  return getActiveSessions(workspacePath);
}

function getActiveSessions(workspacePath: string): SessionEntry[] {
  const state = readState();
  if (!state) return [];

  const normalized = normalizePath(workspacePath);

  return state.sessions
    .filter(
      (s) => s.active && normalizePath(s.workspace_path) === normalized
    )
    .sort((a, b) => {
      const aTime = new Date(a.last_active_at ?? a.started_at).getTime();
      const bTime = new Date(b.last_active_at ?? b.started_at).getTime();
      return bTime - aTime;
    });
}

function readState(): SessionState | null {
  try {
    const raw = fs.readFileSync(SESSION_FILE, "utf-8");
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function normalizePath(p: string): string {
  return path.resolve(p).replace(/\/+$/, "");
}
