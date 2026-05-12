//! Surface dimension for messages/sessions: the host environment where the
//! AI conversation happened (vscode, cursor, jetbrains, terminal, unknown).
//!
//! Today the `provider` column collapses every Copilot Chat row into a
//! single `copilot_chat` value regardless of whether the session ran in VS
//! Code, JetBrains, or a remote dev container. Surface is the orthogonal
//! axis: `provider` answers "which agent", `surface` answers "which host".
//! Forking the provider key would fragment the Billing API reconciliation
//! contract in ADR-0092 §3 and the manifest/alias work in ADR-0091; one
//! provider, one bill, surface separate.
//!
//! Inference is parser-local — each provider returns the surface alongside
//! the existing fields it produces. No global cwd-sniffing.

use std::path::Path;

/// Canonical surface values. Lowercase, no aliasing.
pub const VSCODE: &str = "vscode";
pub const CURSOR: &str = "cursor";
pub const JETBRAINS: &str = "jetbrains";
pub const TERMINAL: &str = "terminal";
/// Hard fallback so the column stays NOT NULL even when we cannot infer.
pub const UNKNOWN: &str = "unknown";

/// Default surface for a provider when the provider does not set one
/// itself. Used by the ingest path as a `COALESCE`-style fallback so a
/// hypothetical future writer that skips inference still produces a valid
/// (if coarse) tag rather than NULL.
///
/// `claude_code`, `copilot` (CLI), `codex`, and the legacy proxy path all
/// run in the user's terminal — there is no IDE binding to capture. Cursor
/// is its own surface. Copilot Chat must always populate `surface` itself
/// (path-based per ADR-0092 §2.1); falling through to UNKNOWN here is the
/// "we do not know" signal rather than guessing.
///
/// `jetbrains_ai_assistant` is the JetBrains-published, Anthropic-backed
/// product (see [`crate::providers::jetbrains_ai_assistant`]). Every row
/// it emits is bound to the JetBrains IDE that wrote it, so the default
/// here collapses to `jetbrains` even if a future writer forgets to set
/// `surface` explicitly.
pub fn default_for_provider(provider: &str) -> &'static str {
    match provider {
        "claude_code" => TERMINAL,
        "cursor" => CURSOR,
        // The Copilot CLI provider id is `copilot_cli` (not `copilot`);
        // it tails `~/.copilot/session-state/` and runs in any shell.
        "copilot_cli" => TERMINAL,
        "codex" => TERMINAL,
        "jetbrains_ai_assistant" => JETBRAINS,
        _ => UNKNOWN,
    }
}

/// Infer the surface for a Copilot Chat session file from the path it was
/// discovered under. Threaded through alongside the path so the parser
/// stays surface-agnostic and only the discovery layer needs to map watch
/// roots → host kind.
///
/// Mapping (per ADR-0092 §2.1 + ticket #701):
///
/// - `Cursor/User/...` → `cursor`
/// - `Code/User/...`, `Code - Insiders/User/...`, `Code - Exploration/User/...`,
///   `VSCodium/User/...` → `vscode`
/// - `~/.vscode-server/...`, `~/.vscode-server-insiders/...`,
///   `~/.vscode-remote/...`, `/tmp/.vscode-server/...`,
///   `/workspace/.vscode-server/...` → `vscode`
/// - JetBrains-shaped paths (`JetBrains/<IDE>/...`, `Library/Logs/JetBrains/...`,
///   `~/.config/JetBrains/...`) → `jetbrains` (placeholder; the JetBrains
///   parser lands later — until then the row never reaches this function)
/// - Anything else → `unknown`
pub fn infer_copilot_chat_surface(path: &Path) -> &'static str {
    let path_str = path.to_string_lossy();

    // Cursor's VS Code fork keeps its `User/` directory under a top-level
    // `Cursor` segment. Match the segment name literally so a folder named
    // `Cursor Backup` or a file inside `mycursorlib` does not collide.
    if path_segment_matches(path, |s| s == "Cursor") {
        return CURSOR;
    }

    // VS Code stable / insiders / exploration / VSCodium all live under a
    // recognizable top-level directory.
    if path_segment_matches(path, |s| {
        s == "Code" || s == "Code - Insiders" || s == "Code - Exploration" || s == "VSCodium"
    }) {
        return VSCODE;
    }

    // Remote installs (SSH remote, dev containers, Codespaces, Tunnels).
    if path_segment_matches(path, |s| {
        s == ".vscode-server" || s == ".vscode-server-insiders" || s == ".vscode-remote"
    }) {
        return VSCODE;
    }

    // Catch the absolute remote roots that live outside the user's home
    // (`/tmp/.vscode-server`, `/workspace/.vscode-server`) — they share
    // the directory name but `path_segment_matches` already covers that.
    // The substring fallback below is a last-resort signal for paths that
    // do not segment cleanly (e.g. a URI representation).
    if path_str.contains(".vscode-server") || path_str.contains(".vscode-remote") {
        return VSCODE;
    }

    // JetBrains-shaped path placeholder. The JetBrains Copilot parser is
    // out of 8.4 scope (ADR-0092 §"Out of scope"); this branch exists so
    // the matrix is explicit and a hand-crafted test fixture can exercise
    // the surface rule without wiring up a real parser.
    if path_segment_matches(path, |s| s == "JetBrains") {
        return JETBRAINS;
    }

    // #758: the JetBrains Copilot plugin writes under
    // `~/.config/github-copilot/<ide-slug>/...` (mac/linux) or
    // `AppData/Local/github-copilot/...` (windows). The CLI provider keeps
    // its state under `~/.copilot/` and the VS Code extension under
    // `Code/User/...`, so a `github-copilot` *segment* is JetBrains-
    // exclusive in practice. Match the segment literally rather than
    // hard-coding the IDE slugs (`iu`, `ic`, `ws`, `pc`, `go`, …) — new
    // JetBrains IDEs would otherwise keep leaking into `surface=unknown`
    // every time JetBrains shipped a new product code.
    if path_segment_matches(path, |s| s == "github-copilot") {
        return JETBRAINS;
    }

    UNKNOWN
}

fn path_segment_matches<F>(path: &Path, pred: F) -> bool
where
    F: Fn(&str) -> bool,
{
    path.components()
        .any(|c| c.as_os_str().to_str().is_some_and(&pred))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cursor_user_root_maps_to_cursor() {
        let p = PathBuf::from(
            "/Users/ivan/Library/Application Support/Cursor/User/workspaceStorage/abc/chatSessions/x.jsonl",
        );
        assert_eq!(infer_copilot_chat_surface(&p), CURSOR);
    }

    #[test]
    fn code_user_root_maps_to_vscode() {
        let p = PathBuf::from(
            "/Users/ivan/Library/Application Support/Code/User/workspaceStorage/abc/chatSessions/x.jsonl",
        );
        assert_eq!(infer_copilot_chat_surface(&p), VSCODE);
    }

    #[test]
    fn code_insiders_maps_to_vscode() {
        let p = PathBuf::from(
            "/Users/ivan/Library/Application Support/Code - Insiders/User/workspaceStorage/abc/chatSessions/x.jsonl",
        );
        assert_eq!(infer_copilot_chat_surface(&p), VSCODE);
    }

    #[test]
    fn vscode_server_remote_maps_to_vscode() {
        let p = PathBuf::from(
            "/home/ivan/.vscode-server/data/User/workspaceStorage/abc/chatSessions/x.jsonl",
        );
        assert_eq!(infer_copilot_chat_surface(&p), VSCODE);
    }

    #[test]
    fn vscode_server_absolute_maps_to_vscode() {
        let p = PathBuf::from(
            "/tmp/.vscode-server/data/User/workspaceStorage/abc/chatSessions/x.jsonl",
        );
        assert_eq!(infer_copilot_chat_surface(&p), VSCODE);
    }

    #[test]
    fn jetbrains_shape_maps_to_jetbrains_placeholder() {
        let p = PathBuf::from(
            "/Users/ivan/Library/Application Support/JetBrains/IdeaIC2026.1/copilot/sessions/x.json",
        );
        assert_eq!(infer_copilot_chat_surface(&p), JETBRAINS);
    }

    /// #758: `~/.config/github-copilot/<ide-slug>/<session-type>/` is the
    /// JetBrains-side Copilot config root on Mac/Linux. The session-type
    /// dirs (`chat-sessions`, `chat-edit-sessions`, `chat-agent-sessions`,
    /// `bg-agent-sessions`) are what `health_sources` reports, and the
    /// IDE slugs (`iu`, `ic`, `ws`, `pc`, `go`, etc.) are open-ended.
    /// Match on the `github-copilot` segment so new IDE slugs don't keep
    /// leaking into `surface=unknown`.
    #[test]
    fn github_copilot_config_root_maps_to_jetbrains() {
        for slug in ["iu", "ic", "ws", "pc", "go", "rr"] {
            for session_type in [
                "chat-sessions",
                "chat-edit-sessions",
                "chat-agent-sessions",
                "bg-agent-sessions",
            ] {
                let p = PathBuf::from(format!(
                    "/Users/ivan/.config/github-copilot/{slug}/{session_type}"
                ));
                assert_eq!(
                    infer_copilot_chat_surface(&p),
                    JETBRAINS,
                    "expected JETBRAINS for {}/{}",
                    slug,
                    session_type
                );
            }
        }
    }

    /// #758: Windows JetBrains Copilot writes under
    /// `AppData/Local/github-copilot/...`. The `github-copilot` segment
    /// rule must carry across platforms. `PathBuf::from` only parses
    /// backslashes as separators on Windows, so the assertion is gated
    /// to that target.
    #[cfg(target_os = "windows")]
    #[test]
    fn github_copilot_windows_root_maps_to_jetbrains() {
        let p = PathBuf::from(r"C:\Users\ivan\AppData\Local\github-copilot\iu\chat-sessions");
        assert_eq!(infer_copilot_chat_surface(&p), JETBRAINS);
    }

    #[test]
    fn unknown_path_maps_to_unknown() {
        let p = PathBuf::from("/some/random/path/x.jsonl");
        assert_eq!(infer_copilot_chat_surface(&p), UNKNOWN);
    }

    #[test]
    fn default_for_provider_maps_known_providers() {
        assert_eq!(default_for_provider("claude_code"), TERMINAL);
        assert_eq!(default_for_provider("cursor"), CURSOR);
        assert_eq!(default_for_provider("copilot_cli"), TERMINAL);
        assert_eq!(default_for_provider("codex"), TERMINAL);
        assert_eq!(default_for_provider("copilot_chat"), UNKNOWN);
        assert_eq!(default_for_provider("jetbrains_ai_assistant"), JETBRAINS);
        assert_eq!(default_for_provider("anything_else"), UNKNOWN);
    }
}
