use super::{ChunkKeywordSignals, RepoPlugin};

pub(crate) const PLUGIN: RepoPlugin = RepoPlugin::simple(
    "express",
    &[],
    ChunkKeywordSignals::new(
        &["javascript", "typescript"],
        &[],
        &[],
        &[
            "from 'express'",
            "from \"express\"",
            "require('express')",
            "require(\"express\")",
            "express.router(",
        ],
    ),
    &["express", "express router", "express middleware"],
);
