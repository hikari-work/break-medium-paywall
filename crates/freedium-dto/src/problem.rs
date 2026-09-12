//! `application/problem+json` — RFC 9457, and the closed set of problems.
//!
//! The *shape* lives here because it is part of the contract; the *mapping* from
//! this codebase's errors onto it lives in `freedium-web`'s `api::problem`,
//! because that is where `FetchError` and axum are.

use serde::{Deserialize, Serialize};

/// An RFC 9457 problem document.
///
/// # `type` is a relative reference
///
/// RFC 9457 §3.1 allows it and recommends it when there is no documentation
/// domain to point at, which is the case here: the types are
/// `"/problems/not-found"` and friends. Minting an `https://` URL that resolves
/// to nothing would be worse than a relative reference that makes no promise.
///
/// # `instance` and `request_id`
///
/// `instance` is the request path and `request_id` is the same correlation token
/// the HTML error page calls a "transponder code" and the response carries as
/// `X-Request-ID`. Both are here so that a consumer reporting a problem has
/// something an operator can search a log for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Problem {
    pub schema_version: u8,
    /// A relative reference: `"/problems/not-found"`.
    pub r#type: String,
    /// The HTTP status phrase, per RFC 9457's recommendation. Derive it from
    /// [`ProblemKind::status`] rather than reading meaning into it — the
    /// machine-readable part of a problem is `type` and `status`.
    pub title: String,
    /// The status code, repeated in the body, which RFC 9457 §3.1 requires.
    pub status: u16,
    /// What actually went wrong, specific to this occurrence.
    pub detail: String,
    /// The request path.
    pub instance: String,
    pub request_id: String,
}

impl Problem {
    #[must_use]
    pub fn new(
        kind: ProblemKind,
        detail: impl Into<String>,
        instance: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            r#type: kind.problem_type(),
            title: kind.title().to_string(),
            status: kind.status(),
            detail: detail.into(),
            instance: instance.into(),
            request_id: request_id.into(),
        }
    }
}

/// The closed set of problems this API can report.
///
/// Closed on purpose: a consumer can enumerate the failure modes, and a
/// [`Problem::r#type`] that is not one of these is a bug rather than a new
/// state to handle.
///
/// Note there is no variant for a configuration error. `Config::from_env()` runs
/// before the listener binds and `main` prints and exits, so an HTTP status for
/// it would be dead code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    /// The `{id}` path segment is not a Medium post id.
    InvalidPostId,
    /// `?url=` was absent or not an absolute URL.
    InvalidUrl,
    /// `?url=` is a URL, but not one of Medium's.
    NotAMediumUrl,
    /// `X-API-TOKEN` was sent and does not match the configured token.
    InvalidToken,

    NotFound,
    MethodNotAllowed,

    /// Upstream answered, but not with something we can use.
    UpstreamBadResponse,
    /// Upstream is unreachable or is failing.
    UpstreamError,
    /// Upstream took too long.
    UpstreamTimeout,
    /// This instance is deliberately not fetching (shadow mode).
    UpstreamUnavailable,
    /// There is no exit to fetch through.
    NoUpstream,

    /// A dependency this instance needs is not answering.
    NotReady,
    /// A rate-limit bucket is empty.
    RateLimited,
    /// The request took too long on our side.
    Timeout,
    /// A bug on our side.
    Internal,
}

impl ProblemKind {
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::InvalidPostId | Self::InvalidUrl | Self::NotAMediumUrl => 400,
            Self::InvalidToken => 401,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::RateLimited => 429,
            Self::Internal => 500,
            Self::UpstreamBadResponse | Self::UpstreamError => 502,
            Self::UpstreamUnavailable | Self::NoUpstream | Self::NotReady => 503,
            Self::UpstreamTimeout | Self::Timeout => 504,
        }
    }

    /// The trailing segment of [`Problem::r#type`], and the only part that
    /// varies — so the table that decides a problem's identity is this one
    /// function.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::InvalidPostId => "invalid-post-id",
            Self::InvalidUrl => "invalid-url",
            Self::NotAMediumUrl => "not-a-medium-url",
            Self::InvalidToken => "invalid-token",
            Self::NotFound => "not-found",
            Self::MethodNotAllowed => "method-not-allowed",
            Self::UpstreamBadResponse => "upstream-bad-response",
            Self::UpstreamError => "upstream-error",
            Self::UpstreamTimeout => "upstream-timeout",
            Self::UpstreamUnavailable => "upstream-unavailable",
            Self::NoUpstream => "no-upstream",
            Self::NotReady => "not-ready",
            Self::RateLimited => "rate-limited",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
        }
    }

    /// `"/problems/{slug}"`.
    #[must_use]
    pub fn problem_type(self) -> String {
        format!("/problems/{}", self.slug())
    }

    /// The HTTP status phrase, which RFC 9457 §3.1 says `title` should be.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self.status() {
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            // Unreachable: `status` is total over this enum and every arm is
            // listed above. A `_` arm would silently swallow a new variant.
            _ => "Error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The media type RFC 9457 defines. Consumers switch on it, so it is part of
    /// the contract and pinned in `freedium-web` where the response is built.
    #[test]
    fn the_type_is_a_relative_reference() {
        assert_eq!(ProblemKind::NotFound.problem_type(), "/problems/not-found");
        assert_eq!(
            ProblemKind::UpstreamBadResponse.problem_type(),
            "/problems/upstream-bad-response"
        );
        for kind in ALL_KINDS {
            let problem_type = kind.problem_type();
            assert!(
                problem_type.starts_with("/problems/"),
                "{problem_type} is not a relative reference"
            );
            assert!(
                !problem_type.contains("://"),
                "{problem_type} promises a documentation site that does not exist"
            );
        }
    }

    /// `title` is the status phrase, so a reader can never find a `429` labelled
    /// "Not Found". `status` and `title` agreeing is the whole point of
    /// deriving one from the other.
    #[test]
    fn the_title_is_the_phrase_for_the_status() {
        for kind in ALL_KINDS {
            let expected = match kind.status() {
                400 => "Bad Request",
                401 => "Unauthorized",
                404 => "Not Found",
                405 => "Method Not Allowed",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                502 => "Bad Gateway",
                503 => "Service Unavailable",
                504 => "Gateway Timeout",
                other => panic!("{kind:?} has an unexpected status {other}"),
            };
            assert_eq!(kind.title(), expected, "{kind:?}");
        }
    }

    /// Every `slug` is unique, or two different failures would be
    /// indistinguishable to a consumer switching on `type`.
    #[test]
    fn the_slugs_are_unique() {
        let mut slugs: Vec<&str> = ALL_KINDS.iter().map(|kind| kind.slug()).collect();
        let count = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "two kinds share a slug");
    }

    /// The statuses are all real HTTP error statuses, and the set is the one
    /// `freedium-web`'s mapping table claims to produce.
    #[test]
    fn every_status_is_an_error_status_this_api_documents() {
        for kind in ALL_KINDS {
            assert!(
                (400..600).contains(&kind.status()),
                "{kind:?} maps to {}, which is not an error status",
                kind.status()
            );
        }
    }

    /// The version is on the problem too, so a client can read it from a failed
    /// response without having succeeded once.
    #[test]
    fn a_problem_carries_the_schema_version() {
        let problem = Problem::new(
            ProblemKind::NotFound,
            "no such post",
            "/api/v1/posts/aaaaaaaaaaaa",
            "alpha-bravo-charlie",
        );
        assert_eq!(problem.schema_version, crate::SCHEMA_VERSION);
        assert_eq!(problem.status, 404);
        assert_eq!(problem.r#type, "/problems/not-found");
        assert_eq!(problem.title, "Not Found");
    }

    const ALL_KINDS: [ProblemKind; 15] = [
        ProblemKind::InvalidPostId,
        ProblemKind::InvalidUrl,
        ProblemKind::NotAMediumUrl,
        ProblemKind::InvalidToken,
        ProblemKind::NotFound,
        ProblemKind::MethodNotAllowed,
        ProblemKind::UpstreamBadResponse,
        ProblemKind::UpstreamError,
        ProblemKind::UpstreamTimeout,
        ProblemKind::UpstreamUnavailable,
        ProblemKind::NoUpstream,
        ProblemKind::NotReady,
        ProblemKind::RateLimited,
        ProblemKind::Timeout,
        ProblemKind::Internal,
    ];
}
