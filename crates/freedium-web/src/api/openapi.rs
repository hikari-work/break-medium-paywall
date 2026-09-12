//! `/api/v1/openapi.json` and `/api/v1/docs` — the spec and the UI that reads it.
//!
//! # `DISABLE_EXTERNAL_DOCS` hides the routes by answering, not by unregistering
//!
//! The flag is honoured by serving a `404 not-found` problem when it is set,
//! rather than by not calling `.route()` for them. Removing the routes is the
//! more faithful reading of the legacy name, and it is worse here: with the
//! routes gone, `/api/v1/docs` falls through to the page catch-all `/{*path}`,
//! which tries to resolve `api/v1/docs` as a Medium URL and answers an **HTML**
//! error from a route that has nothing to do with the API. A machine-readable
//! `404` costs nothing when the flag is on and is the honest answer for a path
//! whose existence is discoverable.
//!
//! The default is `true`, so this ships dark.
//!
//! # Where the spec lives
//!
//! In a `OnceLock` rather than in `AppState`. The spec is a pure function of the
//! compiled code — no `Config` reaches it — so a field on the state would be a
//! field every construction site has to thread through for a value that is the
//! same in every process. This is a deviation from the plan's "`Arc<OpenApi>` in
//! `AppState`", and it is one because the value turned out not to depend on the
//! state at all.
//!
//! The HTML is cached beside it for the same reason: `Scalar::to_html` serialises
//! the whole spec on every call, and building it once makes `/docs` a `memcpy`.
//!
//! # `HEAD` works without being described, and without being registered
//!
//! The plan asked for a `.head()` beside every `.get()`. It turns out axum
//! already routes `HEAD` to a `get` handler and strips the body afterwards
//! (`routing/route.rs`, `RouteFuture::poll`) — so a route registered with
//! `.get()` answers `HEAD` with the same status, the same headers and no body,
//! which is exactly what a `HEAD` is for, and an explicit `.head()` would only
//! re-register the same handler to do the same work.
//!
//! The spec therefore lists one `get` per path and `HEAD` is undocumented, which
//! is the *second* reason not to add a second annotation per handler.

use std::sync::{Arc, OnceLock};

use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::Response;
use freedium_dto::problem::{Problem, ProblemKind};
use utoipa::OpenApi;
use utoipa_scalar::Scalar;

use crate::api::http_cache::{HTML, JSON, respond};
use crate::api::problem::ApiError;
use crate::middleware::Correlation;
use crate::state::AppState;

/// The API surface. Nine handlers, seven paths.
///
/// `Problem` is in `components(schemas(...))` and `ProblemKind` is not: the
/// enum has no `Serialize` (its wire form is the `slug`/`status` pair the
/// `Problem` root carries), so it has no schema to register.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Freedium API",
        description = "Read Medium posts through Freedium.\n\n\
            Every response is anonymous: no key is required, and presenting an \
            `X-API-TOKEN` only moves the caller to a larger rate-limit allowance. \
            Requests are limited per IP and, for anything that reaches the \
            upstream, by a process-global budget.",
        version = env!("CARGO_PKG_VERSION"),
    ),
    paths(
        crate::api::posts::post,
        crate::api::posts::meta,
        crate::api::posts::html,
        crate::api::posts::markdown,
        crate::api::feed::feed,
        crate::api::resolve::resolve,
        crate::api::health::health,
    ),
    components(schemas(
        freedium_dto::PostDto,
        freedium_dto::MetaDto,
        freedium_dto::BlockDto,
        freedium_dto::InlineDto,
        freedium_dto::TagDto,
        freedium_dto::CreatorDto,
        freedium_dto::CollectionDto,
        freedium_dto::ImageDto,
        freedium_dto::QuoteStyleDto,
        freedium_dto::FeedDto,
        freedium_dto::ResolveDto,
        freedium_dto::HealthDto,
        freedium_dto::CheckDto,
        freedium_dto::HealthStatus,
        freedium_dto::Problem,
    )),
    tags(
        (name = "posts", description = "Post representations."),
        (name = "feed", description = "A page of recent posts."),
        (name = "resolve", description = "Turn a Medium URL into a post id."),
        (name = "health", description = "Liveness and dependency status."),
    )
)]
struct ApiDoc;

/// The spec and the UI built from it, once per process.
struct Docs {
    spec: Arc<utoipa::openapi::OpenApi>,
    html: String,
}

fn docs() -> &'static Docs {
    static DOCS: OnceLock<Docs> = OnceLock::new();
    DOCS.get_or_init(|| {
        let spec = Arc::new(ApiDoc::openapi());
        // `Scalar::new` takes the spec by value, so the clone is the price of
        // keeping an `Arc` for the JSON route. It happens once.
        let html = Scalar::new(spec.as_ref().clone()).to_html();
        Docs { spec, html }
    })
}

/// The `Cache-Control` both documentation routes carry.
///
/// An hour, and it is the one place in the API where a long `max-age` is safe:
/// this body changes only when the binary does, and a deploy that changes it also
/// changes the URL's ETag.
pub const DOCS_CACHE_CONTROL: &str = "public, max-age=3600";

/// `GET /api/v1/openapi.json`
#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    responses(
        (status = 200, description = "The OpenAPI document.", body = serde_json::Value),
        (status = 404, description = "`DISABLE_EXTERNAL_DOCS` is on.", body = Problem),
    ),
    tag = "health"
)]
pub async fn openapi_json(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(hidden) = hidden(&state, &uri, &correlation) {
        return hidden;
    }

    let bytes =
        serde_json::to_vec(docs().spec.as_ref()).expect("an OpenAPI document always serialises");
    respond(&headers, bytes, JSON, DOCS_CACHE_CONTROL)
}

/// `GET /api/v1/docs` — the Scalar UI.
#[utoipa::path(
    get,
    path = "/api/v1/docs",
    responses(
        (status = 200, description = "The Scalar UI, with the spec inlined.", body = String, content_type = "text/html"),
        (status = 404, description = "`DISABLE_EXTERNAL_DOCS` is on.", body = Problem),
    ),
    tag = "health"
)]
pub async fn docs_ui(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if let Some(hidden) = hidden(&state, &uri, &correlation) {
        return hidden;
    }

    respond(
        &headers,
        docs().html.clone().into_bytes(),
        HTML,
        DOCS_CACHE_CONTROL,
    )
}

/// The `404` both routes answer when the spec is switched off.
fn hidden(state: &AppState, uri: &axum::http::Uri, correlation: &Correlation) -> Option<Response> {
    state.config.disable_external_docs.then(|| {
        ApiError::new(
            ProblemKind::NotFound,
            "the API documentation is disabled on this instance".to_string(),
        )
        .resolve(uri, Some(correlation))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    /// The spec builds, and it describes the seven paths the plan promised —
    /// pinned as a list, because "the spec exists" is a much weaker claim than
    /// "the spec covers the surface".
    #[test]
    fn the_spec_covers_every_api_path() {
        let spec = serde_json::to_value(docs().spec.as_ref()).unwrap();
        let mut paths: Vec<&str> = spec["paths"]
            .as_object()
            .expect("a spec has paths")
            .keys()
            .map(String::as_str)
            .collect();
        paths.sort_unstable();

        assert_eq!(
            paths,
            [
                "/api/v1/feed",
                "/api/v1/health",
                "/api/v1/posts/{id}",
                "/api/v1/posts/{id}/html",
                "/api/v1/posts/{id}/markdown",
                "/api/v1/posts/{id}/meta",
                "/api/v1/resolve",
            ]
        );

        // Every one of them is a GET, and None of them advertises a security
        // scheme: the API is anonymous, and a spec that said otherwise would be
        // a spec that lies about the contract.
        for (path, item) in spec["paths"].as_object().unwrap() {
            assert!(item.get("get").is_some(), "{path} is not a GET");
        }
        assert!(spec.get("components").is_some(), "the schemas are missing");
    }

    /// The schemas a consumer binds against are all published. `ProblemKind` is
    /// deliberately absent — see the `ApiDoc` docs.
    #[test]
    fn every_public_shape_has_a_schema() {
        let spec = serde_json::to_value(docs().spec.as_ref()).unwrap();
        let schemas = spec["components"]["schemas"].as_object().unwrap();
        for name in [
            "PostDto",
            "MetaDto",
            "BlockDto",
            "InlineDto",
            "TagDto",
            "CreatorDto",
            "CollectionDto",
            "ImageDto",
            "QuoteStyleDto",
            "FeedDto",
            "ResolveDto",
            "HealthDto",
            "CheckDto",
            "HealthStatus",
            "Problem",
        ] {
            assert!(schemas.contains_key(name), "`{name}` has no schema");
        }
    }

    /// The UI is self-contained: the spec is inlined by `Scalar::to_html`, so
    /// there is no second request and no CDN dependency at page load beyond the
    /// bundle itself. Asserted because a `Scalar::with_url` refactor would
    /// silently change that.
    #[test]
    fn the_docs_page_has_the_spec_inlined() {
        let html = &docs().html;
        assert!(html.contains("Freedium API"), "the title is not inlined");
        assert!(
            html.contains("/api/v1/posts/{id}"),
            "the spec's paths are not inlined"
        );
    }

    /// A disabled spec is a `404` from *this* handler rather than a fall through
    /// to the page catch-all, which would answer HTML.
    ///
    /// `#[tokio::test]` for a reason that has nothing to do with the assertions:
    /// an [`AppState`](crate::state::AppState) holds a lazy `sqlx` pool, and
    /// **dropping** one requires a Tokio context (`pool/inner.rs`'s
    /// `close_event`). A plain `#[test]` here panics in `Drop`, at the end of the
    /// body, with a message about Tokio rather than about the spec.
    #[tokio::test]
    async fn a_disabled_spec_is_a_problem_and_not_a_page() {
        use crate::state::tests::{offline_state, offline_state_with, test_config};

        let uri = axum::http::Uri::from_static("/api/v1/docs");
        let correlation = Correlation::new("a-b-c".to_string(), 1, "/raw".to_string());

        // `DISABLE_EXTERNAL_DOCS` defaults to true, so the shipped config hides
        // the routes and `test_config` says so — asserted rather than assumed,
        // because the two arms together are what make each one mean something.
        let off = offline_state();
        assert!(off.config.disable_external_docs);

        let mut config = test_config();
        config.disable_external_docs = false;
        assert!(
            hidden(&offline_state_with(&config), &uri, &correlation).is_none(),
            "the switch has to work in both directions"
        );

        let response = hidden(&off, &uri, &correlation).expect("the flag hides the spec");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some(crate::api::problem::PROBLEM_CONTENT_TYPE)
        );
    }

    /// `respond` needs a body to hash, and the spec's bytes are the same on every
    /// call: the ETag is a property of the binary, so a caller that revalidates
    /// gets a `304` for the whole document.
    #[tokio::test]
    async fn the_spec_has_a_stable_validator() {
        let bytes = serde_json::to_vec(docs().spec.as_ref()).unwrap();
        let first = respond(&HeaderMap::new(), bytes.clone(), JSON, DOCS_CACHE_CONTROL);
        let tag = first.headers().get("etag").unwrap().clone();

        let mut headers = HeaderMap::new();
        headers.insert("if-none-match", tag);
        let second = respond(&headers, bytes, JSON, DOCS_CACHE_CONTROL);
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(second.into_body(), 1024).await.unwrap().is_empty());
    }
}
