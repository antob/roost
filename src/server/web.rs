//! Serves the embedded PWA (`web/dist`), falling back to index.html for client-side routes.

use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

use crate::server::AppState;

#[derive(Embed)]
#[folder = "web/dist"]
#[allow_missing = true]
struct Assets;

pub async fn serve(State(st): State<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let instance = st.config.instance.as_deref();
    // The service worker shows notifications with /icon-192.png, so an instance swaps the files themselves.
    let path = match (instance, path) {
        (Some(_), "icon-192.png") => "icon-instance-192.png",
        (Some(_), "icon-512.png") => "icon-instance-512.png",
        _ => path,
    };
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
    let data = match instance {
        Some(i) if name == "index.html" => brand_html(&String::from_utf8_lossy(&file.data), i)
            .into_bytes()
            .into(),
        Some(i) if name.ends_with(".webmanifest") => brand_manifest(&file.data, i).into(),
        _ => file.data,
    };
    (
        [
            (CONTENT_TYPE, file.metadata.mimetype().to_string()),
            (CACHE_CONTROL, cache.to_string()),
        ],
        data,
    )
        .into_response()
}

/// Renames the app, points the icons at the orange set, and tells the page which instance it is.
fn brand_html(html: &str, instance: &str) -> String {
    let name = format!("roost {instance}");
    let mut out = String::with_capacity(html.len() + 64);
    for line in html.lines() {
        let l = line.trim_start();
        if l.starts_with("<meta name=\"apple-mobile-web-app-title\"") {
            out.push_str(&format!(
                "<meta name=\"apple-mobile-web-app-title\" content=\"{name}\" />\n\
                 <meta name=\"roost-instance\" content=\"{instance}\" />"
            ));
        } else if l.starts_with("<link rel=\"apple-touch-icon\"") {
            out.push_str(
                "<link rel=\"apple-touch-icon\" href=\"/apple-touch-icon-instance.png\" />",
            );
        } else if l.starts_with("<link rel=\"icon\"") {
            out.push_str(
                "<link rel=\"icon\" href=\"/icon-instance.svg\" type=\"image/svg+xml\" />",
            );
        } else {
            out.push_str(&line.replace("<title>roost</title>", &format!("<title>{name}</title>")));
        }
        out.push('\n');
    }
    out
}

fn brand_manifest(data: &[u8], instance: &str) -> Vec<u8> {
    let Ok(mut m) = serde_json::from_slice::<serde_json::Value>(data) else {
        return data.to_vec();
    };
    let name = format!("roost {instance}");
    m["name"] = name.clone().into();
    m["short_name"] = name.into();
    serde_json::to_vec(&m).unwrap_or_else(|_| data.to_vec())
}

/// Bun names bundled assets `name-xxxxxxxx.ext`; only those are safe to cache forever.
fn is_hashed(name: &str) -> bool {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.rsplit_once('-')
        .is_some_and(|(_, h)| h.len() == 8 && h.bytes().all(|b| b.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_renames_the_app_and_swaps_its_icons() {
        let html = include_str!("../../web/src/index.html");
        let out = brand_html(html, "dev");
        assert!(out.contains(r#"<meta name="apple-mobile-web-app-title" content="roost dev" />"#));
        assert!(out.contains(r#"<meta name="roost-instance" content="dev" />"#));
        assert!(out.contains(r#"href="/apple-touch-icon-instance.png""#));
        assert!(out.contains(r#"href="/icon-instance.svg""#));
        assert!(out.contains("<title>roost dev</title>"));
        assert!(out.contains(r#"<link rel="manifest""#));

        let manifest = include_bytes!("../../web/src/manifest.webmanifest");
        let m: serde_json::Value =
            serde_json::from_slice(&brand_manifest(manifest, "dev")).unwrap();
        assert_eq!(m["name"], "roost dev");
        assert_eq!(m["short_name"], "roost dev");
        assert_eq!(m["icons"][0]["src"], "/icon-192.png");
    }
}
