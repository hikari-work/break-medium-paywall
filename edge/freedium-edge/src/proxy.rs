//! The Pingora service: proxy everything, mirror a sample.
//!
//! # This is a pass-through, and that is the whole feature
//!
//! `upstream_peer` sends every request to the primary — Python, in this phase.
//! No deny-list, no static-file serving, no `pingora-cache`, no rate limiting:
//! those are the real edge's jobs in Fase 7, and putting them in now would mean
//! Fase 4's evidence was gathered by a component doing four things the production
//! edge will do differently. One thing at a time.
//!
//! # The request path, and the one thing added to it
//!
//! ```text
//! request_filter   → is this in the sample?        (a hash, no allocation)
//! upstream_peer    → Python
//! response_filter  → record the status and type
//! body_filter      → accumulate bytes, if eligible  ← the only cost
//! logging          → spawn the comparison, return
//! ```
//!
//! The client's response is complete before `logging` runs. Everything the
//! comparison does — a second HTTP request, two HTML parses, a file write —
//! happens in a spawned task whose errors are logged and dropped. There is no
//! path by which the shadow can change, delay, or fail the response it is
//! shadowing; that is the property the whole phase rests on, and it is
//! structural rather than a rule anyone has to remember.
//!
//! # Fail-open, and the kill switch
//!
//! `SHADOW_ENABLED=false` makes this binary a plain proxy: `request_filter`
//! answers `Excluded` for everything, the body is never accumulated, and nothing
//! is written. Repointing one compose line at Caddy puts the old path back
//! entirely — §5's requirement that the rollback be one line.
//!
//! # The body buffer is bounded on the request path
//!
//! `response_body_filter` runs for every eligible chunk, and a response with no
//! `Content-Length` could be arbitrarily large. Over `SHADOW_MAX_BODY` the
//! context marks itself overflowed, drops the bytes it holds, and records the
//! request as excluded — a truncated body would compare as a difference and read
//! as a renderer regression, which is the one wrong answer this harness could
//! give that would cost real debugging time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use page_canonical::{Served, ShadowRecord, Side};
use pingora::http::ResponseHeader;
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, Result};

use crate::config::Config;
use crate::eligibility;
use crate::shadow::{Pending, Shadow};

/// Headers that describe *this* hop and must not be replayed onto the shadow
/// request.
///
/// `host` is here because the two servers are reached at different addresses and
/// forwarding the client's `Host` would be a lie to the shadow about which name
/// it was asked for; the shadow's own client sets it correctly. The rest are
/// connection-management headers that belong to a connection the shadow request
/// is not sharing, and `content-length` because it describes the primary's
/// request body, which a `GET` does not have.
const HOP_BY_HOP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// Headers that must not be replayed because the edge sets them itself, or
/// because forwarding them would make the shadow fetch something the primary did
/// not.
///
/// `accept-encoding` is the important one: the primary's body may be compressed
/// (in which case the request is excluded before this matters), and asking the
/// shadow for a compression the primary did not use would compare two different
/// encodings. [`crate::shadow`] forces `identity` on every shadow request for the
/// same reason.
const REWRITTEN: &[&str] = &["accept-encoding"];

/// What the edge carries from `request_filter` to `logging`.
///
/// One struct rather than several, because Pingora's context is per-request and
/// has no type-level shape — putting every field in one place is what keeps the
/// hooks' contract readable.
pub struct EdgeCtx {
    /// The path as it arrived, without the query. A declaration matches on this.
    path: String,
    /// The query string, forwarded verbatim to the shadow. `None` when there was
    /// none, which is not the same as an empty one.
    query: Option<String>,

    /// The client's headers, to replay. Empty unless the request is eligible —
    /// the clone is not paid for a request that is about to be skipped.
    headers: Vec<(String, String)>,

    /// `None` when the request is eligible. Decided once, in `request_filter`,
    /// so no later hook has to re-derive it or reach for the config.
    exclusion: Option<&'static str>,

    /// When this request reached `request_filter`. The primary's latency is
    /// measured from here, which includes whatever the edge itself took — which
    /// is the honest number for "how long did this request take".
    started: Instant,

    /// From `response_filter`.
    status: Option<u16>,
    content_type: Option<String>,
    content_encoding: Option<String>,

    /// The primary's body, accumulated in `response_body_filter`. Only ever
    /// non-empty for an eligible request.
    body: Vec<u8>,

    /// The body went over `SHADOW_MAX_BODY`; the request is excluded.
    overflow: bool,
}

impl EdgeCtx {
    fn new() -> Self {
        Self {
            path: String::new(),
            query: None,
            headers: Vec::new(),
            exclusion: None,
            started: Instant::now(),
            status: None,
            content_type: None,
            content_encoding: None,
            body: Vec::new(),
            overflow: false,
        }
    }
}

/// The service.
pub struct FreediumEdge {
    /// `host:port` of the primary. An `Arc<str>` rather than a `String` because
    /// `upstream_peer` builds a peer per request and the address is the same for
    /// all of them.
    upstream: Arc<str>,

    /// Kept here, not read through the shadow: two hooks need it directly —
    /// `request_filter` for the eligibility rules and `response_body_filter` for
    /// the body cap — and both run on the request path, where a shared `Arc` is
    /// the cheapest way to have it.
    config: Arc<Config>,

    shadow: Arc<Shadow>,
}

impl FreediumEdge {
    pub fn new(config: Arc<Config>, shadow: Shadow) -> Self {
        Self {
            upstream: Arc::from(config.upstream.as_str()),
            config,
            shadow: Arc::new(shadow),
        }
    }
}

/// The `#[async_trait]` is mandatory, not stylistic: Pingora's `ProxyHttp` is
/// declared the same way, so its async methods are boxed futures with bounds the
/// compiler will not match against plain `async fn`s in this impl. See the note
/// on the `async-trait` dependency for the error that produces.
#[async_trait]
impl ProxyHttp for FreediumEdge {
    type CTX = EdgeCtx;

    fn new_ctx(&self) -> Self::CTX {
        EdgeCtx::new()
    }

    /// Decide eligibility, and capture what the shadow request will need.
    ///
    /// Returning `Ok(false)` means "not handled here, go upstream". The rule this
    /// hook enforces is that everything the later hooks need is decided *now*, on
    /// the request, and never re-derived from the response — a decision made from
    /// the response would be a decision that could differ between the two
    /// servers, which is the whole thing being measured.
    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let header = session.req_header();

        let path = header.uri.path().to_string();
        let query = header.uri.query().map(str::to_string);

        ctx.started = Instant::now();
        ctx.exclusion = eligibility::exclusion(
            header.method.as_str(),
            &path,
            query.as_deref(),
            &self.config,
        );

        // Only for eligible requests: this is an allocation per header, and an
        // excluded request should cost the hash and nothing else.
        if ctx.exclusion.is_none() {
            ctx.headers = replayable_headers(&header.headers);
        }

        ctx.path = path;
        ctx.query = query;

        Ok(false)
    }

    /// The primary. Plain HTTP: TLS terminates outside the edge (§2.8).
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // `false` for TLS, and an empty SNI — there is no certificate to verify
        // here because there is no TLS. The address is resolved by
        // `HttpPeer::new` through std's `ToSocketAddrs`, which is what makes a
        // compose service name work.
        Ok(Box::new(HttpPeer::new(
            &*self.upstream,
            false,
            String::new(),
        )))
    }

    /// Record what the primary answered. No body yet.
    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx.exclusion.is_some() {
            return Ok(());
        }

        // Through `Deref` to `http::response::Parts`; `ResponseHeader`'s own
        // fields are private and Pingora offers no accessors for either.
        ctx.status = Some(upstream_response.status.as_u16());
        ctx.content_type = header_string(&upstream_response.headers, "content-type");
        ctx.content_encoding = header_string(&upstream_response.headers, "content-encoding");

        Ok(())
    }

    /// Accumulate the primary's body, if this request is being compared.
    ///
    /// Sync, and on the response path: every byte a client receives passes
    /// through here. It does one length check and one `extend_from_slice` for an
    /// eligible request, and returns immediately for every other request — which
    /// is the majority once `SHADOW_SAMPLE` is lowered.
    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if ctx.exclusion.is_some() || ctx.overflow {
            return Ok(None);
        }

        let Some(chunk) = body.as_ref() else {
            return Ok(None);
        };

        if ctx.body.len() + chunk.len() > self.config.shadow_max_body {
            ctx.overflow = true;
            // Free what was held. The request is excluded now and the buffer
            // would otherwise sit there for the rest of the response.
            ctx.body = Vec::new();
            tracing::warn!(
                path = %ctx.path,
                "primary body exceeded SHADOW_MAX_BODY; not comparing this request"
            );
            return Ok(None);
        }

        ctx.body.extend_from_slice(chunk);
        Ok(None)
    }

    /// Hand the comparison to a task and return.
    ///
    /// Pingora runs this after the response is finished, and it is `async` — but
    /// everything heavy in here is pushed to [`Shadow::spawn`], so the hook's own
    /// work is a few moves and one `tokio::spawn`.
    ///
    /// `error` is Pingora's per-request result. Any error at all means the client
    /// did not get a complete, trustworthy response, so there is nothing to
    /// compare against and the request is recorded as `primary-error` — the
    /// outcome the report counts separately so that a broken primary cannot read
    /// as a clean run.
    async fn logging(&self, _session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        let primary_ms = ctx.started.elapsed().as_secs_f64() * 1000.0;

        // Taken first, so every early return below still records a path.
        let path = std::mem::take(&mut ctx.path);
        let query = ctx.query.take();
        let primary_side = Side {
            status: ctx.status.unwrap_or(0),
            ms: primary_ms,
            bytes: ctx.body.len(),
        };

        if let Some(error) = error {
            tracing::debug!(path = %path, %error, "primary request failed; nothing to compare");
            self.write(ShadowRecord::primary_error(
                crate::shadow::now_ms(),
                &path,
                query.as_deref(),
                &error.to_string(),
                primary_side,
            ));
            return;
        }

        if let Some(reason) = ctx.exclusion {
            self.write(ShadowRecord::excluded(
                crate::shadow::now_ms(),
                &path,
                query.as_deref(),
                reason,
                primary_side,
            ));
            return;
        }

        // The two conditions the response had to meet, checked here rather than
        // in `response_body_filter` because both are facts about the response as
        // a whole. Neither is a difference; both are exclusions with a reason, so
        // a rule that quietly swallowed the corpus is visible in the report.
        if ctx.overflow {
            self.write(ShadowRecord::excluded(
                crate::shadow::now_ms(),
                &path,
                query.as_deref(),
                eligibility::BODY_TOO_LARGE,
                primary_side,
            ));
            return;
        }
        if !eligibility::encoding_allows_comparison(ctx.content_encoding.as_deref()) {
            self.write(ShadowRecord::excluded(
                crate::shadow::now_ms(),
                &path,
                query.as_deref(),
                eligibility::PRIMARY_ENCODED,
                primary_side,
            ));
            return;
        }

        let Some(status) = ctx.status else {
            // No exclusion, no error, and no response — Pingora does not do this,
            // but a `Served` invented from a missing status would be a fake
            // comparison, so it is recorded as the failure it is.
            self.write(ShadowRecord::primary_error(
                crate::shadow::now_ms(),
                &path,
                query.as_deref(),
                "no upstream response and no error",
                primary_side,
            ));
            return;
        };

        let pending = Pending {
            path,
            query,
            headers: std::mem::take(&mut ctx.headers),
            primary: Served::new(
                status,
                ctx.content_type.take(),
                String::from_utf8_lossy(&ctx.body).into_owned(),
            ),
            primary_side,
        };

        self.shadow.spawn(pending);
    }
}

impl FreediumEdge {
    /// Append a record the comparison did not produce.
    ///
    /// The comparison's own records go through [`Shadow::spawn`] so they can be
    /// written on a background thread; the ones built in `logging` are already
    /// final, and writing them synchronously keeps their ordering relative to the
    /// request honest — a soak's log should read in the order the edge did the
    /// work.
    fn write(&self, record: ShadowRecord) {
        self.shadow.record(&record);
    }
}

/// The client's headers, minus the ones that describe a different hop.
///
/// Names are already lowercase — both HTTP/2 requires it and `http::HeaderName`
/// stores it that way — so the shadow's client cannot be handed a header it
/// rejects.
fn replayable_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            !HOP_BY_HOP.contains(&name) && !REWRITTEN.contains(&name)
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

/// One header's value, as a string, if it is there and readable.
fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(|value| value.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    /// Everything the app could vary on travels; everything that describes the
    /// connection it arrived on does not. A forwarded `connection: keep-alive`
    /// would be a header the shadow's client is entitled to reject, and a
    /// forwarded `host` would tell the shadow it was asked for a name it was not.
    #[test]
    fn the_connection_and_the_client_travel_but_the_connection_headers_do_not() {
        let replayable = replayable_headers(&headers(&[
            ("accept", "text/html"),
            ("cookie", "session=abc"),
            ("x-forwarded-for", "203.0.113.7"),
            ("host", "freedium.cfd"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("content-length", "0"),
            ("accept-encoding", "gzip, br"),
        ]));

        let names: Vec<&str> = replayable.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec!["accept", "cookie", "x-forwarded-for"],
            "the two lists disagree about what travels"
        );
    }

    /// Values are preserved exactly, including ones that are not valid ASCII.
    ///
    /// A header the shadow's client cannot represent is dropped rather than
    /// corrupted — `HeaderValue::to_str` fails on opaque bytes, and forwarding a
    /// lossy version would compare the two servers on a request neither received.
    #[test]
    fn a_value_that_is_not_text_is_dropped_rather_than_mangled() {
        let mut map = HeaderMap::new();
        map.insert("accept", http::HeaderValue::from_static("text/html"));
        map.insert(
            "x-opaque",
            http::HeaderValue::from_bytes(&[0x80, 0xff]).unwrap(),
        );

        let replayable = replayable_headers(&map);
        assert_eq!(replayable.len(), 1);
        assert_eq!(replayable[0].0, "accept");
        assert_eq!(replayable[0].1, "text/html");
    }

    /// The header lookup is case-insensitive and trims, because the value is
    /// compared against `identity` and a stray space would make an ordinary
    /// uncompressed response look encoded — turning every request into an
    /// exclusion, which reads as a corpus that mysteriously emptied.
    #[test]
    fn a_header_is_found_whatever_its_case_and_trimmed() {
        let map = headers(&[("content-encoding", " identity ")]);
        assert_eq!(
            header_string(&map, "content-encoding").as_deref(),
            Some("identity")
        );
        assert!(eligibility::encoding_allows_comparison(
            header_string(&map, "content-encoding").as_deref()
        ));

        assert_eq!(header_string(&headers(&[]), "content-type"), None);
    }

    /// A fresh context is not eligible, holds nothing, and has no status.
    ///
    /// Worth pinning because `exclusion` being `None` means "eligible": if the
    /// default were ever inverted, every request would be compared and the
    /// kill switch would do nothing.
    #[test]
    fn a_new_context_is_empty_and_eligible() {
        let ctx = EdgeCtx::new();
        assert!(ctx.exclusion.is_none(), "the default is eligible");
        assert!(ctx.path.is_empty());
        assert!(ctx.query.is_none());
        assert!(ctx.headers.is_empty());
        assert!(ctx.status.is_none());
        assert!(ctx.body.is_empty());
        assert!(!ctx.overflow);
    }
}
