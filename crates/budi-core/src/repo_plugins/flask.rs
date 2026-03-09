use super::{ChunkKeywordSignals, RepoPlugin};

pub(crate) const PLUGIN: RepoPlugin = RepoPlugin::simple(
    "flask",
    &[],
    ChunkKeywordSignals::new(
        &["python"],
        &["flask/", "/flask/"],
        &["/wsgi.py"],
        &[
            "from flask",
            "import flask",
            "flask(__name__",
            "blueprint(",
            "@app.route(",
            "@bp.route(",
            "@blueprint.route(",
            "current_app",
            "wsgi_app(",
        ],
    ),
    &[
        "flask",
        "blueprint",
        "jinja",
        "wsgi_app",
        "current_app",
        "app.route",
    ],
);
