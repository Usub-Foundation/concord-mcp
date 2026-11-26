//! The embedded web UI: static files compiled into the binary (no CDN, no build step).

use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::http::Gate;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const APP_CSS: &str = include_str!("../ui/app.css");
const ICONS: &str = include_str!("../ui/icons.svg");
const FAVICON: &str = include_str!("../ui/favicon.svg");
const GEIST: &[u8] = include_bytes!("../ui/fonts/Geist-Variable.woff2");
const GEIST_MONO: &[u8] = include_bytes!("../ui/fonts/GeistMono-Variable.woff2");

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; font-src 'self'; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn respond(body: impl IntoResponse, content_type: &'static str, cache: &'static str) -> Response {
    let mut r = body.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    r
}

const FRESH: &str = "no-cache";
const LONG: &str = "public, max-age=604800, immutable";

pub fn router() -> Router<Arc<Gate>> {
    Router::new()
        .route("/", get(|| async { respond(INDEX, "text/html; charset=utf-8", FRESH) }))
        .route("/ui/app.js", get(|| async { respond(APP_JS, "text/javascript; charset=utf-8", FRESH) }))
        .route("/ui/app.css", get(|| async { respond(APP_CSS, "text/css; charset=utf-8", FRESH) }))
        .route("/ui/icons.svg", get(|| async { respond(ICONS, "image/svg+xml", FRESH) }))
        .route("/ui/favicon.svg", get(|| async { respond(FAVICON, "image/svg+xml", FRESH) }))
        .route("/ui/fonts/Geist-Variable.woff2", get(|| async { respond(GEIST, "font/woff2", LONG) }))
        .route("/ui/fonts/GeistMono-Variable.woff2", get(|| async { respond(GEIST_MONO, "font/woff2", LONG) }))
}
