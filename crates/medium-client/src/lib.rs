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
//! | [`http`] | [`http::AnonymousSource`] — the same, with no path to a credential |
//! | [`source`] | The trait, and nothing else |
//! | [`resolver`] | [`resolver::HttpLinkResolver`] — `link.medium.com` → post id |
//! | [`media`] | [`media::MediaFetcher`] — the `@miro/` and `render_iframe/` passthroughs |
//! | [`wreq_transport`] | [`wreq_transport::WreqTransport`] — the impersonating client (§3.1) |
//!
//! # The two ways to get a post
//!
//! [`http::HttpPostSource`] is the real one. It goes through
//! [`wreq_transport::WreqTransport`], which presents a Chrome fingerprint —
//! without it `medium.com/_/graphql` rejects the request, which is what SPIKE-1
//! settled. [`http::ReqwestTransport`] is *not* interchangeable for this:
//! read its docs for why, and for the paths where it is still the right client.
//!
//! Everything else that needs posts takes a `dyn PostSource`, so a test — or
//! Fase 3's server — can supply fixed JSON instead.
//!
//! [`http::AnonymousSource`] is the same fetch with the credential removed at the
//! type level. Its docs are the ones to read before touching anything that
//! serves `/api/v1`.
//!
//! # Three callers, two transports
//!
//! [`http::HttpPostSource`], [`resolver::HttpLinkResolver`] and
//! [`media::MediaFetcher`] are the three outbound shapes the server needs, and
//! all three go through the same [`http::Transport`] trait. That was the point
//! of the seam, and it held: §3.1's verdict replaced the transport for one of
//! the three and nothing else moved.
//!
//! They do not all use the *same* transport, and that is deliberate rather than
//! an oversight. Only the GraphQL post fetch impersonates; the resolver and the
//! media passthrough stay on [`http::ReqwestTransport`], matching what the
//! legacy implementation does — see [`wreq_transport`]'s module docs for the
//! three-way comparison that establishes it.

pub mod error;
pub mod http;
pub mod media;
pub mod proxy;
pub mod request;
pub mod resolver;
pub mod response;
pub mod retry;
pub mod source;

/// The impersonating transport. Gated on `wreq-transport`, which is on by
/// default — see the crate manifest for why, and for what turning it off buys.
#[cfg(feature = "wreq-transport")]
pub mod wreq_transport;

/// A real HTTP conversation and a scripted transport, for the unit tests of all
/// three callers. Not compiled outside `cfg(test)`.
#[cfg(test)]
pub(crate) mod test_support;
