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
use medium_client::http::{AnonymousSource, HttpPostSource, ReqwestTransport};
use medium_client::media::MediaFetcher;
use medium_client::proxy::{HealthProbe, ProxyEndpoint, ProxyPool, WarpTraceProbe};
use medium_client::request;
use medium_client::resolver::HttpLinkResolver;
use medium_client::source::PostSource;
use medium_doc::resolve::LinkResolver;
use medium_render::templates;
use minijinja::Environment;

use crate::api::limit::Limits;
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
    /// The same fetch, anonymously. See the field docs on [`AppState::new`].
    pub api_source: AnonymousSource,
    /// The API's four rate-limit buckets, built once.
    pub limits: Arc<Limits>,
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

        // `MEDIUM_GRAPHQL_ENDPOINT` overrides the upstream for both this and the
        // API's source — it exists so the failure paths can be exercised against
        // a local server, and `request::ENDPOINT` is the production value. It has
        // to be applied to *both*: a walkthrough with `SHADOW_MODE=false` that
        // left this source pointing at `medium.com` would send real requests to
        // Medium while claiming to be offline, which is how the two lines below
        // read before this was fixed.
        let endpoint = config
            .medium_graphql_endpoint
            .clone()
            .unwrap_or_else(|| request::ENDPOINT.to_string());
        let source = HttpPostSource::new(transport.clone(), Arc::clone(&pool))
            .with_timeout(config.request_timeout)
            .with_endpoint(endpoint.clone())
            .with_auth_cookies(config.medium_auth_cookies.clone());

        // The API's source, built from the same transport and the same pool and
        // **never** from `source`: `AnonymousSource` has no way to reach
        // `with_auth_cookies`, which is what makes "the API spends the account's
        // unlock quota" impossible rather than merely absent. See
        // `medium_client::http`'s docs on the type, and §2.7's warning 2.
        let api_source = AnonymousSource::new(
            transport.clone(),
            Arc::clone(&pool),
            config.request_timeout,
            endpoint,
        );

        let resolver = HttpLinkResolver::new(transport.clone());

        let media = MediaFetcher::new(transport.clone(), Arc::clone(&pool))
            .with_timeout(config.request_timeout);

        let notifier = Telegram::new(transport, &config);

        let limits = Arc::new(Limits::new(&config));
        // The keyed maps have no eviction of their own and their keys are chosen
        // by the caller, so the sweep is part of the design rather than an
        // optimisation. See `api::limit`'s module docs.
        crate::api::limit::spawn_housekeeping(Arc::clone(&limits));

        Ok(Self {
            config,
            postgres,
            redis,
            source: Arc::new(source),
            api_source,
            limits,
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
            // The shipped defaults, not tighter ones: a test that reads a limit
            // should be reading what a deployment gets. A test that needs a
            // different number builds its own config rather than changing this,
            // so the fixture cannot drift away from `Config::from_env`.
            api_rate_limit_per_minute: crate::config::DEFAULT_API_RATE_LIMIT_PER_MINUTE,
            api_rate_limit_burst: crate::config::DEFAULT_API_RATE_LIMIT_BURST,
            api_miss_limit_per_minute: crate::config::DEFAULT_API_MISS_LIMIT_PER_MINUTE,
            api_miss_limit_burst: crate::config::DEFAULT_API_MISS_LIMIT_BURST,
            api_fetch_budget_per_minute: crate::config::DEFAULT_API_FETCH_BUDGET_PER_MINUTE,
            api_fetch_budget_burst: crate::config::DEFAULT_API_FETCH_BUDGET_BURST,
            api_token_limit_per_minute: crate::config::DEFAULT_API_TOKEN_LIMIT_PER_MINUTE,
            api_token_limit_burst: crate::config::DEFAULT_API_TOKEN_LIMIT_BURST,
            // The token tier is **off** here, which is the default deployment:
            // `api_token: None` means the header is ignored, so an API test that
            // never mentions the header is testing the untokened path.
            api_token: None,
            api_cache_seconds: crate::config::DEFAULT_API_CACHE_SECONDS,
            // Off, because the stub has no proxy in front of it and a test that
            // wants proxied addresses sets its own.
            api_trust_proxy: false,
            cors_allow_origins: Vec::new(),
            medium_graphql_endpoint: None,
        }
    }

    /// Builds an [`AppState`] with no infrastructure. See the module docs.
    pub(crate) fn stub_state(source: Arc<dyn PostSource>) -> AppState {
        stub_state_with(&test_config(), source)
    }

    /// [`stub_state`] with a config the caller chose.
    ///
    /// A `&Config` rather than a mutation of [`test_config`]'s result, because
    /// `Config` is behind the `Arc` every stub state holds: a helper that edited
    /// it in place would make one test's change visible to another.
    pub(crate) fn stub_state_with(config: &Config, source: Arc<dyn PostSource>) -> AppState {
        let config = Arc::new(config.clone());
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
            // A real `AnonymousSource`, deliberately: the point of the API tests
            // is that the *page's* source is the stubbed one and this one is
            // untouched. `127.0.0.1:1` is the unreachable endpoint
            // `medium-client`'s own tests use — a closed port refuses
            // immediately, so a handler test that accidentally fetches fails
            // fast rather than hanging on a timeout.
            api_source: AnonymousSource::new(
                transport.clone(),
                Arc::clone(&pool),
                config.request_timeout,
                "http://127.0.0.1:1",
            ),
            limits: Arc::new(Limits::new(&config)),
            resolver: Arc::new(OfflineLinkResolver),
            media: Arc::new(MediaFetcher::new(transport.clone(), Arc::clone(&pool))),
            notifier: Arc::new(Telegram::new(transport, &config)),
            templates: Arc::new(templates::environment()),
            config,
        }
    }

    /// The default stub: a source that fails every fetch.
    pub(crate) fn offline_state() -> AppState {
        offline_state_with(&test_config())
    }

    /// [`offline_state`] with a config the caller chose — for a test that needs
    /// the routing of a real [`AppState`] and a different flag, limit or host.
    pub(crate) fn offline_state_with(config: &Config) -> AppState {
        stub_state_with(config, Arc::new(RecordingSource(Mutex::new(Vec::new()))))
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

    /// One `GET` through the whole application, exactly as a client makes it.
    ///
    /// The full [`crate::router::router`] rather than `crate::api::router`, so
    /// the request passes through the nest and the layers a deployment has. A
    /// test that reached a handler directly could not tell `/api/v1` from the
    /// page catch-all, which is half of what these tests are about.
    async fn get(state: AppState, path: &str) -> axum::response::Response {
        use tower::ServiceExt as _;

        crate::router::router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .expect("a well-formed request"),
            )
            .await
            .expect("the router answers")
    }

    /// The two sources are different objects, and that is the structural half of
    /// the `MEDIUM_AUTH_COOKIES` invariant (§2.7 warning 2, decision 3).
    ///
    /// `Arc::as_ptr` and not a behaviour: this asserts the *wiring*, so it holds
    /// even for a deployment whose config has no cookies at all. If the two ever
    /// became the same `Arc`, the API would be one `.with_auth_cookies` call away
    /// from spending the account's unlock quota, and no other test here would
    /// notice.
    #[tokio::test]
    async fn the_api_and_the_page_use_different_sources() {
        let state = offline_state();

        let page: *const dyn PostSource = Arc::as_ptr(&state.source);
        let api: *const dyn PostSource = Arc::as_ptr(state.api_source.inner());

        assert!(
            !std::ptr::eq(page, api),
            "the API and the page share one source, so nothing stops the API from \
             reaching the page's cookies"
        );
    }

    /// An API cache miss reaches the **anonymous** source and not the page's.
    ///
    /// The recorder is the page's source, so an empty recording is the API
    /// having gone elsewhere. The status is what makes that non-vacuous: a `502`
    /// is only producible by the fetch path, so the handler really did reach a
    /// source (the stub's `127.0.0.1:1` refuses immediately, which is a transport
    /// error, which is an upstream error). Without the status assertion the
    /// recording could be empty because the request stopped at the rate limiter.
    ///
    /// The converse is [`a_page_cache_miss_does_touch_the_page_source`], and the
    /// pair is the point: either one alone passes for the wrong reason.
    #[tokio::test]
    async fn an_api_cache_miss_does_not_touch_the_page_source() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = stub_state(recorder.clone());

        let response = get(state, "/api/v1/posts/0291df856c77").await;

        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_GATEWAY,
            "the request never reached a source, so the recording below is vacuous"
        );
        assert!(
            recorder.0.lock().unwrap().is_empty(),
            "the API fetched through the page's source: {:?}",
            recorder.0.lock().unwrap()
        );
    }

    /// The same request as a *page*, which does reach the page's source.
    ///
    /// Same state, same stubbed source, same miss — only the path differs. That
    /// is what makes it the converse rather than a second test of the same
    /// thing: if the recorder were simply never wired up, this one fails.
    #[tokio::test]
    async fn a_page_cache_miss_does_touch_the_page_source() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = stub_state(recorder.clone());

        let response = get(state, "/0291df856c77").await;

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            recorder.0.lock().unwrap().as_slice(),
            ["0291df856c77"],
            "the page's source is the one a page request fetches through"
        );
    }

    /// The API serves in the configuration production runs, which is the one the
    /// invariant is about.
    ///
    /// `MEDIUM_AUTH_COOKIES` set means the page's source carries an account's
    /// credentials. The assertion is a **comparison**: the same request, through
    /// two states that differ in nothing but that config field, produces the same
    /// status, the same problem `type` and the same `detail`. A status alone
    /// would not catch an API that routed on the config, and the pair is what
    /// makes the equality mean something.
    ///
    /// `detail` is comparable and the body is not: every problem carries a fresh
    /// `request_id`.
    ///
    /// The other half lives in `medium-client`: `AnonymousSource` has no
    /// `with_auth_cookies`, and its own test asserts no `cookie` header is sent.
    /// This test cannot see that — it asserts the two things a config field can
    /// affect from here. The **third** thing it could affect, `/api/v1/health`'s
    /// `auth_cookies_configured`, is not asserted here: the stub's Postgres probe
    /// fails, so the handler answers a `503` problem and never builds the DTO.
    /// The field's DTO half is pinned in `api::health`'s tests instead.
    #[tokio::test]
    async fn the_api_serves_with_cookies_configured() {
        let mut with_cookies = test_config();
        with_cookies.medium_auth_cookies = Some("session=an-account's-real-cookie".to_string());

        let mut answers = Vec::new();
        for config in [&with_cookies, &test_config()] {
            let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
            let state = stub_state_with(config, recorder.clone());

            let response = get(state, "/api/v1/posts/0291df856c77").await;
            assert!(
                recorder.0.lock().unwrap().is_empty(),
                "a configured cookie reached the API's fetch path"
            );

            assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("the problem body is small");
            let json: Value = serde_json::from_slice(&body).expect("a problem is JSON");
            answers.push((json["type"].clone(), json["detail"].clone()));
        }

        assert_eq!(
            answers[0], answers[1],
            "`MEDIUM_AUTH_COOKIES` changed what the API answers, so the API can see it"
        );
        assert_eq!(answers[0].0, "/problems/upstream-error");
    }
}
