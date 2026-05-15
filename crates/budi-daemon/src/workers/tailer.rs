//! Filesystem tailer worker.
//!
//! The tailer is the live ingestion path introduced by [ADR-0089] §1 / #319.
//! It registers a recursive `notify` watcher on every directory returned by
//! [`Provider::watch_roots`] and feeds appended JSONL content through
//! [`Pipeline::default_pipeline`] into the same tables `budi db import` writes
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
//! 2. [`run`] snapshots the `agents.toml` enable/disable set at boot, then
//!    hops into a blocking thread (`notify` is fundamentally blocking and
//!    we don't want to bind a Tokio worker thread for it). The blocking
//!    entry builds a `(provider, watch_root)` map from the current
//!    [`Provider::watch_roots`] results and seeds
//!    [`tail_offsets`](budi_core::analytics::set_tail_offset) with
//!    `byte_offset = file_len` for every transcript that already exists
//!    on disk. That is the "skip the backfill, leave history to
//!    `budi db import`" property called out in the ticket Acceptance.
//!    `seed_offsets` is intentionally a one-shot boot step: a file that
//!    first appears *after* boot (under a root that materializes later;
//!    see #385 below) is treated as live content and ingested from
//!    offset 0.
//! 3. A `notify-debouncer-mini` watcher with a 500 ms debounce dispatches
//!    grown / created `*.jsonl` paths into a `std::sync::mpsc` channel; the
//!    main loop drains the channel and runs [`process_path`].
//! 4. Every 5 s the loop rebuilds routes + attaches watchers for any
//!    newly-materialized roots (#385) and calls [`backstop_scan`] to cover
//!    the well-known macOS / WSL `notify` edge cases (rotated files,
//!    mtime jitter, missed events on network volumes) as well as the
//!    "agent installed after daemon started" case.
//!
//! ## Why a separate `tail_offsets` table
//!
//! The existing `sync_state` table is keyed on `file_path` only and is
//! shared with `budi db import`. If the user runs `budi db import` and the live
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
//!   uses `Pipeline::default_pipeline` exactly as `budi db import` does.
//! - No writes that mutate or reinterpret retained legacy proxy history.
//!   After #326, 8.1-era `cost_confidence='proxy_estimated'` rows remain
//!   queryable read-only; `budi db import` carries its own overlap guard so a
//!   later historical backfill does not double-count that retained window.
//!
//! [ADR-0089]: https://github.com/siropkin/budi/blob/main/docs/adr/0089-reverse-proxy-first-jsonl-tailing-as-sole-live-path.md

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use budi_core::analytics::{self, get_tail_offset, ingest_messages_with_sync, set_tail_offset};
use budi_core::pipeline::Pipeline;
use budi_core::provider::Provider;
use notify::{RecursiveMode, Watcher};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
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

/// Per-tick byte cap for `read_tail` (see #696). The tailer reads at most
/// this many bytes from a transcript in a single tick; remaining bytes are
/// consumed on subsequent ticks. 32 MB is well above any realistic
/// single-tick append for any current provider (Claude Code / Codex /
/// Copilot Chat / Cursor / OpenCode all stream KB-class deltas) and well
/// below the OOM threshold on a modest laptop. Without this cap a runaway
/// agent / corrupted transcript / oversized fixture could grow a JSONL
/// file to multi-GB and the daemon would allocate that much RSS in a
/// single `read_to_end`.
const MAX_TAIL_BYTES: usize = 32 * 1024 * 1024;

/// Spawn the tailer in a blocking task and return immediately.
///
/// The caller (`daemon::main`) owns the `shutdown` flag — flipping it
/// causes the loop to exit at the next event or backstop tick. Dropping
/// the flag without flipping is also fine; the worker just keeps running
/// for the lifetime of the daemon process.
///
/// Provider enablement is snapshotted at boot from `agents.toml` and
/// does not hot-reload (ADR-0089 §1; the #385 reconcile loop only
/// rechecks filesystem-level availability, not config flags). If the
/// snapshot has no enabled providers at all, the worker exits
/// immediately because no later event could give it work.
pub(crate) async fn run(db_path: PathBuf, shutdown: Arc<AtomicBool>) {
    let agents_config = budi_core::config::load_agents_config();
    let providers: Vec<Box<dyn Provider>> = match &agents_config {
        Some(cfg) => budi_core::provider::all_providers()
            .into_iter()
            .filter(|p| cfg.is_agent_enabled(p.name()))
            .collect(),
        None => budi_core::provider::all_providers(),
    };
    if providers.is_empty() {
        tracing::info!(
            target: "budi_daemon::tailer",
            "no enabled providers in config snapshot; tailer exiting"
        );
        return;
    }
    let _ = tokio::task::spawn_blocking(move || run_blocking(db_path, providers, shutdown)).await;
}

/// Blocking entry point. Public for the integration test in
/// `tests/tailer_offsets.rs`, which constructs a stub provider and drives
/// the loop directly.
///
/// `providers` is the config-enabled snapshot handed in by [`run`]. Each
/// provider's `watch_roots()` / `discover_files()` implementation is
/// filesystem-sensitive, so the reconcile loop below can attach
/// watchers as agent directories materialize (`#385`). We deliberately
/// do not re-filter the set by `is_available()` — a provider whose home
/// directory appears mid-run should start producing routes on the very
/// next backstop tick without any daemon restart.
pub(crate) fn run_blocking(
    db_path: PathBuf,
    providers: Vec<Box<dyn Provider>>,
    shutdown: Arc<AtomicBool>,
) {
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

    // #385: the worker no longer exits when `watch_roots()` is empty at
    // boot. Instead we keep the loop alive and reconcile attached
    // watchers on every backstop tick, so the watcher attaches the moment
    // an agent dir materializes (e.g. user installs the agent after
    // starting the daemon, or encrypted/network home is mounted late).
    let mut routes = build_routes(&providers_by_name);
    let mut attached_roots: HashSet<PathBuf> = HashSet::new();
    attach_new_watchers(&mut debouncer, &routes, &mut attached_roots);
    if routes.is_empty() {
        tracing::debug!(
            target: "budi_daemon::tailer",
            "no watch roots available yet; will retry on every backstop tick"
        );
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
                // #385: rebuild routes + attach watchers for freshly
                // materialized roots. We deliberately do NOT re-run
                // `seed_offsets` here: seed_offsets marks discovered
                // files at EOF on the assumption that they are
                // pre-existing history (left to `budi db import` per
                // ADR-0089 §1). A file first appearing between backstop
                // ticks under a post-boot-materialized root is live
                // content, not history, and must ingest from offset 0
                // through backstop_scan / notify events.
                routes = build_routes(&providers_by_name);
                attach_new_watchers(&mut debouncer, &routes, &mut attached_roots);
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

/// Re-query every provider's `watch_roots()` and return a fresh [`Routes`].
///
/// Called once at boot and then on every backstop tick (#385). Providers
/// whose watch-root directory doesn't exist yet return an empty vector,
/// so their routes drop out until the directory materializes — at which
/// point the next tick picks them up. The result is still sorted by
/// longest-prefix so [`provider_for_path`] stays deterministic.
fn build_routes(providers_by_name: &HashMap<String, Box<dyn Provider>>) -> Routes {
    let mut routes: Routes = providers_by_name
        .iter()
        .flat_map(|(name, p)| {
            p.watch_roots()
                .into_iter()
                .map(move |root| (root, name.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    routes.sort_by_key(|r| std::cmp::Reverse(r.0.components().count()));
    routes
}

/// Attach a recursive watcher for every route we haven't attached yet.
///
/// Idempotent per-root (#385 acceptance): a root already present in
/// `attached_roots` is skipped so repeated reconcile calls don't register
/// duplicate watchers. Attach failures are logged at `debug` so we don't
/// spam the operator's log once per 5 s tick on a persistently
/// unreachable path (e.g. a network share that's currently detached);
/// the backstop scan still covers that root through `discover_files`.
fn attach_new_watchers<W: Watcher>(
    debouncer: &mut Debouncer<W>,
    routes: &Routes,
    attached_roots: &mut HashSet<PathBuf>,
) {
    for (root, provider_name) in routes {
        if attached_roots.contains(root) {
            continue;
        }
        match debouncer.watcher().watch(root, RecursiveMode::Recursive) {
            Ok(()) => {
                attached_roots.insert(root.clone());
                tracing::info!(
                    target: "budi_daemon::tailer",
                    provider = %provider_name,
                    root = %root.display(),
                    "watching"
                );
            }
            Err(e) => tracing::debug!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                root = %root.display(),
                error = %e,
                "failed to attach watcher; backstop poll still covers this root, will retry next tick"
            ),
        }
    }
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
/// what we ingest. This is how the ticket says we keep `budi db import` as
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
/// ingest_messages_with_sync (writes messages, tags, and the new
/// tail_offsets row in a single transaction)`. Logs at the
/// `budi_daemon::tailer` target with `provider`, `path`, `bytes_read`,
/// `messages_parsed`, `ingested` per the ticket's structured-logging
/// requirement.
///
/// Atomicity (#382): when there are messages to ingest, the offset
/// advance is inlined into the ingest transaction so a daemon crash or
/// power loss between persisting messages and persisting the offset
/// cannot leave the tailer pointing at a pre-batch byte position. The
/// no-message branch (line discipline only) still calls
/// [`set_tail_offset`] directly because it has no other write to atomize
/// with.
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

    match ingest_messages_with_sync(
        conn,
        &messages,
        Some(&tags),
        None,
        Some((provider_name, &path_str, new_offset)),
    ) {
        Ok(ingested) => {
            tracing::info!(
                target: "budi_daemon::tailer",
                provider = %provider_name,
                path = %path.display(),
                bytes_read = bytes_read,
                messages_parsed = messages_parsed,
                ingested = ingested,
                new_offset = new_offset,
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
/// rotation does not desync the tailer from `budi db import`.
///
/// Tolerates partial UTF-8 at the file boundary (#383): under live
/// tailing the agent may be mid-write of a multi-byte character when
/// our notify event fires. Rather than failing the whole batch with
/// `InvalidData` (which would spam `read_tail failed` warnings on
/// non-ASCII transcripts until the next tick completed the write), we
/// truncate the read to the longest valid-UTF-8, line-aligned prefix.
/// The partial character (and any trailing incomplete line) is left on
/// disk for the next event / backstop tick. This matches the
/// incomplete-final-line contract `jsonl::parse_transcript` already
/// applies at the line layer.
fn read_tail(path: &Path, stored_offset: usize, file_len: usize) -> Result<(String, usize)> {
    read_tail_capped(path, stored_offset, file_len, MAX_TAIL_BYTES)
}

/// Cap-aware implementation of [`read_tail`], parameterised on the
/// per-tick byte cap so tests can drive the truncation path without
/// having to materialise a 32 MB fixture (see #696).
///
/// When `file_len - effective_offset > cap`, we read exactly `cap` bytes,
/// drop the trailing partial line / partial UTF-8 (same contract as the
/// uncapped path), and advance the offset by the line-aligned amount we
/// actually consumed. The remaining bytes get picked up on the next
/// tick. Per-truncation `tracing::warn!` so ops can spot the pathological
/// case in `daemon.log`.
fn read_tail_capped(
    path: &Path,
    stored_offset: usize,
    file_len: usize,
    cap: usize,
) -> Result<(String, usize)> {
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
    let pending = file_len.saturating_sub(effective_offset);
    let to_read = pending.min(cap);
    if pending > cap {
        tracing::warn!(
            target: "budi_daemon::tailer",
            path = %path.display(),
            file_len = file_len,
            offset = effective_offset,
            pending = pending,
            consumed = to_read,
            cap = cap,
            "tail append exceeds per-tick cap; truncating this tick (next tick will consume the rest)"
        );
    }
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    file.seek(SeekFrom::Start(effective_offset as u64))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut bytes = Vec::with_capacity(to_read);
    file.by_ref()
        .take(to_read as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    let valid_up_to = match std::str::from_utf8(&bytes) {
        Ok(s) => s.len(),
        Err(e) => e.valid_up_to(),
    };
    let valid = &bytes[..valid_up_to];
    let consume_len = valid
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    // `consume_len <= valid_up_to`, and every byte at or before a `\n`
    // is inside the valid-UTF-8 prefix, so this conversion cannot fail.
    let content = std::str::from_utf8(&bytes[..consume_len])
        .expect("valid UTF-8 up to last newline by construction")
        .to_string();
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
        roots: Arc<Mutex<Vec<PathBuf>>>,
        files: Arc<Mutex<Vec<PathBuf>>>,
    }

    /// Shared handles the test keeps so it can mutate the stub's
    /// discovered files / watch roots while the tailer owns the
    /// `Box<dyn Provider>` (#385).
    #[derive(Clone)]
    struct StubHandles {
        roots: Arc<Mutex<Vec<PathBuf>>>,
        files: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl StubHandles {
        fn add_file(&self, path: PathBuf) {
            self.files.lock().unwrap().push(path);
        }

        fn set_root(&self, root: PathBuf) {
            let mut r = self.roots.lock().unwrap();
            r.clear();
            r.push(root);
        }
    }

    impl StubProvider {
        fn new(name: &'static str, root: PathBuf) -> Self {
            Self {
                name,
                roots: Arc::new(Mutex::new(vec![root])),
                files: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// #385: exercises the "watch root materializes after boot" path.
        fn with_no_roots(name: &'static str) -> Self {
            Self {
                name,
                roots: Arc::new(Mutex::new(Vec::new())),
                files: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn handles(&self) -> StubHandles {
            StubHandles {
                roots: Arc::clone(&self.roots),
                files: Arc::clone(&self.files),
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
            self.roots.lock().unwrap().clone()
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
        let routes = build_routes(&providers_by_name);
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

    /// Acceptance for #383: a 4-byte UTF-8 character (emoji) split
    /// across two appends must not cause a `read_tail failed` warning
    /// or lose the message. The first tick sees a partial trailing
    /// character; it must consume through the previous line boundary
    /// (zero bytes here) without error. The second tick, after the
    /// agent flushes the rest of the character and the line
    /// terminator, must ingest exactly one message with no duplicates
    /// and leave the offset at EOF.
    #[test]
    fn process_path_tolerates_partial_utf8_at_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let f = root.join("session.jsonl");
        std::fs::write(&f, "").unwrap();

        let provider = StubProvider::new("stub", root.clone());
        provider.add_file(f.clone());
        let (db_path, _) = open_test_db(tmp.path());
        let mut conn = budi_core::analytics::open_db(&db_path).unwrap();
        let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers_by_name.insert("stub".to_string(), Box::new(provider));
        let routes: Routes = vec![(root.clone(), "stub".to_string())];
        let mut pipeline = Pipeline::default_pipeline(None);

        // U+1F600 GRINNING FACE encodes to 0xF0 0x9F 0x98 0x80. Build
        // the full line bytes, then flush only the first chunk so the
        // reader sees 3 of the 4 UTF-8 bytes of the emoji and no
        // terminating newline.
        let full_line: Vec<u8> = {
            let mut v = b"prefix ".to_vec();
            v.extend_from_slice("\u{1F600}".as_bytes());
            v.extend_from_slice(b" suffix\n");
            v
        };
        let split = "prefix ".len() + 3; // last byte of emoji withheld

        {
            let mut handle = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
            handle.write_all(&full_line[..split]).unwrap();
        }

        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        let first_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            first_count, 0,
            "partial-UTF-8 + no line terminator must ingest zero messages, not error out"
        );
        let first_offset = get_tail_offset(&conn, "stub", &f.display().to_string())
            .unwrap()
            .unwrap_or(0);
        assert_eq!(
            first_offset, 0,
            "offset must not advance past the partial character"
        );

        {
            let mut handle = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
            handle.write_all(&full_line[split..]).unwrap();
        }

        process_path(&mut conn, &mut pipeline, &providers_by_name, &routes, &f);
        let second_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            second_count, 1,
            "once the rest of the emoji and newline arrive, the line ingests exactly once"
        );
        let final_offset = get_tail_offset(&conn, "stub", &f.display().to_string())
            .unwrap()
            .unwrap();
        assert_eq!(
            final_offset,
            std::fs::metadata(&f).unwrap().len() as usize,
            "offset must land at EOF after the completing write"
        );
    }

    /// Acceptance for #384: flipping the `shutdown` flag must cause
    /// `run_blocking` to return at the next backstop tick (≤ 5 s) and
    /// log `shutdown requested`. The production wiring that flips the
    /// flag lives in `daemon::main::install_shutdown_listener` — this
    /// test covers the tailer-side contract the listener depends on.
    #[test]
    fn run_blocking_exits_when_shutdown_flag_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (db_path, _) = open_test_db(tmp.path());

        let provider: Box<dyn Provider> = Box::new(StubProvider::new("stub", root.clone()));
        let providers = vec![provider];
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_clone = shutdown.clone();
        let handle = std::thread::spawn(move || run_blocking(db_path, providers, shutdown_clone));

        // Give the watcher a moment to settle before we ask it to stop.
        std::thread::sleep(Duration::from_millis(50));
        shutdown.store(true, Ordering::SeqCst);

        let started = std::time::Instant::now();
        let deadline = started + BACKSTOP_POLL + Duration::from_secs(2);
        while !handle.is_finished() {
            if std::time::Instant::now() >= deadline {
                panic!(
                    "run_blocking did not exit within {:?} of shutdown flag flip",
                    deadline - started
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        handle.join().expect("tailer thread panicked");
    }

    /// #385: `build_routes` must surface a provider's watch root as
    /// soon as `watch_roots()` starts returning it, without the
    /// provider being re-registered. Models the "agent dir materializes
    /// after boot" case (encrypted home mount, fresh-install sequence,
    /// etc.).
    #[test]
    fn build_routes_picks_up_new_roots_after_materialization() {
        let stub = StubProvider::with_no_roots("stub");
        let handles = stub.handles();
        let mut providers_by_name: HashMap<String, Box<dyn Provider>> = HashMap::new();
        providers_by_name.insert("stub".to_string(), Box::new(stub));

        assert!(
            build_routes(&providers_by_name).is_empty(),
            "no roots at boot snapshot must yield empty routes, not a panic or stale entry"
        );

        let root = PathBuf::from("/tmp/budi-385-materialized");
        handles.set_root(root.clone());

        assert_eq!(
            build_routes(&providers_by_name),
            vec![(root, "stub".to_string())],
            "second call must see the new root without any provider reconstruction"
        );
    }

    /// #385 acceptance: a root already present in `attached_roots` is
    /// not re-attached. Prevents the reconcile tick from leaking
    /// duplicate watchers on backends where `.watch()` is not itself
    /// idempotent.
    #[test]
    fn attach_new_watchers_is_idempotent_across_reconcile_ticks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        let (tx, _rx) = std::sync::mpsc::channel::<PathBuf>();
        let mut debouncer = new_debouncer(DEBOUNCE, move |res: DebounceEventResult| {
            if let Ok(events) = res {
                for ev in events {
                    let _ = tx.send(ev.path);
                }
            }
        })
        .expect("create debouncer");

        let routes: Routes = vec![(root.clone(), "stub".to_string())];
        let mut attached: HashSet<PathBuf> = HashSet::new();

        attach_new_watchers(&mut debouncer, &routes, &mut attached);
        assert_eq!(attached.len(), 1, "first reconcile must attach the root");
        assert!(attached.contains(&root));

        attach_new_watchers(&mut debouncer, &routes, &mut attached);
        assert_eq!(
            attached.len(),
            1,
            "second reconcile must be a no-op for an already-attached root"
        );
    }

    /// #385 end-to-end acceptance: a tailer started with zero
    /// materialized watch roots (fresh install — Budi running before
    /// any agent is installed) must not exit, and must attach the
    /// watcher and ingest within one backstop interval after the root
    /// appears.
    #[test]
    fn run_blocking_recovers_when_watch_root_materializes_post_boot() {
        let tmp = tempfile::tempdir().unwrap();
        let future_root = tmp.path().join("projects");
        let future_file = future_root.join("session.jsonl");
        let (db_path, _) = open_test_db(tmp.path());

        let stub = StubProvider::with_no_roots("stub");
        let handles = stub.handles();
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(stub)];
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_clone = shutdown.clone();
        let db_path_for_thread = db_path.clone();
        let thread_handle =
            std::thread::spawn(move || run_blocking(db_path_for_thread, providers, shutdown_clone));

        // Healthy idle state: no exit despite empty routes.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !thread_handle.is_finished(),
            "tailer must not exit when no watch roots are available yet"
        );

        // Materialize the agent dir and tell the stub about the file
        // that will be written to it. We deliberately do not create
        // the file yet — at the next backstop tick the watcher
        // attaches to the empty directory, and the *first* content we
        // write after that is treated as live (not pre-existing
        // history) because `seed_offsets` only runs at boot.
        std::fs::create_dir_all(&future_root).unwrap();
        handles.set_root(future_root.clone());
        handles.add_file(future_file.clone());

        // Give the reconcile tick room to run (BACKSTOP_POLL=5s) plus
        // a small buffer so the watcher is attached before we write.
        std::thread::sleep(BACKSTOP_POLL + Duration::from_millis(500));

        // Simulate the agent's first transcript write. Either the
        // notify event or the next backstop_scan must deliver this
        // through process_path with stored_offset=None→0, ingesting
        // one message.
        std::fs::write(&future_file, "line1\n").unwrap();

        // Wait up to one backstop + buffer for the event or backstop
        // fallback to land the message.
        let deadline = std::time::Instant::now() + BACKSTOP_POLL + Duration::from_secs(3);
        let mut ingested: i64 = 0;
        while std::time::Instant::now() < deadline {
            let conn = budi_core::analytics::open_db(&db_path).unwrap();
            ingested = conn
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
                .unwrap();
            if ingested > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        assert!(
            ingested >= 1,
            "tailer must ingest the transcript content after the root materializes (got {ingested})"
        );

        shutdown.store(true, Ordering::SeqCst);
        let stop_deadline = std::time::Instant::now() + BACKSTOP_POLL + Duration::from_secs(2);
        while !thread_handle.is_finished() {
            if std::time::Instant::now() >= stop_deadline {
                panic!("tailer did not exit after shutdown flag flip");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        thread_handle.join().expect("tailer thread panicked");
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

    /// #696 — when the pending tail exceeds the per-tick cap, `read_tail`
    /// reads exactly `cap` bytes (line-aligned), reports the offset it
    /// started at, and leaves the trailing bytes for the next tick.
    /// Driving `cap=24` with three 8-byte lines means tick 1 returns the
    /// first two complete lines (16 bytes consumed) and the third line
    /// stays on disk for tick 2 to pick up.
    #[test]
    fn read_tail_caps_per_tick_and_resumes() {
        let dir = tempdir();
        let path = dir.join("big.jsonl");
        // Three 8-byte lines: "AAAAAAA\n", "BBBBBBB\n", "CCCCCCC\n"
        std::fs::write(&path, b"AAAAAAA\nBBBBBBB\nCCCCCCC\n").unwrap();
        let len = std::fs::metadata(&path).unwrap().len() as usize;
        assert_eq!(len, 24);

        // Tick 1: cap=20 forces a truncated read — we get 20 raw bytes,
        // line-align down to 16 (drops the partial third line), report
        // start_offset=0 so the caller resumes at 16.
        let (content, start) = read_tail_capped(&path, 0, len, 20).unwrap();
        assert_eq!(start, 0);
        assert_eq!(content, "AAAAAAA\nBBBBBBB\n");

        // Tick 2: cap=20 again, but only 8 bytes pending, so cap is
        // moot — full remainder consumed.
        let consumed = content.len();
        let (content2, start2) = read_tail_capped(&path, consumed, len, 20).unwrap();
        assert_eq!(start2, consumed);
        assert_eq!(content2, "CCCCCCC\n");
    }

    /// Without the cap, the caller would request a buffer sized to the
    /// full pending range. With the cap, the buffer never exceeds `cap`
    /// even when the file is far larger. (Indirect check: read returns at
    /// most `cap` raw bytes.)
    #[test]
    fn read_tail_buffer_bounded_by_cap() {
        let dir = tempdir();
        let path = dir.join("huge.jsonl");
        // 100 lines of 100 bytes + newline = ~10 KB. Cap at 256 bytes.
        let mut payload = String::new();
        for i in 0..100 {
            payload.push_str(&format!(
                "{:0>99}\n",
                format!("line-{i}").chars().collect::<String>()
            ));
        }
        std::fs::write(&path, &payload).unwrap();
        let len = std::fs::metadata(&path).unwrap().len() as usize;

        let (content, start) = read_tail_capped(&path, 0, len, 256).unwrap();
        assert_eq!(start, 0);
        assert!(content.len() <= 256);
        assert!(content.ends_with('\n'));
        // And the truncation is line-aligned: every line in `content`
        // ends with `\n`.
        for line in content.lines() {
            assert!(!line.is_empty());
        }
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("budi-tailer-cap-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
