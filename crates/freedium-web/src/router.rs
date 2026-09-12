//! The router, and the layer order that is not arbitrary.
//!
//! # The order mirrors Starlette's, and Starlette's is inverted from the source
//!
//! `main.py:70-71` registers `LoggerMiddleware` and then `CORSMiddleware`, and
//! `app.add_middleware` **inserts at the front** of the list. So the running order
//! is the reverse of the reading order:
//!
//! ```text
//! CORSMiddleware      ← registered second, outermost
//!   LoggerMiddleware
//!     routes
//! ```
//!
//! Two consequences that are observable, and are why the order here is written
//! out rather than left to look natural:
//!
//! - CORS headers are added to the page `LoggerMiddleware` renders on a timeout,
//!   because CORS is *outside* it. Put the other way round, a cross-origin client
//!   would get an error page its browser refuses to read.
//! - `X-Request-ID` and `X-Process-Time` are set inside CORS, so they are present
//!   on every response including the ones CORS amends.
//!
//! axum applies layers in the opposite direction to `add_middleware`: the **last**
//! `.layer()` call is the outermost. So the calls below are in the reverse of the
//! order they run, which is why [`cors`] is last.
//!
//! # Where `catch_panics` sits, and why it is inside `correlation`
//!
//! It reads the correlation out of the request extensions, which
//! [`crate::middleware::correlation`] put there — so it must be closer to the
//! handler. Inside it, a panic is caught while the correlation is still in scope
//! and the error page carries a code an operator can search for. Outside it,
//! there would be no code and the page would have to be generic.
//!
//! # The two `ServeDir`s
//!
//! Production never reaches this: Caddy answers `/favicon.ico` and the rest with
//! explicit `handle_path` blocks (`caddy/Caddyfile`) and forwards nothing else to
//! the app. The `ServeDir` below exists so that `cargo run` serves the same pages
//! *with* their images and icons, which is what makes the server testable without
//! the whole compose stack — §5's reason for adding it.
//!
//! It is a **fallback** rather than a set of routes, so a static file wins over
//! the catch-all and everything else falls through to it. `tower_http` returns
//! 405 rather than calling the fallback for a non-`GET`/`HEAD` request
//! (`call_fallback_on_method_not_allowed` defaults to false) — which is what the
//! legacy does too, since its one page route is registered `methods=["GET",
//! "HEAD"]`.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::Method;
use axum::routing::{get, post};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;

use crate::api;
use crate::config::Config;
use crate::handlers::{main, misc};
use crate::middleware;
use crate::state::AppState;

/// The two page handlers, as one `MethodRouter`.
///
/// # The `.head()`s are redundant, and are kept
///
/// This comment used to say that `get` does not imply `head` in axum. That is
/// false as of 0.8: `MethodRouter::get` puts `GET,HEAD` in its `Allow` header and
/// a `HEAD` is routed to the `get` handler, whose body is then emptied
/// (`routing/route.rs`, `RouteFuture::poll`). The explicit `.head()` therefore
/// changes nothing at all — the same `main::dispatch` runs either way.
///
/// They are kept because the legacy registers both (`handlers/main.py:71`) and
/// because removing them is a change to the page routes with no observable
/// benefit; the API's routes, which are new, rely on axum's behaviour instead —
/// see `api::openapi`'s docs for why that is the better side of the trade.
///
/// `/` is registered separately from `/{*path}` because matchit does not
/// guarantee that a catch-all matches the empty path. Both point at
/// [`main::dispatch`], which is what the legacy does — its single `/{path:path}`
/// route receives an empty string for `/` and branches on it (`handlers/main.py:18`).
fn pages(state: AppState) -> Router {
    Router::new()
        .route("/", get(main::dispatch).head(main::dispatch))
        .route("/{*path}", get(main::dispatch).head(main::dispatch))
        .with_state(state)
}

/// Builds the application.
///
/// The state is provided twice, on purpose: once to [`pages`], which becomes a
/// fully-stated service for `ServeDir` to fall back to, and once at the end for
/// the two POST routes. `AppState` is `Clone` over `Arc`s and pooled handles, so
/// the clone is a handful of refcount bumps.
pub fn router(state: AppState) -> Router {
    let static_dir = state.config.static_dir.clone();
    let pages = pages(state.clone());
    // `Router<AppState>` and **not** a stated router: `.nest()` requires a router
    // whose state has not been applied. `crate::api::router` is written for this
    // and `pages` is not, which is the whole of the difference between the two.
    let api = api::router(state.clone());
    let config = state.config.clone();

    Router::new()
        .route("/delete-from-cache", post(misc::delete_from_cache))
        .route("/report-problem", post(misc::report_problem))
        // **Before the fallback and before every `.layer()` below, and both
        // halves of that matter.** `Router::layer` wraps the routes *and the
        // fallback* that exist at the moment it is called, so anything added
        // afterwards is outside all of them: a `.nest()` below these lines would
        // be an API with no correlation, no CORS and no compression. And the
        // fallback has to come after the nest for the same family of reasons —
        // `ServeDir`'s fallback is the page catch-all, which would otherwise be
        // reachable for a mistyped `/api/v1` path. (matchit prefers a static
        // segment over a wildcard, so the two do not actually overlap today; the
        // ordering is what makes that a property rather than a coincidence.)
        .nest("/api/v1", api)
        // **The fallback is registered before the layers, and that is not
        // cosmetic.** `Router::layer` wraps the routes *and the fallback* that
        // exist at the moment it is called; anything added afterwards is outside
        // every one of them. With this line below the `.layer()`s, `/` and
        // `/{*path}` — which is every page this server serves — reached the
        // handler with no correlation, no CORS and no compression, and only the
        // two POST routes above were layered at all.
        .fallback_service(ServeDir::new(static_dir).fallback(pages))
        // The forms are two short strings. axum's default cap is 2 MB, which is
        // inherited from `DefaultBodyLimit` and is far more than either route can
        // use; 16 KB is generous for a description and a URL, and keeps an
        // unauthenticated POST from being a way to make the process allocate.
        //
        // **It stays here.** Every `/api/v1` route is a GET with no body
        // extractor — `/resolve?url=` is a query string, which this limit does
        // not see — so moving it inside the nest would change nothing for the
        // API and would stop covering the two admin POSTs, which are the routes
        // that need it.
        .layer(DefaultBodyLimit::max(16 * 1024))
        // Innermost of the layers: catches a panicking handler while the
        // correlation is still reachable. See the module docs.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::catch_panics,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::correlation,
        ))
        .layer(cors(&config))
        // Outermost, because production gzips at the edge and this is the
        // standalone equivalent: `caddy/Caddyfile`'s `common` snippet ends with
        // `encode gzip`, so a response from a deployed Freedium is compressed and
        // one from this binary should be too.
        .layer(CompressionLayer::new())
        .with_state(state)
}

/// `CORS_ALLOW_ORIGINS = ["*"]` with `CORS_ALLOW_CREDENTIALS = True`
/// (`middlewares/__init__.py:12-15`), `CORS_ALLOW_METHODS`/`_HEADERS` both `["*"]`.
///
/// # `mirror_request` is the faithful translation, and `permissive()` is not
///
/// The two settings look contradictory — a wildcard origin and credentials —
/// and Starlette resolves them by **echoing the request's origin** instead of
/// sending `*`, because `Access-Control-Allow-Origin: *` is illegal alongside
/// `Access-Control-Allow-Credentials: true` and every browser rejects the pair.
///
/// `CorsLayer::permissive()` sends the literal `*`, which would break every
/// credentialed cross-origin request that works today. `mirror_request` echoes,
/// which is what Starlette does.
///
/// # The two `Any`s this used to pass were wrong twice over
///
/// `allow_methods(Any)` and `allow_headers(Any)` send `*`, and this said that
/// was "legal with credentials and what the config asks for". All three parts of
/// that were mistaken, and `tower-http` refuses the pair outright:
/// `ensure_usable_cors_rules` **panics at construction** on `*` methods or
/// headers with credentials, so the router could not be built at all.
///
/// It is also not what Starlette does. Reading `middleware/cors.py`:
///
/// - `"*" in allow_headers` sets `allow_all_headers`, and a wildcard list is
///   **never** sent: the preflight mirrors back the request's own
///   `Access-Control-Request-Headers` (`cors.py:125-126`), and a plain response
///   carries no such header at all. Hence `AllowHeaders::mirror_request()`.
/// - `"*" in allow_methods` is expanded to `ALL_METHODS`
///   (`cors.py:27-28`, the tuple on `cors.py:11`), which is then **joined and
///   sent literally** — `DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT`, not
///   `*`. Hence the explicit list.
///
/// # One divergence, and it is not reachable from a browser
///
/// For a non-preflight request Starlette builds `Access-Control-Allow-Origin:
/// *` from `simple_headers` and then **overwrites it with the origin only when
/// the request carries a `Cookie`** (`cors.py:159-160`). So a cookieless
/// cross-origin fetch gets the literal `*` — alongside `Allow-Credentials:
/// true`, which is exactly the pair browsers reject. `mirror_request` echoes
/// unconditionally, which sends a usable response in that case instead.
///
/// `Vary: Origin` follows the same split: Starlette adds it only when it echoes
/// (`cors.py:172`), `tower-http` always does. With every origin allowed it is a
/// cache hint rather than a poisoning risk, and CORS headers are not part of
/// the post-page parity gate (decision 1).
///
/// # `CORS_ALLOW_ORIGINS` closes the API and leaves the pages alone
///
/// The config is the whole of the difference, and an **empty** list — the
/// default — is exactly the behaviour above, so a deployment that never sets it
/// gets no change at all.
///
/// When it is set, the predicate splits on the path: a request under `/api/`
/// must come from a listed origin, and everything else keeps the mirror. Two
/// reasons for the path split rather than a second `CorsLayer`:
///
/// - **Only one `CorsLayer` may exist.** Two would each add
///   `Access-Control-Allow-Origin`, and a duplicated header is one browsers
///   reject.
/// - The pages are embedded in other people's sites and are read by browsers
///   directly; locking them down is a different decision with a different blast
///   radius, and it is not this one. What the allowlist is for is the API — a
///   JSON contract a third party's front-end calls — where an allowlist is the
///   difference between "any site can drive our rate limits from its visitors'
///   browsers" and "only the sites we named can".
///
/// `parts.uri.path()` is the full path, because this layer wraps the nest rather
/// than sitting inside it. The comparison is exact and case-sensitive, which is
/// what an origin is: a browser sends the scheme, host and port, lowercased for
/// the scheme and host, and never a trailing slash.
fn cors(config: &Config) -> CorsLayer {
    /// `ALL_METHODS` (`starlette/middleware/cors.py:11`), in its own order — the
    /// header is a joined list, so the order is observable.
    const ALL_METHODS: [Method; 7] = [
        Method::DELETE,
        Method::GET,
        Method::HEAD,
        Method::OPTIONS,
        Method::PATCH,
        Method::POST,
        Method::PUT,
    ];

    let allow_origin = if config.cors_allow_origins.is_empty() {
        AllowOrigin::mirror_request()
    } else {
        let allowed = config.cors_allow_origins.clone();
        AllowOrigin::predicate(move |origin, parts| {
            if !parts.uri.path().starts_with("/api/") {
                return true;
            }
            let Ok(origin) = origin.to_str() else {
                return false;
            };
            allowed.iter().any(|entry| entry == origin)
        })
    };

    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_credentials(true)
        .allow_methods(AllowMethods::list(ALL_METHODS))
        .allow_headers(AllowHeaders::mirror_request())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::tests::offline_state;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    /// `HEAD` must work on a page route, because the legacy registers it
    /// (`handlers/main.py:72`, `methods=["GET", "HEAD"]`). axum's `get` does not
    /// imply `head`, so without the explicit `.head()` this is a 405.
    ///
    /// The *status* below is not the assertion — the stub's database is
    /// unreachable, so the handler renders an error page. What is asserted is
    /// that the route accepted the method rather than rejecting it.
    #[tokio::test]
    async fn a_page_route_answers_head() {
        let response = router(offline_state())
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_ne!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "HEAD must be registered on the page route"
        );
    }

    /// A method neither route accepts is a 405, as it is in Starlette, and the
    /// `ServeDir` fallback is configured not to swallow it
    /// (`call_fallback_on_method_not_allowed` defaults to false).
    #[tokio::test]
    async fn a_page_route_rejects_a_post() {
        let response = router(offline_state())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/some-post")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// CORS has to echo the origin: `Access-Control-Allow-Origin: *` alongside
    /// `Access-Control-Allow-Credentials: true` is rejected by every browser, and
    /// `CorsLayer::permissive()` is exactly that mistake.
    #[tokio::test]
    async fn cors_echoes_the_origin_rather_than_wildcarding_it() {
        let response = router(offline_state())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::ORIGIN, "https://example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let headers = response.headers();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .map(|value| value.to_str().unwrap()),
            Some("https://example.com"),
            "the origin must be echoed, not `*`"
        );
        assert_eq!(
            headers
                .get("access-control-allow-credentials")
                .map(|value| value.to_str().unwrap()),
            Some("true")
        );
    }

    /// With no `Origin` there is nothing to echo, and Starlette sends no CORS
    /// headers at all — the header is an answer to a cross-origin request.
    #[tokio::test]
    async fn cors_is_silent_without_an_origin() {
        let response = router(offline_state())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    }

    /// The correlation middleware must run on the fallback too, not only on the
    /// routes: `X-Request-ID` is on every response, including the error page a
    /// failed request produces.
    #[tokio::test]
    async fn every_response_carries_a_request_id() {
        for uri in ["/", "/some-post", "/delete-from-cache"] {
            let response = router(offline_state())
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let id = response
                .headers()
                .get(crate::middleware::REQUEST_ID_HEADER)
                .unwrap_or_else(|| panic!("no {uri} in the response headers"));
            // `<word>-<word>-<word>`, the shape `transponder::generate_words`
            // promises. A panic caught by `catch_panics` would set no id at all,
            // and that is the failure this catches.
            assert_eq!(id.to_str().unwrap().split('-').count(), 3, "{uri}");
        }
    }
}
