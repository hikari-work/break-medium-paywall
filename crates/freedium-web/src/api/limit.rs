//! The four rate-limit buckets, and the client identity they are keyed on.
//!
//! # Why `governor` directly
//!
//! Three of the four buckets are not shaped like a layer. The miss bucket is
//! charged from *inside* a handler, after the cache lookup that decides whether
//! there is anything to charge for; the fetch budget is process-global rather
//! than keyed. The two that are layer-shaped want `X-RateLimit-*` headers and an
//! RFC 9457 body, neither of which `tower_governor` produces — so the layer would
//! be a dozen lines of glue plus 100% of a dependency.
//!
//! # The buckets
//!
//! | bucket | scope | anonymous | with a token | what it protects |
//! |---|---|---|---|---|
//! | request | per client IP, the whole governed surface | 10/min, burst 5 | 60/min, burst 20 | the general cost of an API call |
//! | miss | per client IP, charged only on a durable-cache miss | 3/min, burst 1 | 15/min, burst 5 | one IP walking the id space |
//! | fetch | process-global, charged before every outbound fetch | 30/min, burst 5 | **the same** | the single WARP exit |
//!
//! The global budget is the one that matters and the one a per-IP bucket cannot
//! replace: a scraper with a fresh id per request misses every time, and a
//! distributed one has fresh addresses. What it protects is not a policy — it is
//! how much one exit can be asked for before the site stops answering, which is
//! why **a token cannot raise it**. The numbers shipped are safe rather than
//! correct: the correct number is a measurement of what one WARP exit takes, and
//! that is Fase 7's evidence. Every miss is logged with what is left so the
//! number has a source.
//!
//! # The two tiers are two limiters, not one limiter with a variable quota
//!
//! `governor` fixes a `Quota` per limiter, so a tier that changes the numbers
//! means a second limiter over the same key. Hence five limiters for four
//! buckets — and it is also why a caller cannot ramp between tiers within a
//! window: switching tiers switches which budget is being spent, which is the
//! honest reading of "a trusted client gets a larger allowance".
//!
//! # The keyed map has to be swept
//!
//! An unbounded map keyed by a value the attacker chooses is a memory DoS, and
//! `governor` does not evict on its own. [`spawn_housekeeping`] is the sweep:
//! `retain_recent` drops every key whose state is indistinguishable from a fresh
//! one. That is also why a hand-rolled `HashMap<IpAddr, Bucket>` is not the
//! alternative — its memory bound would become our problem instead of the
//! library's.
//!
//! # The health, spec and docs routes are outside the governor structurally
//!
//! Not by an `if` inside this middleware: they are registered on the outer API
//! router and this layer is applied only to the governed sub-router. A monitor
//! polling every ten seconds must not be able to collect a 429, and the spec is
//! static. CORS preflights are outside it too, because `router::cors` wraps the
//! whole application — cheap, correct, and pinned by a test so a future
//! reordering of the layers cannot quietly break it.

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use freedium_dto::problem::ProblemKind;
use governor::clock::{Clock, DefaultClock};
use governor::middleware::StateInformationMiddleware;
use governor::{DefaultDirectRateLimiter, DefaultKeyedRateLimiter, Quota, RateLimiter};
use subtle::ConstantTimeEq;

use crate::api::problem::ApiError;
use crate::config::Config;
use crate::middleware::{ConnectInfo, Correlation};
use crate::state::AppState;

/// The header a client may present to move into the trusted tier.
pub const API_TOKEN_HEADER: &str = "x-api-token";

/// How often the keyed maps are swept.
///
/// Well inside the retention the limiter itself considers "recent": a key is
/// droppable once its theoretical arrival time is in the past, which for the
/// loosest bucket here is `burst × interval` — 20 × 1s for the token tier. Five
/// minutes is two orders of magnitude of headroom, which is what makes the sweep
/// an implementation detail rather than a tuning knob.
pub const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Which budget a caller is spending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// No token, or no token configured. The tier every caller gets by default
    /// and the only one that exists when `API_TOKEN` is unset.
    Anonymous,
    /// A matching `X-API-TOKEN` was presented.
    Token,
}

/// Who this request is, for the buckets that are keyed on it.
///
/// Inserted into the request extensions by [`guard`], so a handler charges the
/// same bucket the request bucket was charged against. Governed routes always
/// have it; a handler that extracts it outside the governed sub-router fails
/// with axum's own `500`, which is a routing mistake rather than a runtime case.
#[derive(Debug, Clone, Copy)]
pub struct ApiClient {
    pub ip: IpAddr,
    pub tier: Tier,
}

/// The `X-RateLimit-*` triple, as a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// The bucket's burst capacity. The scale `remaining` is measured on, so the
    /// two are comparable — which is the only reason a client can use them.
    pub limit: u32,
    pub remaining: u32,
    /// Seconds until the bucket refills by one.
    pub reset: u64,
}

impl RateLimit {
    /// From a decision `governor` just made.
    fn allowed(snapshot: &governor::middleware::StateSnapshot) -> Self {
        let quota = snapshot.quota();
        Self {
            limit: quota.burst_size().get(),
            remaining: snapshot.remaining_burst_capacity(),
            // On the allowed path the snapshot does not say when the *next* cell
            // arrives, only how many are in hand. One replenishment interval is
            // the honest answer to "when does this bucket change", and it is what
            // `reset` means for a token bucket that refills continuously.
            reset: quota.replenish_interval().as_secs().max(1),
        }
    }
}

/// The first of the `X-RateLimit-*` triple.
///
/// A constant because [`guard`] has to ask whether a response already carries the
/// triple, and a literal in two places is a literal that drifts. It is also the
/// marker of "a bucket already refused this": only [`RateLimit::headers`] writes
/// it, and it is written on the refusal path alone.
const RATE_LIMIT_LIMIT: &str = "x-ratelimit-limit";

impl RateLimit {
    fn headers(&self) -> [(&'static str, String); 3] {
        [
            (RATE_LIMIT_LIMIT, self.limit.to_string()),
            ("x-ratelimit-remaining", self.remaining.to_string()),
            ("x-ratelimit-reset", self.reset.to_string()),
        ]
    }
}

/// Every bucket, built once at boot.
pub struct Limits {
    request: DefaultKeyedRateLimiter<IpAddr, StateInformationMiddleware>,
    request_token: DefaultKeyedRateLimiter<IpAddr, StateInformationMiddleware>,
    miss: DefaultKeyedRateLimiter<IpAddr, StateInformationMiddleware>,
    miss_token: DefaultKeyedRateLimiter<IpAddr, StateInformationMiddleware>,
    fetch: DefaultDirectRateLimiter<StateInformationMiddleware>,
}

/// A keyed bucket that reports what it has left.
///
/// `RateLimiter::keyed` builds a `NoOpMiddleware` limiter, whose positive
/// outcome is `()` — no remaining count, and so no `X-RateLimit-Remaining` on
/// the responses that are *not* refused. `with_middleware` is what swaps it, and
/// it is the reason every bucket here is built through this function rather than
/// through the constructor directly.
fn keyed(quota: Quota) -> DefaultKeyedRateLimiter<IpAddr, StateInformationMiddleware> {
    RateLimiter::keyed(quota).with_middleware::<StateInformationMiddleware>()
}

impl Limits {
    #[must_use]
    pub fn new(config: &Config) -> Self {
        Self {
            request: keyed(quota(
                config.api_rate_limit_per_minute,
                config.api_rate_limit_burst,
            )),
            request_token: keyed(quota(
                config.api_token_limit_per_minute,
                config.api_token_limit_burst,
            )),
            miss: keyed(quota(
                config.api_miss_limit_per_minute,
                config.api_miss_limit_burst,
            )),
            miss_token: keyed(quota(
                config.api_token_limit_per_minute,
                config.api_token_limit_burst,
            )),
            fetch: RateLimiter::direct(quota(
                config.api_fetch_budget_per_minute,
                config.api_fetch_budget_burst,
            ))
            .with_middleware::<StateInformationMiddleware>(),
        }
    }

    /// Charges the per-IP miss bucket.
    ///
    /// Called only when the durable cache did *not* have the post, which is the
    /// only path that can spend an upstream request. A cached post costs nothing
    /// here, so a client reading the same post repeatedly is never throttled for
    /// it — the request bucket is what bounds that.
    pub fn spend_miss(&self, client: &ApiClient) -> Result<RateLimit, ApiError> {
        let limiter = match client.tier {
            Tier::Anonymous => &self.miss,
            Tier::Token => &self.miss_token,
        };
        charge(limiter, &client.ip, "per-IP cache-miss").map_err(|limit| {
            exhausted("per-IP cache miss", limit).with_headers(limit.headers().to_vec())
        })
    }

    /// Charges the process-global fetch budget, returning the cells left.
    ///
    /// The returned value is what goes into the miss log line, which is how the
    /// shipped number gets a source.
    pub fn spend_fetch(&self) -> Result<u32, ApiError> {
        match self.fetch.check() {
            Ok(snapshot) => Ok(snapshot.remaining_burst_capacity()),
            Err(not_until) => {
                let limit = RateLimit {
                    limit: not_until.quota().burst_size().get(),
                    remaining: 0,
                    reset: wait_seconds(not_until.wait_time_from(DefaultClock::default().now())),
                };
                Err(exhausted("global fetch", limit).with_headers(limit.headers().to_vec()))
            }
        }
    }

    /// Drops every key whose state is indistinguishable from a fresh one.
    pub fn retain_recent(&self) {
        self.request.retain_recent();
        self.request_token.retain_recent();
        self.miss.retain_recent();
        self.miss_token.retain_recent();
        // `self.fetch` is not keyed: there is one state for the whole process and
        // nothing to evict.
    }

    /// Live keys across the two per-IP tiers of the request bucket. For the
    /// housekeeping test, which has to be able to see the sweep happen.
    #[must_use]
    pub fn request_keys(&self) -> usize {
        self.request.len() + self.request_token.len()
    }
}

/// Spawns the sweep. Detached: it runs for the process's lifetime and there is
/// nothing to await.
pub fn spawn_housekeeping(limits: Arc<Limits>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HOUSEKEEPING_INTERVAL);
        // `interval`'s first tick is immediate, and sweeping at boot would only
        // walk an empty map.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            limits.retain_recent();
            tracing::debug!(
                keys = limits.request_keys(),
                "swept the api rate limiter's keyed state"
            );
        }
    });
}

/// `Quota::per_minute(rate)` with the burst overridden.
///
/// `per_minute(n)` sets burst **to** `n`, so the second call is not decoration —
/// without it "10/min, burst 5" would be "10/min, burst 10". The replenish
/// interval is untouched by `allow_burst`, which is what keeps the rate at one
/// cell per six seconds.
fn quota(per_minute: u32, burst: u32) -> Quota {
    Quota::per_minute(nonzero(per_minute)).allow_burst(nonzero(burst))
}

/// `Config::positive` refuses a zero at boot, so the only way to reach the
/// panic below is a `Config` built by hand that skipped validation — a test
/// fixture, not a deployment.
fn nonzero(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("Config rejects a zero rate limit before the listener binds")
}

/// One decision against a keyed bucket.
fn charge(
    limiter: &DefaultKeyedRateLimiter<IpAddr, StateInformationMiddleware>,
    key: &IpAddr,
    bucket: &str,
) -> Result<RateLimit, RateLimit> {
    match limiter.check_key(key) {
        Ok(snapshot) => Ok(RateLimit::allowed(&snapshot)),
        Err(not_until) => {
            tracing::debug!(%key, bucket, "an api rate-limit bucket is empty");
            Err(RateLimit {
                limit: not_until.quota().burst_size().get(),
                remaining: 0,
                reset: wait_seconds(not_until.wait_time_from(DefaultClock::default().now())),
            })
        }
    }
}

fn exhausted(bucket: &str, limit: RateLimit) -> ApiError {
    ApiError::new(
        ProblemKind::RateLimited,
        format!(
            "the {bucket} budget is empty; retry in {}s",
            limit.reset.max(1)
        ),
    )
    .with_retry_after(limit.reset.max(1))
}

/// Rounds a wait up to whole seconds, never to zero.
///
/// `Retry-After: 0` reads as "retry now" and would produce a client that spins;
/// the limiter's decision was that this request cannot proceed, so the answer
/// has to be at least a second.
fn wait_seconds(wait: Duration) -> u64 {
    let seconds = wait.as_secs();
    if wait.subsec_nanos() > 0 {
        seconds + 1
    } else {
        seconds
    }
    .max(1)
}

/// The client's address, as the buckets should see it.
///
/// # Trusting a header is opt-in, and the default is right for a bare process
///
/// `API_TRUST_PROXY` defaults to **false**, so the peer address is used: that is
/// the honest answer when this process is the one accepting the connection.
/// Caddy sets `X-Real-IP` and `X-Forwarded-For` to `{remote_host}` — the peer it
/// saw, overwriting anything the client sent — so behind that specific Caddy the
/// headers are trustworthy and the flag is what says so.
///
/// **Production's topology needs a look before this flag is turned on**: an
/// external proxy in front of Caddy makes `{remote_host}` the proxy, so every
/// real client collapses into one bucket and the per-IP limits become global.
/// Fixing that is a Caddyfile policy change (consuming the upstream's
/// `X-Forwarded-For`), not something this flag can reach — see `.env_template`.
///
/// `X-Real-IP` wins over `X-Forwarded-For`'s leftmost entry, which is the order
/// Caddy's own directives imply and the narrower of the two headers.
#[must_use]
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, trust_proxy: bool) -> IpAddr {
    if trust_proxy && let Some(forwarded) = forwarded_ip(headers) {
        return forwarded;
    }

    // No peer at all: an HTTP/1.0 request, or a test harness with no connect
    // info. `0.0.0.0` deliberately shares one bucket with every other unknown
    // caller — an unknown identity is not a fresh one.
    peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let parse = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
    };

    if let Some(ip) = parse("x-real-ip") {
        return Some(ip);
    }

    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.trim().parse::<IpAddr>().ok())
}

/// The tier a request belongs to, or the `401` to answer with.
///
/// # What "the token is wrong" means
///
/// `API_TOKEN` **unset** turns the whole idea off: the header is ignored and
/// everyone is anonymous. That is not a convenience — a deployment that shipped
/// the tier on by default would answer `401` to every client that happened to
/// send the header, and the API is anonymous by design.
///
/// `API_TOKEN` **set** makes the header meaningful, and the two cases differ:
///
/// - **No header** → anonymous. Nothing was offered, so nothing is wrong. This
///   is the case that keeps the tier optional: a trusted client that presents
///   its token gets the larger allowance, and everyone else is unaffected.
/// - **Header that does not match** → `401`. The client believes it is trusted
///   and is not, and answering with the anonymous allowance instead would hide a
///   misconfigured token until the day the limit mattered.
///
/// "Does not match" includes an *empty* value: a present header is a claim, and
/// `X-API-TOKEN:` with nothing after it is a claim of a token that is not the
/// configured one. Treating it as "nothing offered" would make a client library
/// or a proxy that drops the value into a silent downgrade to the anonymous
/// bucket — the same failure the bullet above refuses, arriving more quietly.
/// A proxy that strips the header entirely lands in the no-header case, which is
/// the right answer for it.
///
/// The comparison is constant-time (`subtle`, already in the tree for the
/// transponder). Nothing here is a secret worth a timing attack today, but the
/// cost is one function call and the alternative is a habit.
pub fn token_tier(headers: &HeaderMap, configured: Option<&str>) -> Result<Tier, ApiError> {
    let Some(configured) = configured else {
        return Ok(Tier::Anonymous);
    };

    let Some(offered) = headers.get(API_TOKEN_HEADER) else {
        return Ok(Tier::Anonymous);
    };

    let matches: bool = offered.as_bytes().ct_eq(configured.as_bytes()).into();
    if matches {
        Ok(Tier::Token)
    } else {
        Err(ApiError::new(
            ProblemKind::InvalidToken,
            "the X-API-TOKEN header does not match".to_string(),
        ))
    }
}

/// The request bucket, then the tier.
///
/// Applied to the governed sub-router only. It decides identity, charges the
/// request bucket, and hands the identity on as an extension so the miss bucket
/// is charged against the same client.
pub async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    // `OriginalUri`, not `request.uri()`: this layer runs inside
    // `.nest("/api/v1")`, where the request's own URI has the prefix stripped.
    // A problem's `instance` built from it would name `/posts/{id}` — a path
    // that does not exist on this server.
    let uri = crate::api::request_uri(&request);
    let correlation = request.extensions().get::<Correlation>().cloned();

    let tier = match token_tier(request.headers(), state.config.api_token.as_deref()) {
        Ok(tier) => tier,
        Err(error) => return error.resolve(&uri, correlation.as_ref()),
    };

    let peer = request
        .extensions()
        .get::<ConnectInfo>()
        .map(|peer| peer.0.ip());
    let ip = client_ip(request.headers(), peer, state.config.api_trust_proxy);
    request.extensions_mut().insert(ApiClient { ip, tier });

    let limiter = match tier {
        Tier::Anonymous => &state.limits.request,
        Tier::Token => &state.limits.request_token,
    };

    let limit = match charge(limiter, &ip, "per-IP request") {
        Ok(limit) => limit,
        Err(limit) => {
            // The resolved address goes in the log because the pre-deploy check
            // in `.env_template` is "two clients must land in different
            // buckets", and a 429 is where that is decided.
            tracing::debug!(%ip, tier = ?tier, "refusing a request over the api limit");
            return exhausted("per-IP request", limit)
                .with_headers(limit.headers().to_vec())
                .resolve(&uri, correlation.as_ref());
        }
    };

    let mut response = next.run(request).await;
    // Not onto a refusal that came from further in. A `429` from the miss bucket
    // or from the global fetch budget already carries the triple of the bucket
    // that actually refused it, and this bucket's numbers are not that bucket's:
    // the two sets are the same three header names, so writing them here does not
    // add anything, it *replaces* the true answer. The result reads as quota to
    // spare on a refusal — `X-RateLimit-Remaining: 95` beside `Retry-After: 2` —
    // and its `X-RateLimit-Reset` would describe a different bucket than its own
    // `Retry-After` does. Every other status still gets them, which is the point:
    // the caller spent a request cell and is entitled to know what is left.
    if !response.headers().contains_key(RATE_LIMIT_LIMIT) {
        apply(&mut response, &limit.headers());
    }
    response
}

/// Writes header pairs onto a response, skipping any that cannot be a header.
///
/// The values here are decimal digits, so the `Err` arm is unreachable; it
/// exists because `HeaderValue::from_str` returns a `Result` and a `429` without
/// its `Retry-After` is worse than a `429` without its `X-RateLimit-Reset`.
fn apply(response: &mut Response, headers: &[(&'static str, String)]) {
    for (name, value) in headers {
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        response
            .headers_mut()
            .insert(HeaderName::from_static(name), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn limits(edit: impl FnOnce(&mut Config)) -> Limits {
        let mut config = crate::state::tests::test_config();
        edit(&mut config);
        Limits::new(&config)
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    /// The four cases the resolution rule has to get right, and the one that is
    /// the whole reason the flag exists.
    #[test]
    fn the_peer_wins_unless_a_proxy_is_trusted() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        let spoofed = headers(&[
            ("x-real-ip", "203.0.113.7"),
            ("x-forwarded-for", "203.0.113.8, 10.0.0.9"),
        ]);

        // Default: the header is the client's own claim and is ignored.
        assert_eq!(client_ip(&spoofed, Some(peer), false), peer);

        // Trusted: the proxy overwrote it, so it is the proxy's observation.
        assert_eq!(
            client_ip(&spoofed, Some(peer), true),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    /// `X-Forwarded-For` is the fallback, and its **leftmost** entry is the
    /// origin — the rest of the list is proxies the request already passed.
    #[test]
    fn a_forwarded_for_list_resolves_to_its_first_entry() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        let forwarded = headers(&[("x-forwarded-for", "203.0.113.8, 10.0.0.9")]);
        assert_eq!(
            client_ip(&forwarded, Some(peer), true),
            "203.0.113.8".parse::<IpAddr>().unwrap()
        );
    }

    /// A trusted proxy header that is not an address falls back rather than
    /// becoming an error: the request still has to be limited against something.
    #[test]
    fn an_unparseable_proxy_header_falls_back_to_the_peer() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        for value in ["", "unknown", "203.0.113.7:443", "<html>"] {
            let bogus = headers(&[("x-real-ip", value), ("x-forwarded-for", value)]);
            assert_eq!(client_ip(&bogus, Some(peer), true), peer, "{value:?}");
        }
    }

    /// Unknown identity is not fresh identity. Every caller with no connect info
    /// shares one bucket, which fails closed — the opposite choice would hand an
    /// attacker a new bucket per request.
    #[test]
    fn a_client_with_no_peer_shares_one_bucket() {
        assert_eq!(
            client_ip(&HeaderMap::new(), None, false),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        // Even with the flag on, an absent peer is absent.
        assert_eq!(
            client_ip(&HeaderMap::new(), None, true),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn ipv6_addresses_are_keys_too() {
        let peer = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(client_ip(&HeaderMap::new(), Some(peer), false), peer);
    }

    /// The three cases of the token rule, which is the one place a `401` can
    /// come from.
    #[test]
    fn the_token_tier_is_off_until_it_is_configured() {
        let with_header = headers(&[(API_TOKEN_HEADER, "anything")]);

        // Unset: the header is not read at all. Not a 401 — a default
        // deployment must not reject clients for sending a header.
        assert_eq!(token_tier(&with_header, None).unwrap(), Tier::Anonymous);
        assert_eq!(
            token_tier(&HeaderMap::new(), None).unwrap(),
            Tier::Anonymous
        );

        // Set, but nothing offered: anonymous, and that is what keeps the tier
        // optional rather than mandatory.
        assert_eq!(
            token_tier(&HeaderMap::new(), Some("s3cret")).unwrap(),
            Tier::Anonymous
        );

        // Set, and an *empty* header offered: a `401`, not a silent downgrade.
        // The header is present, so something was claimed; the claim is wrong.
        // See the rule in `token_tier`'s docs.
        assert_eq!(
            token_tier(&headers(&[(API_TOKEN_HEADER, "")]), Some("s3cret"))
                .unwrap_err()
                .kind,
            ProblemKind::InvalidToken
        );
    }

    #[test]
    fn a_wrong_token_is_a_401_and_a_right_one_is_a_tier() {
        let configured = Some("s3cret");
        assert_eq!(
            token_tier(&headers(&[(API_TOKEN_HEADER, "s3cret")]), configured).unwrap(),
            Tier::Token
        );

        let error = token_tier(&headers(&[(API_TOKEN_HEADER, "s3cre")]), configured).unwrap_err();
        assert_eq!(error.kind, ProblemKind::InvalidToken);
        assert_eq!(error.kind.status(), 401);

        // A prefix, a superset and a same-length mismatch all have to fail: the
        // comparison is over the whole value, not a prefix of it.
        for wrong in ["s3cret ", " s3cret", "s3cretx", "s3cres"] {
            assert!(
                token_tier(&headers(&[(API_TOKEN_HEADER, wrong)]), configured).is_err(),
                "{wrong:?} was accepted"
            );
        }
    }

    /// **The WARP test.** One global budget, spent by callers that have nothing
    /// else in common, so a flood of distinct ids from distinct addresses still
    /// stops. This is the property a per-IP bucket cannot provide and the reason
    /// the global bucket exists.
    #[test]
    fn the_global_fetch_budget_stops_a_flood_of_distinct_ids() {
        let limits = limits(|config| {
            config.api_fetch_budget_per_minute = 30;
            config.api_fetch_budget_burst = 5;
        });

        // The burst is spent, one cell per distinct client.
        for index in 0..5 {
            let remaining = limits
                .spend_fetch()
                .unwrap_or_else(|_| panic!("cell {index} of the burst was refused"));
            assert_eq!(remaining, 4 - index, "remaining counts down from the burst");
        }

        let error = limits
            .spend_fetch()
            .expect_err("the sixth distinct caller must be refused");
        assert_eq!(error.kind, ProblemKind::RateLimited);
        assert_eq!(error.kind.status(), 429);
        // Two seconds, not twelve: the refill rate is what
        // `Quota::per_minute(30)` sets — one cell every `60/30`s — and
        // `allow_burst(5)` raises only how many may be held at once. So a budget
        // of 30/minute with a burst of 5 empties in a burst and comes back one
        // caller at a time, which is what a *budget* means and what a
        // "5 per minute" reading of the two numbers together would get wrong.
        assert_eq!(error.retry_after, Some(2), "one cell per two seconds");
        assert_eq!(
            error.headers,
            vec![
                ("x-ratelimit-limit", "5".to_string()),
                ("x-ratelimit-remaining", "0".to_string()),
                ("x-ratelimit-reset", "2".to_string()),
            ]
        );
    }

    /// A token buys a larger allowance, and it does **not** buy a larger global
    /// budget. §2.7 warning 1 is a physical limit on one WARP exit, not a policy
    /// about who may ask.
    #[test]
    fn a_token_raises_the_per_ip_tiers_and_not_the_global_one() {
        let limits = limits(|config| {
            config.api_rate_limit_per_minute = 10;
            config.api_rate_limit_burst = 1;
            config.api_token_limit_per_minute = 60;
            config.api_token_limit_burst = 20;
            config.api_miss_limit_per_minute = 3;
            config.api_miss_limit_burst = 1;
            config.api_fetch_budget_per_minute = 30;
            config.api_fetch_budget_burst = 2;
        });

        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let anonymous = ApiClient {
            ip,
            tier: Tier::Anonymous,
        };
        let trusted = ApiClient {
            ip,
            tier: Tier::Token,
        };

        // One cell each, because they are different limiters over the same key.
        assert_eq!(limits.spend_miss(&anonymous).unwrap().limit, 1);
        assert_eq!(limits.spend_miss(&trusted).unwrap().limit, 20);
        assert!(limits.spend_miss(&anonymous).is_err(), "burst 1 is spent");
        // The trusted tier's own bucket is untouched by the anonymous one's.
        assert_eq!(limits.spend_miss(&trusted).unwrap().remaining, 18);

        // The global budget is the same limiter for both tiers.
        limits.spend_fetch().unwrap();
        limits.spend_fetch().unwrap();
        let error = limits
            .spend_fetch()
            .expect_err("the global budget is exhausted regardless of tier");
        assert_eq!(error.kind, ProblemKind::RateLimited);
    }

    /// Two addresses are two buckets, which is the pre-deploy check the plan
    /// asks for as a curl and this pins as a property.
    #[test]
    fn two_clients_have_independent_request_buckets() {
        let limits = limits(|config| {
            config.api_rate_limit_per_minute = 10;
            config.api_rate_limit_burst = 2;
        });
        let limiter = &limits.request;

        let first: IpAddr = "203.0.113.7".parse().unwrap();
        let second: IpAddr = "203.0.113.8".parse().unwrap();

        charge(limiter, &first, "test").unwrap();
        let left = charge(limiter, &first, "test").unwrap();
        assert_eq!(left.remaining, 0);
        assert!(charge(limiter, &first, "test").is_err());

        // The second client still has its whole burst.
        assert_eq!(charge(limiter, &second, "test").unwrap().remaining, 1);
    }

    /// A fresh bucket reports the burst, and `reset` is the replenish interval
    /// rather than the whole window: `10/min` means a cell every six seconds.
    #[test]
    fn a_full_bucket_reports_its_burst_and_its_replenish_interval() {
        let limits = limits(|config| {
            config.api_rate_limit_per_minute = 10;
            config.api_rate_limit_burst = 5;
        });
        let limit = charge(&limits.request, &"203.0.113.7".parse().unwrap(), "test").unwrap();
        assert_eq!(limit.limit, 5, "burst, not the per-minute rate");
        assert_eq!(limit.remaining, 4, "the cell this call just spent");
        assert_eq!(limit.reset, 6, "60s / 10 per minute");
    }

    /// The sweep is what keeps a key an attacker chooses from being a key that
    /// stays forever. Asserted through `len` because that is the only observable
    /// of the map's size, and asserted at all because `governor` will not evict
    /// on its own — `spawn_housekeeping` is the whole of the fix.
    #[test]
    fn the_housekeeping_sweep_drops_stale_keys() {
        // 600/min puts the replenish interval at 100 ms, so a key is droppable
        // two intervals — 200 ms — after its last use. A wait far longer than
        // that keeps the test from being a race against the clock.
        let limits = limits(|config| {
            config.api_rate_limit_per_minute = 600;
            config.api_rate_limit_burst = 1;
        });

        for index in 0..64u8 {
            let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, index));
            charge(&limits.request, &ip, "test").unwrap();
        }
        assert_eq!(limits.request_keys(), 64);

        std::thread::sleep(Duration::from_millis(500));
        limits.retain_recent();
        assert_eq!(
            limits.request_keys(),
            0,
            "the sweep left keys an attacker chose"
        );
    }

    /// `Retry-After: 0` is a client that spins. Every path that reports a wait
    /// has to report at least a second.
    #[test]
    fn a_wait_is_never_rounded_down_to_zero() {
        assert_eq!(wait_seconds(Duration::ZERO), 1);
        assert_eq!(wait_seconds(Duration::from_millis(1)), 1);
        assert_eq!(wait_seconds(Duration::from_millis(1500)), 2);
        assert_eq!(wait_seconds(Duration::from_secs(12)), 12);
    }

    /// The 429 as a response, headers included, because the triple is the part a
    /// client is supposed to act on.
    #[test]
    fn an_exhausted_bucket_carries_the_full_header_set() {
        let error = exhausted(
            "per-IP request",
            RateLimit {
                limit: 5,
                remaining: 0,
                reset: 6,
            },
        );
        assert_eq!(error.kind, ProblemKind::RateLimited);
        assert_eq!(error.retry_after, Some(6));
        assert!(error.detail.contains("per-IP request"), "{}", error.detail);
    }

    /// A `429` from a bucket further in keeps **its own** triple.
    ///
    /// This is the test for the collision at the end of [`guard`]. All four
    /// buckets answer with the same three header names, so the request bucket's
    /// numbers are not additive to a refusal that already carries its own — they
    /// replace it, and the result was a `429` reporting quota to spare
    /// (`X-RateLimit-Remaining: 95`) next to a `Retry-After` counted down by a
    /// different bucket.
    ///
    /// Through the real router, because the collision is between two layers and
    /// only the assembled stack has both. The fetch budget is given a burst of
    /// one and a rate of thirty a minute, so its numbers — `1` and a two-second
    /// reset — cannot be confused with the request bucket's `5` and six.
    #[tokio::test]
    async fn a_refusal_from_a_downstream_bucket_keeps_its_own_headers() {
        use axum::body::Body;
        use axum::http::StatusCode;
        use tower::ServiceExt as _;

        let mut config = crate::state::tests::test_config();
        config.api_fetch_budget_burst = 1;
        config.api_fetch_budget_per_minute = 30;
        // The stub is already not a shadow; said out loud because a shadow
        // *declines* a miss and never reaches the fetch budget at all, so this
        // test would pass against the decline path without proving anything.
        assert!(!config.shadow_mode);
        let state = crate::state::tests::offline_state_with(&config);
        state.limits.spend_fetch().expect("the only cell is free");

        let response = crate::router::router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/posts/aaaaaaaaaaaa")
                    .body(Body::empty())
                    .expect("a well-formed request"),
            )
            .await
            .expect("the router answers");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|value| value.to_str().expect("a visible header").to_string())
        };
        assert_eq!(header(RATE_LIMIT_LIMIT).as_deref(), Some("1"));
        assert_eq!(header("x-ratelimit-remaining").as_deref(), Some("0"));
        // The point of the whole test: `reset` and `Retry-After` are the fetch
        // bucket's two seconds, not the request bucket's six. A response whose
        // own headers disagree about which bucket refused it is a response that
        // tells a client to retry at one time and to expect a reset at another.
        assert_eq!(header("x-ratelimit-reset").as_deref(), Some("2"));
        assert_eq!(header("retry-after").as_deref(), Some("2"));
    }

    /// The other half: a response that is *not* a refusal still gets the request
    /// bucket's triple, so the rule above did not turn into "never apply them".
    ///
    /// A shadow instance declining a miss is the cheapest non-`429` through the
    /// governed surface — a `503`, which is exactly the status a caller most
    /// wants the remaining quota alongside.
    #[tokio::test]
    async fn anything_but_a_refusal_still_carries_the_request_buckets_headers() {
        use axum::body::Body;
        use tower::ServiceExt as _;

        // A shadow, so the miss *declines* instead of spending the fetch budget.
        // `test_config` is not one — that is `shadow_state`'s assertion — so the
        // flag is set here rather than inherited.
        let mut config = crate::state::tests::test_config();
        config.shadow_mode = true;
        let state = crate::state::tests::offline_state_with(&config);

        let response = crate::router::router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/posts/aaaaaaaaaaaa")
                    .body(Body::empty())
                    .expect("a well-formed request"),
            )
            .await
            .expect("the router answers");

        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|value| value.to_str().expect("a visible header").to_string())
        };
        // The defaults from `test_config`: 10/min, burst 5, so this caller has
        // four cells left of its five.
        assert_eq!(header(RATE_LIMIT_LIMIT).as_deref(), Some("5"));
        assert_eq!(header("x-ratelimit-remaining").as_deref(), Some("4"));
        assert_eq!(header("x-ratelimit-reset").as_deref(), Some("6"));
    }
}
