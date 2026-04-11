import * as fs from "fs";
import * as path from "path";
import * as os from "os";

/**
 * cursor-sessions.json v1 contract (ADR-0086 §3.4, §6).
 *
 * This file is written by the Cursor extension to signal which workspace is
 * currently active. The daemon may read it to correlate proxy events with the
 * active Cursor workspace.
 *
 * Format (v1):
 * ```json
 * {
 *   "version": 1,
 *   "active_workspace": "/absolute/path/to/project",
 *   "updated_at": "2026-04-11T20:00:00.000Z"
 * }
 * ```
 *
 * Breaking format changes require bumping `version`.
 */

export const CONTRACT_VERSION = 1;

interface CursorSessionsV1 {
  version: number;
  active_workspace: string;
  updated_at: string;
}

const STATE_DIR = path.join(os.homedir(), ".local", "share", "budi");
export const SESSION_FILE = path.join(STATE_DIR, "cursor-sessions.json");

/**
 * Write the cursor-sessions.json v1 contract file to signal the active
 * workspace. Called by the extension on activate and when workspace changes.
 */
export function writeActiveWorkspace(workspacePath: string): void {
  const data: CursorSessionsV1 = {
    version: CONTRACT_VERSION,
    active_workspace: path.resolve(workspacePath),
    updated_at: new Date().toISOString(),
  };

  try {
    fs.mkdirSync(STATE_DIR, { recursive: true });
    fs.writeFileSync(SESSION_FILE, JSON.stringify(data, null, 2) + "\n");
  } catch {
    // Best-effort — don't crash the extension if the file can't be written.
  }
}

/**
 * Read the current active workspace from cursor-sessions.json.
 * Returns null if the file doesn't exist or is invalid.
 */
export function readActiveWorkspace(): string | null {
  try {
    const raw = fs.readFileSync(SESSION_FILE, "utf-8");
    const parsed = JSON.parse(raw);
    if (parsed.version === CONTRACT_VERSION && typeof parsed.active_workspace === "string") {
      return parsed.active_workspace;
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Clear the active workspace signal (e.g., on deactivate).
 */
export function clearActiveWorkspace(): void {
  try {
    if (fs.existsSync(SESSION_FILE)) {
      fs.unlinkSync(SESSION_FILE);
    }
  } catch {
    // Best-effort cleanup.
  }
}
