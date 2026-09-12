//! The two page routes: `/` and everything that resolves to an article.
//!
//! Ports `handlers/post.py`, and with it `MediumParser.query` /
//! `render_as_html` (`core.py:159-211`, `:685-692`) — the three-level read that
//! the whole server exists to serve:
//!
//! ```text
//! Redis (v2:) ──miss──▶ Postgres `cache` ──miss──▶ GraphQL ──▶ render ──▶ both caches
//! ```
//!
//! # What each layer holds, and why the order is what it is
//!
//! Redis holds the **rendered page** ([`RenderedPost`]), Postgres holds the
//! **raw GraphQL payload**. That is the legacy's split and it is not
//! interchangeable: a template change invalidates Redis and leaves Postgres
//! alone, so the re-render after a template fix costs no Medium request at all.
//!
//! # The status codes are the legacy's exception table
//!
//! `handlers/post.py:72-89` maps six exception types onto three statuses. The
//! mapping is reproduced from [`resolve_id`], which is `MediumParser.resolve`
//! (`core.py:69-101`) including its fallback: a path that is a bare 12-hex post
//! id is accepted as one when the URL machinery fails. That fallback is why
//! `/0291df856c77` serves an article rather than a 404.

use std::time::Duration;

use axum::response::Response;
use futures_util::future::join_all;

use freedium_cache::decode::decode_json;
use freedium_cache::keys;
use medium_client::error::FetchError;
use medium_client::response::validate;
use medium_doc::metadata::{self, PostMetadata};
use medium_doc::parse::{PostPayload, parse};
use medium_doc::resolve::{
    NotValidMediumUrl, correct_url, extract_hex_string, is_has_valid_medium_post_id,
    is_valid_medium_url, is_valid_url, resolve_medium_url,
};
use medium_render::page::{PageConfig, render_base, render_homepage, render_main, render_post};
use medium_render::post::RenderedPost;

use crate::error::{PageError, SHADOW_NO_FETCH, handler_error, html_error};
use crate::handlers::html;
use crate::middleware::Correlation;
use crate::state::AppState;

/// `handlers/post.py:76` — `InvalidURL`.
pub const MSG_INVALID_URL: &str = "Unable to identify the Medium article URL.";

/// `handlers/post.py:81` — `InvalidMediumPostURL`, `MediumPostQueryError`,
/// `PageLoadingError`.
pub const MSG_NO_ARTICLE: &str = "Unable to identify the link as a Medium.com article page. Please check the URL for any typing errors.";

/// `handlers/post.py:85` — `InvalidMediumPostID`, and the only 500 of the four.
///
/// **Unreferenced, on purpose.** Nothing in `medium_parser` ever raises that
/// exception, so the branch that renders this message is dead code and there is
/// no [`PageError`] here that carries it — see [`fetch_error`]. The constant
/// stays so that a reader comparing this file against `handlers/post.py:72-89`
/// can find all four messages in one place rather than wondering which is
/// missing.
pub const MSG_NO_POST_ID: &str = "Unable to identify the Medium article ID.";

/// `handlers/post.py:87` — `NotValidMediumURL`. 404, and quiet.
pub const MSG_NOT_VALID_URL: &str = "You sure that this is a valid Medium.com URL?";

/// How long the homepage fragment is cached for — `@aio_redis_cache(10 * 60)`
/// (`handlers/post.py:21`).
///
/// Deliberately **not** `config.cache_life_time` (five hours): the homepage is
/// what a deployment looks at to see whether it came up, and a ten-minute TTL is
/// the legacy's way of keeping it roughly fresh without rebuilding it per
/// request.
pub const HOMEPAGE_CACHE_LIFE_TIME: Duration = Duration::from_secs(10 * 60);

/// `main_page` (`handlers/main.py:53-62`) — `/`.
///
/// Three renders, matching the legacy exactly: `homepage.html` (the cached
/// fragment), `main.html` around it, `base.html` around that. The html5lib
/// round-trip between the steps is dropped — §5 decision 2 — so this is one byte
/// less surprising than the legacy, not differently shaped.
///
/// # Why the fragment is the cached unit and not the page
///
/// `@aio_redis_cache` wraps `render_homepage`, which returns the `homepage.html`
/// output *before* `main.html` and `base.html` are applied. Caching the whole
/// page instead would be a different cache: `HOST_ADDRESS` and
/// `ENABLE_ADS_BANNER` are read at request time in the legacy, so a config
/// change shows up on the next request rather than in ten minutes. Keeping the
/// boundary where the legacy has it is what makes that true here.
pub async fn render_index(state: &AppState, correlation: &Correlation) -> Response {
    let fragment = match homepage_fragment(state).await {
        Ok(fragment) => fragment,
        Err(error) => return html_error(state, correlation, error).await,
    };

    let config = page_config(state);
    let env = state.templates();
    // `handlers/main.py:56-59`: the homepage goes in as `postleter`, then the
    // whole thing into `base.html` with **no** title and **no** description, so
    // both tags take their template defaults.
    let page =
        render_main(env, &fragment).and_then(|main| render_base(env, &main, "", "", &config));

    match page {
        Ok(html_body) => html(html_body),
        Err(err) => handler_error(state, correlation, err, None, 500, false).await,
    }
}

/// The cached homepage fragment: `homepage.html`, from Redis or rebuilt.
async fn homepage_fragment(state: &AppState) -> Result<String, PageError> {
    // The decorator checks Redis *itself* (`utils/cache.py:12-14`) and runs the
    // function directly when it is down, so an outage costs a rebuild per
    // request rather than the page.
    let redis_available = state.redis_available().await;

    if redis_available {
        match state.redis.get_msgpack::<String>(keys::HOMEPAGE_KEY).await {
            Ok(Some(fragment)) => {
                tracing::debug!("homepage: Redis cache hit");
                return Ok(fragment);
            }
            Ok(None) => tracing::debug!("homepage: Redis cache miss"),
            // A corrupt entry is a miss. The legacy would raise out of
            // `pickle.loads` and 500 the homepage on a bad value, which is a
            // worse trade for a cache that exists to make this route cheap.
            Err(err) => tracing::warn!(error = %err, "homepage: could not read Redis"),
        }
    }

    let limit = state.config.home_page_max_posts;
    let rows = state.postgres.random(limit).await.map_err(|err| {
        // No `handle_exception` here: `handlers/post.py:32`'s only try is around
        // the *per-post* work, so a failing `medium_cache.random` propagates out
        // of `render_homepage`, past `main_page` (`handlers/main.py:55`), to the
        // middleware's `except Exception` (`middlewares/logger.py:49`) — a 500
        // with a random message, not the 404 of the article route.
        tracing::error!(error = ?err, "the homepage query failed");
        PageError::unspecified()
    })?;

    let posts = homepage_posts(state, rows).await;
    let fragment = render_homepage(state.templates(), &posts).map_err(|err| {
        tracing::error!(error = %err, "could not render the homepage");
        PageError::unspecified()
    })?;

    if redis_available
        && let Err(err) = state
            .redis
            .set_msgpack(keys::HOMEPAGE_KEY, &fragment, HOMEPAGE_CACHE_LIFE_TIME)
            .await
    {
        tracing::warn!(error = %err, "homepage: could not store the fragment");
    }

    Ok(fragment)
}

/// `render_homepage`'s loop (`handlers/post.py:23-41`), minus the query.
///
/// The legacy collects `medium_cache.random(limit)`'s keys and then calls
/// `medium_parser.query(post_id, force_cache=True, retry=1)` for each — which,
/// with `force_cache=True`, reads the same row `random` just returned. Here the
/// row's value *is* the payload, so the re-read is dropped and only the decode
/// is left. That is the note in the plan: `random` already answers `(key,
/// value)`.
///
/// A post that cannot be decoded is skipped, not fatal: the legacy's per-post
/// `try` calls `handle_exception` and lets the homepage render without it. One
/// malformed row must not blank the front page.
///
/// The dedupe in the legacy (`set([i.key for i in ...])`) has no counterpart
/// here and needs none: `cache.key` is the table's `PRIMARY KEY`, so a single
/// query cannot return the same post twice.
async fn homepage_posts(state: &AppState, rows: Vec<(String, String)>) -> Vec<PostMetadata> {
    // Concurrent, as `asyncio.gather` is. There is no network here — decoding is
    // CPU-only — but the legacy's shape is kept so the two are comparable if a
    // fetch is ever reintroduced.
    let decoded = join_all(rows.iter().map(|(key, value)| async move {
        match decode_json(value) {
            Ok(value) => PostPayload::from_value(value)
                .map(|payload| (key.clone(), payload))
                .map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        }
    }))
    .await;

    let mut posts = Vec::with_capacity(decoded.len());
    for outcome in decoded {
        match outcome {
            Ok((post_id, payload)) => {
                posts.push(metadata::from_payload(&payload, &post_id));
            }
            Err(err) => {
                // `handlers/post.py:36`: `handle_exception(..., message=
                // f"Couldn't render post_id for postleter: {post_id}. Just
                // ignore that")`. The legacy renders an error page there and
                // throws it away, so only the log and the alert are reproduced.
                tracing::warn!(error = %err, "couldn't render a post_id for the homepage; skipping it");
                state
                    .notifier
                    .send(
                        &format!(
                            "⚠️ Couldn't render a post for the homepage: <code>{err}</code>. Just ignore that"
                        ),
                        false,
                        crate::notify::MessageStatus::Error,
                    )
                    .await;
            }
        }
    }

    posts
}

/// `render_medium_post_link` (`handlers/post.py:50-103`).
///
/// # `use_db_cache` does *less* than its name says, and that is reproduced
///
/// The legacy threads `use_cache` into exactly one place — the guard on the
/// **rendered-page** Redis read at `handlers/post.py:57`. The call that follows
/// is `medium_parser.render_as_html(post_id)` → `query(post_id)` with defaults
/// (`core.py:159-165`), so `use_cache=True` and the Postgres payload cache is
/// consulted either way. `no-db-cache` therefore does not bypass the durable
/// cache at all; it only forces a re-render of a page Redis already had.
///
/// That reads like a bug, and it may well be one — but the parameter is part of
/// the observable contract (it is what an operator reaches for when a rendered
/// page is stale), and changing what it does here would make the two servers
/// answer the same URL differently. It is kept, and flagged.
pub async fn render_medium_post_link(
    state: &AppState,
    correlation: &Correlation,
    path: &str,
    use_db_cache: bool,
    use_redis: bool,
) -> Response {
    let redis_available = state.redis_available().await;
    tracing::debug!("Redis available: {redis_available}");

    let post_id = match resolve_id(state, path).await {
        Ok(post_id) => post_id,
        Err(error) => return html_error(state, correlation, error).await,
    };
    // `keys::post_key` — the `v2:` prefix is what keeps this from colliding with
    // the Python instance's unprefixed keys during Fase 4's shadow traffic.
    let redis_key = keys::post_key(&post_id);

    // The rendered page, if Redis has it and the bypass parameters allow it.
    let cached = if redis_available && use_db_cache && use_redis {
        match state.redis.get_msgpack::<RenderedPost>(&redis_key).await {
            Ok(Some(rendered)) => {
                tracing::debug!("Loaded rendered post from Redis cache");
                Some(rendered)
            }
            Ok(None) => {
                tracing::debug!("No Redis cache found, querying...: {post_id}");
                None
            }
            Err(err) => {
                // A corrupt entry is treated as a miss, as in the legacy, where
                // `pickle.loads` raising inside the try would be caught below.
                tracing::warn!(error = %err, "could not read the rendered post from Redis");
                None
            }
        }
    } else {
        None
    };

    let rendered = match cached {
        Some(rendered) => rendered,
        None => match render_article(state, path, &post_id).await {
            Ok(rendered) => {
                if redis_available && use_redis {
                    if let Err(err) = state
                        .redis
                        .set_msgpack(&redis_key, &rendered, state.config.cache_life_time)
                        .await
                    {
                        tracing::warn!(error = %err, "could not store the rendered post");
                    } else {
                        tracing::debug!("Stored rendered post in Redis cache: {post_id}");
                    }
                }
                rendered
            }
            Err(error) => return html_error(state, correlation, error).await,
        },
    };

    // `handlers/post.py:102` sends `"✅ Successfully rendered post: {path}"` with
    // status `GOOD`, which `notify.py:20-22` drops. §7 item 8 removes the call
    // rather than reproducing a no-op.
    html(rendered.html)
}

/// `MediumParser.resolve` (`core.py:69-101`), including the hex fallback.
///
/// Returns the [`PageError`] the caller should render, so the status-code table
/// stays in one place.
pub async fn resolve_id(state: &AppState, path: &str) -> Result<String, PageError> {
    let sanitized = correct_url(path);
    let resolver = state.resolver.as_ref();

    // `resolve_url` (`core.py:90-93`), including Python's `or` short-circuit:
    // `is_valid_url` sees the **raw** input and `is_valid_medium_url` the
    // sanitised one, and a raw input that is not an absolute URL never reaches
    // the domain check at all. `path` here is the URL with its origin stripped
    // (`handlers/main.py:34`), so for `/medium.com/foo` it is the schemeless
    // `medium.com/foo` and this is the branch that fires.
    let attempt: Result<String, PageError> = if !is_valid_url(path) {
        // `InvalidURL` — handlers/post.py:73-77.
        Err(PageError::new(MSG_INVALID_URL, 404))
    } else {
        match is_valid_medium_url(&sanitized, resolver).await {
            // `NotValidMediumURL` — 404, and quiet: handlers/post.py:87.
            Err(NotValidMediumUrl) => Err(PageError::new(MSG_NOT_VALID_URL, 404).quiet()),
            // `InvalidURL` — handlers/post.py:73-77.
            Ok(false) => Err(PageError::new(MSG_INVALID_URL, 404)),
            Ok(true) => match resolve_medium_url(&sanitized, resolver).await {
                Some(post_id) => Ok(post_id.as_str().to_string()),
                // `InvalidMediumPostURL` — handlers/post.py:78-83.
                None => Err(PageError::new(MSG_NO_ARTICLE, 404)),
            },
        }
    };

    match attempt {
        Ok(post_id) => Ok(post_id),
        Err(error) => {
            // `core.py:76-86`: any failure falls back to reading the input as a
            // bare post id, and only re-raises when that fails too. The check is
            // on the *raw* `unknown`, not the sanitised URL.
            if is_has_valid_medium_post_id(path)
                && let Some(hex) = extract_hex_string(path)
            {
                tracing::debug!(hex, "Seems like it's valid post_id");
                return Ok(hex.to_string());
            }

            tracing::error!(path, "Unknown data: {path}");
            Err(error)
        }
    }
}

/// `MediumParser.render_as_html` (`core.py:685-692`): query, then render.
async fn render_article(
    state: &AppState,
    path: &str,
    post_id: &str,
) -> Result<RenderedPost, PageError> {
    let payload = query(state, post_id).await?;

    let document = parse(&payload, &state.config.host_address);
    let post_metadata = metadata::from_payload(&payload, post_id);
    let config = page_config(state);

    render_post(state.templates(), &document, &post_metadata, &config).map_err(|err| {
        tracing::error!(path, error = %err, "could not render the post");
        PageError::unspecified()
    })
}

/// `MediumParser.query` (`core.py:159-211`): Postgres `cache`, then GraphQL.
///
/// Two legacy behaviours are deliberately not reproduced, both flagged in
/// `medium-client`'s docs: the `retry=2` loop that raises
/// `MediumPostQueryError` is [`PostSource`](medium_client::source::PostSource)'s
/// own retry policy, and the `reason` string that loop builds has a bug that
/// makes it always report `"Unknown"`. The *exception type* is the same, which
/// is what the caller's status code depends on.
///
/// # A cache row that is not a payload is a miss, and that is *nearly* the legacy
///
/// `core.py:172-197` does not trust the cache: it re-checks the shape of
/// whatever came back (`:178-187`) and, when a check fails, sets `reason` and
/// retries against the API. [`decode_cached`] applies those same checks to the
/// row, so a bad row costs one Medium request rather than a blank page.
///
/// One case does not line up, and it is unreachable rather than subtle. `{}` is
/// *falsy* in Python, so `query_get` discards it and refetches — same as here.
/// `{"data": {}}` is *truthy*, so it survives `query_get` and reaches the `:186`
/// check, which 404s it without a refetch. This refetches instead. Distinguishing
/// the two would mean reproducing a truthiness accident for a row nothing
/// writes: `push` only ever stores a payload that `validate` accepted.
pub async fn query(state: &AppState, post_id: &str) -> Result<PostPayload, PageError> {
    // A failing cache *read* is a miss, not a 404 — `get_post_data_from_cache`
    // wraps the read in `try/except` and returns `None` on any exception
    // (`core.py:119-127`), so a Postgres outage in the legacy sends the request
    // to the API rather than failing it. Taking the error here would turn a
    // database blip into a 404 for every post that was not already cached.
    let cached = match state.postgres.pull(post_id).await {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(post_id, error = %err, "the cache read failed; treating it as a miss");
            None
        }
    };

    if let Some(raw) = cached {
        match decode_cached(&raw) {
            Ok(payload) => {
                tracing::debug!("post query was found on cache");
                return Ok(payload);
            }
            Err(err) => {
                // The legacy's `post_data.json()` raises here and is caught by
                // `_get_from_cache`'s `except Exception` (`core.py:124-127`),
                // which returns `None` so the caller goes to the API. A bad
                // cache row must not become a 404 for a post that exists.
                tracing::warn!(post_id, error = %err, "the cached value is unusable; refetching");
            }
        }
    }

    // Fase 4's interlock, and the single line that keeps a shadow instance off
    // the network. Placed here — **after** the cache has been given its chance
    // and **before** `fetch_post` — because it is a genuine miss that must
    // decline: a post that is cached renders normally and is compared normally,
    // which is where almost all of the evidence comes from.
    //
    // Why the shadow may not fetch: SPIKE-1's *pooled* gate is still open, so a
    // fetching Rust instance would go out through the same WARP exit production
    // serves from, and `RUST_REWRITE_PLAN` §2.7 is explicit that exhausting that
    // exit takes the whole site down rather than just the shadow. Decision 2 of
    // the Fase 4 plan, and the reason there is a decision to make at all.
    //
    // The cost, stated plainly: a post that is *not* cached cannot be compared,
    // so a difference that only shows on an uncached post is invisible to this
    // phase. That is a real blind spot, and it is the trade the pool's state
    // forces. It is why `difftest gen-shadow-seed` exists — seeding the durable
    // cache is what puts a corpus in front of the comparison at all.
    if state.config.shadow_mode {
        tracing::debug!(
            post_id,
            "shadow instance: declining to fetch on a cache miss"
        );
        return Err(PageError::declined(SHADOW_NO_FETCH));
    }

    let value = state
        .source
        .fetch_post(post_id)
        .await
        .map_err(|err| fetch_error(post_id, err))?;

    // `core.py:206-208`: push to the durable cache only when the cache was not
    // the source. A payload that came from Postgres is already there.
    if let Err(err) = state.postgres.push(post_id, &value.to_string()).await {
        // Not fatal: the legacy's `self.cache.push` is outside the try, so a
        // failing push would propagate — but that turns a served page into a
        // 500 on a cache hiccup. Logged, and the page is served.
        tracing::warn!(post_id, error = %err, "could not push the payload to cache");
    }

    // Unreachable: `fetch_post` promises a `validate`d payload, so this is an
    // object carrying `data.post` and `serde` accepts it. Kept as the legacy's
    // generic `except Exception` (`handlers/post.py:88`) rather than an
    // `expect`, because a 500 is a better answer than a dropped connection if
    // the promise is ever broken.
    PostPayload::from_value(value).map_err(|err| {
        tracing::error!(post_id, error = %err, "fetched payload is not a payload");
        PageError::unspecified()
    })
}

/// The cached row as a payload, or why it is not one (`core.py:178-187`).
///
/// Returns a `String` rather than an error type because every caller does the
/// same thing with it — logs it and refetches — and the three sources (a decode
/// failure, [`validate`], `serde`) have three unrelated error types.
fn decode_cached(raw: &str) -> Result<PostPayload, String> {
    let value = decode_json(raw).map_err(|err| err.to_string())?;
    validate(&value).map_err(|err| err.to_string())?;
    PostPayload::from_value(value).map_err(|err| err.to_string())
}

/// A failed fetch → the 404 at `handlers/post.py:78-83`, for **every** variant.
///
/// # Why there is no 500 arm, against `handlers/post.py`'s four
///
/// The legacy maps six exception types onto its four branches, and
/// `InvalidMediumPostID` — the only one that gets a 500
/// (`handlers/post.py:84-85`) — is **never raised**. It is defined in
/// `medium_parser_exceptions` and caught here, and that is the whole of its
/// existence: nothing in `medium_parser` raises it. So a `FetchError` cannot be
/// that, and the two failures the legacy's `query` can actually produce are
/// `MediumPostQueryError` (`core.py:202`, when the retry loop gives up) and the
/// `InvalidMediumPostURL` of a URL whose id does not resolve — both 404. The
/// shape problem that looks like it should be a 500 is not: `validate` runs
/// inside `fetch_post`, and its `NoPost`/`Malformed`/`GraphQl` variants are
/// exactly what the legacy's `reason` checks turn into a retry and then a
/// `MediumPostQueryError`.
fn fetch_error(post_id: &str, error: FetchError) -> PageError {
    tracing::error!(post_id, error = %error, "could not fetch the post");
    PageError::new(MSG_NO_ARTICLE, 404)
}

/// `config.HOST_ADDRESS` + `config.ENABLE_ADS_BANNER`.
fn page_config(state: &AppState) -> PageConfig {
    PageConfig::new(&state.config.host_address).with_ads_header(state.config.enable_ads_banner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::body::to_bytes;
    use serde_json::{Value, json};

    use crate::error::{DECLINED_MESSAGE, DECLINED_STATUS, SHADOW_HEADER};
    use crate::state::tests::{FixedSource, RecordingSource, shadow_state, stub_state};

    fn correlation() -> Correlation {
        Correlation::new(
            "alpha-bravo-charlie".to_string(),
            12345,
            "https://freedium.cfd/0291df856c77".to_string(),
        )
    }

    /// A payload shaped the way the GraphQL endpoint answers, carrying every
    /// field `generate_metadata` reads.
    ///
    /// Hand-written rather than read from `xtask/difftest/fixtures/`: those are
    /// Fase 1's *content* fixtures and carry no `creator`, timestamps or
    /// `mediumUrl`, which is exactly the gap the plan's task list records for the
    /// page gate.
    ///
    /// The nested objects are complete because `post.html` reads *into* them and
    /// minijinja is strict about attribute access on an undefined — see
    /// `medium-render/src/templates.rs`. `collection.avatar.id` is the one that
    /// bit: a collection without an `avatar` renders a 500, not a page with a
    /// missing image, and `query.graphql`'s `PostMetaData` always selects
    /// `collection { … avatar { id … } }`, so the omission was the fixture's
    /// fault rather than the template's.
    fn payload() -> Value {
        json!({
            "data": { "post": {
                "title": "A Test Article",
                "previewContent": { "subtitle": "A subtitle" },
                "previewImage": { "id": "1*abc" },
                "creator": {
                    "id": "c1",
                    "name": "Someone",
                    "username": "someone",
                    "bio": "A short bio",
                    "imageId": "1*creator"
                },
                "collection": {
                    "id": "col1",
                    "name": "A Publication",
                    "slug": "a-publication",
                    "shortDescription": "What the publication is about",
                    "avatar": { "id": "1*avatar" }
                },
                "mediumUrl": "https://medium.com/@someone/a-test-article-0291df856c77",
                "readingTime": 3.2,
                "isLocked": false,
                "firstPublishedAt": 1_692_000_000_000_i64,
                "updatedAt": 1_692_000_000_000_i64,
                "tags": [{ "displayTitle": "Rust", "normalizedTagSlug": "rust" }],
                "highlights": [],
                "content": { "bodyModel": { "paragraphs": [
                    { "type": "P", "text": "Hello from the page.", "name": "p1",
                      "layout": "", "markups": [] }
                ] } }
            }}
        })
    }

    async fn body_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        String::from_utf8(bytes.to_vec()).expect("the page is UTF-8")
    }

    /// The whole point of the phase: a request for a bare post id renders an
    /// article page, with no database, no Redis and no network.
    ///
    /// The id in the path is what makes this reachable at all — `/0291df856c77`
    /// is not a URL, so [`resolve_id`] takes `core.py:76-86`'s hex fallback. From
    /// there the cache read fails (the stub's pool points at `.invalid`, which
    /// is treated as a miss, not a 404), the fixed source answers, and the
    /// payload goes through `parse` → `metadata::from_payload` → `render_post` →
    /// `base.html`.
    #[tokio::test]
    async fn a_post_page_renders_end_to_end_without_a_network() {
        let state = stub_state(Arc::new(FixedSource(payload())));
        let response =
            render_medium_post_link(&state, &correlation(), "0291df856c77", true, true).await;

        // Read first, assert after: a 500 here is the error page, and the page
        // says *which* error, so putting the body in the message turns "status
        // 500" into the actual cause.
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .cloned();
        let html = body_text(response).await;

        assert_eq!(status, 200, "the article page was not rendered: {html}");
        assert_eq!(content_type.unwrap(), "text/html; charset=utf-8");

        // The body fragment, the title, and the derived description — one
        // assertion per stage of the pipeline, so a failure says which stage
        // broke rather than "the page is wrong".
        assert!(html.contains("Hello from the page."), "body fragment");
        assert!(html.contains("A Test Article"), "title");
        assert!(html.contains("A subtitle"), "description");
        assert!(html.contains("A Publication"), "collection");
    }

    /// The id the fetch is given is the one from the path — the fallback
    /// normalises `/some-title-0291df856c77` and `/0291df856c77` alike, and a
    /// mangled id would mean serving the wrong article.
    #[tokio::test]
    async fn the_hex_fallback_passes_the_id_on_to_the_fetch() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = stub_state(recorder.clone());

        // The source always fails, so the page is the 404 — that is not the
        // assertion. What is asserted is what the source was asked for.
        let response =
            render_medium_post_link(&state, &correlation(), "0291df856c77", true, true).await;
        assert_eq!(response.status(), 404);

        assert_eq!(
            recorder.0.lock().unwrap().as_slice(),
            ["0291df856c77"],
            "the fetch must be for the id in the path"
        );
    }

    /// `core.py:92`'s first check, on the raw input: a schemeless path is
    /// `InvalidURL` before any domain check, and the fetch is never reached.
    ///
    /// The `is_valid_medium_url` half would also say `false` here, which is why
    /// this test uses a *known* Medium domain — `medium.com/foo` is the input
    /// where the two checks disagree, because the domain half says
    /// "known Medium domain" and only the raw half rejects it.
    #[tokio::test]
    async fn a_schemeless_path_never_reaches_the_fetch() {
        for path in ["medium.com/foo", "not-a-url", "http:medium.com/foo"] {
            let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
            let state = stub_state(recorder.clone());

            let response = render_medium_post_link(&state, &correlation(), path, true, true).await;
            assert_eq!(response.status(), 404, "{path}");

            let html = body_text(response).await;
            assert!(html.contains(MSG_INVALID_URL), "{path}: {html}");

            assert!(
                recorder.0.lock().unwrap().is_empty(),
                "{path} reached the network"
            );
        }
    }

    /// A path with a post-shaped id but nothing else still resolves, which is
    /// the fallback's whole purpose — and it applies to the *raw* input
    /// (`core.py:80`), not the sanitised URL.
    #[tokio::test]
    async fn resolve_id_reads_a_bare_hex_id() {
        let state = stub_state(Arc::new(RecordingSource(Mutex::new(Vec::new()))));

        assert_eq!(
            resolve_id(&state, "0291df856c77").await.unwrap(),
            "0291df856c77"
        );

        // A URL under a domain that is neither known nor bad, with no post id in
        // it: `is_valid_medium_url` falls back to the resolve, which fails.
        let error = resolve_id(&state, "https://example.com/x")
            .await
            .unwrap_err();
        assert_eq!(error.status, 404);
        assert_eq!(error.message.as_deref(), Some(MSG_INVALID_URL));
    }

    /// Fase 4's interlock, and the assertion the phase rests on: on a cache miss
    /// a shadow instance answers without ever reaching the network.
    ///
    /// The response alone cannot show this. A 503 with a body looks the same
    /// whether the source was asked and failed or was never asked at all — which
    /// is why the assertion is on [`RecordingSource`]'s recording and not on the
    /// page. Same instrument, same reason, as
    /// [`a_schemeless_path_never_reaches_the_fetch`] above.
    #[tokio::test]
    async fn a_shadow_instance_declines_instead_of_fetching() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = shadow_state(recorder.clone());

        let response =
            render_medium_post_link(&state, &correlation(), "0291df856c77", true, true).await;

        // Read the header before the body consumes the response.
        let status = response.status();
        let marker = response
            .headers()
            .get(SHADOW_HEADER)
            .map(|value| value.to_str().unwrap().to_string());
        let html = body_text(response).await;

        assert_eq!(
            status, DECLINED_STATUS,
            "the shadow did not decline: {html}"
        );
        assert_eq!(
            marker.as_deref(),
            Some(SHADOW_NO_FETCH),
            "the edge keys on this header, so an unmarked decline is uncomparable \
             and a marked answer is skipped — backwards in both directions"
        );
        assert!(
            html.contains(DECLINED_MESSAGE),
            "the declined body is the fixed message, not a random one: {html}"
        );
        assert!(
            recorder.0.lock().unwrap().is_empty(),
            "the shadow reached the network, which is the one thing it must not do"
        );
    }

    /// The control for the test above, and the reason it is not vacuous: with
    /// `SHADOW_MODE` off, the very same request *does* reach the source.
    ///
    /// Without this, an interlock that broke the fetch path entirely — or a
    /// fixture that never reached [`query`] at all — would leave the test above
    /// green while production stopped fetching.
    #[tokio::test]
    async fn an_instance_that_is_not_a_shadow_still_fetches() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = stub_state(recorder.clone());

        let response =
            render_medium_post_link(&state, &correlation(), "0291df856c77", true, true).await;

        assert_eq!(response.status(), 404, "the recording source always fails");
        assert!(
            response.headers().get(SHADOW_HEADER).is_none(),
            "only a decline carries the marker; a real answer must never be skipped"
        );
        assert_eq!(
            recorder.0.lock().unwrap().as_slice(),
            ["0291df856c77"],
            "outside shadow mode the source is still the fallback for a cache miss"
        );
    }

    /// A path that never resolves is still a real 404 in shadow mode — the
    /// interlock is at the *fetch*, not at the door.
    ///
    /// This is what keeps the two kinds of not-found distinguishable. Python
    /// answers 404 for a mistyped URL, and the shadow must answer the same 404
    /// unmarked, or every invalid URL in the traffic sample would be skipped
    /// instead of compared.
    #[tokio::test]
    async fn a_shadow_instance_still_answers_a_real_not_found() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = shadow_state(recorder.clone());

        let response =
            render_medium_post_link(&state, &correlation(), "not-a-url", true, true).await;
        assert_eq!(response.status(), 404);
        assert!(response.headers().get(SHADOW_HEADER).is_none());
        assert!(recorder.0.lock().unwrap().is_empty());
    }
}
