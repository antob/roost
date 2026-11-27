//! Serves the embedded PWA (`web/dist`), falling back to index.html for client-side routes.

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist"]
#[allow_missing = true]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (name, file) = match Assets::get(path).filter(|_| !path.is_empty()) {
        Some(f) => (path, f),
        None if path
            .rsplit('/')
            .next()
            .is_some_and(|last| last.contains('.')) =>
        {
            return StatusCode::NOT_FOUND.into_response();
        }
        None => match Assets::get("index.html") {
            Some(f) => ("index.html", f),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "the PWA is not built: run `bun run build` in web/",
                )
                    .into_response();
            }
        },
    };
    // The shell and service worker must revalidate so updates land; hashed assets can be cached.
    let cache = if is_hashed(name) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (CONTENT_TYPE, file.metadata.mimetype().to_string()),
            (CACHE_CONTROL, cache.to_string()),
        ],
        file.data,
    )
        .into_response()
}

/// Bun names bundled assets `name-xxxxxxxx.ext`; only those are safe to cache forever.
fn is_hashed(name: &str) -> bool {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.rsplit_once('-')
        .is_some_and(|(_, h)| h.len() == 8 && h.bytes().all(|b| b.is_ascii_alphanumeric()))
}
