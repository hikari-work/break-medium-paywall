//! The public API: `/api/v1`, its router, and the two helpers every handler
//! needs to read a request.
//!
//! # Two routers, and the split is the point
//!
//! [`router`] returns a router with a **governed** half and an **ungoverned**
//! half. The governed one carries [`limit::guard`]; the ungoverned one carries
//! `/health`, `/openapi.json` and `/docs`. The split is structural rather than an
//! `if` inside the middleware, because a monitor polling every ten seconds must
//! not be able to collect a `429`, and the spec is a static document whose
//! availability has nothing to do with how busy the API is.
//!
//! # Why the fallbacks are here and not on the outer router
//!
//! `/api/v1/does-not-exist` has to answer a `problem+json` `404` rather than the
//! page catch-all's HTML — a client that asked for JSON should not have to parse
//! a page to learn it asked for the wrong URL. Registering the fallback on this
//! router rather than the application's is what keeps the HTML fallback for
//! everything else.
//!
//! # `OriginalUri`, and why nothing here reads `Uri`
//!
//! [`crate::router`] mounts this router with `.nest("/api/v1", ...)`, and `nest`
//! strips the prefix from the URI an inner router sees (it puts the unstripped
//! one in the `OriginalUri` extension). So a handler inside here that read the
//! request's own `Uri` would put `/posts/{id}` — without the `/api/v1` — into a
//! problem's `instance`, and would find no query parameters at all for `/feed`
//! and `/resolve`, because those are not part of `Uri::path`. Every handler takes
//! the `OriginalUri` extractor, and [`limit::guard`] reads the same extension for
//! the same reason.

pub mod feed;
pub mod health;
pub mod http_cache;
pub mod limit;
pub mod openapi;
pub mod posts;
pub mod problem;
pub mod resolve;

use axum::extract::OriginalUri;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use freedium_dto::problem::ProblemKind;

use crate::middleware::{self, Correlation};
use crate::state::AppState;

/// The API's router, without state applied.
///
/// `Router<AppState>` rather than `Router`: [`crate::router`] mounts it with
/// `.nest()`, and `nest` requires a router that has not had its state applied
/// yet. The page router is the other way round — it is applied and mounted as a
/// `fallback_service` — and that asymmetry is the reason this signature is worth
/// a comment.
pub fn router(state: AppState) -> Router<AppState> {
    // Charged, identified, and — on a rejection — answered with a 429 here.
    let governed = Router::new()
        .route("/posts/{id}", get(posts::post))
        .route("/posts/{id}/meta", get(posts::meta))
        .route("/posts/{id}/html", get(posts::html))
        .route("/posts/{id}/markdown", get(posts::markdown))
        .route("/feed", get(feed::feed))
        .route("/resolve", get(resolve::resolve))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            limit::guard,
        ));

    Router::new()
        .route("/health", get(health::health))
        .route("/openapi.json", get(openapi::openapi_json))
        .route("/docs", get(openapi::docs_ui))
        .merge(governed)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // Inside `middleware::catch_panics` (which renders HTML, and which wraps
        // the whole application) so an API panic is caught here first. Tower
        // layers are innermost-first, so the outer one never sees it and the page
        // routes are unaffected. A panic inside `limit::guard` itself is caught
        // by the outer HTML handler instead — `governor` has no panicking path,
        // and this is the residue that is recorded rather than defended.
        .layer(axum::middleware::from_fn_with_state(
            state,
            middleware::catch_panics_json,
        ))
}

/// The `instance`-and-`request_id` pair every problem from this module needs.
///
/// A thin wrapper over [`ApiError::resolve`] so the fallbacks below read as one
/// line each; it exists because `Response` is not a `Result` and building a
/// problem by hand would otherwise be four lines of builder in two places.
fn problem(
    uri: &Uri,
    correlation: Option<&Correlation>,
    kind: ProblemKind,
    detail: &str,
) -> Response {
    problem::ApiError::new(kind, detail).resolve(uri, correlation)
}

/// No such API route.
async fn not_found(
    OriginalUri(uri): OriginalUri,
    Extension(correlation): Extension<Correlation>,
) -> Response {
    problem(
        &uri,
        Some(&correlation),
        ProblemKind::NotFound,
        "no such API endpoint",
    )
}

/// The route exists, the method does not.
///
/// Its own fallback rather than a `405` from the framework, because axum's
/// default is an empty body with an `Allow` header — and a client that got a bare
/// `405` from an endpoint it thought it knew has no way to tell a typo'd method
/// from a route that moved.
async fn method_not_allowed(
    OriginalUri(uri): OriginalUri,
    Extension(correlation): Extension<Correlation>,
) -> Response {
    problem(
        &uri,
        Some(&correlation),
        ProblemKind::MethodNotAllowed,
        "this endpoint does not accept that method",
    )
}

/// The request's URI as the client wrote it.
///
/// `OriginalUri` is set by `.nest()` and holds the full path including
/// `/api/v1`; the request's own `Uri` holds the path with the prefix stripped.
/// Prefer the extractor — this exists for a caller that has a `Request` rather
/// than an extractor, which is what a middleware is.
#[must_use]
pub fn request_uri(request: &axum::extract::Request) -> Uri {
    request
        .extensions()
        .get::<OriginalUri>()
        .map(|original| original.0.clone())
        .unwrap_or_else(|| request.uri().clone())
}

/// One query parameter, percent-decoded.
///
/// `form_urlencoded::parse` rather than a hand-rolled split on `&` and `=`: the
/// former decodes `%20` and `+`, keeps the *first* occurrence of a repeated key
/// (this returns the first, and it does so by construction rather than by
/// accident), and cannot panic on a malformed body. A hand-rolled version would
/// get the decoding wrong on the first URL that needed it — `/resolve?url=` is
/// a query full of `:` and `/`, which is exactly the case that invites the
/// shortcut.
///
/// An absent parameter and an empty one are `None` and `Some("")` respectively,
/// which is the distinction `/resolve` needs: a missing `url` should read as an
/// invalid URL, not as a different kind of error.
#[must_use]
pub fn query_param(uri: &Uri, name: &str) -> Option<String> {
    form_urlencoded::parse(uri.query()?.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    fn uri(value: &str) -> Uri {
        value.parse().unwrap()
    }

    /// The API mounted the way [`crate::router`] mounts it.
    ///
    /// Nesting matters here and not in the other modules' tests: the whole reason
    /// `OriginalUri` is used rather than the request's own `Uri` is that `.nest()`
    /// rewrites the latter, so a test that called [`router`] directly would be
    /// testing the one arrangement in which the distinction does not exist.
    fn api_app(state: AppState) -> Router {
        Router::new()
            .nest("/api/v1", router(state.clone()))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                middleware::correlation,
            ))
            .with_state(state)
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    /// The decoding, which is the whole reason this is not a `split('&')`.
    #[test]
    fn a_query_parameter_is_read_and_decoded() {
        assert_eq!(
            query_param(
                &uri("/api/v1/resolve?url=https%3A%2F%2Fmedium.com%2Fp%2Fabc"),
                "url"
            ),
            Some("https://medium.com/p/abc".to_string())
        );
        // A `+` is a space, and a bare `:` or `/` passes through.
        assert_eq!(query_param(&uri("/x?a=b+c"), "a"), Some("b c".to_string()));
        assert_eq!(
            query_param(&uri("/x?a=https://medium.com/p/abc"), "a"),
            Some("https://medium.com/p/abc".to_string())
        );

        // Absent, empty and repeated.
        assert_eq!(query_param(&uri("/x"), "a"), None);
        assert_eq!(query_param(&uri("/x?a="), "a"), Some(String::new()));
        assert_eq!(
            query_param(&uri("/x?a=first&a=second"), "a"),
            Some("first".to_string()),
            "the first occurrence is the one that counts"
        );

        // A parameter whose *value* contains the other's name is not a match.
        assert_eq!(
            query_param(&uri("/x?cursor=a&limit=7"), "limit"),
            Some("7".to_string())
        );
    }

    /// **The 404 and the 405 are JSON.**
    ///
    /// Without the fallbacks on this router, `/api/v1/does-not-exist` would reach
    /// the application's page catch-all, which would try to resolve
    /// `api/v1/does-not-exist` as a Medium URL and answer an HTML error from a
    /// route that has nothing to do with the API.
    #[tokio::test]
    async fn an_unknown_api_path_is_a_problem() {
        let state = crate::state::tests::offline_state();
        let app = api_app(state);

        let response = app
            .clone()
            .oneshot(get("/api/v1/does-not-exist"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some(problem::PROBLEM_CONTENT_TYPE)
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/feed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some(problem::PROBLEM_CONTENT_TYPE)
        );
    }

    /// The `instance` in a problem is the path the *client* used.
    ///
    /// This is the one assertion that cannot be made without the nest, and it is
    /// the reason every handler takes `OriginalUri`: get this wrong and the body
    /// names `/does-not-exist`, a path that does not exist on this server.
    #[tokio::test]
    async fn a_problem_names_the_path_the_client_called() {
        let state = crate::state::tests::offline_state();
        let response = api_app(state)
            .oneshot(get("/api/v1/does-not-exist?x=1"))
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["instance"], "/api/v1/does-not-exist");
    }

    /// The two ungoverned routes answer without ever reaching the limiter, and
    /// the point of the test is that this is *structural*: a monitor must not be
    /// able to collect a `429` from `/health`.
    #[tokio::test]
    async fn health_and_the_spec_are_outside_the_governor() {
        let mut config = crate::state::tests::test_config();
        // One cell per minute, and it is spent by the *first* call through the
        // guard: if the ungoverned routes went through it, the second would 429
        // whatever it answered.
        config.api_rate_limit_burst = 1;
        config.api_rate_limit_per_minute = 1;
        config.disable_external_docs = false;
        let app = api_app(crate::state::tests::offline_state_with(&config));

        for path in ["/api/v1/health", "/api/v1/openapi.json", "/api/v1/docs"] {
            for attempt in 1..=3 {
                let response = app.clone().oneshot(get(path)).await.unwrap();
                assert_ne!(
                    response.status(),
                    StatusCode::TOO_MANY_REQUESTS,
                    "{path} was rate limited on attempt {attempt}"
                );
                assert!(
                    response.headers().get("x-ratelimit-limit").is_none(),
                    "{path} carries limiter headers, so it is behind the guard"
                );
            }
        }
    }

    /// A governed route *is* limited, which is the other half of the test above:
    /// without it, both could pass with the guard applied to nothing at all.
    #[tokio::test]
    async fn a_governed_route_is_behind_the_guard() {
        let mut config = crate::state::tests::test_config();
        config.api_rate_limit_burst = 1;
        config.api_rate_limit_per_minute = 1;
        let app = api_app(crate::state::tests::offline_state_with(&config));

        let first = app
            .clone()
            .oneshot(get("/api/v1/posts/0291df856c77/meta"))
            .await
            .unwrap();
        assert_eq!(
            first
                .headers()
                .get("x-ratelimit-remaining")
                .map(|value| value.to_str().unwrap()),
            Some("0"),
            "the guard charged the request bucket"
        );

        let second = app
            .oneshot(get("/api/v1/posts/0291df856c77/meta"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            second
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some(problem::PROBLEM_CONTENT_TYPE)
        );

        // And the `instance` still names the path the client called, from inside
        // the guard rather than from a handler.
        let body = axum::body::to_bytes(second.into_body(), 64 * 1024)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["instance"], "/api/v1/posts/0291df856c77/meta");
    }

    /// `request_uri` prefers the original, which is the one `.nest()` set.
    #[test]
    fn the_original_uri_wins_over_the_nested_one() {
        let mut request = Request::builder()
            .uri("/posts/abc")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_uri(&request).path(), "/posts/abc");

        request
            .extensions_mut()
            .insert(OriginalUri("/api/v1/posts/abc?x=1".parse().unwrap()));
        assert_eq!(request_uri(&request).path(), "/api/v1/posts/abc");
        assert_eq!(request_uri(&request).query(), Some("x=1"));
    }
}
