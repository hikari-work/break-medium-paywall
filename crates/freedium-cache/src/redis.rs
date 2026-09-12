//! The Redis cache, under the `v2:` namespace.
//!
//! # What changes from Python
//!
//! Python stores `pickle.dumps(HtmlResult)` (`legacy/web/server/utils/cache.py`)
//! and `pickle.dumps(...)` for the homepage. A pickle is a Python object graph;
//! Rust cannot read it, and re-implementing the unpickler to read a value with
//! a five-hour lifetime would be absurd. §2.4 replaces the encoding with
//! MessagePack and the keys with [`crate::keys`].
//!
//! So the payload type is generic here. [`RedisStore::get_msgpack`] and
//! [`RedisStore::set_msgpack`] take any `Serialize`/`DeserializeOwned`, and the
//! concrete type — `RenderedPost`, defined in `medium-render` — is the caller's
//! business. That keeps the layering one-way: this crate never needs to know
//! what a rendered post is, and the renderer never needs to know about Redis.
//!
//! # Redis is optional at runtime
//!
//! [`RedisStore::is_available`] is the port of `safe_check_redis_connection`
//! (`legacy/web/server/utils/utils.py:23`), and it exists because every caller
//! in `legacy/web/server/utils/cache.py:12` treats Redis as skippable: if it is
//! down, the handler runs the underlying function directly. A cache outage must
//! degrade to slower, not to broken.
//!
//! That is why every method here returns `Result` rather than panicking, and
//! why `is_available` swallows the error into a plain `bool` — the caller has
//! already decided what to do about it.

use std::time::Duration;

use fred::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::CacheError;

/// A connected Redis client.
#[derive(Debug, Clone)]
pub struct RedisStore {
    client: Client,
}

impl RedisStore {
    /// Connects to `redis://host:port` and completes the handshake.
    ///
    /// Unlike the Postgres pool this eagerly connects, because the legacy
    /// server does the same at startup and because a Redis that is configured
    /// but unreachable should be visible in the logs at boot rather than on the
    /// first request.
    pub async fn connect(redis_url: &str, timeout: Duration) -> Result<Self, CacheError> {
        let client = build_client(redis_url, timeout)?;
        client.init().await?;
        Ok(Self { client })
    }

    /// Builds the client **without connecting**.
    ///
    /// [`RedisStore::connect`] completes the handshake (`client.init()`), so it
    /// fails against an unreachable Redis. This does not, and the failure
    /// surfaces from the first `get`/`set`/`ping` instead — within `timeout`,
    /// because a client that cannot connect is exactly the case `timeout` is
    /// there to bound. Used by `freedium-web`'s tests, which build the router to
    /// assert on its routing without a server.
    pub fn connect_lazy(redis_url: &str, timeout: Duration) -> Result<Self, CacheError> {
        let client = build_client(redis_url, timeout)?;
        Ok(Self { client })
    }

    /// Wraps an already-initialised client.
    pub fn from_client(client: Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Reads and MessagePack-decodes `key`.
    ///
    /// A miss is `Ok(None)`, not an error — that is the normal case the whole
    /// cache exists to make cheap.
    pub async fn get_msgpack<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, CacheError> {
        let raw: Option<Vec<u8>> = self.client.get(key).await?;
        match raw {
            None => Ok(None),
            Some(bytes) => Ok(Some(rmp_serde::from_slice(&bytes)?)),
        }
    }

    /// MessagePack-encodes `value` and stores it under `key` with `ttl`.
    ///
    /// `ttl` is passed in rather than fixed here because it is configuration
    /// (`CACHE_LIFE_TIME`, five hours in `legacy/web/server/config.py:23`) and
    /// this crate does not read configuration.
    pub async fn set_msgpack<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        let bytes = rmp_serde::to_vec(value)?;
        let expiry = Expiration::EX(ttl.as_secs() as i64);
        self.client
            .set::<(), _, _>(key, bytes, Some(expiry), None, false)
            .await?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.client.del::<i64, _>(key).await?;
        Ok(())
    }

    /// `PING`, surfacing the failure.
    pub async fn ping(&self) -> Result<(), CacheError> {
        self.client.ping::<String>(None).await?;
        Ok(())
    }

    /// Whether Redis is reachable — the port of `safe_check_redis_connection`.
    ///
    /// Returns `false` instead of an error so that callers read as the Python
    /// ones do: `if !store.is_available().await { render_without_cache() }`.
    ///
    /// # The timeout is the Redis client's, and that is load-bearing
    ///
    /// The legacy's `safe_check_redis_connection` (`utils.py:20-26`) has no
    /// timeout of its own — it relies on `REDIS_TIMEOUT` being set as both
    /// `socket_timeout` and `socket_connect_timeout` on the client
    /// (`server/__init__.py:73-74`), so a Redis that is down fails the ping in
    /// about 1.75 s. That is where the timeout argument to `connect` and
    /// `connect_lazy` goes, and [`RedisStore::is_available`] is the caller that
    /// depends on it: without it the ping on an unreachable Redis does not fail,
    /// it *waits*, and every request to the homepage waits with it. The bound is
    /// pinned by `the_unreachable_client_fails_an_availability_check`.
    pub async fn is_available(&self) -> bool {
        self.ping().await.is_ok()
    }

    /// Closes the connection pool. `quit` rather than a drop so the server sees
    /// a clean disconnect instead of a half-open socket.
    pub async fn close(self) {
        let _ = self.client.quit().await;
    }
}

/// Builds the client, with `REDIS_TIMEOUT` applied the way the legacy applies it.
///
/// `server/__init__.py:73-74` passes the same 1.75 s as both `socket_timeout`
/// and `socket_connect_timeout`, which are two different things in Python and
/// three here:
///
/// - `connection_timeout` — the TCP connect and the TLS handshake;
/// - `internal_command_timeout` — `AUTH`, `HELLO`, `SELECT`;
/// - `default_command_timeout` — every other command, which fred leaves
///   *disabled* by default and which is the analogue of `socket_timeout`.
///
/// The third is the one that matters for a Redis that is reachable but not
/// answering: without it the command waits forever, and the caller — whose only
/// way to serve through a Redis outage is to find out that it is down — waits
/// with it.
///
/// The three live on two different structs in fred 10, neither of them reachable
/// from `Config` (they moved out of it after fred 9), so this goes through the
/// builder rather than mutating the parsed config in place.
fn build_client(redis_url: &str, timeout: Duration) -> Result<Client, CacheError> {
    let config = Config::from_url(redis_url).map_err(|e| CacheError::Config(e.to_string()))?;

    let mut builder = Builder::from_config(config);
    builder
        .with_connection_config(|connection| {
            connection.connection_timeout = timeout;
            connection.internal_command_timeout = timeout;
        })
        .with_performance_config(|performance| {
            performance.default_command_timeout = timeout;
        });
    builder.build().map_err(CacheError::from)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use medium_render::post::RenderedPost;

    use super::RedisStore;

    /// A representative post. Single-character fields keep the expected byte
    /// string below readable.
    fn post() -> RenderedPost {
        RenderedPost {
            title: "T".to_string(),
            description: "D".to_string(),
            url: "U".to_string(),
            html: "H".to_string(),
        }
    }

    /// **The payload layout, pinned as bytes.**
    ///
    /// `rmp_serde` encodes a struct as a MessagePack array by default, so the
    /// cached entry carries field *order* and not field names. That makes a
    /// reorder catastrophic in a way a rename is not: every field is a `String`,
    /// so swapping two would deserialize cleanly and serve a post whose title
    /// was its HTML — for the five hours until the entry expired, after which
    /// the bug would vanish on its own.
    ///
    /// So the bytes are asserted literally. A JSON test could not do this: JSON
    /// puts names on the wire, so it would pass through exactly the reorder this
    /// is here to catch.
    ///
    /// `94` is a four-element array; `a1` is a one-byte string, and `54 44 55
    /// 48` are `T`, `D`, `U`, `H`. Field order in [`RenderedPost`] is
    /// title, description, url, html.
    #[test]
    fn the_encoded_payload_is_an_array_in_field_order() {
        let bytes = rmp_serde::to_vec(&post()).expect("encodes");
        assert_eq!(
            bytes,
            vec![0x94, 0xa1, b'T', 0xa1, b'D', 0xa1, b'U', 0xa1, b'H'],
            "the cached layout changed; entries written by the previous build \
             would decode into the wrong fields"
        );
    }

    /// Encoding is exercised without a server, because the failure this guards
    /// against is a serialisation one: a `#[serde(skip)]` field or a non-string
    /// map key would break the round-trip, and the symptom in production would
    /// be a cache that never hits.
    #[test]
    fn msgpack_round_trips_a_rendered_post() {
        let post = RenderedPost {
            title: "Judul".to_string(),
            description: "Ringkasan".to_string(),
            url: "https://medium.com/p/abc".to_string(),
            html: "<p>halo 😀</p>".to_string(),
        };

        let bytes = rmp_serde::to_vec(&post).expect("encodes");
        let decoded: RenderedPost = rmp_serde::from_slice(&bytes).expect("decodes");

        assert_eq!(decoded, post);
    }

    /// The other half of the layout claim: a **shorter** array must fail to
    /// decode rather than fill the tail with defaults.
    ///
    /// This is what makes adding a field safe. An entry written before the
    /// addition is one element short of the new struct, and a decode that
    /// shrugged that off would leave the new field silently defaulted — an empty
    /// description served to readers. Failing means a cache miss, which is
    /// correct and self-healing.
    #[test]
    fn a_payload_from_an_older_layout_is_a_miss_not_a_wrong_answer() {
        /// The three-field shape a cache entry written by an earlier build has.
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code, reason = "decoded to prove it does not decode")]
        struct OlderRenderedPost {
            title: String,
            description: String,
            url: String,
        }

        let bytes = rmp_serde::to_vec(&post()).expect("encodes");
        assert!(rmp_serde::from_slice::<OlderRenderedPost>(&bytes).is_err());
    }

    // ---------------------------------------------------------------------
    // Integration test. `#[ignore]`d because this tree has no Redis.
    //
    // Redis is the safer of the two to point at a live instance — everything
    // here is written under the `v2:` namespace, which no Python code reads —
    // so it takes the plain `REDIS_URL`. It still gets a variable of its own
    // rather than defaulting to something already exported, so that running the
    // ignored tests is always deliberate.
    //
    //     docker run -d --rm --name freedium-verify-redis -p 56379:6379 redis:7-alpine
    //     FREEDIUM_TEST_REDIS_URL=redis://127.0.0.1:56379 \
    //       cargo test -p freedium-cache -- --ignored
    //
    // Verified green against redis:7-alpine on 2026-09-12. This test had never
    // been executed before that date — the TTL assertion below is the one thing
    // in the crate that only a server can check, and it was passing on nothing
    // but the compiler.
    // ---------------------------------------------------------------------

    /// The TTL must actually be set, not merely accepted.
    ///
    /// This is the one thing about `set_msgpack` that cannot be checked without
    /// a server, and it is worth checking: `Expiration::EX` takes whole seconds,
    /// and getting the units wrong would produce an entry that either never
    /// expires or expires immediately. Neither is visible from the call site.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_REDIS_URL"]
    async fn a_set_value_expires_after_its_ttl() {
        let url = std::env::var("FREEDIUM_TEST_REDIS_URL").unwrap_or_else(|_| {
            panic!("these tests need FREEDIUM_TEST_REDIS_URL, set to a scratch Redis")
        });
        let store = RedisStore::connect(&url, Duration::from_secs(2))
            .await
            .expect("can connect");
        let key = crate::keys::post_key(&format!("test-ttl-{}", std::process::id()));

        store
            .set_msgpack(&key, &post(), Duration::from_secs(1))
            .await
            .unwrap();

        // Present, and readable back, well inside the TTL.
        assert_eq!(
            store.get_msgpack::<RenderedPost>(&key).await.unwrap(),
            Some(post())
        );

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            store.get_msgpack::<RenderedPost>(&key).await.unwrap(),
            None,
            "the entry outlived its TTL, so the expiry was not applied"
        );

        store.delete(&key).await.unwrap();
        store.close().await;
    }

    /// An unreachable Redis must answer "not available" *promptly*, not hang.
    ///
    /// This is the one behaviour of the whole crate that a stub server cannot
    /// exercise and that a compiler cannot check, and getting it wrong is silent:
    /// `freedium-web`'s tests build the router over a lazy client precisely
    /// because it is supposed to fail rather than connect, and a `ping` that
    /// waits forever makes every one of those tests hang instead of failing.
    /// That is exactly what happened before the timeout was plumbed through.
    ///
    /// `.invalid` is reserved by RFC 2606, so this can never resolve and the test
    /// cannot accidentally reach a real Redis.
    #[tokio::test]
    async fn the_unreachable_client_fails_an_availability_check() {
        let store = RedisStore::connect_lazy(
            "redis://unreachable.invalid:6379",
            Duration::from_millis(500),
        )
        .expect("a lazy client always builds");

        let outcome = tokio::time::timeout(Duration::from_secs(5), store.is_available()).await;
        assert_eq!(
            outcome,
            Ok(false),
            "an unreachable Redis must report itself unavailable within its timeout"
        );
    }
}
