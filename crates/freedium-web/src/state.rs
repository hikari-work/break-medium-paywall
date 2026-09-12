//! Everything a handler needs, built once at boot.
//!
//! The Python equivalent is `legacy/web/server/__init__.py`, which builds module
//! globals at import time (`medium_cache`, `medium_api`, `medium_parser`,
//! `redis_storage`). Those are process-wide and handed to handlers by `from
//! server import ...`; here they are one `Clone`-able struct passed as axum
//! state, which is the same thing with the wiring made explicit.
//!
//! # The source is constructed but is not production-ready
//!
//! [`AppState::source`] is an [`HttpPostSource`] over [`ReqwestTransport`], which
//! has **no TLS impersonation and will not get past Medium's bot check** — see
//! `medium-client`'s docs and §3.1. It is built here because Fase 3's job is to
//! stand the server up and reach parity on everything that is not the fetch; the
//! fetch itself is still SPIKE-1's open question.
//!
//! It is behind `Arc<dyn PostSource>` precisely so that swapping it is a change
//! to [`AppState::new`] and nothing else.

use std::sync::Arc;

use freedium_cache::postgres::PostgresCache;
use freedium_cache::redis::RedisStore;
use medium_client::http::{HttpPostSource, ReqwestTransport};
use medium_client::media::MediaFetcher;
use medium_client::proxy::{HealthProbe, ProxyEndpoint, ProxyPool, WarpTraceProbe};
use medium_client::resolver::HttpLinkResolver;
use medium_client::source::PostSource;
use medium_doc::resolve::LinkResolver;
use medium_render::templates;
use minijinja::Environment;

use crate::config::Config;
use crate::notify::Telegram;

/// Every long-lived dependency, shared across requests.
///
/// `Clone` because that is what axum state is: the cheap handles are `Arc`s and
/// the two backends are themselves `Clone` over pooled connections, so cloning
/// this per request does not clone a connection.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub postgres: PostgresCache,
    pub redis: RedisStore,
    /// The outbound fetch, behind the seam. See the module docs.
    pub source: Arc<dyn PostSource>,
    /// `link.medium.com` → post id.
    pub resolver: Arc<dyn LinkResolver>,
    /// `@miro/` and `render_iframe/`.
    pub media: Arc<MediaFetcher<ReqwestTransport>>,
    pub notifier: Arc<Telegram>,
    /// The six templates, embedded. `Arc` because a `minijinja::Environment` is
    /// not cheap to build and is read-only once built.
    pub templates: Arc<Environment<'static>>,
}

impl AppState {
    /// Builds every backend. Fails if Postgres, Redis or the HTTP client cannot
    /// be constructed.
    ///
    /// **Both data stores are hard boot requirements here.** Postgres because
    /// `__init__.py:48`'s `wait_for_postgres()` raises, as it does in the legacy;
    /// Redis because [`RedisStore::connect`] completes the handshake
    /// (`client.init().await`) and its error is propagated below. The legacy is
    /// *lazier* than this — it tolerates a missing Redis at runtime via
    /// `safe_check_redis_connection` (`utils/utils.py:20-26`) — so a legacy
    /// deployment can run without Redis and this one cannot start. That
    /// difference matters to anyone writing a compose profile: a service running
    /// this binary needs a Redis in the same profile, and `docker-compose.db.yml`
    /// puts `redis_service` in `prod` only.
    ///
    /// This paragraph used to claim the opposite — that `RedisStore::connect`
    /// "only builds a client, it does not connect". It does connect, and the doc
    /// comment was stale; corrected in Fase 4, where the claim was load-bearing
    /// for the shadow's compose wiring rather than merely decorative.
    pub async fn new(config: Config) -> Result<Self, StateError> {
        let config = Arc::new(config);

        let postgres = PostgresCache::connect(&config.database_url)
            .await
            .map_err(StateError::Postgres)?;
        // `__init__.py:54`: `medium_cache.init_db()` on every boot.
        postgres.init_db().await.map_err(StateError::Postgres)?;

        let redis = RedisStore::connect(&config.redis_url(), config.redis_timeout())
            .await
            .map_err(StateError::Redis)?;

        let transport = ReqwestTransport::new().map_err(StateError::Transport)?;

        // `config.PROXY_LIST`, via the same in-process pool that replaced
        // HAProxy in Fase 2. An empty list is allowed and means "go direct",
        // which is also what the legacy does.
        let pool = Arc::new(ProxyPool::new(
            config
                .proxy_list
                .iter()
                .map(ProxyEndpoint::new)
                .collect::<Vec<_>>(),
            health_probe(),
        ));

        let source = HttpPostSource::new(transport.clone(), Arc::clone(&pool))
            .with_timeout(config.request_timeout)
            .with_auth_cookies(config.medium_auth_cookies.clone());

        let resolver = HttpLinkResolver::new(transport.clone());

        let media = MediaFetcher::new(transport.clone(), Arc::clone(&pool))
            .with_timeout(config.request_timeout);

        let notifier = Telegram::new(transport, &config);

        Ok(Self {
            config,
            postgres,
            redis,
            source: Arc::new(source),
            resolver: Arc::new(resolver),
            media: Arc::new(media),
            notifier: Arc::new(notifier),
            templates: Arc::new(templates::environment()),
        })
    }

    /// The embedded Jinja environment.
    pub fn templates(&self) -> &Environment<'static> {
        &self.templates
    }

    /// Whether Redis is answering — `safe_check_redis_connection`
    /// (`utils/utils.py:20-26`), which swallows every error into `false`.
    pub async fn redis_available(&self) -> bool {
        self.redis.is_available().await
    }
}

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("could not connect to Postgres: {0}")]
    Postgres(freedium_cache::error::CacheError),

    #[error("could not build the Redis client: {0}")]
    Redis(freedium_cache::error::CacheError),

    #[error("could not build the HTTP client: {0}")]
    Transport(medium_client::error::TransportError),
}

/// The health probe for the proxy pool.
///
/// `WarpTraceProbe` asks Cloudflare's trace endpoint whether an exit is really
/// on WARP, which is the check the Fase 2 pool was built around. A single
/// `Arc` because the pool holds it for the process's lifetime and
/// `spawn_health_loop` needs a shared handle.
fn health_probe() -> Arc<dyn HealthProbe> {
    Arc::new(WarpTraceProbe::new())
}

/// The build of [`AppState`] that touches no infrastructure.
///
/// The real [`AppState::new`] connects to Postgres and Redis, which makes it
/// useless to any test that wants to assert on *routing* — the admin-key gate,
/// the `no-redis` parsing, which handler a path reaches — or on a response built
/// from a stubbed source. This builds the same struct with:
///
/// - a **lazy** Postgres pool and Redis client, neither of which connects, so a
///   query fails at the query rather than at construction;
/// - [`OfflineLinkResolver`], which refuses the one outbound call the resolve
///   path can make;
/// - a [`FixedSource`], so a post page can be rendered end to end with no
///   network.
///
/// Everything else — the templates, the media fetcher, the notifier, the proxy
/// pool — is the real thing, because none of them reaches out at construction.
/// `ReqwestTransport::new` builds a client and opens no sockets, and the pool is
/// built over an empty list.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use medium_client::error::{FetchError, TransportError};
    use medium_doc::resolve::OfflineLinkResolver;
    use serde_json::Value;
    use std::sync::Mutex;

    /// A post source that answers with whatever it was given.
    pub(crate) struct FixedSource(pub Value);

    #[async_trait]
    impl PostSource for FixedSource {
        async fn fetch_post(&self, _post_id: &str) -> Result<Value, FetchError> {
            Ok(self.0.clone())
        }
    }

    /// A post source that records what it was asked for and always fails.
    ///
    /// The recorded ids are how a test proves a request never reached the
    /// network — a cache hit that still called the source is a bug the response
    /// alone cannot show.
    pub(crate) struct RecordingSource(pub Mutex<Vec<String>>);

    #[async_trait]
    impl PostSource for RecordingSource {
        async fn fetch_post(&self, post_id: &str) -> Result<Value, FetchError> {
            self.0
                .lock()
                .expect("the recording lock is not poisoned")
                .push(post_id.to_string());
            // A transport failure, because that is what "there is no network
            // here" is in this crate's vocabulary — and it is the variant a
            // handler treats as retryable, so a test that accidentally depends
            // on the source failing sees the same path as a real outage.
            Err(FetchError::Transport(TransportError::Other(
                "the recording source does not fetch".to_string(),
            )))
        }
    }

    /// A [`Config`] with every field set, `ADMIN_SECRET_KEY` included.
    ///
    /// A literal rather than [`Config::from_env`] so a test does not depend on
    /// the ambient environment — a developer with `ADMIN_SECRET_KEY` exported
    /// would otherwise get different results from CI.
    pub(crate) fn test_config() -> Config {
        Config {
            host_address: "https://freedium.cfd".to_string(),
            medium_auth_cookies: None,
            admin_secret_key: "test-secret".to_string(),
            telegram_admin_id: 0,
            telegram_bot_token: None,
            log_level_name: "ERROR".to_string(),
            more_logs: false,
            disable_external_docs: true,
            timeout: std::time::Duration::from_secs(38),
            request_timeout: std::time::Duration::from_secs(12),
            worker_timeout: std::time::Duration::from_secs(85),
            cache_life_time: std::time::Duration::from_secs(5 * 60 * 60),
            home_page_max_posts: 45,
            enable_ads_banner: false,
            // The base stub is never a shadow; `shadow_state` below is what
            // makes one, and it asserts on this.
            shadow_mode: false,
            // Also unreachable, and for the same reason as `database_url`: a
            // real hostname would make every `redis_available()` in a test wait
            // on a DNS lookup that cannot answer.
            redis_host: "redis.invalid".to_string(),
            redis_port: 6379,
            redis_timeout: 1.75,
            // Unreachable, and never dialled: the pool is lazy. `.invalid` is
            // reserved by RFC 2606 and can never resolve, so a test that
            // accidentally *does* reach the network fails rather than silently
            // talking to a local server.
            database_url: "postgresql://postgres:postgres@db.invalid:5432/postgres".to_string(),
            proxy_list: Vec::new(),
            port: 7080,
            static_dir: "caddy/static".to_string(),
        }
    }

    /// Builds an [`AppState`] with no infrastructure. See the module docs.
    pub(crate) fn stub_state(source: Arc<dyn PostSource>) -> AppState {
        let config = Arc::new(test_config());
        let transport = ReqwestTransport::new().expect("a reqwest client can be built");
        let pool = Arc::new(ProxyPool::new(Vec::new(), health_probe()));

        AppState {
            // Neither of these dials anything — see the module docs. A query
            // fails with a connection error rather than at construction, which is
            // what lets a routing test run with no database.
            postgres: PostgresCache::connect_lazy(&config.database_url)
                .expect("a lazy pool always builds"),
            redis: RedisStore::connect_lazy(&config.redis_url(), config.redis_timeout())
                .expect("a lazy client always builds"),
            source,
            resolver: Arc::new(OfflineLinkResolver),
            media: Arc::new(MediaFetcher::new(transport.clone(), Arc::clone(&pool))),
            notifier: Arc::new(Telegram::new(transport, &config)),
            templates: Arc::new(templates::environment()),
            config,
        }
    }

    /// The default stub: a source that fails every fetch.
    pub(crate) fn offline_state() -> AppState {
        stub_state(Arc::new(RecordingSource(Mutex::new(Vec::new()))))
    }

    /// A [`stub_state`] whose instance believes it is a Fase 4 shadow.
    ///
    /// Rebuilds the config rather than mutating `test_config()` in place, so the
    /// non-shadow tests cannot be affected by a test that flips this: `Config` is
    /// behind an `Arc` that every stub state shares nothing of, but the *builder*
    /// is shared, and a helper that mutated it would make test order matter.
    pub(crate) fn shadow_state(source: Arc<dyn PostSource>) -> AppState {
        let mut state = stub_state(source);
        let mut config = (*state.config).clone();
        assert!(
            !config.shadow_mode,
            "the base stub is not a shadow; this helper is what makes one"
        );
        config.shadow_mode = true;
        state.config = Arc::new(config);
        state
    }
}
