//! The impersonating transport, behind the production `Transport` seam.
//!
//! Implemented against [`medium_client::http::Transport`] rather than called
//! directly, so that this spike exercises the *real* call path: when the SPIKE-1
//! verdict lands, adopting this client is a new `Transport` implementation and
//! nothing else — which is exactly the seam `http.rs` was written to provide.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use medium_client::error::TransportError;
use medium_client::http::{Method, Transport, TransportRequest, TransportResponse};

/// The impersonation profile to present.
///
/// An enum of this crate's own rather than `wreq_util::Emulation` held directly,
/// because the profile is named in CLI arguments and in the sidecar metadata, and
/// because `wreq` exports a *different* type also called `Emulation`. Keeping the
/// mapping in one place means the name that gets recorded is the name that was
/// used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Chrome110,
    Chrome120,
    Chrome124,
    Chrome136,
}

impl Profile {
    /// The profiles offered on the command line.
    ///
    /// `chrome110` is the one `api.py:75` pins; the rest exist so a profile A/B
    /// is a flag rather than a rebuild. **If this moves, the baseline must be
    /// re-run with the matching `--impersonate`** — otherwise the comparison
    /// measures the difference between two profiles, not between two clients.
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

    pub fn name(self) -> &'static str {
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
    /// `RUST_REWRITE_PLAN.md` §3.1 flagged. Only the measurement settles it; this
    /// comment exists so a gate failure is not misread as a transport bug.
    fn profile(self) -> wreq_util::Profile {
        match self {
            Profile::Chrome110 => wreq_util::Profile::Chrome110,
            Profile::Chrome120 => wreq_util::Profile::Chrome120,
            Profile::Chrome124 => wreq_util::Profile::Chrome124,
            Profile::Chrome136 => wreq_util::Profile::Chrome136,
        }
    }
}

/// A `wreq` client per proxy, mirroring `medium_client::proxy::ProxyClients`.
///
/// `None` is the direct client and is a distinct cache key, not a missing value.
pub struct WreqTransport {
    clients: Mutex<HashMap<Option<String>, wreq::Client>>,
    profile: Profile,
    timeout: Duration,
}

impl WreqTransport {
    pub fn new(profile: Profile, timeout: Duration) -> Result<Self, TransportError> {
        let transport = Self {
            clients: Mutex::new(HashMap::new()),
            profile,
            timeout,
        };
        // Build the direct client eagerly so a TLS/profile configuration failure
        // surfaces here rather than on the first request.
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
            // that fails to parse, which the record cannot distinguish.
            .redirect(wreq::redirect::Policy::none());

        builder = match proxy {
            Some(url) => builder.proxy(
                wreq::Proxy::all(url).map_err(|err| TransportError::Proxy(err.to_string()))?,
            ),
            // Also turns off system-proxy auto-detection, so an `HTTP_PROXY` in
            // the environment cannot silently turn a direct run into a proxied one.
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
/// sending — the same heuristic `medium_client::http` applies, so that a pool
/// would eject the right exit if one is ever configured here.
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
    use std::sync::Arc;

    use medium_client::http::HttpPostSource;
    use medium_client::proxy::{HealthProbe, ProxyEndpoint, ProxyPool};

    use super::*;

    /// A probe that is never called.
    ///
    /// `ProxyPool::new` takes a probe even for an empty slot list, and an empty
    /// list (`ProxyChoice::Direct` on every `next()`) is the direct run this
    /// spike makes. Returning `false` is not a lie about anything reachable: it
    /// is the only value that cannot accidentally mark an exit healthy.
    ///
    /// It lives in the test module rather than beside the transport because that
    /// is the only place it is constructed — the binary has no other use for it,
    /// and a `pub` item in a binary crate is still dead code to the compiler.
    struct NeverProbed;

    #[async_trait::async_trait]
    impl HealthProbe for NeverProbed {
        async fn probe(&self, _endpoint: &ProxyEndpoint) -> bool {
            false
        }
    }

    /// **The point of implementing `Transport` rather than calling `wreq`
    /// directly.** If the SPIKE-1 verdict is to adopt this client, the
    /// production change is one line in the server's wiring — and this test is
    /// what keeps that true, by failing to compile the moment the trait or the
    /// `HttpPostSource` constructor drifts away from it.
    ///
    /// It builds a client but sends nothing, so it needs no network and no
    /// proxy. `ProxyPool::new(vec![], ..)` is the direct run: `next()`
    /// short-circuits to `ProxyChoice::Direct` on an empty slot list and never
    /// reaches [`NeverProbed`].
    #[test]
    fn the_transport_plugs_into_the_production_source_seam() {
        let transport = WreqTransport::new(Profile::Chrome110, Duration::from_secs(12))
            .expect("the direct client builds");
        let pool = Arc::new(ProxyPool::new(Vec::new(), Arc::new(NeverProbed)));

        let source = HttpPostSource::new(transport, pool);
        // Bound to a name so the unused-variable lint cannot drop it, and read
        // back through the type the server will hold it as.
        let _: HttpPostSource<WreqTransport> = source;
    }

    /// The CLI name and the profile it maps to must stay in step in both
    /// directions, or a run recorded as `chrome110` in the sidecar could have
    /// been made as another profile — which would make the measurement
    /// unreproducible, and the sidecar misleading rather than merely incomplete.
    #[test]
    fn profile_names_round_trip_and_reject_typos() {
        for profile in Profile::ALL {
            assert_eq!(Profile::parse(profile.name()), Some(profile));
        }
        assert_eq!(Profile::parse("chrome111"), None);
        assert_eq!(Profile::parse(""), None);
    }
}
