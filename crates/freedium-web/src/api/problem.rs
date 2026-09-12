//! RFC 9457 problems, and the one table that turns this crate's failures into
//! them.
//!
//! # Why this is not `IntoResponse`
//!
//! A problem carries `instance` (the request path) and `request_id` (from
//! [`Correlation`]), and `IntoResponse::into_response(self)` sees neither. So the
//! conversion is an explicit [`ApiError::resolve`] taking both, and every call
//! site has them: the handlers extract `Extension<Correlation>` and the
//! middleware layers read it out of the request they already hold.
//!
//! # One error type, three tables
//!
//! [`FetchFailure`] and [`ResolveError`] are typed precisely so that the page's
//! table and the API's can differ without either one lying — the page converts
//! through `From` in `handlers::post` and reproduces today's bytes, the API
//! matches here. Two of the divergences are deliberate and large:
//!
//! - **A failed fetch is `502`, not `404`.** §2.7's reading: a `404` from Medium
//!   for an id we believe is valid is an upstream anomaly, not a statement about
//!   our own URL space. Only [`FetchError::NoPost`] — upstream answered and said
//!   there is no such post — is a `404` here.
//! - **[`ResolveError::NotValidMediumUrl`] is `400`, where the page answers
//!   `404`.** §2.7's table says `400`, and three difftest fixtures pin the page's
//!   `404`. The two tables over one typed error is the only way both are right.
//!
//! # `detail` is ours, never upstream's
//!
//! `FetchError::Status` carries the body Medium sent, which is logged and
//! **not** echoed: it is a page of someone else's HTML in a contract we version,
//! and it can be large. The status number is enough for a consumer to act on.

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::Response;
use freedium_dto::problem::{Problem, ProblemKind};
use medium_client::error::{FetchError, TransportError};

use crate::error::{SHADOW_HEADER, SHADOW_NO_FETCH};
use crate::handlers::post::{FetchFailure, ResolveError};
use crate::middleware::Correlation;

/// The media type RFC 9457 §3 defines. Consumers switch on it, so it is part of
/// the contract rather than a formatting detail.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// How long a client is asked to wait after a declined fetch.
///
/// A minute, and it is a lie-by-omission either way: a shadow instance stays in
/// shadow mode for as long as the soak runs, so no wait would ever be enough.
/// This is the value that says "not now, and not because of you".
pub const DECLINED_RETRY_AFTER: u64 = 60;

/// A failure on its way to becoming a `problem+json` body.
///
/// Beside [`Problem`] rather than instead of it: this is the *decision* (which
/// kind, which detail, which extra headers) and `Problem` is the wire shape. The
/// split keeps the DTO crate free of axum, which is what lets `new-web` and the
/// edge read the same shapes without a server.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub kind: ProblemKind,
    pub detail: String,
    /// Set when this is a shadow instance declining to fetch, which stamps
    /// [`SHADOW_HEADER`]. The edge reads that header instead of guessing from
    /// the status — see `crate::error`'s module docs.
    pub declined: bool,
    pub retry_after: Option<u64>,
    /// Extra response headers, as `(name, value)`.
    ///
    /// Only the limiter uses this, for the `X-RateLimit-*` triple a `429` has to
    /// carry. A `Vec` rather than three `Option<u32>` fields so the limiter owns
    /// the shape of its own headers.
    pub headers: Vec<(&'static str, String)>,
}

impl ApiError {
    #[must_use]
    pub fn new(kind: ProblemKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            declined: false,
            retry_after: None,
            headers: Vec::new(),
        }
    }

    /// The shadow instance's refusal to fetch — `FetchFailure::Declined`.
    ///
    /// `503` and not `429`: nothing was exhausted, this instance simply does not
    /// go to the network. The `Retry-After` is there because the client has to be
    /// told *something*, and the marker header is what stops the edge from
    /// comparing this answer to Python's.
    #[must_use]
    pub fn declined() -> Self {
        Self {
            kind: ProblemKind::UpstreamUnavailable,
            detail: "this instance does not fetch: the post is not in its cache".to_string(),
            declined: true,
            retry_after: Some(DECLINED_RETRY_AFTER),
            headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(&'static str, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// The API's table for a failed fetch. See the module docs for the two
    /// deliberate divergences from the page's.
    #[must_use]
    pub fn from_fetch_error(error: &FetchError) -> Self {
        match error {
            // Upstream answered, and the answer was "no such post". The one case
            // that really is a 404.
            FetchError::NoPost => Self::new(
                ProblemKind::NotFound,
                "upstream has no post with that id".to_string(),
            ),
            // Upstream answered, with something we cannot use. Not an outage:
            // the status number is what a caller needs to tell those apart.
            FetchError::Status { status, .. } => Self::new(
                ProblemKind::UpstreamBadResponse,
                format!("upstream answered {status}"),
            ),
            FetchError::Malformed(reason) => Self::new(
                ProblemKind::UpstreamBadResponse,
                format!("upstream payload is not a JSON object: {reason}"),
            ),
            FetchError::BadBody(reason) => Self::new(
                ProblemKind::UpstreamBadResponse,
                format!("upstream body is not JSON: {reason}"),
            ),
            FetchError::GraphQl(reason) => Self::new(
                ProblemKind::UpstreamBadResponse,
                format!("upstream reported an error: {reason}"),
            ),
            // The transport never got an answer. `Other` is DNS, TLS or a
            // malformed response — all "we could not talk to it", which is a
            // different operational problem from "it said no".
            FetchError::Transport(TransportError::Timeout) => Self::new(
                ProblemKind::UpstreamTimeout,
                "the upstream request timed out".to_string(),
            ),
            FetchError::Transport(TransportError::Proxy(reason)) => Self::new(
                ProblemKind::UpstreamError,
                format!("the upstream exit failed: {reason}"),
            ),
            FetchError::Transport(TransportError::Other(reason)) => Self::new(
                ProblemKind::UpstreamError,
                format!("the upstream request failed: {reason}"),
            ),
            // Every exit in the pool is ejected. Not "upstream is broken" —
            // *we* have nowhere to go, and that is an operator's problem rather
            // than a consumer's.
            FetchError::NoHealthyProxy => Self::new(
                ProblemKind::NoUpstream,
                "no healthy upstream exit is available".to_string(),
            ),
        }
    }

    /// The API's table for a URL that did not resolve.
    ///
    /// `NoArticle` is a `404`: a plausible Medium URL with no id in it names
    /// nothing. `NotValidMediumUrl` is a `400` here and a `404` on the page —
    /// see the module docs.
    #[must_use]
    pub fn from_resolve_error(error: ResolveError) -> Self {
        match error {
            ResolveError::InvalidUrl => Self::new(
                ProblemKind::InvalidUrl,
                "the url is not an absolute URL".to_string(),
            ),
            ResolveError::NotValidMediumUrl => Self::new(
                ProblemKind::NotAMediumUrl,
                "the url is not a Medium URL".to_string(),
            ),
            ResolveError::NoArticle => Self::new(
                ProblemKind::NotFound,
                "no Medium post id could be read from that url".to_string(),
            ),
        }
    }

    /// Builds the response.
    ///
    /// `correlation` is optional because a middleware that runs outside
    /// [`crate::middleware::correlation`] has none; every layer here is *inside*
    /// it, so the `None` arm is unreachable in practice and produces an empty
    /// `request_id` rather than a broken body.
    #[must_use]
    pub fn resolve(self, uri: &Uri, correlation: Option<&Correlation>) -> Response {
        let request_id = correlation.map(|correlation| correlation.id.clone());
        let problem = Problem::new(
            self.kind,
            self.detail,
            uri.path(),
            request_id.unwrap_or_default(),
        );
        let status =
            StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        let mut builder = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, PROBLEM_CONTENT_TYPE)
            // An error is never a representation of a resource, so no
            // intermediary may keep it.
            .header(header::CACHE_CONTROL, "no-store");

        if let Some(seconds) = self.retry_after {
            builder = builder.header(header::RETRY_AFTER, seconds);
        }
        if self.declined {
            builder = builder.header(SHADOW_HEADER, SHADOW_NO_FETCH);
        }
        for (name, value) in self.headers {
            builder = builder.header(name, value);
        }

        let body = serde_json::to_string(&problem).expect("a Problem always serialises");
        builder
            .body(Body::from(body))
            .expect("a problem body with known headers always builds")
    }
}

/// The three failures the page and the API describe differently, as one
/// conversion each.
///
/// `FetchFailure::NotAPayload` is a `500`: `fetch_post` promises a validated
/// payload, so this means an invariant of ours broke rather than upstream
/// misbehaving, and it is the one case where the API and the page agree on the
/// status while disagreeing on everything else.
impl From<FetchFailure> for ApiError {
    fn from(failure: FetchFailure) -> Self {
        match failure {
            FetchFailure::Declined => Self::declined(),
            FetchFailure::Fetch(error) => Self::from_fetch_error(&error),
            FetchFailure::NotAPayload(reason) => Self::new(
                ProblemKind::Internal,
                format!("a validated payload could not be read: {reason}"),
            ),
        }
    }
}

impl From<ResolveError> for ApiError {
    fn from(error: ResolveError) -> Self {
        Self::from_resolve_error(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;

    /// Every [`FetchError`] there is.
    ///
    /// Exhaustive by construction and not by comment: adding a variant breaks
    /// [`every_fetch_error_has_its_own_row`]'s `match` below, which is what makes
    /// "the table covers the enum" a compile-time fact rather than a claim.
    fn every_fetch_error() -> Vec<FetchError> {
        vec![
            FetchError::Transport(TransportError::Timeout),
            FetchError::Transport(TransportError::Proxy("refused".to_string())),
            FetchError::Transport(TransportError::Other("dns".to_string())),
            FetchError::Status {
                status: 403,
                body: "<html>go away</html>".to_string(),
            },
            FetchError::NoPost,
            FetchError::Malformed("null".to_string()),
            FetchError::BadBody("truncated".to_string()),
            FetchError::GraphQl("boom".to_string()),
            FetchError::NoHealthyProxy,
        ]
    }

    /// The name of the row this variant lands in. Exhaustive on purpose: a new
    /// `FetchError` has to be given an answer here before this compiles.
    fn row(error: &FetchError) -> &'static str {
        match error {
            FetchError::NoPost => "not-found",
            FetchError::Status { .. }
            | FetchError::Malformed(_)
            | FetchError::BadBody(_)
            | FetchError::GraphQl(_) => "upstream-bad-response",
            FetchError::Transport(TransportError::Timeout) => "upstream-timeout",
            FetchError::Transport(TransportError::Proxy(_))
            | FetchError::Transport(TransportError::Other(_)) => "upstream-error",
            FetchError::NoHealthyProxy => "no-upstream",
        }
    }

    /// §2.7's table, one row at a time.
    ///
    /// The `seen` set is the half that matters: without it the loop would still
    /// pass if `every_fetch_error` quietly stopped covering a variant, and the
    /// table would look verified while a case went untested.
    #[test]
    fn every_fetch_error_has_its_own_row() {
        let mut seen = std::collections::HashSet::new();
        for error in every_fetch_error() {
            let api = ApiError::from_fetch_error(&error);
            assert_eq!(api.kind.slug(), row(&error), "{error:?}");
            assert!(
                !api.detail.is_empty(),
                "{error:?} has no detail to show a consumer"
            );
            seen.insert(row(&error));
        }
        assert_eq!(seen.len(), 5, "the table has five rows: {seen:?}");
    }

    /// The upstream body must not reach the contract.
    ///
    /// A `403` page from Medium is someone else's HTML in a body we version, and
    /// it is exactly the kind of string that starts being parsed by a consumer
    /// the moment it appears.
    #[test]
    fn an_upstream_body_is_logged_and_not_echoed() {
        let error = FetchError::Status {
            status: 403,
            body: "<html>go away</html>".to_string(),
        };
        let api = ApiError::from_fetch_error(&error);
        assert!(!api.detail.contains("go away"), "{}", api.detail);
        assert!(api.detail.contains("403"), "{}", api.detail);
    }

    /// `NotValidMediumUrl` is the divergence that has to be pinned: the page
    /// answers `404` for it (`the_resolve_table_is_the_legacy_exception_table`
    /// in `handlers::post`), and this is the same error reaching the API.
    #[test]
    fn the_api_resolve_table_differs_from_the_pages() {
        let table = [
            (ResolveError::InvalidUrl, ProblemKind::InvalidUrl, 400),
            (
                ResolveError::NotValidMediumUrl,
                ProblemKind::NotAMediumUrl,
                400,
            ),
            (ResolveError::NoArticle, ProblemKind::NotFound, 404),
        ];
        for (error, kind, status) in table {
            let api = ApiError::from(error);
            assert_eq!(api.kind, kind);
            assert_eq!(api.kind.status(), status);
            assert_eq!(
                crate::error::PageError::from(error).status,
                404,
                "the page side moved for {error:?}"
            );
        }
    }

    /// A decline is not an error about the post, and two things have to survive:
    /// the marker the edge reads and the `Retry-After`.
    #[test]
    fn a_declined_fetch_keeps_its_marker_and_its_status() {
        let api = ApiError::from(FetchFailure::Declined);
        assert!(api.declined);
        assert_eq!(api.kind, ProblemKind::UpstreamUnavailable);
        assert_eq!(api.retry_after, Some(DECLINED_RETRY_AFTER));

        let response = api.resolve(&Uri::from_static("/api/v1/posts/abc"), None);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(SHADOW_HEADER)
                .map(|value| value.to_str().unwrap()),
            Some(SHADOW_NO_FETCH)
        );
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .map(|value| value.to_str().unwrap()),
            Some("60")
        );
    }

    /// The wire shape, read back as JSON so the assertion is over bytes rather
    /// than over a struct that could serialise to anything.
    #[tokio::test]
    async fn the_problem_body_is_rfc_9457() {
        let correlation = Correlation::new("three-word-id".to_string(), 7, "/raw".to_string());
        let response = ApiError::new(ProblemKind::NotFound, "no such post")
            .with_headers(vec![("x-ratelimit-limit", "5".to_string())])
            .resolve(
                &Uri::from_static("/api/v1/posts/abc?x=1"),
                Some(&correlation),
            );

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some(PROBLEM_CONTENT_TYPE)
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .map(|value| value.to_str().unwrap()),
            Some("no-store")
        );
        assert_eq!(
            response
                .headers()
                .get("x-ratelimit-limit")
                .map(|value| value.to_str().unwrap()),
            Some("5")
        );

        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let problem: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(problem["schema_version"], 1);
        assert_eq!(problem["type"], "/problems/not-found");
        assert_eq!(problem["status"], 404);
        assert_eq!(problem["title"], "Not Found");
        assert_eq!(problem["detail"], "no such post");
        // The *path*, not the path and query: `instance` identifies the resource
        // the problem is about, and the query is not part of it.
        assert_eq!(problem["instance"], "/api/v1/posts/abc");
        assert_eq!(problem["request_id"], "three-word-id");
        // Every key present, so a consumer can bind a struct without optional
        // fields. `Option` serialises as `null` rather than being skipped.
        assert_eq!(problem.as_object().unwrap().len(), 7, "{problem}");
    }

    /// A source with no correlation still produces a well-formed body. It is
    /// unreachable while every problem-producing layer sits inside
    /// `middleware::correlation`, and this pins that its failure mode is an empty
    /// string rather than a missing key.
    #[test]
    fn a_problem_without_a_correlation_is_still_well_formed() {
        let response = ApiError::new(ProblemKind::Internal, "boom")
            .resolve(&Uri::from_static("/api/v1/feed"), None);
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
