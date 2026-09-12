//! Fetching a post: request construction, the transport, and the retry loop.
//!
//! # What this is, and what it deliberately is not
//!
//! Everything in the path is here and complete — headers, body, proxy
//! selection, timeouts, retry, failure classification — and §3.1's question,
//! **how the request is made**, sits behind [`Transport`]. That seam is
//! resolved: [`crate::wreq_transport::WreqTransport`] is the impersonating
//! client production uses.
//!
//! The reason to have built it this way rather than wait is that the
//! impersonation question was genuinely narrow. It was never "can this client
//! talk to Medium", it was "does this client's TLS ClientHello and HTTP/2 frame
//! ordering look like Chrome 110". Everything around that was answerable then,
//! and is tested against a stub server. When the verdict landed, the diff was a
//! new `Transport` implementation and the wiring that selects it.
//!
//! # [`ReqwestTransport`] is not the production source for the GraphQL fetch
//!
//! `reqwest` uses the system TLS stack with a fingerprint of its own, and this
//! section used to say flatly that the endpoint "rejects it". **That absolute is
//! not what was measured.** On 2026-09-12, from the WARP egress this project
//! develops behind (`warp=plus`, `loc=ID`), the full production request through
//! `reqwest` returned HTTP 200 with a real article for 6 of 6 cold posts.
//! Meanwhile plain `curl` sending *the same headers* to the same endpoint got a
//! 403 Cloudflare block page from that same address. So the fingerprint is doing
//! something, and `reqwest`'s happens to pass from a WARP IP today.
//!
//! That is not a reason to move the fetch here, and the distinction matters:
//!
//! * SPIKE-1's gate was defined against `curl_cffi chrome110` — legacy's
//!   production client — and [`crate::wreq_transport`] met it at parity 1.0000.
//!   That is the measured, spec-defined choice. "rustls was not blocked on one
//!   afternoon from one egress" is not.
//! * Cloudflare's bot score is dominated by IP reputation, and a WARP IP is not
//!   a datacenter IP. The case §2.2 warns about — a direct fetch from a
//!   datacenter address — has **not** been measured with `reqwest`, and that is
//!   where the fingerprint becomes the tiebreaker.
//! * The failure mode is silent and total: every cache miss 502s. Holding a
//!   measured client is worth more than a leaner dependency tree.
//!
//! It is still the right client for the other two paths, and the split is not
//! arbitrary — it is what the legacy implementation does. Only the GraphQL fetch
//! impersonates there (`curl_cffi` in `api.py`); the miro media passthrough and
//! the `rsci.app.link` short-link resolver both use plain `aiohttp` with no TLS
//! impersonation. [`crate::media::MediaFetcher`] and
//! [`crate::resolver::HttpLinkResolver`] therefore stay on this transport.
//!
//! It also remains what the whole request path is tested through: the stub
//! server in [`crate::test_support`] speaks cleartext HTTP/1.1, which is the
//! cheapest way to exercise retries, proxy ejection and failure classification
//! without a fingerprint in the way.
//!
//! # The one deliberate asymmetry with [`crate::wreq_transport`]
//!
//! This transport leaves `reqwest`'s system-proxy auto-detection **on**, so an
//! ambient `HTTP_PROXY` will proxify a request that passed `proxy: None`. The
//! impersonating transport turns it off. That is intentional and it is not a bug
//! to reconcile: there, "direct" is the measured baseline and must mean direct;
//! here, following the environment is the conventional behaviour and nothing is
//! being measured.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{FetchError, TransportError};
use crate::proxy::{ProxyChoice, ProxyClients, ProxyPool};
use crate::request;
use crate::response;
use crate::retry::{RetryPolicy, Sleeper, TokioSleeper, with_retry};
use crate::source::PostSource;

/// The verb a [`TransportRequest`] carries.
///
/// Not `http::Method`: the same reasoning as [`TransportRequest`] itself. Two
/// verbs are all the server needs — GraphQL is a POST and the two passthrough
/// fetches are GETs — and an enum that cannot express a third is a smaller thing
/// for §3.1's sidecar to translate than an arbitrary token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    Get,
    /// The default: a `TransportRequest` built without one is the GraphQL call,
    /// which is what this crate existed for before the resolver and the media
    /// fetcher were added.
    #[default]
    Post,
}

/// A request, in the terms every transport can express.
///
/// Deliberately not `http::Request`: the third §3.1 candidate is a Python
/// sidecar reached over HTTP, which would have to translate this into its own
/// shape anyway. A plain struct makes that translation the sidecar's business
/// instead of forcing an HTTP-shaped API onto it.
#[derive(Debug, Clone)]
pub struct TransportRequest {
    pub url: String,
    pub method: Method,
    pub headers: Vec<(String, String)>,
    /// Sent only for [`Method::Post`]. A GET must not carry a body, and a
    /// transport that sent one anyway would have `reqwest` turn it into an
    /// error rather than a request.
    pub body: Vec<u8>,
    /// `None` sends the request directly.
    pub proxy: Option<String>,
    pub timeout: Duration,
}

impl TransportRequest {
    /// A GET with no body — the shape both passthrough fetches need.
    pub fn get(url: impl Into<String>, headers: Vec<(String, String)>) -> Self {
        Self {
            url: url.into(),
            method: Method::Get,
            headers,
            body: Vec::new(),
            proxy: None,
            timeout: Duration::from_secs(12),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status: u16,
    /// Response headers, in the order the server sent them, with the names
    /// lowercased as `reqwest` normalises them.
    ///
    /// The resolver reads `Location` and the media fetcher reads
    /// `Content-Type`, so a response that carries neither is incomplete rather
    /// than wrong. Both lookups go through [`Self::header`] so that a
    /// transport which reports names in their original case still works.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl TransportResponse {
    /// The first value for `name`, case-insensitively.
    ///
    /// First, not last: a duplicate `Content-Type` is malformed and the legacy
    /// client's `request.headers["Content-Type"]` would have raised on it. The
    /// distinction does not matter for a well-formed response and this is the
    /// more predictable reading for a broken one.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// How bytes get to Medium.
///
/// See the module doc: this is the seam SPIKE-1 decides.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, TransportError>;
}

/// An HTTP transport with no impersonation. See the module doc before using it
/// outside tests.
///
/// Clients are kept per proxy by [`ProxyClients`] — `reqwest` 0.13 sets a proxy
/// on the client rather than the request, and one client per exit is also the
/// right shape when the pool rotates between requests.
#[derive(Debug, Clone, Default)]
pub struct ReqwestTransport {
    clients: ProxyClients,
}

impl ReqwestTransport {
    /// Builds the direct client eagerly, so a TLS configuration failure is
    /// reported here rather than on the first request.
    pub fn new() -> Result<Self, TransportError> {
        let transport = Self::default();
        transport.client_for(None)?;
        Ok(transport)
    }

    fn client_for(&self, proxy: Option<&str>) -> Result<reqwest::Client, TransportError> {
        self.clients.get_or_build(proxy, |builder| {
            // Redirects are followed by default. Medium's GraphQL endpoint is
            // not expected to redirect, and following one would turn a clear
            // status-code failure into a confusing body parse.
            builder.redirect(reqwest::redirect::Policy::none())
        })
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        let used_proxy = request.proxy.is_some();
        let client = self.client_for(request.proxy.as_deref())?;

        let mut builder = match request.method {
            Method::Get => client.get(&request.url),
            Method::Post => client.post(&request.url).body(request.body),
        };
        // Per-request, so one caller's timeout does not become the next
        // caller's too.
        builder = builder.timeout(request.timeout);

        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }

        let response = builder
            .send()
            .await
            .map_err(|err| classify(&err, used_proxy))?;

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    // A header value that is not UTF-8 is not something either
                    // caller can use, and dropping it here beats failing the
                    // whole request over an unrelated header.
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|err| classify(&err, used_proxy))?
            .to_vec();

        Ok(TransportResponse {
            status,
            headers,
            body,
        })
    }
}

/// Turns a `reqwest` error into the crate's own.
///
/// `used_proxy` is the heuristic that matters: `reqwest` cannot say whether a
/// connection failure was the proxy's fault, but if a proxy was configured and
/// the connection failed, blaming the proxy is both the likely explanation and
/// the useful one — it is what makes the pool eject the exit and retry
/// elsewhere.
fn classify(err: &reqwest::Error, used_proxy: bool) -> TransportError {
    if err.is_timeout() {
        TransportError::Timeout
    } else if used_proxy && (err.is_connect() || err.is_request()) {
        TransportError::Proxy(err.to_string())
    } else {
        TransportError::Other(err.to_string())
    }
}

/// A [`PostSource`] that fetches from Medium over a [`Transport`].
pub struct HttpPostSource<T: Transport> {
    transport: T,
    pool: Arc<ProxyPool>,
    endpoint: String,
    policy: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
    auth_cookies: Option<String>,
    timeout: Duration,
}

impl<T: Transport> HttpPostSource<T> {
    /// `timeout` defaults to `REQUEST_TIMEOUT` (`config.py:20`, twelve
    /// seconds) and the retry policy to [`RetryPolicy::DEFAULT`].
    pub fn new(transport: T, pool: Arc<ProxyPool>) -> Self {
        Self {
            transport,
            pool,
            endpoint: request::ENDPOINT.to_string(),
            policy: RetryPolicy::DEFAULT,
            sleeper: Arc::new(TokioSleeper),
            auth_cookies: None,
            timeout: Duration::from_secs(12),
        }
    }

    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// The `Cookie` header. Read §2.7 warning 2 before setting this in
    /// production: it is a subscriber account's session, and its unlocks are
    /// quota-bound.
    pub fn with_auth_cookies(mut self, auth_cookies: Option<String>) -> Self {
        self.auth_cookies = auth_cookies;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Points the source somewhere other than `medium.com/_/graphql`. For
    /// tests, and for the sidecar in §3.1 option 3, which is reached at its own
    /// address.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// One attempt: pick an exit, send, and interpret the response.
    ///
    /// Ejecting the proxy on a transport failure happens *here* rather than in
    /// the retry loop, because the pool is this method's business and because
    /// the next call to [`ProxyPool::next`] is what picks the replacement.
    async fn attempt(&self, post_id: &str, attempt: u32) -> Result<Value, FetchError> {
        let choice = self.pool.next()?;

        let proxy = match &choice {
            ProxyChoice::Direct => None,
            ProxyChoice::Via(endpoint) => Some(endpoint.as_str().to_string()),
        };
        tracing::debug!(post_id, attempt, ?proxy, "fetching post");

        let body = serde_json::to_vec(&request::body(post_id))
            .map_err(|err| FetchError::Malformed(err.to_string()))?;

        let transport_request = TransportRequest {
            url: self.endpoint.clone(),
            method: Method::Post,
            headers: request::headers(
                &request::operation_id(),
                request::client_date_ms(),
                self.auth_cookies.as_deref(),
            ),
            body,
            proxy,
            timeout: self.timeout,
        };

        let response = match self.transport.send(transport_request).await {
            Ok(response) => response,
            Err(err) => {
                // Take the exit out of rotation before returning, so the retry
                // (which calls `next` again) goes somewhere else. This is the
                // capability HAProxy does not have — see `proxy.rs`.
                if let ProxyChoice::Via(endpoint) = &choice {
                    self.pool.eject(endpoint);
                }
                return Err(FetchError::Transport(err));
            }
        };

        if response.status != 200 {
            return Err(FetchError::Status {
                status: response.status,
                body: preview(&response.body),
            });
        }

        let payload: Value = serde_json::from_slice(&response.body)
            .map_err(|err| FetchError::BadBody(err.to_string()))?;

        response::validate(&payload)?;
        Ok(payload)
    }
}

#[async_trait]
impl<T: Transport + 'static> PostSource for HttpPostSource<T> {
    async fn fetch_post(&self, post_id: &str) -> Result<Value, FetchError> {
        with_retry(self.policy, self.sleeper.as_ref(), |attempt| {
            self.attempt(post_id, attempt)
        })
        .await
    }
}

/// A [`PostSource`] that cannot carry credentials.
///
/// # Why this exists at all
///
/// `RUST_REWRITE_PLAN.md` §2.7 warns that the public API must not be served
/// anonymously from an instance that sets `MEDIUM_AUTH_COOKIES`, because the
/// unlocks that cookie performs are quota-bound to a real account — and its own
/// suggested mitigation ("don't expose the API there") does not apply, because
/// the configuration that must set the cookie is the production one. So the
/// replacement is not a runtime check but a construction guarantee: **there is no
/// path from this type to [`HttpPostSource::with_auth_cookies`]**, because this
/// type's only constructor builds its own `HttpPostSource` from the raw
/// ingredients — a transport, a pool, a timeout, an endpoint — and never sees a
/// cookie to attach.
///
/// # The limit of the guarantee, stated rather than implied
///
/// This makes "the API forgot to drop the cookie" impossible *from an anonymous
/// handle*. It does not stop someone writing `state.source` — the page's own,
/// cookie-bearing source — inside an API handler. Nothing at this level can: the
/// two are the same trait object type. What closes that is the handler test in
/// `freedium-web::state`, which checks the API path against a recording source and
/// the page path against the same recording source, so neither assertion can pass
/// vacuously.
///
/// Do not add `with_auth_cookies` here, and do not add a constructor that takes an
/// already-built [`HttpPostSource`] — either one would re-open what this closes.
#[derive(Clone)]
pub struct AnonymousSource {
    inner: Arc<dyn PostSource>,
}

impl AnonymousSource {
    /// Builds a source with no credentials and no way to acquire any.
    ///
    /// `endpoint` is [`request::ENDPOINT`] in production; `MEDIUM_GRAPHQL_ENDPOINT`
    /// overrides it so the upstream-failure paths can be exercised against a local
    /// server.
    pub fn new<T: Transport + 'static>(
        transport: T,
        pool: Arc<ProxyPool>,
        timeout: Duration,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(
                HttpPostSource::new(transport, pool)
                    .with_timeout(timeout)
                    .with_endpoint(endpoint),
            ),
        }
    }

    /// The underlying source, for the tests that compare it against the page's by
    /// pointer identity. Handlers should call [`Self::fetch_post`] instead.
    pub fn inner(&self) -> &Arc<dyn PostSource> {
        &self.inner
    }

    /// One post, over an anonymous transport.
    pub async fn fetch_post(&self, post_id: &str) -> Result<Value, FetchError> {
        self.inner.fetch_post(post_id).await
    }
}

/// A character-safe prefix of a response body, for the `Status` error.
///
/// The body of a rejected GraphQL request is small, but a proxy or CDN error
/// page can be tens of kilobytes, and the error ends up in logs.
fn preview(body: &[u8]) -> String {
    const LIMIT: usize = 300;

    let text = String::from_utf8_lossy(body);
    if text.chars().count() <= LIMIT {
        return text.into_owned();
    }

    let cut = text
        .char_indices()
        .nth(LIMIT)
        .map(|(index, _)| index)
        .expect("counted more than LIMIT characters");
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::ProxyEndpoint;
    use crate::test_support::{
        AlwaysHealthy, Behaviour, CountingSleeper, ScriptedTransport, Stub, direct_pool, http_ok,
        http_status,
    };
    use std::time::Duration;

    fn source(endpoint: &str) -> HttpPostSource<ReqwestTransport> {
        HttpPostSource::new(
            ReqwestTransport::new().expect("the transport builds"),
            direct_pool(),
        )
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(5))
    }

    const VALID: &str = r#"{"data":{"post":{"id":"abc"}}}"#;

    #[tokio::test]
    async fn a_valid_response_is_returned() {
        let stub = Stub::serving(VALID);
        let payload = source(&stub.url).fetch_post("abc").await.unwrap();
        assert_eq!(payload["data"]["post"]["id"], "abc");
    }

    /// The request must actually reach the wire as the legacy client sends it —
    /// a stub that answers the same way whatever it is sent would pass every
    /// other test in this module.
    #[tokio::test]
    async fn the_request_carries_the_query_body() {
        let stub = Stub::serving(VALID);
        source(&stub.url).fetch_post("515dd5a43948").await.unwrap();

        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "one fetch, one request");
        let request = &requests[0];
        // hyper lowercases header names on the wire, so compare that way.
        let lowercased = request.to_ascii_lowercase();

        assert!(
            request.starts_with("POST / HTTP/1.1\r\n"),
            "got {request:?}"
        );
        assert!(
            lowercased.contains("x-apollo-operation-name: fullpostquery\r\n"),
            "got {request:?}"
        );
        assert!(
            lowercased.contains(&format!(
                "user-agent: {}\r\n",
                request::USER_AGENT.to_ascii_lowercase()
            )),
            "got {request:?}"
        );

        let body = request
            .split_once("\r\n\r\n")
            .expect("a body follows the headers")
            .1;
        let sent: Value = serde_json::from_str(body).expect("the body is the JSON request");

        assert_eq!(sent["operationName"], "FullPostQuery");
        assert_eq!(sent["variables"]["postId"], "515dd5a43948");
        assert_eq!(sent["query"], request::FULL_POST_QUERY);
    }

    /// No `Cookie` header unless one was configured — §2.7 warning 2 makes an
    /// accidentally-attached subscriber session an expensive mistake.
    #[tokio::test]
    async fn the_cookie_header_is_absent_by_default() {
        let stub = Stub::serving(VALID);
        source(&stub.url).fetch_post("abc").await.unwrap();

        let requests = stub.requests();
        assert!(!requests[0].contains("Cookie:"), "got {:?}", requests[0]);
    }

    #[tokio::test]
    async fn a_non_200_is_a_status_error() {
        let stub = Stub::start(vec![Behaviour::Reply(http_status(
            403,
            "Forbidden",
            "nope",
        ))]);
        let err = source(&stub.url).fetch_post("abc").await.unwrap_err();

        match err {
            FetchError::Status { status, body } => {
                assert_eq!(status, 403);
                assert_eq!(body, "nope");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    /// A 200 with no post is terminal: Medium answered, and asking again gets
    /// the same answer.
    #[tokio::test]
    async fn a_200_without_a_post_is_terminal() {
        let stub = Stub::serving(r#"{"data":{"post":null}}"#);

        let sleeper = Arc::new(CountingSleeper::default());
        let source = source(&stub.url).with_sleeper(sleeper.clone());

        assert_eq!(source.fetch_post("abc").await, Err(FetchError::NoPost));
        assert_eq!(sleeper.calls(), 0, "a terminal failure must not retry");
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_a_bad_body() {
        let stub = Stub::serving("<html>gateway error</html>");
        assert!(matches!(
            source(&stub.url).fetch_post("abc").await,
            Err(FetchError::BadBody(_))
        ));
    }

    /// The failing exit is ejected and the retry goes out again — here it
    /// succeeds, which is the whole premise of moving the pool in-process.
    #[tokio::test]
    async fn a_non_200_twice_is_retried_then_reported() {
        let stub = Stub::start(vec![
            Behaviour::Reply(http_status(500, "Server Error", "boom")),
            Behaviour::Reply(http_ok(VALID)),
        ]);

        let sleeper = Arc::new(CountingSleeper::default());
        let payload = source(&stub.url)
            .with_sleeper(sleeper.clone())
            .fetch_post("abc")
            .await
            .unwrap_or_default();

        assert_eq!(payload["data"]["post"]["id"], "abc");
        assert_eq!(sleeper.calls(), 1, "one sleep between two attempts");
    }

    /// A hung endpoint must surface as a retryable timeout rather than hanging
    /// the caller forever.
    #[tokio::test]
    async fn a_hanging_endpoint_times_out() {
        let stub = Stub::start(vec![Behaviour::Hang]);
        let source = HttpPostSource::new(ReqwestTransport::new().unwrap(), direct_pool())
            .with_endpoint(&stub.url)
            .with_timeout(Duration::from_millis(200))
            .with_policy(RetryPolicy::new(1, Duration::from_millis(1)));

        let err = source.fetch_post("abc").await.unwrap_err();
        assert!(
            matches!(err, FetchError::Transport(TransportError::Timeout)),
            "expected a timeout, got {err:?}"
        );
    }

    /// A failing transport must eject its exit, so the retry picks another.
    #[tokio::test]
    async fn a_transport_failure_ejects_the_proxy() {
        struct FailingTransport;

        #[async_trait]
        impl Transport for FailingTransport {
            async fn send(
                &self,
                _request: TransportRequest,
            ) -> Result<TransportResponse, TransportError> {
                Err(TransportError::Proxy("connection refused".into()))
            }
        }

        let pool = Arc::new(ProxyPool::new(
            vec![
                ProxyEndpoint::new("socks5://wgcf1:1080"),
                ProxyEndpoint::new("socks5://wgcf2:1080"),
            ],
            Arc::new(AlwaysHealthy),
        ));

        let source = HttpPostSource::new(FailingTransport, Arc::clone(&pool))
            .with_policy(RetryPolicy::new(2, Duration::from_millis(1)))
            .with_sleeper(Arc::new(CountingSleeper::default()))
            .with_endpoint("http://127.0.0.1:1");

        let err = source.fetch_post("abc").await.unwrap_err();
        assert!(matches!(err, FetchError::Transport(_)));
        assert_eq!(
            pool.healthy_count(),
            0,
            "both exits should have been ejected"
        );
    }

    /// A body far larger than the preview limit must not panic on a character
    /// boundary.
    #[test]
    fn preview_bounds_long_bodies() {
        let long = "😀".repeat(1000);
        let shown = preview(long.as_bytes());
        assert_eq!(shown.chars().count(), 301);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn preview_keeps_short_bodies() {
        assert_eq!(preview(b"nope"), "nope");
    }

    /// Invalid UTF-8 in an error page must not panic either.
    #[test]
    fn preview_tolerates_invalid_utf8() {
        assert!(preview(&[0xff, 0xfe]).contains('\u{fffd}'));
    }

    // ---------------------------------------------------------------------
    // The anonymous source, and the cookie pair
    // ---------------------------------------------------------------------
    //
    // These two tests are only meaningful together. The first alone would pass in
    // a crate where nothing ever set a `Cookie` header at all — a test of an
    // absence that proves nothing. The second is what makes it a test of *this*
    // code: the same question, asked of a source that is configured to send one.

    /// Fails if any credential-bearing header is on the wire.
    fn assert_no_credentials(requests: &[TransportRequest]) {
        for request in requests {
            // Non-vacuity: a request carrying no headers at all would satisfy the
            // loop below without testing anything.
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("user-agent")),
                "the request carries headers, so `cookie` being absent means something"
            );
            for (name, _) in &request.headers {
                assert!(
                    !name.eq_ignore_ascii_case("cookie")
                        && !name.eq_ignore_ascii_case("authorization"),
                    "{name} reached the wire: {:?}",
                    request.headers
                );
            }
        }
    }

    /// A transport that always answers 200 with [`VALID`], recording what it sent.
    fn scripted_ok() -> ScriptedTransport {
        ScriptedTransport::new(
            vec![],
            crate::test_support::response(
                200,
                &[("content-type", "application/json")],
                VALID.as_bytes(),
            ),
        )
    }

    fn anonymous(transport: ScriptedTransport) -> AnonymousSource {
        AnonymousSource::new(
            transport,
            direct_pool(),
            Duration::from_secs(5),
            "http://127.0.0.1:1",
        )
    }

    /// **The API's source cannot carry the account's session.**
    #[tokio::test]
    async fn the_anonymous_source_sends_no_cookie_header() {
        let transport = scripted_ok();
        let payload = anonymous(transport.clone())
            .fetch_post("abc")
            .await
            .expect("the script is a 200");

        assert_eq!(
            payload["data"]["post"]["id"], "abc",
            "the anonymous source really did fetch, rather than failing early"
        );

        let requests = transport.requests();
        assert_eq!(requests.len(), 1, "one attempt, one request");
        assert_no_credentials(&requests);
    }

    /// **The contrast that gives the test above its meaning.**
    ///
    /// `HttpPostSource` *can* send the cookie. This is the same question asked of
    /// a source that is told to, and it fails if the header-writing code ever
    /// stops being reachable — which is exactly the way
    /// `the_anonymous_source_sends_no_cookie_header` would otherwise pass for the
    /// wrong reason.
    #[tokio::test]
    async fn an_authenticated_source_does_send_the_cookie_header() {
        let transport = scripted_ok();
        let source = HttpPostSource::new(transport.clone(), direct_pool())
            .with_endpoint("http://127.0.0.1:1")
            .with_timeout(Duration::from_secs(5))
            .with_auth_cookies(Some("sid=secret; uid=42".into()));

        source.fetch_post("abc").await.expect("the script is a 200");

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("cookie")
                    && value == "sid=secret; uid=42"),
            "the cookie path exists, so its absence from the anonymous source is \
             the newtype doing work: {:?}",
            requests[0].headers
        );
    }

    /// The anonymous source keeps the rest of the path: the retry loop, the body,
    /// the endpoint. A source that quietly failed early would satisfy the cookie
    /// test for the wrong reason, so this asks the question the other way round.
    #[tokio::test]
    async fn the_anonymous_source_still_retries_and_reports() {
        let transport = ScriptedTransport::new(
            vec![Ok(crate::test_support::response(500, &[], b"boom"))],
            crate::test_support::response(200, &[], VALID.as_bytes()),
        );

        let payload = anonymous(transport.clone())
            .fetch_post("abc")
            .await
            .expect("the second reply is a 200");
        assert_eq!(payload["data"]["post"]["id"], "abc");
        assert_eq!(transport.request_count(), 2, "the 500 was retried");
        assert_no_credentials(&transport.requests());
    }
}
