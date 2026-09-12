//! The impersonating transport — production's way past Medium's bot check.
//!
//! `medium.com/_/graphql` rejects `reqwest`, whose TLS stack has a fingerprint
//! of its own ([`crate::http::ReqwestTransport`] documents that, and is kept for
//! the paths that do not need impersonation). This module is the answer to
//! SPIKE-1 (§3.1): a [`wreq`] client presenting a real Chrome fingerprint.
//!
//! It is implemented against [`crate::http::Transport`] rather than called
//! directly, so it is a drop-in at the seam the rest of the crate was written
//! around. Nothing here knows about the server.
//!
//! # Why only this path impersonates
//!
//! Legacy does the same, and that is the specification rather than a
//! coincidence. `api.py` builds its `curl_cffi` session with
//! `impersonate="chrome110"`; the miro passthrough
//! (`legacy/web/server/handlers/miro.py`) and the short-link resolver
//! (`legacy/medium-parser/medium_parser/utils.py`) both use plain `aiohttp`
//! with a User-Agent *string* and no TLS impersonation at all. So
//! [`crate::media::MediaFetcher`] and [`crate::resolver::HttpLinkResolver`] stay
//! on `reqwest`, and only [`crate::http::HttpPostSource`] and
//! [`crate::http::AnonymousSource`] — the same endpoint under two auth states —
//! come through here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::TransportError;
use crate::http::{Method, Transport, TransportRequest, TransportResponse};

/// The impersonation profile to present.
///
/// An enum of this crate's own rather than `wreq_util::Emulation` held directly,
/// because the profile is named in configuration and recorded in the SPIKE-1
/// sidecar metadata, and because `wreq` exports a *different* type also called
/// `Emulation`. Keeping the mapping in one place means the name that gets
/// recorded is the name that was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Chrome110,
    Chrome120,
    Chrome124,
    Chrome136,
}

impl Profile {
    /// Every profile that can be named.
    ///
    /// `chrome110` is the one `api.py:75` pins and the one SPIKE-1 measured; the
    /// rest exist so a profile A/B is a config change rather than a rebuild.
    /// **A comparison across profiles is not a comparison of clients** — changing
    /// this changes the fingerprint, so the baseline has to move with it.
    pub const ALL: [Profile; 4] = [
        Profile::Chrome110,
        Profile::Chrome120,
        Profile::Chrome124,
        Profile::Chrome136,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|profile| profile.name().eq_ignore_ascii_case(name))
    }

    pub const fn name(self) -> &'static str {
        match self {
            Profile::Chrome110 => "chrome110",
            Profile::Chrome120 => "chrome120",
            Profile::Chrome124 => "chrome124",
            Profile::Chrome136 => "chrome136",
        }
    }

    /// The `wreq_util` profile this names.
    ///
    /// Returns [`wreq_util::Profile`], not `wreq_util::Emulation` — the names are
    /// misleading. `wreq_util::Emulation` is a *builder struct* (profile +
    /// platform + http2 + headers, all defaulted), and `Emulation::Chrome110` is
    /// an associated const on it that holds a `Profile`. `Profile` alone is
    /// exactly what `.emulation()` wants: `impl IntoEmulation for Profile` goes
    /// through `Emulation::builder().profile(self).build()`, which leaves
    /// `Platform::default()` (`MacOS`) — the same platform curl-impersonate's
    /// `chrome110` labels itself with, so `sec-ch-ua-platform` agrees.
    ///
    /// # The profile name is not the more interesting half of pinning
    ///
    /// `wreq_util`'s `v110` module passes `v100::build_emulation` as its TLS and
    /// HTTP/2 source (see `emulate/profile/chrome.rs`): the Chrome 110 profile's
    /// *headers* are 110's, and its *ClientHello and HTTP/2 settings are
    /// Chrome 100's*. curl_cffi's `chrome110` is a different construction — a
    /// patch to curl built from its own fingerprint capture. So the two profiles
    /// sharing a name guarantees nothing about the bytes, which is the risk
    /// `RUST_REWRITE_PLAN.md` §3.1 flagged. Only the measurement settles it —
    /// and it did, at parity 1.0000 against the `curl_cffi` baseline. This
    /// comment exists so that a later failure is not misread as a transport bug.
    fn profile(self) -> wreq_util::Profile {
        match self {
            Profile::Chrome110 => wreq_util::Profile::Chrome110,
            Profile::Chrome120 => wreq_util::Profile::Chrome120,
            Profile::Chrome124 => wreq_util::Profile::Chrome124,
            Profile::Chrome136 => wreq_util::Profile::Chrome136,
        }
    }
}

/// A `wreq` client per proxy, mirroring [`crate::proxy::ProxyClients`].
///
/// `None` is the direct client and is a distinct cache key, not a missing value.
///
/// The `Arc` is load-bearing and is why this is `Clone`. The server hands one
/// transport to several callers by cloning it, and a cloned `HashMap` would give
/// each its own cache — so the direct client built eagerly in [`Self::new`] would
/// be the only shared one, and every per-exit client would be built once per
/// clone. `ProxyClients` is `Arc<Mutex<..>>` for the same reason.
#[derive(Clone)]
pub struct WreqTransport {
    clients: Arc<Mutex<HashMap<Option<String>, wreq::Client>>>,
    profile: Profile,
    timeout: Duration,
}

impl WreqTransport {
    pub fn new(profile: Profile, timeout: Duration) -> Result<Self, TransportError> {
        let transport = Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            profile,
            timeout,
        };
        // Build the direct client eagerly so a TLS/profile configuration failure
        // surfaces at boot rather than on the first request.
        transport.client_for(None)?;
        Ok(transport)
    }

    fn client_for(&self, proxy: Option<&str>) -> Result<wreq::Client, TransportError> {
        let key = proxy.map(str::to_string);
        if let Some(client) = self.lock().get(&key) {
            return Ok(client.clone());
        }

        let mut builder = wreq::Client::builder()
            // Emulation first: the docs warn it overwrites the header, HTTP/1,
            // HTTP/2 and TLS configuration, so nothing may be tuned before it.
            .emulation(self.profile.profile())
            // A *total* timeout, which is what curl's CURLOPT_TIMEOUT is and so
            // what `curl_cffi`'s `timeout=` means. Deliberately no
            // `connect_timeout`: that would move connect slowness across the
            // timeout threshold at a different point than the baseline's.
            .timeout(self.timeout)
            // Following a redirect would turn a clean status code into a body
            // that fails to parse, which the caller cannot distinguish.
            .redirect(wreq::redirect::Policy::none());

        builder = match proxy {
            Some(url) => builder.proxy(
                wreq::Proxy::all(url).map_err(|err| TransportError::Proxy(err.to_string()))?,
            ),
            // Also turns off system-proxy auto-detection, so an `HTTP_PROXY` in
            // the environment cannot silently turn a direct request into a
            // proxied one. `ReqwestTransport` does *not* do this — a deliberate
            // asymmetry, because here a direct request is the measured baseline
            // and there it is not. Do not "fix" one to match the other.
            None => builder.no_proxy(),
        };

        let client = builder
            .build()
            .map_err(|err| TransportError::Other(err.to_string()))?;
        self.lock().insert(key, client.clone());
        Ok(client)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Option<String>, wreq::Client>> {
        self.clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait::async_trait]
impl Transport for WreqTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        let used_proxy = request.proxy.is_some();
        let client = self.client_for(request.proxy.as_deref())?;

        let mut builder = match request.method {
            Method::Get => client.get(&request.url).timeout(request.timeout),
            Method::Post => client
                .post(&request.url)
                .timeout(request.timeout)
                .body(request.body),
        };
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

/// Blame the proxy when a proxy was used and the failure was in connecting or
/// sending.
///
/// This is [`crate::http::classify`] with `wreq::Error` in place of
/// `reqwest::Error`, and the two must agree: the heuristic decides which exit a
/// [`crate::proxy::ProxyPool`] ejects, so a divergence would eject the wrong
/// exit depending on which transport happened to hit the failure. The
/// `both_transports_classify_a_hang_as_a_timeout` test below keeps them
/// honest.
fn classify(err: &wreq::Error, used_proxy: bool) -> TransportError {
    if err.is_timeout() {
        TransportError::Timeout
    } else if used_proxy && (err.is_connect() || err.is_request()) {
        TransportError::Proxy(err.to_string())
    } else {
        TransportError::Other(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config value and the profile it maps to must stay in step in both
    /// directions, or a run recorded as `chrome110` could have been made as
    /// another profile — which would make the SPIKE-1 measurement
    /// unreproducible, and its sidecar misleading rather than merely incomplete.
    #[test]
    fn profile_names_round_trip_and_reject_typos() {
        for profile in Profile::ALL {
            assert_eq!(Profile::parse(profile.name()), Some(profile));
        }
        assert_eq!(Profile::parse("chrome111"), None);
        assert_eq!(Profile::parse(""), None);
    }

    /// `chrome110` is what `api.py:75` pins, what SPIKE-1 measured, and what the
    /// server's config defaults to. If the first entry of `ALL` ever stops being
    /// it, the default silently stops being the measured thing.
    #[test]
    fn the_default_profile_is_the_measured_one() {
        assert_eq!(Profile::ALL[0], Profile::Chrome110);
        assert_eq!(Profile::Chrome110.name(), "chrome110");
    }

    /// [`classify`] here and `crate::http::classify` both decide which exit a
    /// `ProxyPool` ejects, so they must agree.
    ///
    /// They cannot be compared directly — `wreq::Error` and `reqwest::Error` are
    /// different types and neither is constructible by hand. So this drives both
    /// transports against the same stub and pins the one arm both can be made to
    /// reach with no proxy in the picture: a timeout.
    ///
    /// It doubles as the proof that the emulating client speaks plain HTTP/1.1
    /// to a loopback stub *at all*, which the offline 502/504 tests depend on
    /// through `MEDIUM_GRAPHQL_ENDPOINT`. Emulation is a TLS and HTTP/2
    /// fingerprint; it must not make a cleartext request impossible.
    #[tokio::test]
    async fn both_transports_classify_a_hang_as_a_timeout() {
        use crate::http::ReqwestTransport;
        use crate::test_support::{Behaviour, Stub};

        let stub = Stub::start(vec![Behaviour::Hang]);
        let timeout = Duration::from_millis(200);

        let impersonated = WreqTransport::new(Profile::Chrome110, timeout)
            .expect("the impersonating client builds");
        let plain = ReqwestTransport::new().expect("a reqwest client builds");

        let request = || TransportRequest {
            url: stub.url.clone(),
            method: Method::Get,
            headers: Vec::new(),
            body: Vec::new(),
            proxy: None,
            timeout,
        };

        let from_wreq = impersonated
            .send(request())
            .await
            .expect_err("the stub never answers");
        let from_reqwest = plain
            .send(request())
            .await
            .expect_err("the stub never answers");

        assert!(
            matches!(from_wreq, TransportError::Timeout),
            "wreq should call a hang a timeout, got {from_wreq:?}"
        );
        assert!(
            matches!(from_reqwest, TransportError::Timeout),
            "reqwest should call a hang a timeout, got {from_reqwest:?}"
        );
    }
}
