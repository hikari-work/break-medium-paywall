//! Runtime configuration, read from the environment at boot.
//!
//! Ports `legacy/web/server/config.py`. Every default here is that file's
//! default, because the deployed `.env` was written against them and a
//! different default would silently change production behaviour on the first
//! restart.
//!
//! # `std::env`, not a `.env` reader
//!
//! `config.py:3` is `Config(".env")`, so the Python server reads a file *and*
//! the environment. This reads only the environment. That is deliberate:
//! `.dockerignore` excludes `.env`, so in the container the file is absent and
//! the environment is the only source anyway — the compose file passes every
//! value. Keeping the file-reading half would mean the binary behaves
//! differently depending on the working directory it happens to be started in,
//! which is the wrong kind of parity.
//!
//! # Two things are deliberately missing
//!
//! Sentry (`SENTRY_*`) and Logstash (`LOGSTASH_*`) are not ported. §2.5's
//! observability story is `tracing` plus the existing log shipper, and neither
//! has a consumer on the Rust side yet. The variables are left unread rather
//! than stubbed, so nobody mistakes a parsed-but-ignored DSN for a working one.
//!
//! # `ADMIN_SECRET_KEY` is required
//!
//! `config.py:9` has no default, so the legacy server raises at import if it is
//! unset. This refuses to start for the same reason, and [`Config::from_env`]
//! is the only constructor — see [`ConfigError::MissingAdminSecretKey`].

use std::time::Duration;

use medium_client::wreq_transport::Profile;
use thiserror::Error;

/// `config.py:5`.
pub const DEFAULT_HOST_ADDRESS: &str = "https://freedium.cfd";

/// `config.py:32`.
pub const DEFAULT_DATABASE_URL: &str =
    "postgresql://postgres:postgres@postgres_freedium:5432/postgres";

/// `config.py:28`.
pub const DEFAULT_REDIS_HOST: &str = "redis_service";

/// `config.py:29`.
pub const DEFAULT_REDIS_PORT: u16 = 6379;

/// `config.py:30`.
pub const DEFAULT_REDIS_TIMEOUT: f64 = 1.75;

/// `services/cli.py:11` — `--port` with `const=7080, default=7080`.
pub const DEFAULT_PORT: u16 = 7080;

/// Where [`Config::static_dir`] points when `STATIC_DIR` is unset.
///
/// Repo-relative, because that is where the directory is when the binary is run
/// from the repository root — which is the only situation this default serves.
pub const DEFAULT_STATIC_DIR: &str = "caddy/static";

/// The default for [`Config::shadow_mode`] — **not in `config.py`**.
///
/// `false`, and it must stay `false`: this variable decides whether the instance
/// is allowed to fetch, so the two ways of getting it wrong are not symmetric. A
/// shadow that defaults to fetching spends production's WARP exit on mirrored
/// traffic; a production instance that defaults to shadowing quietly serves
/// error pages for every post it has not already cached. Named rather than
/// inlined so the test below can pin it.
pub const DEFAULT_SHADOW_MODE: bool = false;

/// `API_CACHE_SECONDS`' default. See [`Config::api_cache_seconds`] for why it is
/// five minutes rather than the five hours `CACHE_LIFE_TIME` defaults to.
pub const DEFAULT_API_CACHE_SECONDS: u64 = 300;

/// The defaults for the four rate-limit pairs, grouped so the *shape* of the
/// policy is visible in one place: the request bucket is the loose one, the miss
/// bucket is roughly a third of it, the global budget is what the exit can take,
/// and a token buys a looser per-IP pair **and nothing else**.
///
/// Tight on purpose. Every one of these can be raised by editing the environment;
/// none of them can be un-spent after the WARP exit is exhausted, and §2.7
/// warning 1 is that the casualty is the whole site rather than this API.
pub const DEFAULT_API_RATE_LIMIT_PER_MINUTE: u32 = 10;
pub const DEFAULT_API_RATE_LIMIT_BURST: u32 = 5;
pub const DEFAULT_API_MISS_LIMIT_PER_MINUTE: u32 = 3;
pub const DEFAULT_API_MISS_LIMIT_BURST: u32 = 1;
pub const DEFAULT_API_FETCH_BUDGET_PER_MINUTE: u32 = 30;
pub const DEFAULT_API_FETCH_BUDGET_BURST: u32 = 5;
pub const DEFAULT_API_TOKEN_LIMIT_PER_MINUTE: u32 = 60;
pub const DEFAULT_API_TOKEN_LIMIT_BURST: u32 = 20;

/// Why the server could not be configured.
///
/// Both cases are boot failures, not request failures: the process exits rather
/// than serving with a value it guessed.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `ADMIN_SECRET_KEY` is unset.
    ///
    /// Not defaulted on purpose. It gates `/delete-from-cache` and the
    /// `no-redis`/`no-db-cache` bypasses, so a default would be a default
    /// *password* — and the legacy code has no default either, it just raises.
    #[error(
        "ADMIN_SECRET_KEY is not set. It has no default: it gates \
         /delete-from-cache and the cache-bypass query parameters. Set it in \
         the environment (compose reads .env)."
    )]
    MissingAdminSecretKey,

    /// A variable is set but cannot be the type it claims to be.
    #[error("{name} is set to {value:?}, which is not a valid {expected}")]
    Invalid {
        name: &'static str,
        value: String,
        expected: &'static str,
    },
}

/// Everything `legacy/web/server/config.py` exposes, resolved once at boot.
#[derive(Debug, Clone)]
pub struct Config {
    /// `HOST_ADDRESS` — the site's own origin, interpolated into every page.
    pub host_address: String,

    /// `MEDIUM_AUTH_COOKIES`. Held because the legacy passes it to `MediumApi`;
    /// read by [`crate::state::AppState`] when it builds the source.
    pub medium_auth_cookies: Option<String>,

    /// `MEDIUM_IMPERSONATE` — which fingerprint the outbound GraphQL fetch
    /// presents.
    ///
    /// `chrome110` by default, because that is what `api.py:75` pins and what
    /// SPIKE-1 measured at parity 1.0000 against the `curl_cffi` baseline. The
    /// other profiles exist so a profile A/B is a config change rather than a
    /// rebuild — **and a comparison across profiles is not a comparison of
    /// clients**, so a baseline measured at one profile says nothing about
    /// another.
    ///
    /// # The name is not the guarantee
    ///
    /// Worth knowing before "upgrading" this: `wreq_util`'s Chrome 110 profile
    /// takes its *headers* from 110 but its **ClientHello and HTTP/2 settings
    /// from Chrome 100** (`emulate/profile/chrome.rs`), while `curl_cffi`'s
    /// `chrome110` is an unrelated construction built from its own fingerprint
    /// capture. Two profiles sharing a name guarantees nothing about the bytes
    /// on the wire. Only a measurement settles it — see
    /// `xtask/spike-impersonate/README.md`.
    pub medium_impersonate: Profile,

    /// `ADMIN_SECRET_KEY`. Required — see [`ConfigError`].
    pub admin_secret_key: String,

    /// `TELEGRAM_ADMIN_ID`. `0` means "not configured", as in the legacy
    /// (`config.py:11`'s default), and [`crate::notify`] treats it that way.
    pub telegram_admin_id: i64,

    /// `TELEGRAM_BOT_TOKEN`.
    pub telegram_bot_token: Option<String>,

    /// `LOG_LEVEL_NAME` — the `tracing` filter directive.
    pub log_level_name: String,

    /// `MORE_LOGS` — raises the per-request logging to `TRACE`-equivalent
    /// verbosity, including every request and response header.
    pub more_logs: bool,

    /// `DISABLE_EXTERNAL_DOCS`. Fase 6 gives this its first consumer: when true,
    /// `/api/v1/openapi.json` and `/api/v1/docs` answer a `problem+json` 404
    /// instead of the spec. Still only a documentation question — the API itself
    /// is unaffected, which is what the name promises.
    pub disable_external_docs: bool,

    /// `TIMEOUT` — the whole-request budget, the one the middleware enforces.
    pub timeout: Duration,

    /// `REQUEST_TIMEOUT` — the outbound-fetch budget.
    pub request_timeout: Duration,

    /// `WORKER_TIMEOUT`. Read for completeness; the async server has no worker
    /// to time out, and nothing consumes this.
    pub worker_timeout: Duration,

    /// `CACHE_LIFE_TIME` — the Redis TTL for a rendered post.
    pub cache_life_time: Duration,

    /// `HOME_PAGE_MAX_POSTS` — how many random posts `/` samples.
    pub home_page_max_posts: i64,

    /// `ENABLE_ADS_BANNER` — the banner in `base.html`.
    pub enable_ads_banner: bool,

    /// `SHADOW_MODE` — **not in `config.py`**. This is the second Rust-only
    /// variable here, and it exists for Fase 4's shadow traffic.
    ///
    /// When set, this instance is a *shadow*: it is fed a copy of production's
    /// traffic, its answers are compared against Python's, and nothing it does
    /// is allowed to have a consequence. Two places enforce that:
    ///
    /// - [`crate::handlers::post::query`] refuses to fetch on a durable-cache
    ///   miss, and answers with a marked not-comparable response instead
    ///   ([`crate::error`]). This is the interlock that keeps the shadow off the
    ///   WARP exit production serves from — `RUST_REWRITE_PLAN` §2.7 says
    ///   exhausting it takes the whole site down, and SPIKE-1's pooled
    ///   measurement is still open.
    /// - [`crate::error::should_alert`] never sends a Telegram alert, because
    ///   production is already alerting on the same request and a second message
    ///   would be a duplicate at best. Over §5's seven-day soak that is the
    ///   difference between a usable signal and a pager nobody reads.
    ///
    /// Default `false`, so an unset environment is exactly today's behaviour —
    /// this variable must never be what makes production a shadow. The default
    /// matters more than usual here: the failure mode of the opposite default is
    /// a production instance that has quietly stopped fetching.
    pub shadow_mode: bool,

    /// `REDIS_HOST` / `REDIS_PORT` / `REDIS_TIMEOUT`, kept separate rather than
    /// collapsed into one `REDIS_URL`: the deployed `.env` sets these three, and
    /// inventing a fourth name would make the existing file stop applying.
    pub redis_host: String,
    pub redis_port: u16,
    pub redis_timeout: f64,

    /// `DATABASE_URL`.
    pub database_url: String,

    /// `PROXY_LIST`, already split on commas.
    pub proxy_list: Vec<String>,

    /// `--port`, `services/cli.py:11`.
    pub port: u16,

    /// Where `ServeDir` looks for the static files Caddy serves in production.
    ///
    /// **Not in `config.py`** — this is the one Rust-only variable here, and it
    /// exists because the plan puts `ServeDir` under the page routes so the
    /// binary runs standalone in dev (§5). In production Caddy answers those
    /// paths itself with explicit `handle_path` blocks and never forwards them,
    /// so the value only matters to a developer running `cargo run`.
    ///
    /// The default is the repo-relative path, which is where it sits when the
    /// binary is run from the repository root; a container sets it to the copy
    /// the image actually holds.
    pub static_dir: String,

    // ---------------------------------------------------------------- //
    // Fase 6 — everything `/api/v1` reads. None of it is in `config.py`,
    // because there was no public API there; the defaults are chosen so
    // that an environment that sets none of these still boots and still
    // serves, with the tight limits.
    // ---------------------------------------------------------------- //
    /// `API_RATE_LIMIT_PER_MINUTE` / `API_RATE_LIMIT_BURST` — the per-IP bucket
    /// every `/api/v1` request is charged against, keyed on the client address.
    pub api_rate_limit_per_minute: u32,
    pub api_rate_limit_burst: u32,

    /// `API_MISS_LIMIT_PER_MINUTE` / `API_MISS_LIMIT_BURST` — the second per-IP
    /// bucket, charged **only when the durable cache misses**.
    ///
    /// This is the one that matters, and it is deliberately much tighter than the
    /// request bucket: a client walking the id space hits the cache zero times
    /// while the request bucket would happily fund it. A client replaying ids
    /// production already has cached spends nothing here, which is the behaviour
    /// that lets a legitimate consumer page through a corpus.
    pub api_miss_limit_per_minute: u32,
    pub api_miss_limit_burst: u32,

    /// `API_FETCH_BUDGET_PER_MINUTE` / `API_FETCH_BUDGET_BURST` — the
    /// **process-global** budget, charged before every outbound fetch and before
    /// every `link.medium.com` resolve.
    ///
    /// Global because per-IP is not enough: the thing being protected is one WARP
    /// exit, and a distributed scrape has as many client addresses as it wants.
    /// `RUST_REWRITE_PLAN` §2.7 warning 1 is that exhausting that exit takes the
    /// whole site down, not just this API.
    ///
    /// **No token can raise this.** See [`Config::api_token`] — that is a policy
    /// limit and this is a physical one, and conflating them is the failure this
    /// field exists to prevent. The number is *safe, not correct*: what one exit
    /// actually tolerates is SPIKE-1's pooled measurement, still open.
    pub api_fetch_budget_per_minute: u32,
    pub api_fetch_budget_burst: u32,

    /// `API_TOKEN_LIMIT_PER_MINUTE` / `API_TOKEN_LIMIT_BURST` — the numbers a
    /// caller that presents the right `X-API-TOKEN` is charged instead of the two
    /// per-IP pair above.
    pub api_token_limit_per_minute: u32,
    pub api_token_limit_burst: u32,

    /// `API_TOKEN` — one shared token, or `None`.
    ///
    /// `None` (unset **or empty**) means the tier does not exist and the header is
    /// **ignored entirely**, not rejected: an empty default that 401s would fail
    /// every client that sends the header to a server that never opted in.
    ///
    /// It is pure rate-limit identity. It does not select a `PostSource`, does
    /// not reach `MEDIUM_AUTH_COOKIES`, and does not raise
    /// [`Config::api_fetch_budget_per_minute`].
    ///
    /// One shared secret rather than §2.7's `api_keys` table, which is §2.4's
    /// no-migration rule and one more thing to leak. Compared in constant time
    /// (`crate::api::limit`), because a `==` on a secret is a timing oracle.
    pub api_token: Option<String>,

    /// `API_CACHE_SECONDS` — the `max-age` on the API's `Cache-Control`.
    ///
    /// Default **300**, and deliberately not [`Config::cache_life_time`] (five
    /// hours). There is no purge path anywhere in this system:
    /// `/delete-from-cache` removes a Postgres row and invalidates neither Redis
    /// nor any CDN in front of us. A five-hour `max-age` on
    /// `/api/v1/posts/{id}` therefore outlives a deploy, a template fix, and —
    /// during the soak — a corrected parser, and the symptom is a consumer
    /// reporting stale content nobody can flush. Raising this needs a purge story
    /// first; §2.7's number was written as if one existed.
    pub api_cache_seconds: u64,

    /// `API_TRUST_PROXY` — whether to take the client address from
    /// `X-Real-IP`/`X-Forwarded-For` instead of the socket peer.
    ///
    /// Default **false**, and the default is right for a direct deployment: the
    /// headers are client-supplied unless something in front of us overwrites
    /// them, and anything that can set its own `X-Real-IP` can pick its own
    /// bucket.
    ///
    /// **Pre-deploy check, recorded rather than fixed:** production's topology
    /// puts an external reverse proxy in front of Caddy, so `{remote_host}` — the
    /// value Caddy writes into both headers — is that proxy. Every real client
    /// therefore collapses into one bucket, and the per-IP limit becomes a global
    /// 10/min. Turning this on without fixing the Caddyfile would let the client
    /// choose; leaving it off means the limit is shared. Consuming upstream's
    /// `X-Forwarded-For` is a Caddyfile policy change and is out of Fase 6, which
    /// ships the flag, a `debug` log of the resolved address on every 429, and
    /// the check below.
    pub api_trust_proxy: bool,

    /// `CORS_ALLOW_ORIGINS` — the allowlist for `/api/v1`, comma-separated.
    ///
    /// Empty (the default) means *no allowlist*: the API mirrors the request's
    /// origin exactly as the page routes do, which is today's behaviour and what
    /// an unconfigured deployment should get. Filled, the allowlist applies to
    /// `/api/v1` **only** — the page routes keep mirroring, because their
    /// responses are cached per-origin by nothing and a page route's CORS is part
    /// of its byte-parity surface.
    ///
    /// Entries are trimmed and blanks dropped, unlike [`Config::proxy_list`]: a
    /// stray space in `"https://a.com, https://b.com"` would otherwise make the
    /// second origin never match, and there is no legacy behaviour to be faithful
    /// to here.
    pub cors_allow_origins: Vec<String>,

    /// `MEDIUM_GRAPHQL_ENDPOINT` — override the GraphQL endpoint, or `None` for
    /// `medium-client`'s real one.
    ///
    /// **Not a production knob.** It exists so Fase 6's 502/504 paths can be
    /// proved end to end against a local fake GraphQL server, with no internet
    /// and no WARP exit — the same reason the `Transport` trait exists, one level
    /// up. Leaving it unset is the only configuration that talks to Medium.
    ///
    /// Empty means unset, so the default endpoint stays a single constant in
    /// `medium-client` rather than being spelled a second time here.
    pub medium_graphql_endpoint: Option<String>,
}

impl Config {
    /// Reads every variable, with `config.py`'s defaults.
    ///
    /// Fails only on `ADMIN_SECRET_KEY` and on a malformed number or boolean —
    /// never by inventing a value for something security-relevant.
    pub fn from_env() -> Result<Self, ConfigError> {
        let admin_secret_key =
            optional("ADMIN_SECRET_KEY").ok_or(ConfigError::MissingAdminSecretKey)?;

        Ok(Self {
            host_address: text("HOST_ADDRESS", DEFAULT_HOST_ADDRESS),
            medium_auth_cookies: optional("MEDIUM_AUTH_COOKIES"),
            medium_impersonate: impersonate(optional("MEDIUM_IMPERSONATE"))?,
            admin_secret_key,
            telegram_admin_id: number("TELEGRAM_ADMIN_ID", 0_i64)?,
            telegram_bot_token: optional("TELEGRAM_BOT_TOKEN"),
            log_level_name: text("LOG_LEVEL_NAME", "INFO"),
            more_logs: boolean("MORE_LOGS", false)?,
            disable_external_docs: boolean("DISABLE_EXTERNAL_DOCS", true)?,
            timeout: Duration::from_secs(number("TIMEOUT", 38_u64)?),
            request_timeout: Duration::from_secs(number("REQUEST_TIMEOUT", 12_u64)?),
            worker_timeout: Duration::from_secs(number("WORKER_TIMEOUT", 85_u64)?),
            cache_life_time: Duration::from_secs(number("CACHE_LIFE_TIME", 60 * 60 * 5_u64)?),
            home_page_max_posts: number("HOME_PAGE_MAX_POSTS", 45_i64)?,
            enable_ads_banner: boolean("ENABLE_ADS_BANNER", false)?,
            shadow_mode: boolean("SHADOW_MODE", DEFAULT_SHADOW_MODE)?,
            redis_host: text("REDIS_HOST", DEFAULT_REDIS_HOST),
            redis_port: number("REDIS_PORT", DEFAULT_REDIS_PORT)?,
            redis_timeout: number("REDIS_TIMEOUT", DEFAULT_REDIS_TIMEOUT)?,
            database_url: text("DATABASE_URL", DEFAULT_DATABASE_URL),
            // `config.py:38-39`: split on commas, and an empty or unset value is
            // an empty list rather than `[""]`. That distinction matters — a
            // one-element list containing the empty string would make
            // `ProxyPool` try to parse "" as a proxy URL at startup.
            proxy_list: parse_proxy_list(optional("PROXY_LIST")),
            port: number("PORT", DEFAULT_PORT)?,
            static_dir: text("STATIC_DIR", DEFAULT_STATIC_DIR),
            api_rate_limit_per_minute: rate(
                "API_RATE_LIMIT_PER_MINUTE",
                DEFAULT_API_RATE_LIMIT_PER_MINUTE,
            )?,
            api_rate_limit_burst: rate("API_RATE_LIMIT_BURST", DEFAULT_API_RATE_LIMIT_BURST)?,
            api_miss_limit_per_minute: rate(
                "API_MISS_LIMIT_PER_MINUTE",
                DEFAULT_API_MISS_LIMIT_PER_MINUTE,
            )?,
            api_miss_limit_burst: rate("API_MISS_LIMIT_BURST", DEFAULT_API_MISS_LIMIT_BURST)?,
            api_fetch_budget_per_minute: rate(
                "API_FETCH_BUDGET_PER_MINUTE",
                DEFAULT_API_FETCH_BUDGET_PER_MINUTE,
            )?,
            api_fetch_budget_burst: rate("API_FETCH_BUDGET_BURST", DEFAULT_API_FETCH_BUDGET_BURST)?,
            api_token_limit_per_minute: rate(
                "API_TOKEN_LIMIT_PER_MINUTE",
                DEFAULT_API_TOKEN_LIMIT_PER_MINUTE,
            )?,
            api_token_limit_burst: rate("API_TOKEN_LIMIT_BURST", DEFAULT_API_TOKEN_LIMIT_BURST)?,
            // Empty is unset: see `Config::api_token`.
            api_token: optional_non_empty("API_TOKEN"),
            api_cache_seconds: number("API_CACHE_SECONDS", DEFAULT_API_CACHE_SECONDS)?,
            api_trust_proxy: boolean("API_TRUST_PROXY", false)?,
            cors_allow_origins: parse_origins(optional("CORS_ALLOW_ORIGINS")),
            medium_graphql_endpoint: optional_non_empty("MEDIUM_GRAPHQL_ENDPOINT"),
        })
    }

    /// The Redis URL `fred` wants, composed from the three variables the
    /// deployed `.env` actually sets. See [`Config::redis_host`].
    pub fn redis_url(&self) -> String {
        format!("redis://{}:{}", self.redis_host, self.redis_port)
    }

    /// The Redis socket timeout, as a `Duration`.
    pub fn redis_timeout(&self) -> Duration {
        Duration::from_secs_f64(self.redis_timeout)
    }

    /// Whether the two Telegram values are both present.
    ///
    /// `notify.py:16-18` warns and drops every message when either is missing;
    /// this is the same test, asked once instead of per message.
    pub fn telegram_configured(&self) -> bool {
        self.telegram_bot_token.is_some() && self.telegram_admin_id != 0
    }
}

/// `PROXY_LIST` → the endpoints, matching `config.py:38-39`.
///
/// `PROXY_LIST_RAW.split(",") if PROXY_LIST_RAW else []`: an unset *or empty*
/// variable yields no endpoints, but a set one yields whatever is between the
/// commas — including empty entries, which the legacy would also pass through.
/// Kept faithful rather than tidied, so a typo like `a,,b` reaches the pool the
/// same way it does today.
fn parse_proxy_list(raw: Option<String>) -> Vec<String> {
    match raw {
        Some(raw) if !raw.is_empty() => raw.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// A variable, or `None` when unset. An empty string is a *value*, not absence —
/// matching `os.environ`/starlette, where `FOO=` sets `FOO` to `""`.
fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// A variable, or `None` when unset **or empty**.
///
/// The difference from [`optional`] is the whole on/off switch for the two
/// variables that use it: `API_TOKEN=` is how an operator turns the token tier
/// off without deleting the line, and `MEDIUM_GRAPHQL_ENDPOINT=` is how an
/// environment says "the real endpoint" rather than pointing at a stale local
/// override. Both are *empty means default*, which is not what a bare `optional`
/// would say — and for `API_TOKEN` the difference is a 401 for every client that
/// sends the header.
fn optional_non_empty(name: &str) -> Option<String> {
    non_empty(optional(name))
}

/// [`optional_non_empty`]'s decision, isolated so it can be tested: `std::env`
/// is global state and `set_var` is `unsafe` in edition 2024, which
/// `unsafe_code = "forbid"` rules out — so nothing in this crate can test
/// `from_env` itself, and the parsing has to be reachable without the
/// environment.
///
/// Trims, unlike [`optional`]. Both values are used as opaque strings compared or
/// dialled, and a `.env` line is one stray space away from a token that never
/// matches or a URL that cannot connect. `number` and `boolean` trim for the same
/// reason; a token whose value *intentionally* has leading whitespace cannot be
/// sent in an HTTP header anyway.
fn non_empty(raw: Option<String>) -> Option<String> {
    match raw {
        Some(raw) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        _ => None,
    }
}

/// A rate limit, as a strictly positive number of requests per minute.
///
/// Zero is rejected rather than accepted and clamped, because the two ways it can
/// arrive are both mistakes and neither has a graceful reading: `governor`'s
/// `Quota` panics on a zero burst, and a zero rate means "never", which is a way
/// of disabling an endpoint that reads like a typo. A boot failure naming the
/// variable is the useful answer — the same reasoning as
/// [`ConfigError::MissingAdminSecretKey`], one scale down.
fn rate(name: &'static str, default: u32) -> Result<u32, ConfigError> {
    positive(name, number(name, default)?)
}

/// [`rate`]'s check, isolated for the same reason as [`non_empty`].
fn positive(name: &'static str, value: u32) -> Result<u32, ConfigError> {
    if value == 0 {
        return Err(ConfigError::Invalid {
            name,
            value: "0".to_string(),
            expected: "a positive number of requests per minute",
        });
    }
    Ok(value)
}

/// The `MEDIUM_IMPERSONATE` value, or [`Config::medium_impersonate`]'s default.
///
/// Isolated for the same reason as [`non_empty`]: `set_var` is `unsafe` under
/// `unsafe_code = "forbid"`, so `from_env` cannot be tested and the parsing has
/// to be reachable without the environment.
///
/// An unrecognised name is a **boot failure, not a silent fallback**. Falling
/// back to `chrome110` would mean a deployment that asked for one fingerprint
/// and got another, with nothing in the logs to say so — and the whole point of
/// the setting is that the fingerprint is the thing being controlled. The error
/// names the alternatives.
fn impersonate(raw: Option<String>) -> Result<Profile, ConfigError> {
    let raw = match raw {
        Some(raw) if !raw.trim().is_empty() => raw.trim().to_string(),
        _ => DEFAULT_IMPERSONATE.to_string(),
    };

    Profile::parse(&raw).ok_or(ConfigError::Invalid {
        name: "MEDIUM_IMPERSONATE",
        value: raw,
        expected: "one of chrome110, chrome120, chrome124, chrome136",
    })
}

/// [`Config::medium_impersonate`]'s default.
///
/// `api.py:75`'s `impersonate="chrome110"`, and the profile SPIKE-1 measured.
/// A test below pins it against `Profile::ALL[0]`, so this cannot drift away
/// from the profile the transport itself considers the default.
const DEFAULT_IMPERSONATE: &str = "chrome110";

/// `CORS_ALLOW_ORIGINS` → the allowlist, matching [`parse_proxy_list`]'s
/// unset-or-empty rule but not its literalness.
///
/// Entries are trimmed and blanks dropped. `parse_proxy_list` keeps both because
/// its values go straight to a URL parser that reports them and the legacy
/// behaved that way; an origin is compared *by equality*, so a stray space would
/// make `https://a.com` silently never match and the failure would look like a
/// CORS bug in the browser rather than a typo in the environment.
fn parse_origins(raw: Option<String>) -> Vec<String> {
    match raw {
        Some(raw) if !raw.trim().is_empty() => raw
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn text(name: &str, default: &str) -> String {
    optional(name).unwrap_or_else(|| default.to_string())
}

fn number<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
{
    let Some(raw) = optional(name) else {
        return Ok(default);
    };
    raw.trim().parse::<T>().map_err(|_| ConfigError::Invalid {
        name,
        value: raw,
        expected: "number",
    })
}

/// `starlette.config`'s boolean cast: `1/true/yes/on/t/y` and their negatives,
/// case-insensitive.
///
/// Written out rather than using Rust's `bool::from_str` because that accepts
/// only `"true"` and `"false"` — a `.env` carrying `MORE_LOGS=1` would then be a
/// boot failure here and a working server in Python.
fn boolean(name: &'static str, default: bool) -> Result<bool, ConfigError> {
    let Some(raw) = optional(name) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "t" | "y" => Ok(true),
        "0" | "false" | "no" | "off" | "f" | "n" => Ok(false),
        _ => Err(ConfigError::Invalid {
            name,
            value: raw,
            expected: "boolean (1/0, true/false, yes/no, on/off)",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults must be `config.py`'s. A drifting default here is invisible
    /// until a restart changes production behaviour, so each is pinned.
    #[test]
    fn the_defaults_match_config_py() {
        assert_eq!(DEFAULT_HOST_ADDRESS, "https://freedium.cfd");
        assert_eq!(DEFAULT_REDIS_HOST, "redis_service");
        assert_eq!(DEFAULT_REDIS_PORT, 6379);
        assert_eq!(DEFAULT_REDIS_TIMEOUT, 1.75);
        assert_eq!(DEFAULT_PORT, 7080);
        assert_eq!(
            DEFAULT_DATABASE_URL,
            "postgresql://postgres:postgres@postgres_freedium:5432/postgres"
        );
    }

    /// `SHADOW_MODE` must default to *not* a shadow. Both directions of getting
    /// this wrong are bad and only one of them is recoverable by a restart that
    /// nobody knows to perform — see [`DEFAULT_SHADOW_MODE`].
    #[test]
    fn shadow_mode_is_off_unless_it_is_asked_for() {
        // A compile-time assertion, which is the honest strength of this claim:
        // the default is a literal, so the interesting failure is someone
        // *changing* the literal, and that should not need a test run to catch.
        const { assert!(!DEFAULT_SHADOW_MODE) };
        assert!(!sample().shadow_mode);
    }

    /// `starlette.config` accepts all of these; Rust's `bool::from_str` accepts
    /// only `true`/`false`. If this regressed, a working `.env` would stop
    /// booting.
    #[test]
    fn booleans_accept_the_starlette_spellings() {
        for truthy in ["1", "true", "TRUE", "Yes", "on", "T", "y", " true "] {
            assert!(
                boolean_from(truthy) == Some(true),
                "{truthy:?} should parse as true"
            );
        }
        for falsy in ["0", "false", "NO", "off", "F", "n", " false "] {
            assert!(
                boolean_from(falsy) == Some(false),
                "{falsy:?} should parse as false"
            );
        }
        assert_eq!(
            boolean_from("maybe"),
            None,
            "junk must not silently be false"
        );
    }

    /// The classification `boolean` does, isolated so the test above reads
    /// through the same spellings the real parser does.
    fn boolean_from(raw: &str) -> Option<bool> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "t" | "y" => Some(true),
            "0" | "false" | "no" | "off" | "f" | "n" => Some(false),
            _ => None,
        }
    }

    /// A one-element list holding `""` would reach the proxy pool and fail
    /// parsing at startup, so absence and emptiness must both give `[]`.
    #[test]
    fn an_empty_proxy_list_is_empty_not_a_blank_entry() {
        assert!(parse_proxy_list(None).is_empty(), "unset");
        assert!(parse_proxy_list(Some(String::new())).is_empty(), "empty");

        assert_eq!(
            parse_proxy_list(Some("socks5://a:1080".into())),
            vec!["socks5://a:1080"]
        );
        assert_eq!(
            parse_proxy_list(Some("socks5://a:1080,socks5://b:1080".into())),
            vec!["socks5://a:1080", "socks5://b:1080"]
        );
    }

    #[test]
    fn the_redis_url_is_composed_from_three_variables() {
        let config = sample();
        assert_eq!(config.redis_url(), "redis://redis_service:6379");
        assert_eq!(config.redis_timeout(), Duration::from_millis(1750));
    }

    /// `notify.py:16`: either value missing means no Telegram, and `0` is
    /// "missing" because that is `config.py:11`'s default.
    #[test]
    fn telegram_needs_both_values_and_zero_is_missing() {
        let mut config = sample();
        assert!(!config.telegram_configured());

        config.telegram_admin_id = 42;
        assert!(!config.telegram_configured(), "token still absent");

        config.telegram_bot_token = Some("t".into());
        assert!(config.telegram_configured());
    }

    /// Empty means *off* for `API_TOKEN` and `MEDIUM_GRAPHQL_ENDPOINT`, and the
    /// consequence is not symmetric between them: an unset token that 401'd
    /// would fail every client sending the header, while an empty endpoint that
    /// became `""` would make every fetch fail with a URL parse error. Both are
    /// why this is not a bare `optional`.
    #[test]
    fn an_empty_variable_is_off_not_empty() {
        assert_eq!(non_empty(None), None, "unset");
        assert_eq!(non_empty(Some(String::new())), None, "`FOO=`");
        assert_eq!(non_empty(Some("   ".into())), None, "`FOO=   `");

        assert_eq!(non_empty(Some("tok".into())).as_deref(), Some("tok"));
        // Trimmed, unlike `optional`: a `.env` line's stray space must not
        // become a token that never matches or a URL that cannot connect.
        assert_eq!(non_empty(Some(" tok ".into())).as_deref(), Some("tok"));
    }

    /// The profile name is the whole setting, so an unrecognised one has to stop
    /// the boot. See [`impersonate`] for why a silent fallback would be worse
    /// than the failure: the deployment would run a fingerprint nobody asked
    /// for and nothing would say so.
    #[test]
    fn an_unknown_impersonation_profile_is_a_boot_failure() {
        let err = impersonate(Some("chrome999".into())).expect_err("must not fall back");

        match err {
            ConfigError::Invalid {
                name,
                value,
                expected,
            } => {
                assert_eq!(name, "MEDIUM_IMPERSONATE");
                // The offending value is echoed, which is what makes a typo in a
                // `.env` findable from the log line alone.
                assert_eq!(value, "chrome999");
                // And the alternatives are listed, so the failure is also the
                // documentation. Asserted on the name rather than the whole
                // string so that adding a profile does not break this test.
                assert!(expected.contains("chrome110"), "{expected}");
            }
            other => panic!("expected ConfigError::Invalid, got {other:?}"),
        }
    }

    /// Absence and emptiness both mean *default*, the same rule [`non_empty`]
    /// applies to `API_TOKEN` — and here the default is not "off" but the
    /// measured profile.
    #[test]
    fn an_unset_impersonation_profile_is_the_default_one() {
        for raw in [None, Some(String::new()), Some("   ".into())] {
            assert_eq!(
                impersonate(raw.clone()).expect("default must parse"),
                Profile::Chrome110,
                "{raw:?}"
            );
        }
    }

    /// Trimmed and case-insensitive, because `Profile::parse` does both and the
    /// value comes from a `.env` line a human typed. Pinned here rather than only
    /// against `Profile::parse` because the trimming is this helper's job, not
    /// the parser's.
    #[test]
    fn an_impersonation_profile_is_trimmed_and_case_insensitive() {
        for raw in ["chrome120", " CHROME120 ", "Chrome120"] {
            assert_eq!(
                impersonate(Some(raw.into())).expect("must parse"),
                Profile::Chrome120,
                "{raw:?}"
            );
        }
    }

    /// The default is a string literal and `Profile::ALL[0]` is the transport's
    /// own idea of the default, so this is the one thing keeping the two from
    /// drifting: if the transport's ordering ever changes, the config must move
    /// with it rather than keep naming a profile the rest of the crate no longer
    /// calls default.
    #[test]
    fn the_default_impersonation_profile_is_the_transports_default() {
        const { assert!(DEFAULT_IMPERSONATE.eq_ignore_ascii_case(Profile::ALL[0].name())) };
        assert_eq!(
            impersonate(None).expect("default must parse"),
            Profile::ALL[0]
        );
    }

    /// Zero is a boot failure, not a clamped value — `governor`'s `Quota` panics
    /// on a zero burst, so accepting one would move the failure out of the
    /// config and into a dependency's assertion.
    #[test]
    fn a_rate_limit_of_zero_is_a_boot_failure() {
        assert_eq!(positive("API_RATE_LIMIT_BURST", 5).unwrap(), 5);
        assert_eq!(positive("API_RATE_LIMIT_BURST", 1).unwrap(), 1);

        let error = positive("API_RATE_LIMIT_BURST", 0).unwrap_err();
        let ConfigError::Invalid { name, value, .. } = error else {
            panic!("zero must be an Invalid, not a missing-value error");
        };
        assert_eq!(name, "API_RATE_LIMIT_BURST");
        assert_eq!(value, "0");
    }

    /// The shipped limits, pinned as literals rather than compared against
    /// themselves.
    ///
    /// The direction that matters: these can be raised by editing an
    /// environment, but an exhausted WARP exit cannot be un-spent, and §2.7
    /// warning 1 is that the casualty is the whole site rather than this API. A
    /// change here should be a deliberate edit to this test, not a silent one.
    ///
    /// The relationships are pinned too, because they are the policy: the global
    /// fetch budget sits **above** a single client's miss rate (otherwise a lone
    /// scraper could starve the site) and **below** what a token unlocks
    /// (otherwise the token tier would raise a physical limit, which is the one
    /// thing it must not do).
    #[test]
    fn the_api_limits_are_the_tight_defaults() {
        assert_eq!(DEFAULT_API_RATE_LIMIT_PER_MINUTE, 10);
        assert_eq!(DEFAULT_API_RATE_LIMIT_BURST, 5);
        assert_eq!(DEFAULT_API_MISS_LIMIT_PER_MINUTE, 3);
        assert_eq!(DEFAULT_API_MISS_LIMIT_BURST, 1);
        assert_eq!(DEFAULT_API_FETCH_BUDGET_PER_MINUTE, 30);
        assert_eq!(DEFAULT_API_FETCH_BUDGET_BURST, 5);
        assert_eq!(DEFAULT_API_TOKEN_LIMIT_PER_MINUTE, 60);
        assert_eq!(DEFAULT_API_TOKEN_LIMIT_BURST, 20);

        // Five hours of `CACHE_LIFE_TIME` is the number this must never become:
        // there is no purge path, so a consumer would report stale content that
        // nobody can flush.
        assert_eq!(DEFAULT_API_CACHE_SECONDS, 300);

        const {
            assert!(DEFAULT_API_MISS_LIMIT_PER_MINUTE < DEFAULT_API_FETCH_BUDGET_PER_MINUTE);
            assert!(DEFAULT_API_FETCH_BUDGET_PER_MINUTE < DEFAULT_API_TOKEN_LIMIT_PER_MINUTE);
            assert!(DEFAULT_API_RATE_LIMIT_BURST < DEFAULT_API_TOKEN_LIMIT_BURST);
        }

        assert_eq!(sample().api_token, None, "the tier is off by default");
    }

    /// The allowlist trims and drops blanks — see [`parse_origins`] — and an
    /// unconfigured deployment gets no allowlist at all, which the CORS layer
    /// reads as "mirror the origin", today's behaviour.
    #[test]
    fn the_cors_allowlist_is_empty_unless_it_is_filled() {
        assert!(parse_origins(None).is_empty(), "unset");
        assert!(parse_origins(Some(String::new())).is_empty(), "empty");
        assert!(parse_origins(Some("  ".into())).is_empty(), "blank");

        assert_eq!(
            parse_origins(Some("https://a.example".into())),
            vec!["https://a.example"]
        );
        assert_eq!(
            parse_origins(Some("https://a.example, https://b.example".into())),
            vec!["https://a.example", "https://b.example"],
            "a space after the comma is a typo, not an origin"
        );
        assert_eq!(
            parse_origins(Some("https://a.example,,https://b.example".into())),
            vec!["https://a.example", "https://b.example"],
            "an empty entry would never match any request's Origin"
        );
    }

    /// A literal rather than [`Config::from_env`], which is not a convenience:
    /// `std::env::set_var` is `unsafe` in edition 2024 and `unsafe_code =
    /// "forbid"` rules it out, so no test in this crate can point `from_env` at a
    /// known environment. That is why the parsing decisions live in pure helpers
    /// (`non_empty`, `positive`, `parse_origins`, `boolean_from`) and are tested
    /// there.
    fn sample() -> Config {
        Config {
            host_address: DEFAULT_HOST_ADDRESS.into(),
            medium_auth_cookies: None,
            medium_impersonate: Profile::Chrome110,
            admin_secret_key: "s".into(),
            telegram_admin_id: 0,
            telegram_bot_token: None,
            log_level_name: "INFO".into(),
            more_logs: false,
            disable_external_docs: true,
            timeout: Duration::from_secs(38),
            request_timeout: Duration::from_secs(12),
            worker_timeout: Duration::from_secs(85),
            cache_life_time: Duration::from_secs(60 * 60 * 5),
            home_page_max_posts: 45,
            enable_ads_banner: false,
            shadow_mode: false,
            redis_host: DEFAULT_REDIS_HOST.into(),
            redis_port: DEFAULT_REDIS_PORT,
            redis_timeout: DEFAULT_REDIS_TIMEOUT,
            database_url: DEFAULT_DATABASE_URL.into(),
            proxy_list: Vec::new(),
            port: DEFAULT_PORT,
            static_dir: DEFAULT_STATIC_DIR.into(),
            // The shipped defaults, so a test that reads one of these is reading
            // what a deployment gets rather than a number invented here.
            api_rate_limit_per_minute: DEFAULT_API_RATE_LIMIT_PER_MINUTE,
            api_rate_limit_burst: DEFAULT_API_RATE_LIMIT_BURST,
            api_miss_limit_per_minute: DEFAULT_API_MISS_LIMIT_PER_MINUTE,
            api_miss_limit_burst: DEFAULT_API_MISS_LIMIT_BURST,
            api_fetch_budget_per_minute: DEFAULT_API_FETCH_BUDGET_PER_MINUTE,
            api_fetch_budget_burst: DEFAULT_API_FETCH_BUDGET_BURST,
            api_token_limit_per_minute: DEFAULT_API_TOKEN_LIMIT_PER_MINUTE,
            api_token_limit_burst: DEFAULT_API_TOKEN_LIMIT_BURST,
            // Off. `API_TOKEN` unset is the default deployment, and the tests
            // that want the tier set one explicitly.
            api_token: None,
            api_cache_seconds: DEFAULT_API_CACHE_SECONDS,
            api_trust_proxy: false,
            cors_allow_origins: Vec::new(),
            medium_graphql_endpoint: None,
        }
    }
}
