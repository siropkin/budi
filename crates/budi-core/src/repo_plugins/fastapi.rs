use super::{ChunkKeywordSignals, RepoPlugin};

pub(crate) const PLUGIN: RepoPlugin = RepoPlugin::simple(
    "fastapi",
    &[],
    ChunkKeywordSignals::new(
        &["python"],
        &["fastapi/", "/fastapi/"],
        &[],
        &[
            "from fastapi",
            "import fastapi",
            "fastapi(",
            "apirouter(",
            "from starlette",
        ],
    ),
    &["fastapi", "apirouter", "pydantic"],
);
