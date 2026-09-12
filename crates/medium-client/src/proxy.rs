//! The proxy pool that replaces HAProxy.
//!
//! # Why this is not an L4 balancer
//!
//! The deployment put HAProxy in front of the WARP exits purely to round-robin
//! between them, and it fronted a single backend (`wgcf1:1080`) — the other
//! three were commented out. §2.8 deletes it and moves the pool in-process,
//! because a TCP balancer is missing the one piece of information that matters
//! here: **which request failed because of which exit**. A balancer sees a
//! connection close; this client sees "the SOCKS5 handshake to `wgcf1` timed out
//! while fetching post X", and can eject that exit and immediately retry the
//! same post elsewhere.
//!
//! The service and its `haproxy.cfg` were removed along with Fase 2's compose
//! overlay; this module is what replaced them.
//!
//! So the pool's job is threefold: pick an exit, notice when one goes bad, and
//! leave the good ones in rotation.
//!
//! # Health is a request, not a socket
//!
//! `docker-compose/docker-compose.wgcf.yml:18` defines the compose healthcheck
//! as `curl -fs https://www.cloudflare.com/cdn-cgi/trace | grep -q -E
//! 'warp=(on|plus)'`. That is not "is the port open" — it is "does traffic
//! through this exit actually leave through WARP". A SOCKS5 listener can accept
//! a connection perfectly while the tunnel behind it is down, and every request
//! through it then fails. [`WarpTraceProbe`] asks the same question the same
//! way.
//!
//! # An empty pool is not a broken pool
//!
//! `PROXY_LIST` is empty in local development (`legacy/web/server/config.py:38`,
//! and `PROXY_LIST=${PROXY_LIST:-}` for the `min`/`local` compose profiles).
//! `api.py:32-34` sends the request with no proxy at all in that case. That is
//! [`ProxyChoice::Direct`], and it is distinct from a configured pool with
//! nothing healthy in it, which is [`FetchError::NoHealthyProxy`]. Conflating
//! them would make an ejected pool silently start sending direct requests to
//! Medium from the host's own address.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::error::{FetchError, TransportError};

/// The compose healthcheck's target (`docker-compose.wgcf.yml:18`).
pub const WARP_TRACE_URL: &str = "https://www.cloudflare.com/cdn-cgi/trace";

/// A SOCKS5 endpoint, e.g. `socks5://wgcf1:1080`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyEndpoint(String);

impl ProxyEndpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProxyEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which exit the next request should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyChoice {
    /// No pool is configured — send the request unproxied (`api.py:32-34`).
    Direct,
    /// Send through this exit.
    Via(ProxyEndpoint),
}

/// Asks whether an exit can actually reach the internet through WARP.
///
/// A trait so the pool's ejection logic can be tested without a network, which
/// is the only way to test it here: the WARP pool is not reachable from this
/// working tree (`xtask/spike-impersonate/README.md`).
#[async_trait]
pub trait HealthProbe: Send + Sync {
    async fn probe(&self, endpoint: &ProxyEndpoint) -> bool;
}

/// One exit and whether it is currently in rotation.
#[derive(Debug)]
struct Slot {
    endpoint: ProxyEndpoint,
    healthy: AtomicBool,
}

/// One `reqwest::Client` per proxy URL, kept.
///
/// `reqwest` 0.13 sets a proxy on the *client*, not on the request — there is
/// no `RequestBuilder::proxy` — so anything that talks through a rotating pool
/// has to hold a client per exit. This is that, so both [`WarpTraceProbe`] and
/// `http::ReqwestTransport` share one implementation of it.
///
/// Keeping them also keeps the connection: a client owns a connection pool, and
/// rebuilding one per request would redo the TLS configuration and re-handshake
/// every time.
///
/// The proxy is the cache key, and `None` is a valid key — it means "direct",
/// which is a distinct client from any proxied one.
#[derive(Debug, Clone, Default)]
pub struct ProxyClients {
    clients: Arc<Mutex<HashMap<Option<String>, reqwest::Client>>>,
}

impl ProxyClients {
    pub fn new() -> Self {
        Self::default()
    }

    /// The client for `proxy`, built by `configure` on first use.
    ///
    /// `configure` runs once per exit, so per-caller options (a timeout here, a
    /// redirect policy there) are applied at construction rather than per
    /// request.
    pub fn get_or_build<F>(
        &self,
        proxy: Option<&str>,
        configure: F,
    ) -> Result<reqwest::Client, TransportError>
    where
        F: FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    {
        let key = proxy.map(str::to_string);

        if let Some(client) = self.lock().get(&key) {
            return Ok(client.clone());
        }

        let mut builder = reqwest::Client::builder();

        if let Some(url) = proxy {
            let parsed =
                reqwest::Proxy::all(url).map_err(|err| TransportError::Proxy(err.to_string()))?;
            builder = builder.proxy(parsed);
        }

        let client = configure(builder)
            .build()
            .map_err(|err| TransportError::Other(err.to_string()))?;

        self.lock().insert(key, client.clone());
        Ok(client)
    }

    /// The lock guards a cache of clients and nothing else, so a poisoned lock
    /// carries no invariant worth failing a request over.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Option<String>, reqwest::Client>> {
        self.clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A set of exits, their health, and a round-robin cursor over the healthy ones.
pub struct ProxyPool {
    slots: Vec<Slot>,
    next: AtomicUsize,
    probe: Arc<dyn HealthProbe>,
}

/// Renders what an operator needs and nothing else.
///
/// Hand-written because the probe is a `dyn HealthProbe` and traits cannot be
/// derived — but a pool that logged its probe's address would be noise anyway.
/// What is worth seeing in a log line is which exits exist and how many are in
/// rotation.
impl fmt::Debug for ProxyPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyPool")
            .field("endpoints", &self.endpoints())
            .field("healthy", &self.healthy_count())
            .finish_non_exhaustive()
    }
}

impl ProxyPool {
    /// Every exit starts healthy and is corrected by the first health check.
    ///
    /// Optimistic on purpose: starting pessimistic would mean the very first
    /// request after a restart waits for a health check to complete, and
    /// `PROXY_LIST` is configuration that an operator has set deliberately.
    pub fn new(endpoints: Vec<ProxyEndpoint>, probe: Arc<dyn HealthProbe>) -> Self {
        Self {
            slots: endpoints
                .into_iter()
                .map(|endpoint| Slot {
                    endpoint,
                    healthy: AtomicBool::new(true),
                })
                .collect(),
            next: AtomicUsize::new(0),
            probe,
        }
    }

    /// Parses a comma-separated list, as `config.PROXY_LIST` does
    /// (`legacy/web/server/config.py:38-39`).
    ///
    /// Blank entries are dropped, so a trailing comma or an unset variable
    /// expands to nothing rather than to an endpoint whose URL is the empty
    /// string. Python does not do this — `"".split(",")` yields `[""]` — and
    /// the difference is deliberate: an empty string is not a valid proxy and
    /// would fail at request time with an opaque error.
    pub fn from_csv(raw: &str, probe: Arc<dyn HealthProbe>) -> Self {
        let endpoints = raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(ProxyEndpoint::new)
            .collect();
        Self::new(endpoints, probe)
    }

    pub fn endpoints(&self) -> Vec<ProxyEndpoint> {
        self.slots
            .iter()
            .map(|slot| slot.endpoint.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// How many exits are currently in rotation.
    pub fn healthy_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.healthy.load(Ordering::Relaxed))
            .count()
    }

    /// The next exit to try.
    ///
    /// Round-robin over the healthy slots only, so an ejected exit is skipped
    /// rather than retried.
    ///
    /// The cursor is moved to just past the slot handed out, not merely
    /// incremented. With a skip in the way those differ: over `[a, b(ejected),
    /// c]` an incrementing cursor returns `a, c, c, a`, repeating `c` because
    /// the call that scanned past `b` still only advanced by one. Moving past
    /// the chosen slot gives `a, c, a, c`.
    ///
    /// The store is not part of a compare-exchange, so two callers racing here
    /// can leave the cursor behind where the other put it and land on the same
    /// exit. That is a fairness wobble and not a correctness one — a dead exit
    /// is still never returned — and requests here take seconds, so the
    /// contention that would make it visible does not arise.
    pub fn next(&self) -> Result<ProxyChoice, FetchError> {
        if self.slots.is_empty() {
            return Ok(ProxyChoice::Direct);
        }

        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.slots.len() {
            let index = (start + offset) % self.slots.len();
            let slot = &self.slots[index];
            if slot.healthy.load(Ordering::Relaxed) {
                self.next.store(start + offset + 1, Ordering::Relaxed);
                return Ok(ProxyChoice::Via(slot.endpoint.clone()));
            }
        }

        Err(FetchError::NoHealthyProxy)
    }

    /// Takes an exit out of rotation.
    pub fn eject(&self, endpoint: &ProxyEndpoint) {
        if let Some(slot) = self.slots.iter().find(|slot| &slot.endpoint == endpoint) {
            slot.healthy.store(false, Ordering::Relaxed);
            tracing::warn!(proxy = %endpoint, "ejecting proxy");
        }
    }

    /// Puts an exit back into rotation.
    pub fn readmit(&self, endpoint: &ProxyEndpoint) {
        if let Some(slot) = self.slots.iter().find(|slot| &slot.endpoint == endpoint) {
            if !slot.healthy.load(Ordering::Relaxed) {
                tracing::info!(proxy = %endpoint, "readmitting proxy");
            }
            slot.healthy.store(true, Ordering::Relaxed);
        }
    }

    /// Reports that a request through `choice` failed, and returns what to try
    /// next. `Direct` cannot fail over — there is nothing to eject.
    pub fn report_failure(&self, choice: &ProxyChoice) -> Result<ProxyChoice, FetchError> {
        match choice {
            ProxyChoice::Direct => Ok(ProxyChoice::Direct),
            ProxyChoice::Via(endpoint) => {
                self.eject(endpoint);
                self.next()
            }
        }
    }

    /// Re-probes every exit and updates its health.
    pub async fn health_check(&self) {
        for slot in &self.slots {
            let healthy = self.probe.probe(&slot.endpoint).await;
            let was_healthy = slot.healthy.swap(healthy, Ordering::Relaxed);

            match (was_healthy, healthy) {
                (true, false) => {
                    tracing::warn!(proxy = %slot.endpoint, "proxy failed its health check")
                }
                (false, true) => {
                    tracing::info!(proxy = %slot.endpoint, "proxy recovered")
                }
                _ => {}
            }
        }
    }

    /// Runs [`Self::health_check`] forever, every `interval`.
    ///
    /// `interval` is five seconds in the compose healthcheck
    /// (`docker-compose.wgcf.yml:19`); the same cadence means the in-process
    /// pool reacts on the same timescale the container healthcheck did.
    ///
    /// The first check happens immediately, so a restart does not wait one
    /// interval before learning that the pool is down.
    pub fn spawn_health_loop(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                self.health_check().await;
            }
        })
    }
}

/// Whether a `cdn-cgi/trace` body reports WARP as up.
///
/// The compose healthcheck greps `warp=(on|plus)`. `plus` is WARP+, which is
/// still a working exit; `off` is a connection that reaches Cloudflare without
/// the tunnel, which is the failure this whole pool exists to detect.
pub fn is_warp_healthy(trace: &str) -> bool {
    trace.lines().any(|line| {
        let line = line.trim();
        line == "warp=on" || line == "warp=plus"
    })
}

/// The production [`HealthProbe`]: asks Cloudflare what it sees.
///
/// Construction cannot fail — the only fallible step is parsing an endpoint's
/// URL into a proxy, and endpoints arrive one at a time from configuration, so
/// that is reported per-endpoint (as an unhealthy exit) rather than by refusing
/// to build the probe at all.
#[derive(Debug, Clone, Default)]
pub struct WarpTraceProbe {
    clients: ProxyClients,
}

impl WarpTraceProbe {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl HealthProbe for WarpTraceProbe {
    async fn probe(&self, endpoint: &ProxyEndpoint) -> bool {
        let client = match self
            .clients
            .get_or_build(Some(endpoint.as_str()), |builder| {
                // A health check that hangs is worse than one that fails: it holds
                // a task and reports nothing. The compose healthcheck allows 2s
                // (`docker-compose.wgcf.yml:20`).
                builder.timeout(Duration::from_secs(2))
            }) {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(proxy = %endpoint, error = %err, "unusable proxy URL");
                return false;
            }
        };

        match client.get(WARP_TRACE_URL).send().await {
            Ok(response) => match response.text().await {
                Ok(body) => is_warp_healthy(&body),
                Err(err) => {
                    tracing::debug!(proxy = %endpoint, error = %err, "could not read trace body");
                    false
                }
            },
            Err(err) => {
                tracing::debug!(proxy = %endpoint, error = %err, "trace request failed");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A probe whose answers are scripted per endpoint.
    #[derive(Default)]
    struct ScriptedProbe {
        answers: Mutex<HashMap<String, bool>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedProbe {
        fn with(answers: &[(&str, bool)]) -> Arc<Self> {
            let probe = Self::default();
            {
                let mut map = probe.answers.lock().unwrap();
                for (endpoint, healthy) in answers {
                    map.insert((*endpoint).to_string(), *healthy);
                }
            }
            Arc::new(probe)
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HealthProbe for ScriptedProbe {
        async fn probe(&self, endpoint: &ProxyEndpoint) -> bool {
            self.calls.lock().unwrap().push(endpoint.to_string());
            *self
                .answers
                .lock()
                .unwrap()
                .get(endpoint.as_str())
                .unwrap_or(&true)
        }
    }

    fn pool(probe: Arc<dyn HealthProbe>, urls: &[&str]) -> ProxyPool {
        ProxyPool::new(
            urls.iter().map(|url| ProxyEndpoint::new(*url)).collect(),
            probe,
        )
    }

    fn via(choice: ProxyChoice) -> String {
        match choice {
            ProxyChoice::Via(endpoint) => endpoint.to_string(),
            ProxyChoice::Direct => panic!("expected a proxy, got Direct"),
        }
    }

    /// An unset `PROXY_LIST` means direct, not broken — see the module doc.
    #[test]
    fn an_empty_pool_goes_direct() {
        let probe = ScriptedProbe::with(&[]);
        let pool = ProxyPool::from_csv("", probe);
        assert_eq!(pool.next(), Ok(ProxyChoice::Direct));
        assert!(pool.is_empty());
    }

    /// A trailing comma is normal in a `.env`, and must not become an endpoint.
    #[test]
    fn blank_csv_entries_are_dropped() {
        let probe = ScriptedProbe::with(&[]);
        let pool = ProxyPool::from_csv(" socks5://wgcf1:1080 , ,", probe);
        assert_eq!(
            pool.endpoints(),
            vec![ProxyEndpoint::new("socks5://wgcf1:1080")]
        );
    }

    /// A configured pool with nothing healthy is an error, *not* Direct. This
    /// is the difference that stops an exhausted pool from quietly sending
    /// requests from the host's own address.
    #[test]
    fn a_fully_ejected_pool_is_an_error_not_a_direct_request() {
        let probe = ScriptedProbe::with(&[]);
        let pool = pool(probe, &["socks5://wgcf1:1080"]);
        pool.eject(&ProxyEndpoint::new("socks5://wgcf1:1080"));

        assert_eq!(pool.next(), Err(FetchError::NoHealthyProxy));
    }

    #[test]
    fn round_robin_visits_each_healthy_exit() {
        let probe = ScriptedProbe::with(&[]);
        let pool = pool(probe, &["a", "b", "c"]);

        let seen: Vec<String> = (0..6).map(|_| via(pool.next().unwrap())).collect();
        assert_eq!(seen, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn an_ejected_exit_is_skipped() {
        let probe = ScriptedProbe::with(&[]);
        let pool = pool(probe, &["a", "b", "c"]);
        pool.eject(&ProxyEndpoint::new("b"));

        let seen: Vec<String> = (0..4).map(|_| via(pool.next().unwrap())).collect();
        assert_eq!(seen, ["a", "c", "a", "c"]);
        assert_eq!(pool.healthy_count(), 2);
    }

    /// The behaviour HAProxy cannot provide: the failing exit is dropped and
    /// the next one is returned in the same call, so the retry goes elsewhere.
    #[test]
    fn report_failure_ejects_and_returns_a_different_exit() {
        let probe = ScriptedProbe::with(&[]);
        let pool = pool(probe, &["a", "b"]);

        let choice = pool.next().unwrap();
        let failed = via(choice.clone());
        let next = via(pool.report_failure(&choice).unwrap());

        assert_ne!(failed, next);
        assert_eq!(pool.healthy_count(), 1);
    }

    /// Ejecting the last healthy exit yields the error rather than a panic or a
    /// Direct request.
    #[test]
    fn failing_over_from_the_last_healthy_exit_reports_no_proxy() {
        let probe = ScriptedProbe::with(&[]);
        let pool = pool(probe, &["a"]);
        let choice = pool.next().unwrap();

        assert_eq!(
            pool.report_failure(&choice),
            Err(FetchError::NoHealthyProxy)
        );
    }

    /// There is nothing to eject when no pool is configured.
    #[test]
    fn a_direct_request_has_nothing_to_fail_over_to() {
        let probe = ScriptedProbe::with(&[]);
        let pool = ProxyPool::from_csv("", probe);
        assert_eq!(
            pool.report_failure(&ProxyChoice::Direct),
            Ok(ProxyChoice::Direct)
        );
    }

    /// Recovery: the health loop must be able to bring an exit back, or a
    /// transient failure removes it for the life of the process.
    #[tokio::test]
    async fn health_check_ejects_and_readmits() {
        let probe = ScriptedProbe::with(&[("a", false), ("b", true)]);
        let pool = pool(probe.clone(), &["a", "b"]);

        pool.health_check().await;
        assert_eq!(pool.healthy_count(), 1);
        assert_eq!(via(pool.next().unwrap()), "b");

        // The exit recovers.
        probe.answers.lock().unwrap().insert("a".to_string(), true);
        pool.health_check().await;

        assert_eq!(pool.healthy_count(), 2);
        let seen: Vec<String> = (0..2).map(|_| via(pool.next().unwrap())).collect();
        assert!(seen.contains(&"a".to_string()), "got {seen:?}");
        assert_eq!(probe.calls(), ["a", "b", "a", "b"]);
    }

    /// An unknown endpoint is a no-op, not a panic — `report_failure` may be
    /// called with an endpoint from a pool that has since been rebuilt.
    #[test]
    fn ejecting_an_unknown_endpoint_does_nothing() {
        let probe = ScriptedProbe::with(&[]);
        let pool = pool(probe, &["a"]);
        pool.eject(&ProxyEndpoint::new("nope"));
        assert_eq!(pool.healthy_count(), 1);
    }

    #[test]
    fn warp_is_healthy_for_on_and_plus() {
        assert!(is_warp_healthy("ip=1.2.3.4\nwarp=on\n"));
        assert!(is_warp_healthy("warp=plus"));
    }

    /// `warp=off` is the whole point of probing: the SOCKS5 port answers, but
    /// traffic leaves Cloudflare untunnelled, and Medium sees the real IP.
    #[test]
    fn warp_is_unhealthy_for_off() {
        assert!(!is_warp_healthy("ip=1.2.3.4\nwarp=off\n"));
        assert!(!is_warp_healthy(""));
    }

    /// A prefix match would accept `warp=onion`, and a substring match would
    /// accept a trace that merely mentions `warp=on` inside another value.
    #[test]
    fn warp_matching_is_exact_per_line() {
        assert!(!is_warp_healthy("warp=onion"));
        assert!(!is_warp_healthy("x=warp=on"));
        assert!(
            is_warp_healthy("  warp=on  "),
            "surrounding space is trimmed"
        );
    }
}
