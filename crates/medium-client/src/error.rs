//! Errors this crate produces.
//!
//! Two types, split on the seam that matters: [`TransportError`] is whatever the
//! pluggable transport reports and carries no protocol meaning, while
//! [`FetchError`] is what a caller of [`crate::source::PostSource`] has to
//! handle and does.
//!
//! Keeping them apart is what lets the impersonation decision (§3.1) stay
//! deferred. Swapping `reqwest` for `rquest` or a Python sidecar changes
//! `TransportError`'s strings and nothing else — no caller of `fetch_post` is
//! affected.

use thiserror::Error;

/// A failed attempt to put bytes on the wire and get bytes back.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    /// No response within the deadline.
    #[error("request timed out")]
    Timeout,

    /// The proxy could not be used: refused the connection, failed to
    /// negotiate SOCKS5, or dropped mid-request. Separated from `Other`
    /// because it is the one transport failure the pool can react to by
    /// ejecting the exit and retrying elsewhere.
    #[error("proxy failure: {0}")]
    Proxy(String),

    /// Anything else — DNS, TLS, a malformed response.
    #[error("transport failure: {0}")]
    Other(String),
}

/// Why a post could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// The endpoint answered, but not with 200.
    ///
    /// Carries the body because the legacy client logs it
    /// (`api.py:79-81`) and it is the only clue when Medium starts answering
    /// 403 to a fingerprint that used to work.
    #[error("unexpected status {status}: {body}")]
    Status { status: u16, body: String },

    /// A 200 whose payload carried no post: `data` absent or null, or
    /// `data.post` absent or null (`core.py:184-187`).
    #[error("response carried no data.post")]
    NoPost,

    /// The payload was not a JSON object at all (`core.py:180-181`).
    #[error("payload is not a JSON object: {0}")]
    Malformed(String),

    /// The response body could not be parsed as JSON.
    ///
    /// Retryable, and that is not an oversight: `api.py:84` calls
    /// `response.json()`, and a body that is not JSON raises, which `core.py`
    /// catches as an exception and retries. A truncated response from a
    /// flapping exit is exactly the case this covers.
    #[error("response body is not JSON: {0}")]
    BadBody(String),

    /// The payload carried an `error` member (`core.py:182-183`).
    #[error("graphql error: {0}")]
    GraphQl(String),

    /// Every endpoint in the pool is ejected.
    #[error("no healthy proxy in the pool")]
    NoHealthyProxy,
}

impl FetchError {
    /// Whether retrying the same request could plausibly succeed.
    ///
    /// # This is the legacy retry condition, made explicit
    ///
    /// `core.py:172` decides by truthiness: the loop repeats while
    /// `not post_data`. A transport exception, and a non-200 (which returns
    /// `None`, `api.py:82`), are both falsy and so get retried. A payload that
    /// *is* a dict but lacks `data.post` is truthy, so it exits the loop at
    /// once and fails — retrying a GraphQL error just spends another request
    /// on an answer Medium has already given.
    ///
    /// `NoHealthyProxy` is not retryable either: the pool is exhausted, and
    /// sleeping before asking the same empty pool again only delays the error.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::Status { .. } | Self::BadBody(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction the retry loop is built on. A transport failure, a
    /// non-200 and an unparseable body are the same failure mode as far as
    /// `core.py` is concerned — all three leave it retrying. Everything else
    /// is terminal, because Medium has already answered.
    #[test]
    fn only_the_failures_core_py_retries_are_retryable() {
        let retryable = [
            FetchError::Transport(TransportError::Timeout),
            FetchError::Transport(TransportError::Proxy("refused".into())),
            FetchError::Status {
                status: 403,
                body: String::new(),
            },
            FetchError::BadBody("truncated".into()),
        ];
        for err in retryable {
            assert!(err.is_retryable(), "{err:?} should be retryable");
        }

        let terminal = [
            FetchError::NoPost,
            FetchError::Malformed("null".into()),
            FetchError::GraphQl("boom".into()),
            FetchError::NoHealthyProxy,
        ];
        for err in terminal {
            assert!(!err.is_retryable(), "{err:?} should be terminal");
        }
    }
}
