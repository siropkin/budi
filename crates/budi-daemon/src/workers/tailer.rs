//! Filesystem tailer worker.
//!
//! The tailer is the live ingestion path introduced by [ADR-0089] §1 / #319.
//! It registers a recursive `notify` watcher on every directory returned by
//! [`Provider::watch_roots`] and feeds appended JSONL content through
//! [`Pipeline::default_pipeline`] into the same tables `budi import` writes
//! to. There is no second code path — the import command and the live tailer
//! call the same provider parser, the same enricher chain, and the same
//! `ingest_messages_with_sync` sink.
//!
//! ## Lifecycle (R1.4 — default-on)
//!
//! 1. `main.rs` spawns [`run`] unconditionally on every daemon start (the
//!    `BUDI_LIVE_TAIL` gate from R1.3 was removed in R1.4 / #320). In R2.1
//!    (#322) the proxy runtime was removed, so the tailer is the only live
//!    writer to `messages` / `tags` / `sessions`.
//! 2. [`run`] hops into a blocking thread (`notify` is fundamentally
//!    blocking and we don't want to bind a Tokio worker thread for it),
//!    snapshots `enabled_providers()`, builds a `(provider, watch_root)`
//!    map, and seeds [`tail_offsets`](budi_core::analytics::set_tail_offset)
//!    with `byte_offset = file_len` for every existing transcript. That is
//!    the "skip the backfill, leave history to `budi import`" property
//!    called out in the ticket Acceptance.
//! 3. A `notify-debouncer-mini` watcher with a 500 ms debounce dispatches
//!    grown / created `*.jsonl` paths into a `std::sync::mpsc` channel; the
//!    main loop drains the channel and runs [`process_path`].
//! 4. Every 5 s the loop also calls [`backstop_scan`] to cover the well-known
//!    macOS / WSL `notify` edge cases (rotated files, mtime jitter, missed
//!    events on network volumes).
//!
//! ## Why a separate `tail_offsets` table
//!
//! The existing `sync_state` table is keyed on `file_path` only and is
//! shared with `budi import`. If the user runs `budi import` and the live
//! tailer concurrently, their offsets must not stomp on each other. Per-
//! provider scope also lets the post-removal cleanup in #357 prune by
//! provider when a user disables one. See
//! [`budi_core::analytics::set_tail_offset`].
//!
//! ## What this module deliberately does **not** do (Must-not list, #319)
//!
//! - No writes to `proxy_events`. The pipeline writes to `messages`, `tags`,
//!   `sessions`, and `tail_offsets` only.
//! - No changes to `Pipeline` signature or the enricher list — the tailer
//!   uses `Pipeline::default_pipeline` exactly as `budi import` does.
//! - No edits to `analytics/sync.rs` `proxy_cutoff`. Cross-path dedup stays
//!   on until R2.5 (#326) decides the fate of pre-existing
//!   `cost_confidence='proxy_estimated'` rows from 8.1.x users; while those
//!   rows live in the DB, `budi import` still needs the cutoff to avoid
//!   double-counting JSONL backfill against historical proxy ingest.
//!
//! [ADR-0089]: https://github.com/siropkin/budi/blob/main/docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::analytics::{self, get_tail_offset, ingest_messages_with_sync, set_tail_offset};
use budi_core::pipeline::Pipeline;
use budi_core::provider::Provider;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use rusqlite::Connection;

/// `notify-debouncer-mini` collapses duplicate events that arrive within
/// this window. 500 ms is short enough to feel live (well under the 1–10 s
/// budget ADR-0089 §5 sets) and long enough to coalesce the burst of writes
/// most agents emit when streaming a single response.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Backstop poll cadence. Runs whenever the event channel is idle. Catches
/// the documented `notify` edge cases (rotated files on macOS Spotlight,
/// missed events on WSL2 / network shares) without adding meaningful CPU.
const BACKSTOP_POLL: Duration = Duration::from_secs(5);

/// Spawn the tailer in a blocking task and return immediately.
///
/// The caller (`daemon::main`) owns the `shutdown` flag — flipping it
/// causes the loop to exit at the next event or backstop tick. Dropping
/// the flag without flipping is also fine; the worker just keeps running
/// for the lifetime of the daemon process.
pub async fn run(db_path: PathBuf, shutdown: Arc<AtomicBool>) {
    let providers = budi_core::provider::enabled_providers();
    if providers.is_empty() {
        tracing::info!(
            target: "budi_daemon::tailer",
            "no enabled providers; tailer exiting"
        );
        return;
    }
    let _ = tokio::task::spawn_blocking(move || run_blocking(db_path, providers, shutdown)).await;
}

/// Blocking entry point. Public for the integration test in
/// `tests/tailer_offsets.rs`, which constructs a stub provider and drives
/// the loop directly.
pub fn run_blocking(
    db_path: PathBuf,
    providers: Vec<Box<dyn Provider>>,
    shutdown: Arc<AtomicBool>,
) {
    let routes = build_routes(&providers);
    if routes.is_empty() {
        tracing::info!(
            target: "budi_daemon::tailer",
            "no watch roots from enabled providers; tailer exiting"
        );
        return;
    }

    let mut conn = match analytics::open_db(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                target: "budi_daemon::tailer",
                error = %e,
                "failed to open analytics db; tailer exiting"
            );
            return;
        }
    };

    let providers_by_name = index_providers_by_name(providers);

    if let Err(e) = seed_offsets(&mut conn, &providers_by_name) {
        tracing::warn!(
            target: "budi_daemon::tailer",
            error = %format!("{e:#}"),
            "offset seeding failed; tailer continues with default offsets"
        );
    }

    let mut pipeline = Pipeline::default_pipeline(budi_core::config::load_tags_config());

    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let mut debouncer = match new_debouncer(DEBOUNCE, move |res: DebounceEventResult| match res {
        Ok(events) => {
            for ev in events {
                let _ = tx.send(ev.path);
            }
        }
        Err(e) => tracing::warn!(
            target: "budi_daemon::tailer",
            error = %e,
            "debouncer error"
        ),
    }) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                target: "budi_daemon::tailer",
                error = %e,
                "failed to create filesystem debouncer; tailer exiting"
            );
            return;
        }
    };

    for (root, provider_name) in &routes {
        match debouncer.watcher().watch(root, RecursiveMode::Recursive) {
            Ok(()) => tracing::info!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                root = %root.display(),
                "watching"
            ),
            Err(e) => tracing::warn!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                root = %root.display(),
                error = %e,
                "failed to attach watcher; backstop poll will still cover this root"
            ),
        }
    }

    loop {
        if shutdown.load(Ordering::SeqCst) {
            tracing::info!(target: "budi_daemon::tailer", "shutdown requested");
            break;
        }

        match rx.recv_timeout(BACKSTOP_POLL) {
            Ok(path) => {
                process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &path);
                while let Ok(p) = rx.try_recv() {
                    process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &p);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                backstop_scan(&mut conn, &mut pipeline, &providers_by_name);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!(
                    target: "budi_daemon::tailer",
                    "event channel disconnected; tailer exiting"
                );
                break;
            }
        }
    }
}

/// `(watch_root, provider_name)` pairs sorted from longest-prefix to
/// shortest, so [`provider_for_path`] can resolve nested roots
/// deterministically.
type Routes = Vec<(PathBuf, String)>;

fn build_routes(providers: &[Box<dyn Provider>]) -> Routes {
    let mut routes: Routes = providers
        .iter()
        .flat_map(|p| {
            p.watch_roots()
                .into_iter()
                .map(|root| (root, p.name().to_string()))
                .collect::<Vec<_>>()
        })
        .collect();
    routes.sort_by_key(|r| std::cmp::Reverse(r.0.components().count()));
    routes
}

fn index_providers_by_name(
    providers: Vec<Box<dyn Provider>>,
) -> HashMap<String, Box<dyn Provider>> {
    providers
        .into_iter()
        .map(|p| (p.name().to_string(), p))
        .collect()
}

/// Resolve which provider owns a path event by longest matching watch root.
fn provider_for_path<'a>(path: &Path, routes: &'a Routes) -> Option<&'a str> {
    routes
        .iter()
        .find(|(root, _)| path.starts_with(root))
        .map(|(_, name)| name.as_str())
}

/// Skip the backfill on first observation: every transcript already on
/// disk gets `byte_offset = file_len`. Any later append (via `notify`) is
/// what we ingest. This is how the ticket says we keep `budi import` as
/// the only path that processes history.
fn seed_offsets(
    conn: &mut Connection,
    providers_by_name: &HashMap<String, Box<dyn Provider>>,
) -> Result<()> {
    for (name, provider) in providers_by_name {
        let files = provider
            .discover_files()
            .with_context(|| format!("discover_files failed for provider {name}"))?;
        for file in files {
            let path_str = file.path.display().to_string();
            if get_tail_offset(conn, name, &path_str)?.is_some() {
                continue;
            }
            let len = std::fs::metadata(&file.path)
                .map(|m| m.len() as usize)
                .unwrap_or(0);
            set_tail_offset(conn, name, &path_str, len)?;
            tracing::debug!(
                target: "budi_daemon::tailer",
                provider = %name,
                path = %file.path.display(),
                seeded_offset = len,
                "seeded existing transcript at end-of-file"
            );
        }
    }
    Ok(())
}

/// Periodic safety net for missed FS events. Re-discovers each provider's
/// files and runs [`process_path`] on every one — `process_path` is
/// idempotent (it short-circuits when the stored offset already matches
/// `file_len`), so this is cheap when nothing has changed.
fn backstop_scan(
    conn: &mut Connection,
    pipeline: &mut Pipeline,
    providers_by_name: &HashMap<String, Box<dyn Provider>>,
) {
    for (name, provider) in providers_by_name {
        let files = match provider.discover_files() {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(
                    target: "budi_daemon::tailer",
                    provider = %name,
                    error = %format!("{e:#}"),
                    "backstop discover_files failed"
                );
                continue;
            }
        };
        for f in files {
            let routes = vec![(f.path.clone(), name.clone())];
            process_path(conn, pipeline, providers_by_name, &routes, &f.path);
        }
    }
}

/// Process a single path event. Pure data flow:
/// `read tail → provider.parse_file → Pipeline::process →
/// ingest_messages_with_sync → set_tail_offset`. Logs at the
/// `budi_daemon::tailer` target with `provider`, `path`, `bytes_read`,
/// `messages_parsed`, `ingested` per the ticket's structured-logging
/// requirement.
fn process_path(
    conn: &mut Connection,
    pipeline: &mut Pipeline,
    providers_by_name: &HashMap<String, Box<dyn Provider>>,
    routes: &Routes,
    path: &Path,
) {
    if !is_jsonl(path) {
        return;
    }
    let Some(provider_name) = provider_for_path(path, routes) else {
        return;
    };
    let Some(provider) = providers_by_name.get(provider_name) else {
        return;
    };

    let file_len = match std::fs::metadata(path) {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };

    let path_str = path.display().to_string();
    let stored_offset = match get_tail_offset(conn, provider_name, &path_str) {
        Ok(Some(o)) => o,
        Ok(None) => 0,
        Err(e) => {
            tracing::warn!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                error = %format!("{e:#}"),
                "get_tail_offset failed"
            );
            return;
        }
    };

    if stored_offset == file_len {
        return;
    }

    let (content, parse_start_offset) = match read_tail(path, stored_offset, file_len) {
        Ok(slice) => slice,
        Err(e) => {
            tracing::warn!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                error = %format!("{e:#}"),
                "read_tail failed"
            );
            return;
        }
    };

    if content.is_empty() {
        return;
    }

    let bytes_read = content.len();
    let (mut messages, relative_offset) = match provider.parse_file(path, &content, 0) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                error = %format!("{e:#}"),
                "provider.parse_file failed"
            );
            return;
        }
    };
    let new_offset = parse_start_offset.saturating_add(relative_offset);

    if messages.is_empty() {
        if let Err(e) = set_tail_offset(conn, provider_name, &path_str, new_offset) {
            tracing::warn!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                error = %format!("{e:#}"),
                "set_tail_offset failed (no messages)"
            );
        }
        return;
    }

    let messages_parsed = messages.len();
    let tags = pipeline.process(&mut messages);

    match ingest_messages_with_sync(conn, &messages, Some(&tags), None) {
        Ok(ingested) => {
            if let Err(e) = set_tail_offset(conn, provider_name, &path_str, new_offset) {
                tracing::warn!(
                    target: "budi_daemon::tailer",
                    provider = %provider_name,
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "set_tail_offset failed after ingest"
                );
                return;
            }
            tracing::info!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                bytes_read = bytes_read,
                messages_parsed = messages_parsed,
                ingested = ingested,
                "tail batch processed"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                error = %format!("{e:#}"),
                "ingest failed; offset not advanced"
            );
        }
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false)
}

/// Read appended bytes since `stored_offset`, mirroring the truncation
/// behaviour of `analytics::sync::read_transcript_tail` so a single file
/// rotation does not desync the tailer from `budi import`.
fn read_tail(path: &Path, stored_offset: usize, file_len: usize) -> Result<(String, usize)> {
    let effective_offset = if stored_offset > file_len {
        tracing::info!(
            target: "budi_daemon::tailer",
            path = %path.display(),
            stored = stored_offset,
            len = file_len,
            "transcript shrank; resetting offset"
        );
        0
    } else {
        stored_offset
    };
    if effective_offset == file_len {
        return Ok((String::new(), effective_offset));
    }
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    file.seek(SeekFrom::Start(effective_offset as u64))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("read {}", path.display()))?;
    Ok((content, effective_offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use budi_core::jsonl::ParsedMessage;
    use budi_core::provider::DiscoveredFile;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    static UUID_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Deterministic stub provider. Lets us exercise the tailer without
    /// touching `~/.claude` / `~/.codex` / etc. Each call to
    /// `parse_file` consumes the entire `content` and returns one
    /// assistant message per non-blank line, with the line's byte offset
    /// as the new offset.
    struct StubProvider {
        name: &'static str,
        roots: Vec<PathBuf>,
        files: Mutex<Vec<PathBuf>>,
    }

    impl StubProvider {
        fn new(name: &'static str, root: PathBuf) -> Self {
            Self {
                name,
                roots: vec![root],
                files: Mutex::new(Vec::new()),
            }
        }
        fn add_file(&self, path: PathBuf) {
            self.files.lock().unwrap().push(path);
        }
    }

    impl Provider for StubProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        fn display_name(&self) -> &'static str {
            "Stub"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn discover_files(&self) -> Result<Vec<DiscoveredFile>> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .map(|path| DiscoveredFile { path })
                .collect())
        }
        fn parse_file(
            &self,
            _path: &Path,
            content: &str,
            offset: usize,
        ) -> Result<(Vec<ParsedMessage>, usize)> {
            let mut messages = Vec::new();
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let id = UUID_COUNTER.fetch_add(1, Ordering::SeqCst);
                let mut msg = ParsedMessage {
                    uuid: format!("{}-{}", self.name, id),
                    role: "assistant".to_string(),
                    timestamp: chrono::Utc::now(),
                    provider: self.name.to_string(),
                    cost_confidence: "estimated".to_string(),
                    ..Default::default()
                };
                msg.session_id = Some(format!("{}-session-{}", self.name, id));
                messages.push(msg);
            }
            Ok((messages, offset + content.len()))
        }
        fn watch_roots(&self) -> Vec<PathBuf> {
            self.roots.clone()
        }
    }

    fn open_test_db(tmp: &Path) -> (PathBuf, Connection) {
        let db_path = tmp.join("analytics.db");
        budi_core::migration::migrate(&budi_core::analytics::open_db(&db_path).unwrap()).unwrap();
        let conn = budi_core::analytics::open_db(&db_path).unwrap();
        (db_path, conn)
    }

    #[test]
    fn provider_for_path_picks_longest_prefix() {
        let routes: Routes = vec![
            (PathBuf::from("/tmp/a/b"), "specific".to_string()),
            (PathBuf::from("/tmp/a"), "general".to_string()),
        ];
        let mut sorted = routes.clone();
        sorted.sort_by_key(|r| std::cmp::Reverse(r.0.components().count()));
        assert_eq!(
            provider_for_path(Path::new("/tmp/a/b/c.jsonl"), &sorted),
            Some("specific")
        );
        assert_eq!(
            provider_for_path(Path::new("/tmp/a/x.jsonl"), &sorted),
            Some("general")
        );
        assert_eq!(provider_for_path(Path::new("/elsewhere"), &sorted), None);
    }

    #[test]
    fn is_jsonl_recognizes_extension() {
        assert!(is_jsonl(Path::new("a.jsonl")));
        assert!(is_jsonl(Path::new("a.JSONL")));
        assert!(!is_jsonl(Path::new("a.json")));
        assert!(!is_jsonl(Path::new("a.txt")));
        assert!(!is_jsonl(Path::new("a")));
    }

    #[test]
    fn seed_offsets_marks_existing_files_at_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let f1 = root.join("one.jsonl");
        let f2 = root.join("two.jsonl");
        std::fs::write(&f1, "line1\nline2\n").unwrap();
        std::fs::write(&f2, "line3\n").unwrap();

        let provider = StubProvider::new("stub", root.clone());
        provider.add_file(f1.clone());
        provider.add_file(f2.clone());

        let (db_path, _) = open_test_db(tmp.path());
        let mut conn = budi_core::analytics::open_db(&db_path).unwrap();

        let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers_by_name.insert("stub".to_string(), Box::new(provider));

        seed_offsets(&mut conn, &providers_by_name).unwrap();

        let f1_offset = get_tail_offset(&conn, "stub", &f1.display().to_string())
            .unwrap()
            .unwrap();
        let f2_offset = get_tail_offset(&conn, "stub", &f2.display().to_string())
            .unwrap()
            .unwrap();
        assert_eq!(f1_offset, std::fs::metadata(&f1).unwrap().len() as usize);
        assert_eq!(f2_offset, std::fs::metadata(&f2).unwrap().len() as usize);
    }

    #[test]
    fn process_path_advances_offset_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let f = root.join("session.jsonl");
        std::fs::write(&f, "line1\n").unwrap();

        let provider = StubProvider::new("stub", root.clone());
        provider.add_file(f.clone());

        let (db_path, _) = open_test_db(tmp.path());
        let mut conn = budi_core::analytics::open_db(&db_path).unwrap();

        let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers_by_name.insert("stub".to_string(), Box::new(provider));
        let routes = build_routes(
            &providers_by_name
                .values()
                .map(|p| {
                    let p_ref: &dyn Provider = p.as_ref();
                    Box::new(StubProvider::new(p_ref.name(), root.clone())) as Box<dyn Provider>
                })
                .collect::<Vec<_>>(),
        );
        let mut pipeline = Pipeline::default_pipeline(None);

        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        let first_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first_count, 1, "first event should ingest one message");
        let first_offset = get_tail_offset(&conn, "stub", &f.display().to_string())
            .unwrap()
            .unwrap();
        assert_eq!(first_offset, std::fs::metadata(&f).unwrap().len() as usize);

        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        let after_idempotent_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_idempotent_count, first_count,
            "re-processing the same path with no new bytes must be a no-op"
        );

        std::fs::OpenOptions::new()
            .append(true)
            .open(&f)
            .unwrap()
            .write_all(b"line2\n")
            .unwrap();
        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        let after_append_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_append_count, 2,
            "append should ingest one new message"
        );
    }

    use std::io::Write;

    /// Acceptance from #319: "Offsets persist across daemon restarts
    /// (integration test: write, kill, restart, write more, confirm no
    /// dupes and no missed messages)". We simulate the daemon process
    /// boundary by dropping every in-memory handle (pipeline, conn,
    /// providers map) between phases — the only thing carrying state
    /// across the restart is the on-disk `tail_offsets` table.
    #[test]
    fn offsets_survive_simulated_daemon_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let f = root.join("session.jsonl");
        std::fs::write(&f, "before-restart-1\nbefore-restart-2\n").unwrap();

        let (db_path, _) = open_test_db(tmp.path());

        // Phase 1 — first daemon boot. Tailer seeds existing transcripts
        // at EOF (so history stays the import command's job), then a new
        // append is delivered and ingested.
        {
            let provider = StubProvider::new("stub", root.clone());
            provider.add_file(f.clone());
            let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
            providers_by_name.insert("stub".to_string(), Box::new(provider));
            let mut conn = budi_core::analytics::open_db(&db_path).unwrap();
            seed_offsets(&mut conn, &providers_by_name).unwrap();

            let routes: Routes = vec![(root.clone(), "stub".to_string())];
            let mut pipeline = Pipeline::default_pipeline(None);

            std::fs::OpenOptions::new()
                .append(true)
                .open(&f)
                .unwrap()
                .write_all(b"after-boot-1\n")
                .unwrap();
            process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        }

        let conn = budi_core::analytics::open_db(&db_path).unwrap();
        let after_boot: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_boot, 1,
            "phase 1 should ingest exactly the post-seed append, not the seeded history"
        );
        drop(conn);

        // Append more bytes while the "daemon" is dead.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f)
            .unwrap()
            .write_all(b"between-restarts-1\nbetween-restarts-2\n")
            .unwrap();

        // Phase 2 — restart. Brand-new provider map, brand-new pipeline,
        // brand-new connection. seed_offsets must be a no-op for the
        // already-known path; process_path must pick up exactly the
        // bytes appended while we were down — no re-ingest of phase-1
        // content, no skipped lines.
        {
            let provider = StubProvider::new("stub", root.clone());
            provider.add_file(f.clone());
            let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
            providers_by_name.insert("stub".to_string(), Box::new(provider));
            let mut conn = budi_core::analytics::open_db(&db_path).unwrap();
            seed_offsets(&mut conn, &providers_by_name).unwrap();

            let routes: Routes = vec![(root.clone(), "stub".to_string())];
            let mut pipeline = Pipeline::default_pipeline(None);
            process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        }

        let conn = budi_core::analytics::open_db(&db_path).unwrap();
        let after_restart: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_restart, 3,
            "phase 2 must ingest the two bytes-while-down lines on top of phase 1's one (got {after_restart})"
        );

        // Final invariant: the persisted offset matches file_len (we
        // consumed every byte we are responsible for).
        let final_offset = get_tail_offset(&conn, "stub", &f.display().to_string())
            .unwrap()
            .unwrap();
        assert_eq!(final_offset, std::fs::metadata(&f).unwrap().len() as usize);
    }

    #[test]
    fn process_path_recovers_from_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let f = root.join("session.jsonl");
        std::fs::write(&f, "line1\nline2\n").unwrap();

        let provider = StubProvider::new("stub", root.clone());
        provider.add_file(f.clone());
        let (db_path, _) = open_test_db(tmp.path());
        let mut conn = budi_core::analytics::open_db(&db_path).unwrap();
        let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers_by_name.insert("stub".to_string(), Box::new(provider));
        let routes: Routes = vec![(root.clone(), "stub".to_string())];
        let mut pipeline = Pipeline::default_pipeline(None);

        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        assert_eq!(
            get_tail_offset(&conn, "stub", &f.display().to_string())
                .unwrap()
                .unwrap(),
            std::fs::metadata(&f).unwrap().len() as usize
        );

        std::fs::write(&f, "fresh\n").unwrap();
        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        let new_offset = get_tail_offset(&conn, "stub", &f.display().to_string())
            .unwrap()
            .unwrap();
        assert_eq!(new_offset, std::fs::metadata(&f).unwrap().len() as usize);
    }
}
