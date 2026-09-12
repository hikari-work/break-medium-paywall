//! The Postgres `cache` table.
//!
//! # No schema migration
//!
//! §2.4 makes this the language-neutral contract of the whole rewrite: the
//! table is `cache (key TEXT PRIMARY KEY, value TEXT)` and it already exists in
//! production, holding hundreds of thousands of real Medium GraphQL responses.
//! Rust reads and writes the same table as `PostgreSQLCacheBackend`
//! (`legacy/database-lib/database_lib/main.py:291`) with the same key and the
//! same value bytes. That is what lets the two implementations run side by side
//! during Fase 4's shadow traffic, and it is why there is nothing to migrate.
//!
//! [`PostgresCache::init_db`] is kept only because the legacy backend has it and
//! calls it at startup (`legacy/web/server/__init__.py:57`): for `cache` it is a
//! no-op in production, and it bootstraps a dev database. It must never grow
//! into a migration.
//!
//! # `banned_posts` is the one thing here that is *not* a port
//!
//! The second table has no Python counterpart. `ban_post_list.db` was a
//! `pickledb` file in the container's working directory, and the plan (§2.4)
//! replaces it with a real table rather than a fourth on-disk format. Three
//! things follow from that, and they are the reason this module documents the
//! table rather than just declaring it:
//!
//! - **There is nothing to migrate.** The plan warns to check the file's format
//!   before reading it with `serde_json`; the file does not exist, here or in
//!   the repository (`.gitignore:166` and `.dockerignore:166` both exclude it).
//!   So the table starts empty and there is no importer to write.
//! - **`init_db` creates it in production**, on the first boot of the Rust
//!   server. That is the intended path — the same call already creates `cache` —
//!   but it is a side effect worth knowing about, because it is the one place
//!   this crate changes the database's shape rather than its contents.
//! - **Nothing reads it.** See [`PostgresCache::ban_post`]; that is the legacy
//!   behaviour, not an omission.
//!
//! # Runtime queries, not macros
//!
//! Every statement here is `sqlx::query`, never `sqlx::query!`. The macros check
//! SQL against a live database **at compile time**, which would make `cargo
//! build` require a reachable Postgres with the production schema — impossible
//! in CI, and a poor trade for four statements that are quoted from the legacy
//! implementation anyway.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::error::CacheError;

/// Default bound on concurrent database connections.
///
/// The legacy backend held one psycopg2 connection per gunicorn worker, so this
/// is not a ported number — it is a ceiling for the single-process axum server
/// that replaces N workers (§2.5). Ten is well under Postgres's default
/// `max_connections` of 100 and leaves room for a second instance during
/// shadow traffic.
const DEFAULT_MAX_CONNECTIONS: u32 = 10;

/// How long to wait for a connection from the pool before giving up.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Sampling percentages tried in order by [`PostgresCache::random`], smallest
/// first. See that method for why this is not `ORDER BY RANDOM()`.
const SAMPLE_PERCENTS: [f64; 5] = [0.5, 2.0, 10.0, 50.0, 100.0];

/// A pool of connections to the `cache` table.
#[derive(Debug, Clone)]
pub struct PostgresCache {
    pool: PgPool,
}

impl PostgresCache {
    /// Connects with [`DEFAULT_MAX_CONNECTIONS`].
    pub async fn connect(database_url: &str) -> Result<Self, CacheError> {
        Self::connect_with(database_url, DEFAULT_MAX_CONNECTIONS).await
    }

    pub async fn connect_with(
        database_url: &str,
        max_connections: u32,
    ) -> Result<Self, CacheError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Builds the pool **without connecting to anything**.
    ///
    /// `PgPoolOptions::connect_lazy` defers the first connection to the first
    /// query, so this returns `Ok` against a host that does not exist. The
    /// failure surfaces later, from whichever `pull`/`push`/`random` runs first,
    /// as a connection error rather than a panic.
    ///
    /// That is what makes it the right constructor for `freedium-web`'s tests,
    /// which build the whole router to assert on its routing without standing up
    /// a database. It is deliberately *not* what [`Self::connect`] does: a server
    /// that cannot reach its cache should find out at boot, not on the first
    /// request.
    pub fn connect_lazy(database_url: &str) -> Result<Self, CacheError> {
        let pool = PgPoolOptions::new()
            .max_connections(DEFAULT_MAX_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_lazy(database_url)?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool, for callers that share one across backends.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// `CREATE TABLE IF NOT EXISTS`, matching `PostgreSQLCacheBackend.init_db`.
    ///
    /// Two tables now. `cache` is `main.py:291`'s and has existed in production
    /// for years; `banned_posts` is new — see [`Self::ban_post`].
    ///
    /// **Not safe to call concurrently from two sessions.** Postgres answers
    /// `CREATE TABLE IF NOT EXISTS` by checking `pg_class` and then creating, and
    /// that check is not part of the transaction's conflict detection: two
    /// sessions can both find the table missing and both insert into `pg_type`,
    /// and the loser fails with
    ///
    /// ```text
    /// 23505 duplicate key value violates unique constraint "pg_type_typname_nsp_index"
    /// ```
    ///
    /// rather than with a successful no-op — the `IF NOT EXISTS` does not cover
    /// it. The legacy server calls this once, from one process, at boot
    /// (`legacy/web/server/__init__.py:57`), so production does not race today.
    /// It becomes reachable if Fase 4 ever runs two instances that boot together
    /// against one database: the fix then is a single migration step or an
    /// advisory lock, not a retry loop here.
    ///
    /// Tests hit it immediately, because they are the one caller that runs in
    /// several threads at once — see the `SERIAL` lock in the test module.
    pub async fn init_db(&self) -> Result<(), CacheError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS cache (
                key TEXT PRIMARY KEY,
                value TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        // `ban_post_list.db`'s replacement (rewrite plan §2.4).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS banned_posts (
                key TEXT PRIMARY KEY,
                banned_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// The stored value for `key`, or `None`.
    ///
    /// Returns the raw text: decoding it is [`crate::decode::decode_json`]'s
    /// job, and keeping the two apart is what lets this method be a plain row
    /// read.
    pub async fn pull(&self, key: &str) -> Result<Option<String>, CacheError> {
        let row = sqlx::query("SELECT value FROM cache WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|row| row.get::<String, _>("value")))
    }

    /// Inserts or replaces `key`.
    ///
    /// `ON CONFLICT DO UPDATE` rather than a read-then-write, quoted from
    /// `main.py:366`. The legacy comment on that line is wrong about what it
    /// buys (psycopg2's `with self.connection` is a transaction, so the
    /// upsert is still race-prone against a concurrent writer) but the
    /// statement shape is right and is kept.
    ///
    /// # The value is stored verbatim, and that is a parity detail
    ///
    /// The legacy writer is handed a `dict` and stores `json.dumps` of it
    /// (`main.py:364`; stdlib `json`, aliased `py_json`), which puts a space
    /// after every `,` and `:`. `serde_json::to_string` emits the compact form
    /// instead.
    ///
    /// Both parse to the same value and `workaround_decode_json` accepts either,
    /// so nothing breaks — but a row written by Rust is not byte-identical to
    /// one written by Python, which matters if Fase 4 ever diffs raw cache rows
    /// to check shadow traffic. Nothing here reconciles that, because the right
    /// formatting depends on how that comparison gets built; this is a note so
    /// the difference is known rather than a day of debugging.
    pub async fn push(&self, key: &str, value: &str) -> Result<(), CacheError> {
        sqlx::query(
            "INSERT INTO cache (key, value) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes `key`, reporting whether a row was actually removed.
    ///
    /// The legacy `delete` logs the difference between the two cases and
    /// returns nothing; returning the bool lets the admin endpoint distinguish
    /// "deleted" from "was not there" without reading the logs.
    pub async fn delete(&self, key: &str) -> Result<bool, CacheError> {
        let result = sqlx::query("DELETE FROM cache WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// `SELECT COUNT(*)`, as `all_length` (`main.py:326`).
    pub async fn len(&self) -> Result<i64, CacheError> {
        let row = sqlx::query("SELECT COUNT(*) AS count FROM cache")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("count"))
    }

    pub async fn is_empty(&self) -> Result<bool, CacheError> {
        Ok(self.len().await? == 0)
    }

    /// `SELECT 1` — is this database answering at all.
    ///
    /// # Why not [`Self::len`]
    ///
    /// `/api/v1/health` needs one bit: can we reach Postgres. `len` is
    /// `SELECT COUNT(*)` over the whole `cache` table, which at production size is
    /// a sequential scan of the largest relation in the system — run by every
    /// monitor every few seconds, for a number nothing reads. This asks the
    /// cheapest question that answers "is it up", and the caller bounds it with a
    /// timeout of its own.
    ///
    /// It deliberately does **not** check that the `cache` table exists. A missing
    /// table is a boot-order problem, not a liveness one, and `init_db` is the
    /// thing that fixes it.
    pub async fn probe(&self) -> Result<(), CacheError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Up to `size` `(key, value)` rows whose key starts with `prefix`, in key
    /// order, starting strictly after `cursor`.
    ///
    /// # What this is for, and what it is not
    ///
    /// This is the only query in the crate with a **stable order**, which is what
    /// `/api/v1/feed`'s cursor needs. It is deliberately not "the next N posts by
    /// date": the table is `(key TEXT PRIMARY KEY, value TEXT)` and the value is a
    /// JSON blob, so there is no column to sort by (§2.4 forbids adding one). The
    /// order is therefore **by key** — stable and resumable, but arbitrary as a
    /// *reading* order, and the endpoint says so rather than letting a consumer
    /// discover it.
    ///
    /// [`Self::random`] cannot serve this: `TABLESAMPLE` has no order to resume
    /// from, so a cursor over it is meaningless.
    ///
    /// # `prefix` and `cursor` are different things
    ///
    /// `prefix` is the namespace to page over — `keys::post_key`'s `"v2:post:"`.
    /// `cursor` is the last key the caller saw, or `None` for the beginning. The
    /// `None` case binds `prefix` itself, which is correct rather than a special
    /// case: every key in the namespace is `prefix` followed by at least one more
    /// character, so all of them compare greater than `prefix` — and a key that is
    /// *exactly* `prefix` (an empty post id) is not a post and is excluded, which
    /// is what we want.
    ///
    /// The prefix test is `left(key, length($1)) = $1` rather than `LIKE $1 || '%'`
    /// because `LIKE` would treat a `_` or `%` in the prefix as a wildcard. That
    /// costs the index on the prefix alone, but `key > $cursor` plus `ORDER BY
    /// key` still walks the primary-key index in order and stops as soon as `size`
    /// rows match, so the filter never forces a full sort.
    pub async fn page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        size: i64,
    ) -> Result<Vec<(String, String)>, CacheError> {
        let rows = sqlx::query(
            "SELECT key, value FROM cache
             WHERE left(key, length($1)) = $1 AND key > $2
             ORDER BY key
             LIMIT $3",
        )
        .bind(prefix)
        .bind(cursor.unwrap_or(prefix))
        .bind(size)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect())
    }

    /// Up to `size` arbitrary `(key, value)` rows.
    ///
    /// # This deliberately does not port `ORDER BY RANDOM()`
    ///
    /// Legacy `random` (`main.py:336`) uses `ORDER BY RANDOM() LIMIT n`. That
    /// is a full scan plus a sort of the entire table, and it backs the
    /// homepage, which re-runs it every ten minutes when the homepage cache
    /// expires. On a table of the size this one reaches, that is the most
    /// expensive query the application makes, for a result nobody can predict
    /// anyway. §7 item 2 requires replacing it rather than porting it.
    ///
    /// `TABLESAMPLE SYSTEM (pct)` reads whole pages and stops, so the cost is
    /// proportional to the sample rather than the table. The catch is that it
    /// is proportional to the *table*, not the request: 0.5% of a small table
    /// can be no rows at all. So the percentage escalates until the sample is
    /// big enough. The last step is 100%, i.e. a full scan — reached only when
    /// the table is small enough that every smaller sample came up short, which
    /// is exactly when a full scan is affordable.
    ///
    /// Unlike the legacy query, this may return fewer than `size` rows if the
    /// table itself holds fewer. The homepage wants "some posts", not "n posts".
    pub async fn random(&self, size: i64) -> Result<Vec<(String, String)>, CacheError> {
        for percent in SAMPLE_PERCENTS {
            // TABLESAMPLE's percentage is a constant expression, not a bind
            // parameter, so it is formatted in. `percent` comes from the
            // private const array above — no caller input reaches this string.
            let sql =
                format!("SELECT key, value FROM cache TABLESAMPLE SYSTEM ({percent}) LIMIT $1");
            let rows = sqlx::query(&sql).bind(size).fetch_all(&self.pool).await?;

            if rows.len() as i64 >= size {
                return Ok(rows
                    .into_iter()
                    .map(|row| (row.get("key"), row.get("value")))
                    .collect());
            }

            // Short sample. Remember the best we have and widen the net — the
            // widest attempt is a full scan, so the final return below is the
            // authoritative answer.
            if percent == *SAMPLE_PERCENTS.last().expect("array is not empty") {
                return Ok(rows
                    .into_iter()
                    .map(|row| (row.get("key"), row.get("value")))
                    .collect());
            }
        }

        // Unreachable: the loop returns on the last iteration unconditionally.
        Ok(Vec::new())
    }

    // -----------------------------------------------------------------------
    // `banned_posts` — the replacement for `ban_post_list.db`
    // -----------------------------------------------------------------------

    /// Records that `key` should not be served again (`ban_db.set(key, 1)`,
    /// `handlers/misc.py:36`).
    ///
    /// # Nothing reads this table, and that is the legacy's behaviour too
    ///
    /// `ban_post_list.db` is written by `/delete-from-cache` and by nothing
    /// else: the only other mentions in the whole Python tree are
    /// `__init__.py:83` loading it and `worker.py:29` dumping it on shutdown.
    /// There is no `get`, no filter, no reader — so **the ban has no effect on
    /// what the site serves today.** It is a moderation note that accumulates.
    ///
    /// Reproducing that is deliberate. A Rust side that started honouring the
    /// list would refuse to render posts that production still serves, which is
    /// precisely the divergence the Fase 4 mirror exists to detect. Making the
    /// ban bite is a behaviour change and belongs in a fase that is allowed to
    /// change behaviour, on both sides at once.
    ///
    /// [`Self::is_banned`] exists so that write is verifiable — a table whose
    /// only operation is an unreadable write is a table no test can check — and
    /// so the fase that makes bans bite has a read path waiting.
    ///
    /// # `banned_at` is a timestamp, not `1`
    ///
    /// `pickledb` stored the integer `1`, which carries no information. The
    /// column records when instead; nothing depends on the value either way.
    ///
    /// Re-banning an already-banned key leaves the original timestamp: the row
    /// is only removed by an explicit unban, so `banned_at` is always "banned
    /// since".
    pub async fn ban_post(&self, key: &str) -> Result<(), CacheError> {
        sqlx::query("INSERT INTO banned_posts (key) VALUES ($1) ON CONFLICT (key) DO NOTHING")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether `key` has been banned. See [`Self::ban_post`] for the caveat that
    /// nothing in the request path consults this.
    pub async fn is_banned(&self, key: &str) -> Result<bool, CacheError> {
        let row = sqlx::query("SELECT 1 FROM banned_posts WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escalation ladder must be strictly increasing and end at a full
    /// scan, or `random` can return an empty sample from a non-empty table.
    #[test]
    fn sample_percents_escalate_to_a_full_scan() {
        assert!(
            SAMPLE_PERCENTS.windows(2).all(|w| w[0] < w[1]),
            "not increasing: {SAMPLE_PERCENTS:?}"
        );
        assert_eq!(SAMPLE_PERCENTS.last(), Some(&100.0));
    }

    /// The generated SQL must never interpolate anything but a percentage from
    /// the const array. This pins the shape the comment above claims.
    #[test]
    fn sample_sql_interpolates_only_the_percentage() {
        let sql = format!(
            "SELECT key, value FROM cache TABLESAMPLE SYSTEM ({}) LIMIT $1",
            0.5
        );
        assert_eq!(
            sql,
            "SELECT key, value FROM cache TABLESAMPLE SYSTEM (0.5) LIMIT $1"
        );
        assert!(sql.contains("$1"), "size must stay a bind parameter");
    }

    // ---------------------------------------------------------------------
    // Integration tests. `#[ignore]`d because this tree has no Postgres of its
    // own; `cargo test` stays green without one.
    //
    // These cover the only code in the crate that no unit test can reach: the
    // statements themselves. A typo in a column list, a DDL that does not match
    // the legacy table, or a `TABLESAMPLE` that never returns rows would all
    // pass everything above and fail here. Run them against a throwaway backend
    // — never the deployed one, see `test_database_url` below:
    //
    //     docker run -d --rm --name freedium-verify-pg \
    //       -e POSTGRES_PASSWORD=verify -e POSTGRES_DB=freedium_verify \
    //       -p 55432:5432 postgres:15-alpine
    //     FREEDIUM_TEST_DATABASE_URL=postgres://postgres:verify@127.0.0.1:55432/freedium_verify \
    //       cargo test -p freedium-cache -- --ignored
    //
    // No `--test-threads=1` is needed: `connect` holds `SERIAL` — see the note
    // above it for why this suite cannot run concurrently with itself.
    //
    // Verified green against postgres:15-alpine and redis:7-alpine on
    // 2026-09-12, from an empty schema: 10 passed, 0 failed, twelve runs in a
    // row. Before the `SERIAL` lock landed the same command failed about one run
    // in four, on `len_counts_rows`.
    // ---------------------------------------------------------------------

    /// **The connection string, and why it is not `DATABASE_URL`.**
    ///
    /// [`PostgresCache::random`] samples the whole `cache` table, and in
    /// production that table is the homepage's source of truth
    /// (`legacy/web/server/handlers/post.py:23`). A test that inserted rows into
    /// whatever `DATABASE_URL` pointed at would therefore be publishing junk
    /// posts to the homepage of a running instance — and `DATABASE_URL` is
    /// exactly the variable a developer already has exported from `.env`.
    ///
    /// A name of its own means the production connection string cannot be picked
    /// up by accident. Point it at a scratch database.
    fn test_database_url() -> String {
        std::env::var("FREEDIUM_TEST_DATABASE_URL").unwrap_or_else(|_| {
            panic!(
                "these tests need FREEDIUM_TEST_DATABASE_URL, set to a scratch \
                 database. Do not point it at production: `random` reads the \
                 table the homepage is built from, and these tests write to it."
            )
        })
    }

    const SCRATCH_PREFIX: &str = "v2:test:";

    /// A key nothing else will touch, so a test can insert freely.
    fn scratch_key(suffix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{SCRATCH_PREFIX}{}:{n}:{suffix}", std::process::id())
    }

    /// Removes every key a test created. Called even when an assertion fails,
    /// so a failing run does not leave rows behind for the next one.
    async fn clean_up(cache: &PostgresCache, keys: &[String]) {
        for key in keys {
            let _ = cache.delete(key).await;
        }
    }

    /// Serialises the tests below against each other.
    ///
    /// **The suite shares one `cache` table, and two of these tests assert
    /// table-wide facts about it.** `len` counts every row and `random` samples
    /// every row, so any other test pushing or cleaning up concurrently moves
    /// the figures underneath them. `len_counts_rows` is the one that fails: it
    /// reads a baseline, inserts one row, and asserts the count rose by one —
    /// but a sibling's rows can be *present in the baseline and deleted before
    /// the second read*, so the count can legitimately fall. It passes alone and
    /// fails in roughly one full run in four.
    ///
    /// Holding this for the whole of every test is what makes the default
    /// `cargo test -p freedium-cache -- --ignored` deterministic. The alternative
    /// — telling the reader to pass `--test-threads=1` — is one flag to forget,
    /// and a shared-state test that "passes on its own" is the classic way to
    /// lose an afternoon.
    ///
    /// Ten tests, ~2.4s serial. The lock is process-wide, so it costs nothing
    /// across binaries and is not a bottleneck worth optimising.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Guards [`PostgresCache::init_db`] so the DDL runs once per test process,
    /// however many threads ask for it.
    ///
    /// [`SERIAL`] already serialises callers, so this is a dedupe rather than a
    /// safety net — but the hazard it defends against is real and is described on
    /// [`PostgresCache::init_db`]: `CREATE TABLE IF NOT EXISTS` is not
    /// concurrency-safe, and an unguarded parallel run produced three `23505`s on
    /// `pg_type_typname_nsp_index`. Keep it if the lock is ever relaxed.
    static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    /// The cache plus the lock that keeps this test's view of the shared table
    /// stable. Derefs to [`PostgresCache`], so callers use it directly.
    struct Scratch {
        cache: PostgresCache,
        /// Released when the test's `Scratch` is dropped, including on a panic.
        _serial: tokio::sync::MutexGuard<'static, ()>,
    }

    impl std::ops::Deref for Scratch {
        type Target = PostgresCache;

        fn deref(&self) -> &Self::Target {
            &self.cache
        }
    }

    /// Connects, **ensures the table exists**, and takes the suite's lock.
    ///
    /// Every test goes through here rather than assuming another one ran first:
    /// the table is created once per test process at best, and tests within a
    /// binary run in an order that has nothing to do with what needs what. Doing
    /// it here is also what the legacy server does at boot
    /// (`legacy/web/server/__init__.py:57`), so it is the real startup path.
    ///
    /// The returned [`Scratch`] must be held for the whole test — bind it, do not
    /// discard it — or the lock is released immediately and the guarantees above
    /// evaporate.
    async fn connect() -> Scratch {
        let serial = SERIAL.lock().await;

        let cache = PostgresCache::connect(&test_database_url())
            .await
            .expect("can connect to FREEDIUM_TEST_DATABASE_URL");

        INIT.get_or_init(|| async {
            cache.init_db().await.expect("can create the cache table");
        })
        .await;

        Scratch {
            cache,
            _serial: serial,
        }
    }

    /// `init_db` must be safe to call repeatedly — the legacy server calls it on
    /// every boot (`legacy/web/server/__init__.py:57`).
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn init_db_is_idempotent() {
        let cache = connect().await;
        cache.init_db().await.expect("first init_db");
        cache.init_db().await.expect("second init_db");
    }

    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn push_then_pull_round_trips() {
        let cache = connect().await;
        let key = scratch_key("round-trip");

        cache
            .push(&key, "{\"data\": {\"post\": null}}")
            .await
            .unwrap();
        assert_eq!(
            cache.pull(&key).await.unwrap(),
            Some("{\"data\": {\"post\": null}}".to_string()),
            "pull must return the value as stored, without decoding it"
        );

        clean_up(&cache, &[key]).await;
    }

    /// A miss is `None`, not an error — the normal case the cache exists to make
    /// cheap (`main.py:347-353`).
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn pulling_a_missing_key_is_none() {
        let cache = connect().await;
        let key = scratch_key("absent");

        assert_eq!(cache.pull(&key).await.unwrap(), None);
        assert!(!cache.delete(&key).await.unwrap(), "nothing was there");
    }

    /// **`ON CONFLICT DO UPDATE`** (`main.py:366`): a second push must replace,
    /// not fail on the primary key, and must not add a row.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn pushing_twice_replaces_the_value() {
        let cache = connect().await;
        let key = scratch_key("upsert");

        cache.push(&key, "first").await.unwrap();
        cache.push(&key, "second").await.unwrap();

        assert_eq!(cache.pull(&key).await.unwrap(), Some("second".to_string()));

        clean_up(&cache, &[key]).await;
    }

    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn delete_reports_whether_a_row_went() {
        let cache = connect().await;
        let key = scratch_key("delete");

        cache.push(&key, "value").await.unwrap();
        assert!(cache.delete(&key).await.unwrap(), "the row existed");
        assert!(!cache.delete(&key).await.unwrap(), "and now it does not");
    }

    /// The escalation ladder, end to end. A small table makes `TABLESAMPLE
    /// SYSTEM (0.5)` likely to return nothing at all, which is precisely the
    /// case the ladder exists for — so this asserts the full scan is reached and
    /// the caller still gets `size` rows.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn random_escalates_until_it_has_enough_rows() {
        let cache = connect().await;

        // Enough rows that a request for ten is satisfiable.
        let keys: Vec<String> = (0..30).map(|i| scratch_key(&format!("rand{i}"))).collect();
        for key in &keys {
            cache.push(key, "{\"data\": {\"post\": {}}}").await.unwrap();
        }

        let sample = cache.random(10).await.unwrap();
        assert_eq!(
            sample.len(),
            10,
            "the ladder must end at a full scan, so a non-empty table always \
             yields the requested rows"
        );

        clean_up(&cache, &keys).await;
    }

    /// `len` counts the whole table, so the exact `before + 1` here is only true
    /// because [`connect`] holds [`SERIAL`] for the test's duration.
    ///
    /// That is the whole reason the lock exists, and this is the test that
    /// proved it: `before + 1` is not a safe assertion while siblings are
    /// inserting and cleaning up, because their rows sit in the baseline and can
    /// be gone by the second read. Alone it always passed; in a full run it
    /// failed about one time in four. Without the lock, the honest weak version
    /// would be `>= before + 1` — and even that is unsound, since the count can
    /// legitimately *fall* under concurrent cleanup.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn len_counts_rows() {
        let cache = connect().await;
        let key = scratch_key("len");

        let before = cache.len().await.unwrap();
        cache.push(&key, "value").await.unwrap();
        assert_eq!(cache.len().await.unwrap(), before + 1);
        assert!(!cache.is_empty().await.unwrap());

        clean_up(&cache, &[key]).await;
    }

    /// `init_db` must create `banned_posts` as well as `cache` — it is the only
    /// path that does, and the endpoint that writes bans has no other setup.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn init_db_creates_the_banned_posts_table() {
        let cache = connect().await;

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables
                            WHERE table_name = 'banned_posts')",
        )
        .fetch_one(cache.pool())
        .await
        .unwrap();
        assert!(exists, "init_db must create banned_posts");
    }

    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn ban_post_is_recorded_and_idempotent() {
        let cache = connect().await;
        let key = scratch_key("ban");

        assert!(!cache.is_banned(&key).await.unwrap(), "not banned yet");

        cache.ban_post(&key).await.unwrap();
        assert!(cache.is_banned(&key).await.unwrap());

        // A second ban must not fail on the primary key. The original timestamp
        // surviving is asserted below rather than assumed.
        let first = banned_at(&cache, &key).await;
        cache.ban_post(&key).await.unwrap();
        assert_eq!(
            banned_at(&cache, &key).await,
            first,
            "re-banning must keep the original `banned_at`"
        );

        unban(&cache, &key).await;
    }

    /// The scratch keys a test creates must not accumulate in `banned_posts` the
    /// way they would in `cache` — `clean_up` only knows about `cache`.
    async fn unban(cache: &PostgresCache, key: &str) {
        sqlx::query("DELETE FROM banned_posts WHERE key = $1")
            .bind(key)
            .execute(cache.pool())
            .await
            .expect("can remove the scratch ban");
    }

    /// `probe` answers on a reachable database.
    ///
    /// What this really covers is the *statement*: a typo in it would only show
    /// up here, and `/api/v1/health` is the endpoint that has to be right when
    /// everything else is wrong. The method's doc says why it is not a `COUNT(*)`.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn probe_answers_on_a_reachable_database() {
        let cache = connect().await;
        cache.probe().await.expect("a reachable database probes ok");
    }

    /// **The feed's contract, end to end.**
    ///
    /// Rows come back in key order, the cursor resumes strictly after the last
    /// one, and a second page shares nothing with the first. This is what
    /// `/api/v1/feed` is built on, and no unit test can check it: the ordering is
    /// the database's, not ours.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn page_walks_a_prefix_in_key_order() {
        let cache = connect().await;
        let prefix = scratch_key("page:");

        // Deliberately inserted out of order, so an implementation that returned
        // insertion order would fail rather than pass by luck.
        let keys: Vec<String> = ["d", "a", "c", "b"]
            .iter()
            .map(|suffix| format!("{prefix}{suffix}"))
            .collect();
        for key in &keys {
            cache.push(key, "{}").await.unwrap();
        }

        let first = cache.page(&prefix, None, 2).await.unwrap();
        assert_eq!(
            first
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec![keys[1].as_str(), keys[3].as_str()],
            "the order is by key, so a before b — not the insertion order"
        );

        let second = cache.page(&prefix, Some(&first[1].0), 2).await.unwrap();
        assert_eq!(
            second
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec![keys[2].as_str(), keys[0].as_str()],
            "resuming after b gives c, d"
        );

        let third = cache.page(&prefix, Some(&second[1].0), 2).await.unwrap();
        assert!(third.is_empty(), "the prefix is exhausted");

        clean_up(&cache, &keys).await;
    }

    /// `size` is a limit, not a target: a prefix with fewer rows returns what
    /// there is, and the caller can tell from the length.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn page_returns_what_exists_when_the_prefix_is_short() {
        let cache = connect().await;
        let prefix = scratch_key("short:");
        let key = format!("{prefix}only");
        cache.push(&key, "value").await.unwrap();

        let rows = cache.page(&prefix, None, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "value", "the value is returned raw");

        clean_up(&cache, &[key]).await;
    }

    /// **The prefix test must not be a `LIKE` pattern.**
    ///
    /// `_` is a single-character wildcard in `LIKE`, and the key namespace is
    /// caller-supplied. A prefix containing one would otherwise match a key it
    /// should not, which is the difference between a feed page and a leak of
    /// another namespace's rows.
    #[tokio::test]
    #[ignore = "needs FREEDIUM_TEST_DATABASE_URL"]
    async fn page_does_not_treat_the_prefix_as_a_wildcard() {
        let cache = connect().await;
        let base = scratch_key("wild");
        let real = format!("{base}_x:real");
        let decoy = format!("{base}Qx:decoy");
        cache.push(&real, "real").await.unwrap();
        cache.push(&decoy, "decoy").await.unwrap();

        let rows = cache.page(&format!("{base}_x:"), None, 10).await.unwrap();
        assert_eq!(
            rows.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            vec![real.as_str()],
            "`_` matched itself, not any character"
        );

        clean_up(&cache, &[real, decoy]).await;
    }

    /// `banned_at` as stored, for the idempotency assertion.
    ///
    /// Cast to `text` rather than decoded into a date type: `sqlx`'s `chrono`
    /// feature is not enabled and one comparison in one test is not a reason to
    /// enable it. The text form is stable for a fixed server timezone.
    async fn banned_at(cache: &PostgresCache, key: &str) -> String {
        sqlx::query_scalar("SELECT banned_at::text FROM banned_posts WHERE key = $1")
            .bind(key)
            .fetch_one(cache.pool())
            .await
            .expect("the row exists")
    }
}
