use super::{ChunkKeywordSignals, RepoPlugin};

pub(crate) const PLUGIN: RepoPlugin = RepoPlugin::simple(
    "django",
    &[],
    ChunkKeywordSignals::new(
        &["python"],
        &["django/", "/django/"],
        &["/manage.py", "/settings.py", "/urls.py"],
        &[
            "from django",
            "import django",
            "models.model",
            "urlpatterns",
            "from django.urls",
            "from django.db",
            "from django.http",
            "from django.shortcuts",
            "as_view(",
        ],
    ),
    &[
        "django",
        "urlpatterns",
        "as_view",
        "manage.py",
        "settings.py",
    ],
);
