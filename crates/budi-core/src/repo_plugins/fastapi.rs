use super::{ChunkKeywordSignals, RepoPlugin, RepoShapeHint};

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
)
.with_repo_shape(RepoShapeHint::new(
    &[
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
        "Pipfile",
    ],
    &["fastapi"],
    &[],
));
