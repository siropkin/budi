use axum::http::header;
use axum::response::IntoResponse;

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
