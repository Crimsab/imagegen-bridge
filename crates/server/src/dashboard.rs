//! Embedded, dependency-free browser dashboard assets.

use axum::{
    Router,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};

const INDEX_HTML: &str = include_str!("../dashboard/index.html");
const APP_CSS: &str = include_str!("../dashboard/app.css");
const APP_JS: &str = include_str!("../dashboard/app.js");
const API_JS: &str = include_str!("../dashboard/api.js");
const FORM_JS: &str = include_str!("../dashboard/form.js");
const ICON_SVG: &str = include_str!("../dashboard/icon.svg");

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' blob: data:; style-src 'self'; script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Returns the self-contained dashboard route graph for same-origin bridge APIs.
pub fn dashboard_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/dashboard", get(index))
        .route("/dashboard/", get(index))
        .route("/dashboard/app.css", get(stylesheet))
        .route("/dashboard/app.js", get(script))
        .route("/dashboard/api.js", get(api_script))
        .route("/dashboard/form.js", get(form_script))
        .route("/dashboard/icon.svg", get(icon))
}

async fn index() -> Response {
    asset_response(INDEX_HTML, "text/html; charset=utf-8", true)
}

async fn stylesheet() -> Response {
    asset_response(APP_CSS, "text/css; charset=utf-8", false)
}

async fn script() -> Response {
    asset_response(APP_JS, "text/javascript; charset=utf-8", false)
}

async fn api_script() -> Response {
    asset_response(API_JS, "text/javascript; charset=utf-8", false)
}

async fn form_script() -> Response {
    asset_response(FORM_JS, "text/javascript; charset=utf-8", false)
}

async fn icon() -> Response {
    asset_response(ICON_SVG, "image/svg+xml", false)
}

fn asset_response(body: &'static str, content_type: &'static str, document: bool) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    if document {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CONTENT_SECURITY_POLICY),
        );
        headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
        headers.insert(
            "permissions-policy",
            HeaderValue::from_static(
                "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
            ),
        );
    }
    (headers, body).into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn document_has_strict_headers_and_external_assets() {
        let response = index().await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            CONTENT_SECURITY_POLICY
        );
        assert!(INDEX_HTML.contains("/dashboard/app.css"));
        assert!(INDEX_HTML.contains("/dashboard/app.js"));
        assert!(INDEX_HTML.contains("/dashboard/icon.svg"));
        assert!(INDEX_HTML.contains("type=\"module\""));
        assert!(INDEX_HTML.contains("id=\"detail-message\""));
        assert!(INDEX_HTML.contains("id=\"event-table-body\""));
        assert!(APP_JS.contains("copyArtifactPath"));
        assert!(APP_JS.contains("renderOperatorEvents"));
        assert!(APP_JS.contains("Requested and effective parameters"));
        assert!(APP_JS.contains("Applied normalizations"));
        assert!(APP_JS.contains("loadPartialImage"));
        assert!(!INDEX_HTML.contains("<script>"));
        assert!(!INDEX_HTML.contains("style=\""));
    }

    #[tokio::test]
    async fn icon_is_served_as_svg() {
        let response = icon().await;
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/svg+xml");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(ICON_SVG.contains("<title id=\"title\">Imagegen Bridge</title>"));
    }

    #[test]
    fn advanced_dashboard_fixture_is_structurally_valid() -> Result<(), Box<dyn std::error::Error>>
    {
        let request: imagegen_bridge_core::ImageRequest =
            serde_json::from_str(include_str!("../dashboard/advanced-request.fixture.json"))?;
        imagegen_bridge_core::validate_request(
            &request,
            imagegen_bridge_core::RequestLimits::default(),
        )?;
        assert_eq!(request.parameters.n, 2);
        assert_eq!(request.output.directory.as_deref(), Some("tests/red"));
        assert_eq!(request.routing.fallbacks.len(), 1);
        Ok(())
    }
}
