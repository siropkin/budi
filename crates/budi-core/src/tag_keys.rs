//! Canonical tag key constants.
//!
//! Single source of truth for all tag keys emitted by enrichers and used in
//! queries, dedup logic, and dashboard code. Add new keys here rather than
//! scattering string literals across the codebase.

pub const TICKET_ID: &str = "ticket_id";
pub const TICKET_PREFIX: &str = "ticket_prefix";
/// Where the ticket id was derived from. Stable values mirror the
/// constants in `pipeline::mod` — `branch` for the alphanumeric
/// `<PREFIX>-<NUM>` pattern and `branch_numeric` for the pure-numeric
/// fallback from ADR-0082 §9. Reserved for future sources (e.g.
/// `header`, `hint`) as R3 / 9.0 work on ticket enrichment lands. See
/// R1.3 (#221).
pub const TICKET_SOURCE: &str = "ticket_source";
pub const USER: &str = "user";
pub const MACHINE: &str = "machine";
pub const PLATFORM: &str = "platform";
pub const GIT_USER: &str = "git_user";
pub const SESSION_TITLE: &str = "session_title";
pub const PROVIDER: &str = "provider";
pub const MODEL: &str = "model";
pub const SPEED: &str = "speed";
pub const COST_CONFIDENCE: &str = "cost_confidence";
pub const COMPOSER_MODE: &str = "composer_mode";
pub const PERMISSION_MODE: &str = "permission_mode";
pub const ACTIVITY: &str = "activity";
/// Classifier that emitted the activity label for a given message. Stable
/// values: `rule` (default rule-based heuristics) and, reserved for later
/// use, `header` (explicit proxy header override). See `hooks.rs` and
/// ADR-0088 §5.
pub const ACTIVITY_SOURCE: &str = "activity_source";
/// Confidence of the activity label for a given message. Stable values:
/// `high`, `medium`, `low`. See `hooks.rs`.
pub const ACTIVITY_CONFIDENCE: &str = "activity_confidence";
pub const USER_EMAIL: &str = "user_email";
pub const DURATION: &str = "duration";
pub const TOOL: &str = "tool";
pub const TOOL_USE_ID: &str = "tool_use_id";

/// Outcome of a tool call on an assistant message. Stable values (ADR-0088
/// §5, R1.5 #293):
/// - `success` — tool returned a normal result.
/// - `error` — tool returned an error result (`is_error: true` in the
///   Claude Code `tool_result` block, or equivalent on other providers).
/// - `denied` — the user rejected the proposed action (detected via a
///   small set of provider-specific sentinels in the `tool_result`
///   content).
/// - `retry` — a follow-up call to the same tool shortly after an
///   `error` outcome in the same session, attributed by a rule-based
///   heuristic rather than the `tool_result` itself.
///
/// One tag per distinct outcome observed on the assistant message. Empty
/// when the message carried no tool calls or the tool calls have no
/// corresponding `tool_result` yet (open-ended / still-in-flight).
pub const TOOL_OUTCOME: &str = "tool_outcome";
/// Where the outcome label came from. Stable values:
/// - `jsonl_tool_result` — extracted from a provider `tool_result` block.
/// - `heuristic_retry`   — attributed by the `retry` heuristic.
///
/// Emitted once per message as the dominant source of the outcomes on
/// that message; mirrors R1.2 (#222) `activity_source` / R1.4 (#292)
/// `file_path_source`.
pub const TOOL_OUTCOME_SOURCE: &str = "tool_outcome_source";
/// Confidence of the outcome labels on the message. Stable values:
/// - `high`   — outcome came from an explicit `tool_result` block.
/// - `medium` — outcome came from the rule-based retry heuristic.
///
/// Emitted once per message.
pub const TOOL_OUTCOME_CONFIDENCE: &str = "tool_outcome_confidence";

/// Repo-relative file path derived from a tool-call argument (e.g. the
/// `file_path` input of Read/Write/Edit, Cursor's `target_file`, etc.).
/// One tag per file on the assistant message. Added in R1.4 (#292).
///
/// Contract: value is always repo-relative, forward-slashed, and
/// inside the resolved repo root. Absolute paths and paths that
/// escape the repo are dropped before this tag is emitted — see
/// `crate::file_attribution` and ADR-0083.
pub const FILE_PATH: &str = "file_path";
/// Where the file path came from. Stable values:
/// - `tool_arg` — extracted directly from a known tool file argument
///   (Read/Write/Edit/NotebookEdit/Grep/Glob/Cursor equivalents).
/// - `cwd_relative` — path was absolute; stripped against the message
///   cwd / resolved repo root.
///
/// Emitted once per message as a sibling to `file_path` so the source
/// is queryable the same way R1.2 (#222) did for `activity_source`.
pub const FILE_PATH_SOURCE: &str = "file_path_source";
/// Confidence in the file-path attribution. Stable values: `high`
/// (path was already repo-relative from a known arg), `medium`
/// (normalized from an absolute path against cwd/repo). Emitted once
/// per message.
pub const FILE_PATH_CONFIDENCE: &str = "file_path_confidence";

/// Identity tags: constant for the entire session, deduplicated to one
/// assistant message per session.
pub const SESSION_IDENTITY_KEYS: &[&str] = &[
    USER,
    MACHINE,
    PLATFORM,
    GIT_USER,
    USER_EMAIL,
    COMPOSER_MODE,
    PERMISSION_MODE,
    DURATION,
    SESSION_TITLE,
];
