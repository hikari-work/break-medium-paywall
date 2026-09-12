//! Following a `link.medium.com` short link to the article it points at.
//!
//! Ports `resolve_medium_short_link` (`medium_parser/utils.py:240-255`), which
//! is one GET with a browser user agent and redirects switched off:
//!
//! ```text
//! GET https://rsci.app.link/{short_url_id}   302 + Location
//! ```
//!
//! Four things about the legacy version are load-bearing and are reproduced
//! here rather than improved on:
//!
//! - **Redirects are not followed.** The whole point is to read the `Location`
//!   header of the 302, so following it would consume the answer. Every
//!   [`Transport`] in this crate is expected to set
//!   `redirect::Policy::none()` — [`crate::http::ReqwestTransport`] does — and a
//!   transport that followed redirects would silently break this. It would also
//!   break [`fetch_post`](crate::source::PostSource::fetch_post), which wants a
//!   status code, not a fetched page.
//! - **It goes out directly.** There is no connector in the legacy
//!   `ClientSession`, so this request never uses the WARP pool, and neither does
//!   this one. `rsci.app.link` is Branch.io's link server, not Medium, and
//!   routing it through the pool would spend an exit to no purpose.
//! - **The timeout is five seconds**, not `REQUEST_TIMEOUT`'s twelve: it is the
//!   function's own default argument.
//! - **A missing `Location` is not an error** — it is `None`. The legacy code
//!   would raise `KeyError` on `request.headers["Location"]`, which inside
//!   `resolve_medium_url` is a 500 rather than an unresolvable link. §7's
//!   preference is for a remote response's shape not to become a panic, and the
//!   caller cannot tell the two apart anyway: it treats both as "no post id".
//!
//! Retries are `ExponentialRetry(attempts=3)` (`medium_parser/__init__.py:7`) —
//! three attempts, sleeping 1s then 2s — which is
//! [`RetryPolicy::new(3, 1s)`](crate::retry::RetryPolicy::new).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use medium_doc::resolve::LinkResolver;

use crate::error::FetchError;
use crate::http::{Transport, TransportRequest};
use crate::media::BROWSER_USER_AGENT;
use crate::retry::{RetryPolicy, Sleeper, TokioSleeper, with_retry};

/// `utils.py:246`.
pub const SHORT_LINK_ENDPOINT: &str = "https://rsci.app.link/";

/// `resolve_medium_short_link`'s own default (`utils.py:240`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// `ExponentialRetry(attempts=3)`, with `aiohttp_retry`'s default one-second
/// `start_timeout` and factor of two.
pub const DEFAULT_POLICY: RetryPolicy = RetryPolicy::new(3, Duration::from_secs(1));

/// The [`LinkResolver`] `medium-doc`'s seam was carved out for.
///
/// Generic over [`Transport`] for the same reason
/// [`HttpPostSource`](crate::http::HttpPostSource) is: §3.1's impersonation
/// verdict replaces the transport, not the code around it. A `reqwest`
/// transport will do here — Branch.io is not Medium and does not fingerprint
/// TLS — but sharing the type means one client configuration in the server
/// rather than two.
pub struct HttpLinkResolver<T: Transport> {
    transport: T,
    /// Overridable so a test can point it at a local server. It is the whole
    /// URL prefix, not a host, because the id is appended to it directly.
    endpoint: String,
    timeout: Duration,
    policy: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl<T: Transport> HttpLinkResolver<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            endpoint: SHORT_LINK_ENDPOINT.to_string(),
            timeout: DEFAULT_TIMEOUT,
            policy: DEFAULT_POLICY,
            sleeper: Arc::new(TokioSleeper),
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// One attempt: GET, then read `Location` from whatever came back.
    ///
    /// The status is deliberately not checked. The legacy call passes
    /// `raise_for_status=False` and reads the header regardless, so a 200 with
    /// a `Location` resolves and a 302 without one does not. Filtering on
    /// status first would be a different function.
    async fn attempt(&self, short_url_id: &str) -> Result<Option<String>, FetchError> {
        let mut request = TransportRequest::get(
            format!("{}{short_url_id}", self.endpoint),
            vec![("User-Agent".to_string(), BROWSER_USER_AGENT.to_string())],
        );
        request.timeout = self.timeout;

        let response = self.transport.send(request).await?;
        match response.header("Location") {
            Some(location) => Ok(Some(location.to_string())),
            None => {
                tracing::warn!(
                    short_url_id,
                    status = response.status,
                    "the short link did not redirect; no Location header"
                );
                Ok(None)
            }
        }
    }
}

#[async_trait]
impl<T: Transport + 'static> LinkResolver for HttpLinkResolver<T> {
    async fn resolve_short_link(&self, short_url_id: &str) -> Option<String> {
        // The attempt number is ignored: there is no pool to rotate here, and
        // the same URL is the only request this can make.
        let outcome = with_retry(self.policy, self.sleeper.as_ref(), |_| {
            self.attempt(short_url_id)
        })
        .await;

        match outcome {
            Ok(location) => location,
            Err(err) => {
                tracing::warn!(short_url_id, error = %err, "could not resolve the short link");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ReqwestTransport;
    use crate::test_support::{
        Behaviour, CountingSleeper, ScriptedTransport, Stub, http_with_headers,
    };

    fn resolver(endpoint: &str) -> HttpLinkResolver<ReqwestTransport> {
        HttpLinkResolver::new(ReqwestTransport::new().expect("the transport builds"))
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(5))
    }

    /// The stub's URL has no trailing slash; [`SHORT_LINK_ENDPOINT`] does.
    fn prefix(stub: &Stub) -> String {
        format!("{}/", stub.url)
    }

    /// The whole function, against a real HTTP conversation: the id lands in the
    /// path, the browser user agent goes out, and the `Location` of a 302 is
    /// what comes back.
    ///
    /// The `requests.len()` assertion is the load-bearing one. A transport that
    /// followed the redirect would consume the answer and fetch the article
    /// instead — which is precisely what `allow_redirects=False` prevents.
    #[tokio::test]
    async fn a_redirect_location_is_read_and_not_followed() {
        let stub = Stub::start(vec![Behaviour::Reply(http_with_headers(
            302,
            "Found",
            &[("Location", "https://medium.com/@x/t-0291df856c77")],
            "",
        ))]);

        let resolved = resolver(&prefix(&stub)).resolve_short_link("abc123").await;
        assert_eq!(
            resolved.as_deref(),
            Some("https://medium.com/@x/t-0291df856c77")
        );

        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "the redirect must not be followed");
        let request = &requests[0];
        assert!(request.starts_with("GET /abc123 HTTP/1.1\r\n"), "{request}");
        assert!(request.contains(BROWSER_USER_AGENT), "{request}");
    }

    /// A 200 with no `Location` is the `KeyError` the legacy code would have
    /// raised on. Both mean "no post id" to the caller, so this is `None`.
    #[tokio::test]
    async fn a_response_without_a_location_does_not_resolve() {
        let stub = Stub::serving("no redirect here");
        assert_eq!(
            resolver(&prefix(&stub)).resolve_short_link("abc123").await,
            None
        );
    }

    /// The status is never consulted — `raise_for_status=False` in the legacy
    /// call, and it reads the header regardless. A 500 that happens to carry a
    /// `Location` therefore resolves, which is odd but is what production does.
    #[tokio::test]
    async fn a_non_redirect_status_is_not_filtered_out() {
        let stub = Stub::start(vec![Behaviour::Reply(http_with_headers(
            500,
            "Internal Server Error",
            &[("Location", "https://medium.com/@x/t-0291df856c77")],
            "",
        ))]);
        assert_eq!(
            resolver(&prefix(&stub))
                .resolve_short_link("abc123")
                .await
                .as_deref(),
            Some("https://medium.com/@x/t-0291df856c77")
        );
    }

    /// Three attempts, two sleeps — `ExponentialRetry(attempts=3)`.
    #[tokio::test]
    async fn a_transport_failure_is_retried() {
        let sleeper = Arc::new(CountingSleeper::default());
        let transport = ScriptedTransport::always_failing(crate::error::TransportError::Timeout);

        let resolved = HttpLinkResolver::new(transport)
            .with_endpoint("http://127.0.0.1:1/")
            // `.clone()`, not `Arc::clone(&…)`: the latter would have to infer
            // `Arc<dyn Sleeper>` from the argument and fail.
            .with_sleeper(sleeper.clone())
            .resolve_short_link("abc123")
            .await;

        assert_eq!(resolved, None);
        assert_eq!(sleeper.calls(), 2, "three attempts sleep twice");
    }

    /// And it gives up rather than retrying forever.
    #[tokio::test]
    async fn every_attempt_can_fail() {
        let transport = ScriptedTransport::always_failing(crate::error::TransportError::Timeout);
        let attempts = transport.clone();
        let sleeper = Arc::new(CountingSleeper::default());

        let resolved = HttpLinkResolver::new(transport)
            .with_endpoint("http://127.0.0.1:1/")
            .with_sleeper(sleeper)
            .resolve_short_link("abc123")
            .await;

        assert_eq!(resolved, None);
        assert_eq!(attempts.request_count(), 3, "the policy's three attempts");
    }

    /// The defaults are the legacy function's, not the crate's: five seconds
    /// (`utils.py:240`), not `REQUEST_TIMEOUT`'s twelve.
    #[test]
    fn the_defaults_match_the_legacy_call() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(SHORT_LINK_ENDPOINT, "https://rsci.app.link/");
        assert_eq!(DEFAULT_POLICY.attempts, 3);
        assert_eq!(DEFAULT_POLICY.delay(0), Duration::from_secs(1));
        assert_eq!(DEFAULT_POLICY.delay(1), Duration::from_secs(2));
    }

    /// The resolver never asks for an exit: the legacy session has no connector,
    /// and `rsci.app.link` is not Medium.
    #[tokio::test]
    async fn no_proxy_is_used() {
        let transport = ScriptedTransport::new(
            vec![],
            crate::test_support::response(302, &[("Location", "https://medium.com/x")], b""),
        );
        let seen = transport.clone();

        HttpLinkResolver::new(transport.clone())
            .with_endpoint("http://127.0.0.1:1/")
            .resolve_short_link("abc123")
            .await;

        assert!(seen.requests()[0].proxy.is_none());
    }

    /// It implements `medium-doc`'s trait, which is the only reason it exists:
    /// `resolve_medium_url` must be able to use it as a `&dyn LinkResolver`.
    #[tokio::test]
    async fn it_resolves_a_medium_short_link() {
        let stub = Stub::start(vec![Behaviour::Reply(http_with_headers(
            302,
            "Found",
            &[("Location", "https://medium.com/@x/t-0291df856c77")],
            "",
        ))]);
        let resolver = resolver(&prefix(&stub));

        let post_id =
            medium_doc::resolve::resolve_medium_url("https://link.medium.com/abc123", &resolver)
                .await;

        assert_eq!(
            post_id.map(|id| id.as_str().to_string()),
            Some("0291df856c77".to_string())
        );
    }
}
