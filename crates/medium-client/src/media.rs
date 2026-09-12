//! The two passthrough fetches: `/@miro/*` and `/render_iframe/*`.
//!
//! Both are "get bytes from Medium and hand them back untouched", and both
//! exist because the browser cannot make the request itself —
//! `miro.medium.com` and `medium.com/media/*` are not CORS-friendly, and the
//! app's own origin is what has to serve them.
//!
//! | | `@miro/` | `render_iframe/` |
//! |---|---|---|
//! | Upstream | `miro.medium.com/{id}` | `medium.com/media/{id}` |
//! | Exit | direct — `miro_proxy`'s `use_proxy` defaults to `False`, and the one caller takes the default | through the pool, one exit at random |
//! | Body | bytes, verbatim | text, patched — but not here |
//!
//! # Two things this deliberately does not do
//!
//! **It does not patch the iframe body.** `iframe.py:47-52` replaces
//! `document.domain = document.domain` and then runs the whole document through
//! `BeautifulSoup.prettify()`. The replacement is a workaround for the sandboxed
//! frame and is kept; `prettify` reindents the entire document and is dropped
//! (Fase 3 decision 3) because it changes the bytes without changing the page,
//! and it is the only CPU-bound step in the request path besides the html5lib
//! round-trip that decision 2 removes. The replacement itself belongs with the
//! handler — it is a string transformation, not a network concern, and this
//! crate is the only one allowed to touch the outbound network.
//!
//! **It does not decide the response status.** Both legacy handlers build a bare
//! `fastapi.Response`, whose default is 200, and neither passes the upstream
//! status — so a missing image comes back as `200` with a 404 body inside it.
//! That is a quirk worth reproducing for parity, so [`Media`] carries the
//! upstream status and the handler is responsible for discarding it. Making the
//! status authoritative here would be an invisible behaviour change on every
//! broken image in the archive.
//!
//! # The id is concatenated, not escaped
//!
//! Both ids come straight off the request path and are appended to a fixed
//! prefix without any encoding. That is what the legacy handlers do, and
//! reproducing it is deliberate: escaping here would fetch *different* URLs than
//! production does, which is exactly the kind of silent divergence the parity
//! gate cannot see — it would show up as a Rust-side 400 that Python never
//! produces. Whether the legacy app should escape is a security question about
//! the legacy app, and §8 is where that belongs.
//!
//! # Content-Type
//!
//! `miro_proxy` reads the upstream `Content-Type` and forwards it, so an SVG, a
//! PNG and a video are all served as whatever `miro.medium.com` labelled them.
//! A response without that header raised `KeyError` in the legacy code; here it
//! is `None` and the caller picks a default.

use std::sync::Arc;
use std::time::Duration;

use crate::error::FetchError;
use crate::http::{Transport, TransportRequest};
use crate::proxy::{ProxyChoice, ProxyPool};
use crate::retry::{RetryPolicy, Sleeper, TokioSleeper, with_retry};

/// The browser user agent all three of the legacy passthroughs send.
///
/// One constant because it is one string: `utils.py:249` (the short-link
/// resolver), `miro.py:14` and `iframe.py:29` are byte-identical, and it is
/// *not* [`crate::request::USER_AGENT`] — that one is the `YandexMobileBot`
/// iPhone string the GraphQL endpoint is sent, and it is wrong for these.
pub const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/83.0.4103.116 Safari/537.36";

/// `miro.py:12`.
pub const MIRO_ENDPOINT: &str = "https://miro.medium.com/";

/// `iframe.py:32`.
pub const IFRAME_ENDPOINT: &str = "https://medium.com/media/";

/// `config.REQUEST_TIMEOUT` (`config.py:20`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(12);

/// What came back, in the terms the handler needs to build a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    /// The **upstream** status. See the module doc: the legacy handlers answer
    /// 200 regardless, so a caller that wants parity logs this and does not
    /// send it.
    pub status: u16,
    /// The upstream `Content-Type`, verbatim, including any parameters.
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl Media {
    /// The body as text, replacing anything that is not valid UTF-8.
    ///
    /// `aiohttp`'s `await request.text()` decodes with the response's charset
    /// and substitutes on error, so this is the same reading and not a lossy
    /// shortcut. Only the iframe path needs it; `@miro/` is bytes.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

/// Fetches `miro.medium.com` and `medium.com/media`.
///
/// Holds the pool but does not always use it — [`Self::fetch_miro`] goes out
/// directly, [`Self::fetch_iframe`] takes an exit. That asymmetry is the
/// legacy's, not an oversight.
pub struct MediaFetcher<T: Transport> {
    transport: T,
    pool: Arc<ProxyPool>,
    miro_endpoint: String,
    iframe_endpoint: String,
    timeout: Duration,
    policy: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl<T: Transport> MediaFetcher<T> {
    pub fn new(transport: T, pool: Arc<ProxyPool>) -> Self {
        Self {
            transport,
            pool,
            miro_endpoint: MIRO_ENDPOINT.to_string(),
            iframe_endpoint: IFRAME_ENDPOINT.to_string(),
            timeout: DEFAULT_TIMEOUT,
            policy: RetryPolicy::DEFAULT,
            sleeper: Arc::new(TokioSleeper),
        }
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

    /// Points both fetches at a local server. Tests only.
    pub fn with_endpoints(mut self, miro: impl Into<String>, iframe: impl Into<String>) -> Self {
        self.miro_endpoint = miro.into();
        self.iframe_endpoint = iframe.into();
        self
    }

    /// `GET https://miro.medium.com/{miro_data}`, directly.
    ///
    /// `miro_data` is everything after `@miro/` in the request path and is
    /// appended verbatim. The legacy handler does no validation on it, and
    /// neither does this — see the escaping note in the module docs.
    pub async fn fetch_miro(&self, miro_data: &str) -> Result<Media, FetchError> {
        self.fetch("miro", format!("{}{miro_data}", self.miro_endpoint), false)
            .await
    }

    /// `GET https://medium.com/media/{iframe_id}`, through the pool.
    ///
    /// `iframe.py:28` picks `random.choice(config.PROXY_LIST)`, so this rotates
    /// like every other pooled request rather than pinning an exit.
    pub async fn fetch_iframe(&self, iframe_id: &str) -> Result<Media, FetchError> {
        self.fetch(
            "iframe",
            format!("{}{iframe_id}", self.iframe_endpoint),
            true,
        )
        .await
    }

    /// One fetch, retried, rotating the exit between attempts.
    ///
    /// `through_pool` is `false` for `@miro/` — see the module doc.
    async fn fetch(
        &self,
        what: &'static str,
        url: String,
        through_pool: bool,
    ) -> Result<Media, FetchError> {
        with_retry(self.policy, self.sleeper.as_ref(), |attempt| {
            self.attempt(what, &url, through_pool, attempt)
        })
        .await
    }

    /// One attempt: pick an exit, send, collect.
    ///
    /// The exit is chosen **per attempt**, not per fetch, which is what makes
    /// the eject below worth doing: the next call to [`ProxyPool::next`] picks a
    /// replacement, so a retry does not go back to the exit that just failed.
    /// [`HttpPostSource::attempt`](crate::http::HttpPostSource) is arranged the
    /// same way for the same reason.
    async fn attempt(
        &self,
        what: &'static str,
        url: &str,
        through_pool: bool,
        attempt: u32,
    ) -> Result<Media, FetchError> {
        let choice = if through_pool {
            self.pool.next()?
        } else {
            ProxyChoice::Direct
        };
        let proxy = match &choice {
            ProxyChoice::Direct => None,
            ProxyChoice::Via(endpoint) => Some(endpoint.as_str().to_string()),
        };
        tracing::debug!(what, url, attempt, ?proxy, "fetching media");

        let mut request = TransportRequest::get(
            url,
            vec![("User-Agent".to_string(), BROWSER_USER_AGENT.to_string())],
        );
        request.timeout = self.timeout;
        request.proxy = proxy;

        let response = match self.transport.send(request).await {
            Ok(response) => response,
            Err(err) => {
                if let ProxyChoice::Via(endpoint) = &choice {
                    self.pool.eject(endpoint);
                }
                return Err(FetchError::Transport(err));
            }
        };

        // The status is not checked. `raise_for_status=False` on both legacy
        // calls means a 404 body is forwarded like any other — see the module
        // doc on why the handler, not this, decides what to answer with.
        Ok(Media {
            status: response.status,
            content_type: response.header("Content-Type").map(str::to_string),
            body: response.body,
        })
    }
}

/// The `document.domain` workaround `iframe.py:48-50` applies.
///
/// The legacy code replaces a no-op assignment with a `console.log` so that the
/// frame stops trying to reach across origins. `prettify()` is not reproduced —
/// see the module doc.
///
/// Here rather than in the server because it is a property of the fetched body
/// and is the one thing about `render_iframe` that is worth a test of its own.
pub fn patch_iframe_content(content: &str) -> String {
    content.replace(
        "document.domain = document.domain",
        r#"console.log("[FREEDIUM] iframe workaround started")"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::ReqwestTransport;
    use crate::test_support::{
        Behaviour, CountingSleeper, ScriptedTransport, Stub, http_status, http_with_headers,
        one_exit_pool, response,
    };

    /// A fetcher pointed at a stub, with no exits configured.
    fn fetcher(endpoint: &str) -> MediaFetcher<ReqwestTransport> {
        MediaFetcher::new(
            ReqwestTransport::new().expect("the transport builds"),
            crate::test_support::direct_pool(),
        )
        .with_endpoints(endpoint, endpoint)
        .with_timeout(Duration::from_secs(5))
    }

    fn prefix(stub: &Stub) -> String {
        format!("{}/", stub.url)
    }

    /// The upstream's `Content-Type` is forwarded verbatim, which is what makes
    /// an image come back as an image rather than as `text/html`.
    #[tokio::test]
    async fn the_upstream_content_type_is_forwarded() {
        let stub = Stub::start(vec![Behaviour::Reply(http_with_headers(
            200,
            "OK",
            &[("Content-Type", "image/png; charset=binary")],
            "bytes",
        ))]);

        let media = fetcher(&prefix(&stub))
            .fetch_miro("v2/resize:fit:48/1*abc.png")
            .await
            .unwrap();

        assert_eq!(media.status, 200);
        assert_eq!(
            media.content_type.as_deref(),
            Some("image/png; charset=binary"),
            "the parameter is part of the value and must survive"
        );
        assert_eq!(media.body, b"bytes");
    }

    /// The id is appended to the endpoint, and the browser user agent goes out
    /// — not the GraphQL one.
    #[tokio::test]
    async fn the_miro_path_is_appended_verbatim() {
        let stub = Stub::start(vec![Behaviour::Reply(http_with_headers(
            200,
            "OK",
            &[("Content-Type", "image/jpeg")],
            "",
        ))]);

        fetcher(&prefix(&stub))
            .fetch_miro("v2/resize:fit:48/1*abc.png")
            .await
            .unwrap();

        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("GET /v2/resize:fit:48/1*abc.png HTTP/1.1\r\n"),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains(BROWSER_USER_AGENT), "{}", requests[0]);
        assert!(
            !requests[0].contains(crate::request::USER_AGENT),
            "the GraphQL user agent must not be sent here"
        );
    }

    /// **The asymmetry the module doc describes.** `fetch_miro` ignores the pool
    /// and `fetch_iframe` uses it, which is what `miro_proxy`'s `use_proxy=False`
    /// default and `iframe_proxy`'s `random.choice` amount to.
    #[tokio::test]
    async fn miro_goes_out_directly_and_the_iframe_uses_the_pool() {
        let transport = ScriptedTransport::new(
            vec![],
            response(200, &[("Content-Type", "image/png")], b"x"),
        );
        let seen = transport.clone();
        let fetcher = MediaFetcher::new(transport, one_exit_pool())
            .with_endpoints("http://miro.test/", "http://media.test/");

        fetcher.fetch_miro("1*abc.png").await.unwrap();
        fetcher.fetch_iframe("abc123").await.unwrap();

        let requests = seen.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].proxy.is_none(),
            "`@miro/` must not spend an exit"
        );
        assert!(
            requests[1].proxy.is_some(),
            "`render_iframe/` must go through one"
        );
        assert_eq!(requests[0].url, "http://miro.test/1*abc.png");
        assert_eq!(requests[1].url, "http://media.test/abc123");
    }

    /// A 404 is returned, not turned into an error. Both legacy calls pass
    /// `raise_for_status=False` and forward the body; the handler is where the
    /// (discarded) status lives.
    #[tokio::test]
    async fn a_non_200_status_is_returned_rather_than_raised() {
        let stub = Stub::start(vec![Behaviour::Reply(http_status(
            404,
            "Not Found",
            "no such image",
        ))]);

        let media = fetcher(&prefix(&stub))
            .fetch_miro("1*gone.png")
            .await
            .unwrap();
        assert_eq!(media.status, 404);
        assert_eq!(media.body, b"no such image");
    }

    /// A missing `Content-Type` is `None` rather than the `KeyError` the legacy
    /// code raised. The caller picks a default.
    #[tokio::test]
    async fn a_missing_content_type_is_none() {
        let stub = Stub::start(vec![Behaviour::Reply(http_with_headers(
            200,
            "OK",
            &[],
            "body",
        ))]);

        let media = fetcher(&prefix(&stub)).fetch_miro("x").await.unwrap();
        assert_eq!(media.content_type, None);
    }

    /// A transport failure ejects the exit it used, so the retry goes
    /// elsewhere — the same contract `HttpPostSource` has.
    #[tokio::test]
    async fn a_transport_failure_ejects_the_exit() {
        let pool = one_exit_pool();
        let sleeper = Arc::new(CountingSleeper::default());
        let fetcher = MediaFetcher::new(
            ScriptedTransport::always_failing(crate::error::TransportError::Proxy(
                "refused".into(),
            )),
            Arc::clone(&pool),
        )
        .with_endpoints("http://miro.test/", "http://media.test/")
        .with_sleeper(sleeper.clone());

        assert!(fetcher.fetch_iframe("abc").await.is_err());
        assert_eq!(pool.healthy_count(), 0, "the exit should be ejected");
        assert_eq!(sleeper.calls(), 1, "one sleep between two attempts");
    }

    #[test]
    fn the_iframe_patch_replaces_the_no_op_assignment() {
        let patched = patch_iframe_content("<script>document.domain = document.domain;</script>");
        assert_eq!(
            patched,
            r#"<script>console.log("[FREEDIUM] iframe workaround started");</script>"#
        );
    }

    /// `str.replace` in Python replaces every occurrence, and so does this.
    #[test]
    fn the_iframe_patch_replaces_every_occurrence() {
        let patched = patch_iframe_content(
            "a document.domain = document.domain b document.domain = document.domain c",
        );
        assert_eq!(patched.matches("document.domain").count(), 0);
        assert_eq!(patched.matches("FREEDIUM").count(), 2);
    }

    /// A body that never mentions the assignment comes back untouched, which is
    /// the common case for a real media page.
    #[test]
    fn the_iframe_patch_leaves_other_content_alone() {
        let html = "<html><body><h1>Media</h1></body></html>";
        assert_eq!(patch_iframe_content(html), html);
    }

    #[test]
    fn the_browser_user_agent_is_not_the_graphql_one() {
        assert_ne!(BROWSER_USER_AGENT, crate::request::USER_AGENT);
        assert!(BROWSER_USER_AGENT.starts_with("Mozilla/5.0 (X11; Linux x86_64)"));
    }

    /// `text()` must read a body the `Content-Type` lied about rather than
    /// failing: `aiohttp` substitutes here too.
    #[test]
    fn text_substitutes_invalid_utf8() {
        let media = Media {
            status: 200,
            content_type: Some("text/html".into()),
            body: vec![b'a', 0xff, b'b'],
        };
        assert_eq!(media.text(), "a\u{fffd}b");
    }
}
