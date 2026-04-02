use axum::extract::Path;
use axum::http::header;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use include_dir::{Dir, include_dir};

static DASHBOARD_V2_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static/dashboard-v2-dist");

pub async fn dashboard() -> impl IntoResponse {
    let html = include_str!("../../static/dashboard.html");
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        html,
    )
}

pub async fn dashboard_css() -> impl IntoResponse {
    let css = include_str!("../../static/dashboard.css");
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        css,
    )
}

pub async fn dashboard_js() -> impl IntoResponse {
    let js = concat!(
        include_str!("../../static/js/state.js"),
        "\n",
        include_str!("../../static/js/utils.js"),
        "\n",
        include_str!("../../static/js/api.js"),
        "\n",
        include_str!("../../static/js/stats.js"),
        "\n",
        include_str!("../../static/js/views.js"),
        "\n",
        include_str!("../../static/js/views-insights.js"),
        "\n",
        include_str!("../../static/js/views-sessions.js"),
        "\n",
        include_str!("../../static/js/views-settings.js"),
        "\n",
        include_str!("../../static/js/events.js"),
    );
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        js,
    )
}

pub async fn dashboard_v2() -> impl IntoResponse {
    match DASHBOARD_V2_DIST.get_file("index.html") {
        Some(index) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            index.contents(),
        )
            .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "dashboard-v2 bundle is missing; run frontend/dashboard-v2 build",
        )
            .into_response(),
    }
}

pub async fn dashboard_v2_asset(Path(path): Path<String>) -> impl IntoResponse {
    let normalized = path.trim_start_matches('/');
    if normalized.is_empty() || normalized.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }

    match DASHBOARD_V2_DIST.get_file(normalized) {
        Some(file) => {
            let content_type = mime_guess::from_path(normalized)
                .first_or_octet_stream()
                .essence_str()
                .to_string();
            (
                [
                    (header::CONTENT_TYPE, content_type.as_str()),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                ],
                file.contents(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
