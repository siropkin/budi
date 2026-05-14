//! Analytics query functions: summaries, messages, repos, activity, branches,
//! tags, models, providers, cache efficiency, cost curves, and statusline stats.
//!
//! Split into four siblings along natural seams (helpers, summary/message/repo
//! breakdowns, tag/ticket/activity/model breakdowns, dimension/file/filter
//! options). Public surface is the union — every `pub` item is re-exported
//! through this module so callers see `super::queries::X` unchanged.

mod breakdowns;
mod dimensions;
mod helpers;
mod summary;

pub use breakdowns::*;
pub use dimensions::*;
pub use helpers::*;
pub use summary::*;
