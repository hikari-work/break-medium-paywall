//! Talking to Medium.
//!
//! This is the only crate in the workspace that reaches the outbound network.
//! Everything it does is in service of one method — [`source::PostSource::fetch_post`]
//! — and every part of it is designed so that the one unresolved question
//! (how the request is made, §3.1) can change without anything else moving.
//!
//! # Layout
//!
//! | Module | What it holds |
//! |---|---|
//! | [`request`] | The endpoint, the query, and the exact headers — transcribed from `api.py` |
//! | [`response`] | Turning a payload into a verdict, `core.py`'s validation order |
//! | [`error`] | [`error::FetchError`], and which of its cases are worth retrying |
//! | [`retry`] | The retry loop, with the legacy `reason` bug deliberately not reproduced |
//! | [`proxy`] | The in-process WARP pool that replaces HAProxy |
//! | [`http`] | [`http::HttpPostSource`] — the whole path, over a pluggable [`http::Transport`] |
//! | [`http`] | [`http::AnonymousSource`] — the same, with no path to a credential |//! | [`source`] | The trait, and nothing else |
//! | [`resolver`] | [`resolver::HttpLinkResolver`] — `link.medium.com` → post id |
//! | [`media`] | [`media::MediaFetcher`] — the `@miro/` and `render_iframe/` passthroughs |
//!
//! # The two ways to get a post
//!
//! [`http::HttpPostSource`] is the real one and is **not production-ready**:
//! its [`http::ReqwestTransport`] has no TLS impersonation and Medium will
//! reject it. See that module's docs, and §3.1.
//!
//! Everything else that needs posts takes a `dyn PostSource`, so a test — or
//! Fase 3's server, before SPIKE-1 resolves — can supply fixed JSON instead.
//!
//! [`http::AnonymousSource`] is the same fetch with the credential removed at the
//! type level, and it exists because `MEDIUM_AUTH_COOKIES` is set in production:
//! read its docs before touching anything that serves `/api/v1`.
//!
//! # Three callers, one transport
//!
//! [`http::HttpPostSource`], [`resolver::HttpLinkResolver`] and
//! [`media::MediaFetcher`] are the three outbound shapes the server needs, and
//! all three go through the same [`http::Transport`]. That is the point of the
//! seam: §3.1's verdict replaces the transport, and none of this changes.

pub mod error;
pub mod http;
pub mod media;
pub mod proxy;
pub mod request;
pub mod resolver;
pub mod response;
pub mod retry;
pub mod source;

/// A real HTTP conversation and a scripted transport, for the unit tests of all
/// three callers. Not compiled outside `cfg(test)`.
#[cfg(test)]
pub(crate) mod test_support;
