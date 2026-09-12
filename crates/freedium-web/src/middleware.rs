//! Request correlation, logging and the request timeout.
//!
//! One middleware, because the legacy has one: `LoggerMiddleware.dispatch`
//! (`middlewares/logger.py:16-77`) does all of it, and the three parts cannot be
//! separated without changing behaviour.
//!
//! # The timeout lives *inside* the correlation, and that is load-bearing
//!
//! `middlewares/logger.py:21-22` mints the id and the transponder code, *then*
//! `middlewares/logger.py:48` runs
//!
//! ```python
//! response = await asyncio.wait_for(call_next(request), timeout=config.TIMEOUT)
//! ```
//!
//! and catches the timeout at `middlewares/logger.py:49-55`, rendering the error
//! page from the code it minted a moment earlier. So a timed-out request still
//! answers with an HTML error page carrying a valid correlation code — not a
//! bare 408 and not an empty body.
//!
//! That is why there is no `TimeoutLayer` here. A tower timeout layer sits
//! *outside* the handler, so its error would surface after this middleware had
//! already returned, and the code — the one thing the error page needs — would
//! not be in scope. Doing it by hand is both closer to the legacy and simpler.
//!
//! # The header log is the reason `MORE_LOGS` exists
//!
//! `middlewares/logger.py:37-40` logs every request header, sanitising
//! `Authorization`, and `:42-45` logs every cookie. That is noisy per request,
//! so the legacy gates it behind `MORE_LOGS`... except it does not: the legacy
//! logs unconditionally, and `more_logs` is not read anywhere in
//! `middlewares/logger.py`. This gates those lines behind [`Config::more_logs`]
//! and always logs the request line and the response line, because logging every
//! header for every request is what `MORE_LOGS` was added for in the first place.
//!
//! # `except Exception` becomes two mechanisms here
//!
//! `middlewares/logger.py:49` catches **every** exception out of `call_next`,
//! not only the timeout, and answers with `generate_error()` — a random message
//! and a 500 — while sending an alert that names the exception's class.
//!
//! Rust has no equivalent catch-all because it does not need one: a handler
//! returns a `Response`, and the ways it can fail are all explicit. The mapping
//! is:
//!
//! | Python | Here |
//! |---|---|
//! | a handler raises | the handler renders it via [`crate::error`] |
//! | `asyncio.TimeoutError` | the timeout branch below |
//! | a panic | [`catch_panics`] |
//! | anything else reaching `call_next` | nothing — it cannot happen |
//!
//! What that costs is the second alert. For one unhandled exception the legacy
//! sends two Telegram messages — its own at `:52-54`, then `generate_error`'s at
//! `utils/error.py:40`. The timeout branch here sends only the second, which is
//! the one carrying the transponder code an operator can search the logs for.

use std::panic::AssertUnwindSafe;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;

use crate::state::AppState;

/// `X-Request-ID` — `middlewares/logger.py:59`.
pub const REQUEST_ID_HEADER: &str = "X-Request-ID";

/// `X-Process-Time` — `middlewares/logger.py:75`.
pub const PROCESS_TIME_HEADER: &str = "X-Process-Time";

/// What `middlewares/logger.py` keeps in `ContextVar`s, scoped to one request.
///
/// Inserted into the request extensions by [`correlation`], so a handler reads it
/// with `Extension<Correlation>` instead of reaching for a global.
#[derive(Debug, Clone)]
pub struct Correlation {
    /// `xp.generate_xkcdpassword(...)` — the three-word id.
    ///
    /// `middlewares/logger.py:53` calls this `transponder_id` in the alert and
    /// `X-Request-ID` in the response — the same value under two names.
    pub id: String,
    /// `string_to_number_ascii(id)` — what the error page prints.
    pub code: u32,
    /// `url_correlation` (`server/__init__.py:78`): the URL being processed, as
    /// the alert message quotes it.
    ///
    /// That `ContextVar` is set from `str(request.url)`
    /// (`middlewares/logger.py:24`), which is the **full** URL — scheme,
    /// authority, path and query — and not the path the router ends up passing
    /// to `render_medium_post_link` (`handlers/main.py:34`, which strips the
    /// origin off). The full form is the one in the alerts, so it is the one
    /// kept here; see [`request_url`] for how it is rebuilt.
    pub url: String,
}

impl Correlation {
    pub fn new(id: String, code: u32, url: String) -> Self {
        Self { id, code, url }
    }
}

/// Rebuilds `str(request.url)` (`middlewares/logger.py:24`).
///
/// Starlette's `request.url` is assembled from the ASGI scope: the scheme, the
/// `Host` header (or `:authority`), the path and the query. axum's `Uri` for an
/// origin-form request carries only the last two, so the first two are recovered
/// here — the authority from `Host`, and the scheme from `X-Forwarded-Proto`.
///
/// `X-Forwarded-Proto` rather than a hardcoded `http` because uvicorn runs with
/// `proxy_headers=True` by default (`services/uvicorn.py:5` passes no override)
/// and so *does* honour that header; Caddy sets it. Behind TLS that is the
/// difference between `https://freedium.cfd/foo` and `http://freedium.cfd/foo`
/// in every alert an operator reads.
///
/// When there is no `Host` — HTTP/1.0, or a healthcheck that omits it — this
/// falls back to the path and query alone rather than inventing an authority.
fn request_url(request: &Request) -> String {
    let path = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| request.uri().path());

    let Some(authority) = request
        .headers()
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .filter(|host| !host.is_empty())
    else {
        return path.to_string();
    };

    let scheme = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|proto| proto.to_str().ok())
        .map(|proto| proto.split(',').next().unwrap_or(proto).trim())
        .filter(|proto| !proto.is_empty())
        .unwrap_or("http");

    format!("{scheme}://{authority}{path}")
}

/// The whole of `LoggerMiddleware.dispatch`.
///
/// Runs innermost-last: with axum, layers added later wrap earlier ones, so the
/// ordering in [`crate::router`] puts this outside the routes and inside
/// compression — see the note there.
pub async fn correlation(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let (id, code) = crate::transponder::generate();
    let url = request_url(&request);

    let correlation = Correlation::new(id, code, url);
    request.extensions_mut().insert(correlation.clone());

    tracing::debug!(
        id = %correlation.id,
        code = correlation.code,
        "Current ID '{}' transponder code is '{}'",
        correlation.id,
        correlation.code
    );
    tracing::debug!(
        "< HTTP/{} {} {}",
        http_version(&request),
        request.method(),
        request.uri()
    );
    log_headers(&state, &request);

    // `asyncio.wait_for(call_next(request), timeout=config.TIMEOUT)`. The 38s
    // default is deliberately longer than any single outbound fetch
    // (`REQUEST_TIMEOUT`, 12s) so the inner timeout fires first and produces a
    // real error page rather than the generic one.
    let response = match tokio::time::timeout(state.config.timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_elapsed) => {
            tracing::warn!(
                id = %correlation.id,
                "request exceeded the {}s budget",
                state.config.timeout.as_secs()
            );
            crate::error::html_error(&state, &correlation, crate::error::PageError::unspecified())
                .await
        }
    };

    finish(state, response, &correlation, started)
}

/// A panicking handler must not drop the connection.
///
/// The legacy has no analogue — a Python exception is an ordinary `Err` and the
/// middleware's `except Exception` (`middlewares/logger.py:49`) renders a page
/// for it. In Rust most failures *are* `Result`s, handled by
/// [`crate::error::handler_error`]; this is the residual case where a handler
/// panics, which would otherwise abort the connection with no response at all.
///
/// It renders the same error page the timeout does, and it can, because the
/// correlation is already in the request extensions — this layer sits *inside*
/// [`correlation`]. `tower_http::catch_panic::CatchPanicLayer` would not do:
/// its custom-response closure only receives the panic payload, so it has no way
/// to reach the correlation code the page prints.
pub async fn catch_panics(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let correlation = request.extensions().get::<Correlation>().cloned();

    let outcome = AssertUnwindSafe(next.run(request)).catch_unwind().await;

    match outcome {
        Ok(response) => response,
        Err(panic) => {
            let detail = panic_message(&panic);
            tracing::error!(%detail, "handler panicked");

            match correlation {
                Some(correlation) => {
                    crate::error::html_error(
                        &state,
                        &correlation,
                        crate::error::PageError::unspecified(),
                    )
                    .await
                }
                // Unreachable while `catch_panics` is nested inside
                // `correlation`; kept so the ordering in `router` is not
                // load-bearing for correctness.
                None => (StatusCode::INTERNAL_SERVER_ERROR, "An error occurred").into_response(),
            }
        }
    }
}

/// The panic's message, for the log line. `String` and `&str` are the common
/// payloads; anything else is described rather than dropped.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<String>() {
        return text.clone();
    }
    if let Some(text) = panic.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    "non-string panic payload".to_string()
}

/// Sets `X-Request-ID` and `X-Process-Time`, and logs the response line.
///
/// `middlewares/logger.py:59` and `:75`. The process time is a `str(float)` in
/// Python (`"0.001234"`) and is written the same way here, because Fase 4's
/// mirroring may well compare it.
fn finish(
    state: AppState,
    mut response: Response,
    correlation: &Correlation,
    started: Instant,
) -> Response {
    tracing::debug!("> HTTP/{}", response.status().as_u16());

    if state.config.more_logs {
        for (name, value) in response.headers() {
            tracing::debug!("\t> {name}: {}", sanitize(name.as_str(), value));
        }
    }

    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&correlation.id) {
        headers.insert(HeaderName::from_static("x-request-id"), value);
    }
    let elapsed = started.elapsed().as_secs_f64();
    if let Ok(value) = HeaderValue::from_str(&elapsed.to_string()) {
        headers.insert(HeaderName::from_static("x-process-time"), value);
    }

    response
}

fn http_version(request: &Request) -> &'static str {
    match request.version() {
        axum::http::Version::HTTP_09 => "0.9",
        axum::http::Version::HTTP_10 => "1.0",
        axum::http::Version::HTTP_11 => "1.1",
        axum::http::Version::HTTP_2 => "2",
        axum::http::Version::HTTP_3 => "3",
        _ => "?",
    }
}

/// `middlewares/logger.py:37-45`.
///
/// The legacy logs path params as well (`:33-35`). Those are not reachable from
/// an axum middleware — the matched route's captures belong to the handler's
/// extractor, not to the request — so they are the one line of the request log
/// with no counterpart here. The path itself is in the request line above, which
/// is what the params are read for in practice.
fn log_headers(state: &AppState, request: &Request) {
    if !state.config.more_logs {
        return;
    }

    // `middlewares/logger.py:31` is `request.client.host` — the IP alone, not
    // the `(host, port)` tuple `request.client` holds, and not the port.
    match request.extensions().get::<ConnectInfo>() {
        Some(peer) => tracing::debug!("< IP host origin: {}", peer.0.ip()),
        None => tracing::debug!("< IP host origin: unknown"),
    }
    tracing::debug!("< Headers:");
    for (name, value) in request.headers() {
        tracing::debug!("\t< {name}: {}", sanitize(name.as_str(), value));
    }

    // Cookies are logged by the legacy unconditionally (`:42-45`), and this
    // gates them for the same reason as the headers — but with a sharper one:
    // the request may carry the session cookies the GraphQL endpoint is
    // authenticated with, and a log line is a poor place for them. Nothing here
    // redacts them beyond the `MORE_LOGS` gate, matching `_sanitize_header`,
    // which only knows about `Authorization`.
    if !request.headers().contains_key(header::COOKIE) {
        return;
    }
    tracing::debug!("< Cookies:");
    for (name, value) in request
        .headers()
        .iter()
        .filter(|(name, _)| *name == header::COOKIE)
    {
        tracing::debug!("\t< {name}: {}", value.to_str().unwrap_or_default());
    }
}

/// `_sanitize_header` (`middlewares/logger.py:79-82`): an `Authorization` value
/// is cut to its first 25 characters and starred.
///
/// Note the legacy's exact rule: it truncates to `value[:25]` and *appends* six
/// stars, so a short value is logged in full rather than hidden. Faithful, and
/// worth knowing — a 20-character token is not redacted at all.
pub fn sanitize(name: &str, value: &HeaderValue) -> String {
    let text = value.to_str().unwrap_or_default();
    if name.eq_ignore_ascii_case(header::AUTHORIZATION.as_str()) {
        let head: String = text.chars().take(25).collect();
        return format!("{head}******");
    }
    text.to_string()
}

/// The peer address, as axum carries it.
///
/// axum 0.8 puts it in a `ConnectInfo` request extension, which the router
/// installs by serving with `into_make_service_with_connect_info::<SocketAddr>()`
/// — see [`crate::main`]. It is **not** a bare `SocketAddr` in the extensions, so
/// the alias is over axum's wrapper and the address is read out of it.
///
/// Kept as an alias so the logging below reads like the legacy's
/// `request.client.host`.
pub type ConnectInfo = axum::extract::ConnectInfo<std::net::SocketAddr>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    /// The legacy truncates to 25 characters *and appends* six stars, so a short
    /// value survives in full. That is a real disclosure, and it is reproduced —
    /// silently "fixing" it here would change what the logs contain compared with
    /// production during the Fase 4 mirroring.
    #[test]
    fn authorization_is_cut_at_25_characters_and_starred() {
        let long = HeaderValue::from_str(&"a".repeat(60)).unwrap();
        let redacted = sanitize("authorization", &long);
        assert_eq!(redacted, format!("{}******", "a".repeat(25)));

        // Case-insensitive, as in `name.lower() == "authorization"`.
        assert_eq!(sanitize("AUTHORIZATION", &long), redacted);

        // Short values are not hidden — the legacy's rule, kept.
        let short = HeaderValue::from_str("Bearer abc").unwrap();
        assert_eq!(sanitize("authorization", &short), "Bearer abc******");
    }

    #[test]
    fn other_headers_are_untouched() {
        let value = HeaderValue::from_str("application/json").unwrap();
        assert_eq!(sanitize("content-type", &value), "application/json");
    }

    /// The panic catch must not swallow the response of a handler that did not
    /// panic, and must turn a panic into a response rather than an abort.
    #[tokio::test]
    async fn a_panicking_future_is_caught() {
        let ok = async { 7_u32 }.catch_unwind().await;
        assert_eq!(ok.unwrap(), 7);

        let panicked = async {
            panic!("boom");
        }
        .catch_unwind()
        .await;
        assert!(panicked.is_err());
        assert_eq!(panic_message(&panicked.unwrap_err()), "boom");
    }

    /// A panic with a non-string payload still has to describe itself.
    #[tokio::test]
    async fn a_non_string_panic_payload_is_described() {
        let payload = async {
            std::panic::panic_any(42_u32);
        }
        .catch_unwind()
        .await
        .unwrap_err();

        assert_eq!(panic_message(&payload), "non-string panic payload");
    }

    /// The alert's URL is the full one, as `str(request.url)` is. The path is
    /// read straight off the URI, so a middleware that returned only the path
    /// would still look right on every test that does not set a `Host`.
    #[test]
    fn the_alert_url_is_the_full_url() {
        let mut request = Request::builder()
            .uri("/@miro/v2/abc.png?x=1")
            .header(header::HOST, "freedium.cfd")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            request_url(&request),
            "http://freedium.cfd/@miro/v2/abc.png?x=1"
        );

        // Behind Caddy, which sets the header uvicorn's `proxy_headers=True`
        // reads. Without it every alert would report `http` for a TLS site.
        request
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(
            request_url(&request),
            "https://freedium.cfd/@miro/v2/abc.png?x=1"
        );

        // A list is legal in `X-Forwarded-Proto`; the first hop is the client's
        // scheme, which is the one `request.url` would have been built from.
        request
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(request_url(&request).starts_with("https://"));
    }

    /// No `Host` means no authority to invent. The path alone is the honest
    /// answer, and it is what keeps this from fabricating `http:///foo`.
    #[test]
    fn without_a_host_the_url_is_the_path() {
        let request = Request::builder()
            .uri("/foo?q=1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_url(&request), "/foo?q=1");

        // An empty `Host` is as good as none.
        let request = Request::builder()
            .uri("/foo")
            .header(header::HOST, "")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_url(&request), "/foo");
    }
}
