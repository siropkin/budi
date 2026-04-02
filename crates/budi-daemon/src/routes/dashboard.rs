use axum::extract::Path;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use include_dir::{Dir, include_dir};

static DASHBOARD_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static/dashboard-dist");

pub async fn dashboard() -> impl IntoResponse {
    match DASHBOARD_DIST.get_file("index.html") {
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
            "dashboard bundle is missing; run scripts/build-dashboard.sh",
        )
            .into_response(),
    }
}

pub async fn favicon() -> impl IntoResponse {
    let svg = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>&#x1f4ca;</text></svg>";
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        svg,
    )
}

pub async fn dashboard_asset(Path(path): Path<String>) -> impl IntoResponse {
    let normalized = path.trim_start_matches('/');
    if normalized.is_empty() || normalized.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }

    match DASHBOARD_DIST.get_file(normalized) {
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
