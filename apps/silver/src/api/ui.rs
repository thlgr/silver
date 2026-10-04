//! The embedded web UI: a known asset is served as is, any other GET gets `index.html` so client
//! routes work. Debug builds read `apps/web/dist` from disk, so a UI rebuild needs no recompile.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../web/dist"]
#[allow_missing = true]
struct Assets;

/// Content-Security-Policy for UI responses: same-origin everything, inline styles only
/// because Svelte sets element styles.
pub const CSP: &str =
    "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'";

/// True when a request path belongs to the UI rather than the HTTP API.
pub fn is_ui_path(path: &str) -> bool {
    path != "/health" && path != "/v1" && path != "/agent" && !path.starts_with("/v1/")
}

pub async fn serve(uri: Uri) -> Response {
    if !is_ui_path(uri.path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let (path, file) = match Assets::get(path) {
        Some(file) => (path, file),
        None if path.starts_with("assets/") => return StatusCode::NOT_FOUND.into_response(),
        None => match Assets::get("index.html") {
            Some(file) => ("index.html", file),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "web UI not built: run `npm install && npm run build` in apps/web",
                )
                    .into_response()
            }
        },
    };
    // Vite fingerprints everything under assets/, so those never change under one name.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, file.metadata.mimetype().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        file.data,
    )
        .into_response()
}
