//! `budi db import` — historical transcript import.
//!
//! The command walks every enabled provider (Claude Code, Codex, Copilot CLI,
//! Cursor), ingests new transcript messages into the analytics DB, and
//! renders a per-agent progress feed while it runs. Before #440 the surface
//! was a silent 4 m 30 s `15s... 30s... 45s...` heartbeat with no per-agent
//! breakdown; the CLI now polls `/sync/status` every 2 s and flushes each
//! per-agent snapshot, then prints a reconciled final table keyed on the
//! per-provider report returned by the daemon.

use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use budi_core::analytics::{ProviderSyncStats, SyncProgress};
use serde_json::json;

use crate::client::{DaemonClient, SyncResponse};

use super::ansi;

/// How often the CLI polls `/sync/status` for per-agent progress. The daemon
/// emits progress ticks at ~900 ms inside the sync loop, so 2 s here gives
/// every poll at least one fresh snapshot while staying well under the
/// user's "is this hung?" threshold (the #440 bug report pointed to the
/// previous 15 s heartbeat as the "is this hung?" pain).
const POLL_INTERVAL: Duration = Duration::from_millis(2_000);

/// Entry point for `budi db import` / `budi db import --force` /
/// `budi db import --format json`.
///
/// `force`: clear existing sync state and re-ingest from scratch (wires to
/// `POST /sync/reset`). Non-force defaults to `POST /sync/all` (quick 30-day
/// window; the daemon's historical path feeds from the same function).
/// `json`: emit a structured per-agent summary on stdout instead of the
/// text table. Progress chatter is suppressed in JSON mode so stdout stays
/// parseable.
pub fn cmd_import(force: bool, json: bool) -> Result<()> {
    let client = DaemonClient::connect()?;
    let quiet = json;
    let is_tty = std::io::stdout().is_terminal();

    if !quiet {
        if force {
            println!("Force re-importing all data (this may take a while)...");
        } else {
            println!("Importing historical transcripts (this may take a while)...");
        }
    }

    let start = Instant::now();

    // Run the sync POST on a background thread so the main thread can poll
    // `/sync/status` for live per-agent progress. Using `mpsc::channel`
    // instead of `JoinHandle::is_finished` keeps the poll loop portable to
    // older toolchains and lets us carry an `anyhow::Error` across threads.
    let (tx, rx) = mpsc::channel::<Result<SyncResponse>>();
    let send_client = client.clone();
    let worker = thread::spawn(move || {
        let result = if force {
            send_client.sync_reset()
        } else {
            send_client.history()
        };
        let _ = tx.send(result);
    });

    let mut renderer = ProgressRenderer::new(is_tty, quiet);

    let final_result = loop {
        // Block up to POLL_INTERVAL on the worker's completion. When the
        // worker sends its result, recv_timeout returns Ok and we drop out;
        // otherwise we fall through to poll `/sync/status`.
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(status) = client.sync_status()
                    && let Some(progress) = status.progress.as_ref()
                {
                    renderer.render(progress);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Worker panicked or dropped the sender without sending;
                // translate to a user-visible error rather than hanging.
                break Err(anyhow::anyhow!(
                    "import worker thread died before completing — run `budi doctor` to check daemon status"
                ));
            }
        }
    };
    let _ = worker.join();
    renderer.finish();

    let elapsed = start.elapsed().as_secs_f64();
    let response = final_result?;

    if json {
        let body = json!({
            "ok": true,
            "elapsed_seconds": elapsed,
            "files_synced": response.files_synced,
            "messages_ingested": response.messages_ingested,
            "warnings": response.warnings,
            "per_provider": response
                .per_provider
                .iter()
                .map(|p| json!({
                    "name": p.name,
                    "display_name": p.display_name,
                    "files_total": p.files_total,
                    "files_synced": p.files_synced,
                    "messages": p.messages,
                }))
                .collect::<Vec<_>>(),
        });
        super::print_json(&body)?;
        return Ok(());
    }

    print_summary(&response, elapsed);
    print_warnings(&response.warnings);
    print_stats_hint(&client);
    Ok(())
}

/// Tracks the last-rendered state so TTY repaints only change the current
/// line, and non-TTY output only prints when something interesting changes
/// (provider transitions + per-provider finalization). This avoids both the
/// "4 minutes of identical dots" failure mode and the "200 lines of
/// per-file chatter" failure mode.
struct ProgressRenderer {
    is_tty: bool,
    quiet: bool,
    last_provider: Option<String>,
    last_line: String,
    already_printed_line: bool,
    non_tty_finalized: std::collections::HashSet<String>,
}

impl ProgressRenderer {
    fn new(is_tty: bool, quiet: bool) -> Self {
        Self {
            is_tty,
            quiet,
            last_provider: None,
            last_line: String::new(),
            already_printed_line: false,
            non_tty_finalized: std::collections::HashSet::new(),
        }
    }

    fn render(&mut self, progress: &SyncProgress) {
        if self.quiet {
            return;
        }
        if self.is_tty {
            self.render_tty(progress);
        } else {
            self.render_plain(progress);
        }
    }

    fn finish(&mut self) {
        if self.quiet {
            return;
        }
        if self.is_tty && self.already_printed_line {
            // Wipe the trailing progress line so the summary starts on its own row.
            print!("\r\x1b[2K");
            let _ = std::io::stdout().flush();
        }
    }

    fn render_tty(&mut self, progress: &SyncProgress) {
        let Some(line) = current_provider_line(progress) else {
            return;
        };
        if line == self.last_line {
            return;
        }
        self.last_line = line.clone();
        self.already_printed_line = true;
        print!("\r\x1b[2K  {line}");
        let _ = std::io::stdout().flush();
    }

    fn render_plain(&mut self, progress: &SyncProgress) {
        // On non-TTY (piped, log file, CI) emit one line per provider when
        // it finishes — avoids spamming the file with N per-file ticks.
        let current = progress.current_provider.as_deref();
        if let Some(prev) = self.last_provider.clone()
            && current != Some(prev.as_str())
            && let Some(stats) = progress.per_provider.iter().find(|p| p.name == prev)
            && !self.non_tty_finalized.contains(&prev)
        {
            print_provider_plain(stats);
            self.non_tty_finalized.insert(prev);
        }
        self.last_provider = current.map(|s| s.to_string());

        // Flush any other already-finished providers (e.g. zero-file agents
        // the sync loop skipped in under a second without a visible
        // transition through current_provider).
        for stats in &progress.per_provider {
            if stats.files_total == 0 && stats.messages == 0 {
                continue;
            }
            if current == Some(stats.name.as_str()) {
                continue;
            }
            if self.non_tty_finalized.contains(&stats.name) {
                continue;
            }
            print_provider_plain(stats);
            self.non_tty_finalized.insert(stats.name.clone());
        }
    }
}

fn current_provider_line(progress: &SyncProgress) -> Option<String> {
    let current_name = progress.current_provider.as_ref()?;
    let stats = progress
        .per_provider
        .iter()
        .find(|p| &p.name == current_name)?;
    let label = if stats.display_name.is_empty() {
        stats.name.clone()
    } else {
        stats.display_name.clone()
    };
    if stats.files_total == 0 {
        return Some(format!(
            "[{label}] {msgs} messages ingested so far…",
            msgs = format_int(stats.messages),
        ));
    }
    let pct = if stats.files_total > 0 {
        (stats.files_synced as f64 / stats.files_total as f64 * 100.0).round() as usize
    } else {
        0
    };
    Some(format!(
        "[{label}] {done} / {total} files ({pct}%), {msgs} messages",
        done = format_int(stats.files_synced),
        total = format_int(stats.files_total),
        pct = pct,
        msgs = format_int(stats.messages),
    ))
}

fn print_provider_plain(stats: &ProviderSyncStats) {
    let label = if stats.display_name.is_empty() {
        stats.name.clone()
    } else {
        stats.display_name.clone()
    };
    if stats.files_total == 0 {
        println!(
            "  [{label}] {msgs} messages",
            msgs = format_int(stats.messages)
        );
    } else {
        println!(
            "  [{label}] {done} / {total} files, {msgs} messages",
            done = format_int(stats.files_synced),
            total = format_int(stats.files_total),
            msgs = format_int(stats.messages),
        );
    }
}

fn print_summary(response: &SyncResponse, elapsed: f64) {
    let bold = ansi("\x1b[1m");
    let green = ansi("\x1b[32m");
    let dim = ansi("\x1b[2m");
    let reset = ansi("\x1b[0m");

    println!(
        "{green}✓{reset} Imported in {bold}{elapsed:.1}s{reset}",
        elapsed = elapsed
    );

    let label_width = response
        .per_provider
        .iter()
        .map(|p| {
            if p.display_name.is_empty() {
                p.name.len()
            } else {
                p.display_name.len()
            }
        })
        .max()
        .unwrap_or(12)
        .max(12);

    if !response.per_provider.is_empty() {
        for stats in &response.per_provider {
            let label = if stats.display_name.is_empty() {
                stats.name.clone()
            } else {
                stats.display_name.clone()
            };
            // Per-agent rows: `  Claude Code     118,442 messages   from 2,035 files`.
            println!(
                "    {label:<width$}   {msgs:>11} messages   from {files:>5} files",
                label = label,
                width = label_width,
                msgs = format_int(stats.messages),
                files = format_int(stats.files_total.max(stats.files_synced)),
            );
        }
        // Horizontal rule + total keyed on the daemon's grand totals so the
        // summary is a reconciliation, not a sum the CLI derives itself.
        let rule_width = label_width + 3 + 11 + "   messages   from ".len() + 5 + " files".len();
        println!("    {dim}{rule}{reset}", rule = "─".repeat(rule_width));
        println!(
            "    {label:<width$}   {msgs:>11} messages   from {files:>5} files",
            label = "Total",
            width = label_width,
            msgs = format_int(response.messages_ingested),
            files = format_int(response.files_synced),
        );
    } else {
        // Defensive: a legacy daemon that didn't populate `per_provider`
        // still gets a usable (if collapsed) summary. The CLI contract is
        // that the final grand totals are authoritative.
        println!(
            "    {bold}{msgs}{reset} messages from {bold}{files}{reset} files.",
            msgs = format_int(response.messages_ingested),
            files = format_int(response.files_synced),
        );
    }
}

fn print_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    let yellow = ansi("\x1b[33m");
    let reset = ansi("\x1b[0m");
    for w in warnings {
        eprintln!("{yellow}Warning:{reset} {w}");
    }
}

/// Opportunistically surface last-30-day totals so the user sees
/// "you already spent $X this month" right after import instead of having
/// to hunt for `budi stats -p 30d` (#440 acceptance #4). Any failure here
/// is silent — the import itself succeeded, and the hint is a nice-to-have.
fn print_stats_hint(client: &DaemonClient) {
    let Ok(summary) = client.summary(Some("30d"), None, None) else {
        return;
    };
    if summary.total_messages == 0 {
        return;
    }
    let dollars = summary.total_cost_cents / 100.0;
    let dim = ansi("\x1b[2m");
    let reset = ansi("\x1b[0m");
    println!(
        "{dim}Last 30 days: {cost} across {msgs} messages — run `budi stats -p 30d` to see the breakdown.{reset}",
        cost = format_dollars(dollars),
        msgs = format_int(summary.total_messages as usize),
    );
}

fn format_int(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn format_dollars(d: f64) -> String {
    if d >= 1_000.0 {
        format!("${:.0}", d)
    } else {
        format!("${:.2}", d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(
        name: &str,
        display: &str,
        files_total: usize,
        files_synced: usize,
        msgs: usize,
    ) -> ProviderSyncStats {
        ProviderSyncStats {
            name: name.to_string(),
            display_name: display.to_string(),
            files_total,
            files_synced,
            messages: msgs,
        }
    }

    #[test]
    fn format_int_inserts_thousands_separators() {
        assert_eq!(format_int(0), "0");
        assert_eq!(format_int(999), "999");
        assert_eq!(format_int(1_000), "1,000");
        assert_eq!(format_int(1_234_567), "1,234,567");
    }

    #[test]
    fn current_provider_line_renders_pct_with_discovery() {
        let progress = SyncProgress {
            current_provider: Some("claude_code".to_string()),
            per_provider: vec![stats("claude_code", "Claude Code", 200, 50, 1234)],
        };
        let line = current_provider_line(&progress).expect("provider line");
        assert!(line.contains("Claude Code"));
        assert!(line.contains("50 / 200 files (25%)"));
        assert!(line.contains("1,234 messages"));
    }

    #[test]
    fn current_provider_line_handles_direct_sync_provider() {
        // Direct-sync providers (Cursor Usage API) report `files_total = 0`
        // because there are no transcript files to iterate. Line should
        // still surface ingested-message momentum without a "/ 0 files"
        // ratio that reads like a failure.
        let progress = SyncProgress {
            current_provider: Some("cursor".to_string()),
            per_provider: vec![stats("cursor", "Cursor", 0, 0, 29_038)],
        };
        let line = current_provider_line(&progress).expect("direct-sync provider line");
        assert!(!line.contains("/ 0 files"));
        assert!(line.contains("29,038 messages ingested"));
    }

    #[test]
    fn renderer_plain_prints_once_per_provider_on_transition() {
        // On non-TTY output, provider transitions should flush the finished
        // provider exactly once. This is the thing that prevents the import
        // log from becoming a 2,000-line scrollback.
        let mut renderer = ProgressRenderer::new(false, false);
        let stats_cc_partial = stats("claude_code", "Claude Code", 200, 100, 500);
        let stats_cc_done = stats("claude_code", "Claude Code", 200, 200, 1_000);
        let stats_codex = stats("codex", "Codex", 70, 20, 100);

        // In the middle of Claude Code — nothing should finalize yet.
        renderer.render(&SyncProgress {
            current_provider: Some("claude_code".to_string()),
            per_provider: vec![stats_cc_partial.clone()],
        });
        assert!(renderer.non_tty_finalized.is_empty());

        // Provider transition — Claude Code should be printed, even with its
        // partial snapshot (the sync moved on; we finalize with whatever the
        // last tick said). This is fine because the final per-agent summary
        // reconciles against the daemon's authoritative totals.
        renderer.render(&SyncProgress {
            current_provider: Some("codex".to_string()),
            per_provider: vec![stats_cc_done, stats_codex],
        });
        assert!(renderer.non_tty_finalized.contains("claude_code"));
        assert_eq!(renderer.non_tty_finalized.len(), 1);
    }
}
