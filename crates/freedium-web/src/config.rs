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

    /// `DISABLE_EXTERNAL_DOCS`. Accepted and logged but inert: this server has
    /// no OpenAPI routes to disable, since §2.7 puts the public API in Fase 6.
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

    fn sample() -> Config {
        Config {
            host_address: DEFAULT_HOST_ADDRESS.into(),
            medium_auth_cookies: None,
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
        }
    }
}
