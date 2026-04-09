//! Canonical tag key constants.
//!
//! Single source of truth for all tag keys emitted by enrichers and used in
//! queries, dedup logic, and dashboard code. Add new keys here rather than
//! scattering string literals across the codebase.

pub const TICKET_ID: &str = "ticket_id";
pub const TICKET_PREFIX: &str = "ticket_prefix";
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
pub const USER_EMAIL: &str = "user_email";
pub const DURATION: &str = "duration";
pub const TOOL: &str = "tool";
pub const TOOL_USE_ID: &str = "tool_use_id";

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
