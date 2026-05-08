//! Small filesystem helpers shared across providers.
//!
//! Background: `std::fs::read_to_string(path)` allocates a `Vec` sized to the
//! file's full length and reads in one shot — no upper bound. Provider
//! enrichment code reads a handful of small JSON / text files per session
//! (`workspace.json`, `.code-workspace`, `.git/HEAD`, Copilot CLI's
//! `workspace.yaml`, Cursor's `worker.log`). Those files are tiny "by design"
//! but the daemon ingests user-controlled paths, so a runaway agent /
//! filesystem corruption / oversized fixture turning one of them into a
//! gigabyte-class file would OOM the daemon. See #696.
//!
//! [`read_capped`] is the bounded equivalent: returns `Ok(None)` (treat as
//! not-readable, fall through to the no-enrichment path) when the file
//! exceeds the cap. The pricing manifest fetch already uses the same
//! pattern (`MAX_PAYLOAD_BYTES = 10 MB`,
//! `crates/budi-daemon/src/workers/pricing_refresh.rs`).
//!
//! For the live tailer's append slice the cap-and-resume path lives in
//! `budi_daemon::workers::tailer::read_tail` directly; that one needs to
//! advance the offset, not return `None`.

use std::io::Read;
use std::path::Path;

/// Cap (in bytes) for the small JSON / text probe files: `workspace.json`,
/// `.code-workspace`, `.git/HEAD`, Copilot CLI's `workspace.yaml`. These are
/// kilobyte-class by design — anything past 1 MB is pathological.
pub const PROBE_FILE_CAP: usize = 1024 * 1024;

/// Cap (in bytes) for Cursor's per-project `worker.log`. Worker logs grow
/// over a session's lifetime; 16 MB is the documented headroom in #696
/// (well above realistic worker.log sizes, well below an OOM).
pub const WORKER_LOG_CAP: usize = 16 * 1024 * 1024;

/// Read `path` as UTF-8, returning `Ok(None)` when:
/// - the file's length exceeds `cap` bytes (warns via `tracing` so ops can
///   spot the pathological case in `daemon.log`),
/// - the file does not exist or is not readable,
/// - the file is not valid UTF-8.
///
/// Callers in provider enrichment treat both "I/O failure" and "over cap"
/// as "no enrichment available", which is why all three failure modes
/// collapse into a single `Ok(None)` return — the call sites already take
/// the no-enrichment branch on `read_to_string(...).ok().is_none()`.
pub fn read_capped(path: &Path, cap: usize) -> std::io::Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    if len > cap {
        tracing::warn!(
            target: "budi_core::fs_util",
            path = %path.display(),
            file_len = len,
            cap = cap,
            "file exceeds read cap; skipping enrichment read"
        );
        return Ok(None);
    }
    // `len` is a hint; some files may grow between metadata and read. Cap
    // the buffer at `cap + 1` so a race that pushes the file past the cap
    // is detected by the post-read length check below.
    let mut buf = Vec::with_capacity(len.min(cap));
    let read = file.by_ref().take(cap as u64 + 1).read_to_end(&mut buf)?;
    if read > cap {
        tracing::warn!(
            target: "budi_core::fs_util",
            path = %path.display(),
            read = read,
            cap = cap,
            "file grew past read cap mid-read; skipping enrichment read"
        );
        return Ok(None);
    }
    Ok(String::from_utf8(buf).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn fresh_dir(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("budi-fs-util-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn read_capped_returns_content_under_cap() {
        let dir = fresh_dir("under");
        let p = dir.join("a.txt");
        std::fs::write(&p, b"hello").unwrap();
        let got = read_capped(&p, 1024).unwrap();
        assert_eq!(got.as_deref(), Some("hello"));
    }

    #[test]
    fn read_capped_returns_none_over_cap() {
        let dir = fresh_dir("over");
        let p = dir.join("big.txt");
        std::fs::write(&p, vec![b'x'; 2048]).unwrap();
        let got = read_capped(&p, 1024).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_capped_returns_none_for_missing_file() {
        let dir = fresh_dir("missing");
        let p = dir.join("missing.txt");
        let got = read_capped(&p, 1024).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_capped_returns_none_for_invalid_utf8() {
        let dir = fresh_dir("badutf8");
        let p = dir.join("bad.bin");
        std::fs::write(&p, [0xFFu8, 0xFE, 0xFD]).unwrap();
        let got = read_capped(&p, 1024).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_capped_handles_exactly_at_cap() {
        let dir = fresh_dir("atcap");
        let p = dir.join("at_cap.txt");
        std::fs::write(&p, vec![b'a'; 1024]).unwrap();
        let got = read_capped(&p, 1024).unwrap();
        assert_eq!(got.as_ref().map(|s| s.len()), Some(1024));
    }
}
