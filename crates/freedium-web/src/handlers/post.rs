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
use medium_client::source::PostSource;
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
/// no [`PageError`] here that carries it — see `impl From<FetchFailure> for
/// PageError`. The constant
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
        Err(error) => return html_error(state, correlation, error.into()).await,
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

/// Why a fetch did not produce a payload.
///
/// # Why this exists rather than a bare [`PageError`]
///
/// The page route collapses every failure onto one status (`handlers/post.py`'s
/// exception table is why), but `/api/v1` cannot: §2.7 gives 404, 502, 503 and 504
/// to four different causes that the page answers identically. A typed failure is
/// the only way both tables can be right over the same code — the page converts
/// through [`From`] and gets today's bytes, and the API matches on the variants
/// and gets its own.
///
/// Splitting the fetch out of [`query`] is what makes the variants *visible*: a
/// function that returns a `PageError` has already thrown the distinction away.
#[derive(Debug)]
pub enum FetchFailure {
    /// `SHADOW_MODE` is on and the durable cache missed: this instance will not go
    /// to the network. Fase 4's interlock — see [`query`].
    Declined,

    /// The fetch itself failed. The page answers 404 for all of
    /// [`FetchError`]'s variants; the API reads the variant.
    Fetch(FetchError),

    /// A payload that `validate` accepted could not be turned into a
    /// [`PostPayload`].
    ///
    /// **Unreachable**, because `fetch_post` promises a validated payload and
    /// `serde` accepts anything `validate` lets through. Kept as its own variant
    /// rather than an `expect`, so the answer is the 500 the legacy's generic
    /// `except Exception` gives rather than a dropped connection — and so a
    /// caller that wants to distinguish "upstream is broken" from "our own
    /// invariant broke" can.
    NotAPayload(String),
}

/// Why a path did not resolve to a post id.
///
/// Three variants, not four: the plan sketched a `ShortLink(FetchError)` for the
/// `link.medium.com` hop, and there is nowhere to put it. `LinkResolver` returns
/// `Option<String>` and a failed hop becomes `None`, which resolves to
/// [`Self::NoArticle`] — `medium-doc` cannot see a `FetchError`, and teaching it
/// to would invert §2.6's layering for a distinction the legacy does not make
/// either (its `InvalidMediumPostURL` covers both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveError {
    /// `InvalidURL` — `handlers/post.py:73-77`. The input is not an absolute URL
    /// at all.
    InvalidUrl,

    /// `NotValidMediumURL` — `handlers/post.py:87`. A real URL, on a domain known
    /// not to be Medium's.
    NotValidMediumUrl,

    /// `InvalidMediumPostURL` — `handlers/post.py:78-83`. A plausible Medium URL
    /// from which no post id could be read.
    NoArticle,
}

/// The page's table: 404 twice, quietly once, and a 500.
///
/// **Every string here is the legacy's, verbatim**, which is what lets
/// [`resolve_id`] and [`query`] change shape without the rendered pages changing
/// at all. The difftest gate is what proves it, and it was re-run after this
/// refactor for exactly that reason.
impl From<ResolveError> for PageError {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::InvalidUrl => PageError::new(MSG_INVALID_URL, 404),
            // Quiet: `utils/error.py:39-40` — a mistyped URL is routine traffic,
            // so it does not reach Telegram.
            ResolveError::NotValidMediumUrl => PageError::new(MSG_NOT_VALID_URL, 404).quiet(),
            ResolveError::NoArticle => PageError::new(MSG_NO_ARTICLE, 404),
        }
    }
}

/// The same, for a failed fetch.
///
/// # Why every fetch failure is a 404 here, against `handlers/post.py`'s four
/// branches
///
/// The legacy maps six exception types onto four branches, and
/// `InvalidMediumPostID` — the only one that gets a 500
/// (`handlers/post.py:84-85`) — is **never raised**. It is defined in
/// `medium_parser_exceptions` and caught there, and that is the whole of its
/// existence: nothing in `medium_parser` raises it. So a [`FetchError`] cannot be
/// that, and the two failures the legacy's `query` actually produces are
/// `MediumPostQueryError` (`core.py:202`, when the retry loop gives up) and the
/// `InvalidMediumPostURL` of a URL whose id does not resolve — both 404. The shape
/// problem that looks like it should be a 500 is not: `validate` runs inside
/// `fetch_post`, and its `NoPost`/`Malformed`/`GraphQl` variants are exactly what
/// the legacy's `reason` checks turn into a retry and then a
/// `MediumPostQueryError`.
///
/// **§2.7 says something different on purpose, and only for the API** — it makes
/// a failed upstream fetch a 502, because a 404 from Medium for an id we believe
/// is valid is an upstream anomaly rather than a statement about our URL space.
/// That divergence is the API's to make, in `crate::api`, and this conversion
/// stays as it is so the pages do not move.
impl From<FetchFailure> for PageError {
    fn from(failure: FetchFailure) -> Self {
        match failure {
            // A decline is not a failure to fetch; it is a refusal to try. It
            // keeps its own status and its marker header so the edge skips it.
            FetchFailure::Declined => PageError::declined(SHADOW_NO_FETCH),
            FetchFailure::Fetch(_) => PageError::new(MSG_NO_ARTICLE, 404),
            FetchFailure::NotAPayload(_) => PageError::unspecified(),
        }
    }
}

/// `MediumParser.resolve` (`core.py:69-101`), including the hex fallback.
///
/// Returns the [`ResolveError`] the caller should map, so the status-code table
/// stays in one place — and so `/api/v1/resolve` can answer 400 where the page
/// answers 404 without either one lying about what happened.
pub async fn resolve_id(state: &AppState, path: &str) -> Result<String, ResolveError> {
    let sanitized = correct_url(path);
    let resolver = state.resolver.as_ref();

    // `resolve_url` (`core.py:90-93`), including Python's `or` short-circuit:
    // `is_valid_url` sees the **raw** input and `is_valid_medium_url` the
    // sanitised one, and a raw input that is not an absolute URL never reaches
    // the domain check at all. `path` here is the URL with its origin stripped
    // (`handlers/main.py:34`), so for `/medium.com/foo` it is the schemeless
    // `medium.com/foo` and this is the branch that fires.
    let attempt: Result<String, ResolveError> = if !is_valid_url(path) {
        // `InvalidURL` — handlers/post.py:73-77.
        Err(ResolveError::InvalidUrl)
    } else {
        match is_valid_medium_url(&sanitized, resolver).await {
            // `NotValidMediumURL` — 404, and quiet: handlers/post.py:87.
            Err(NotValidMediumUrl) => Err(ResolveError::NotValidMediumUrl),
            // `InvalidURL` — handlers/post.py:73-77.
            Ok(false) => Err(ResolveError::InvalidUrl),
            Ok(true) => match resolve_medium_url(&sanitized, resolver).await {
                Some(post_id) => Ok(post_id.as_str().to_string()),
                // `InvalidMediumPostURL` — handlers/post.py:78-83.
                None => Err(ResolveError::NoArticle),
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

/// The durable cache read: Postgres, and nothing else.
///
/// **No network, no render, no template.** That is what makes it usable as a
/// cheap first step for `/api/v1` — the metadata endpoints need no `Document`, and
/// the miss rate-limiter has to know a miss happened *before* anything expensive
/// is attempted.
///
/// # A failing read is a miss, and that is why this is not a `Result`
///
/// `get_post_data_from_cache` wraps the read in `try/except` and returns `None`
/// on any exception (`core.py:119-127`), so a Postgres outage in the legacy sends
/// the request to the API rather than failing it. Taking the error here would turn
/// a database blip into a 404 for every post that was not already cached. Since
/// there is no case in which this returns `Err`, it returns an `Option` — a
/// `Result` whose error arm is unreachable is a comment pretending to be a type.
pub async fn query_cached(state: &AppState, post_id: &str) -> Option<PostPayload> {
    // A failing cache *read* is a miss, not a 404.
    let raw = match state.postgres.pull(post_id).await {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(post_id, error = %err, "the cache read failed; treating it as a miss");
            None
        }
    }?;

    match decode_cached(&raw) {
        Ok(payload) => {
            tracing::debug!("post query was found on cache");
            Some(payload)
        }
        Err(err) => {
            // The legacy's `post_data.json()` raises here and is caught by
            // `_get_from_cache`'s `except Exception` (`core.py:124-127`), which
            // returns `None` so the caller goes to the API. A bad cache row must
            // not become a 404 for a post that exists.
            tracing::warn!(post_id, error = %err, "the cached value is unusable; refetching");
            None
        }
    }
}

/// The shadow gate, the fetch, and the push. **The only path to the network in
/// this crate**, and the only place that decides whether to take it.
///
/// # `source` is a parameter on purpose
///
/// The page route passes `state.source` — the one that may carry
/// `MEDIUM_AUTH_COOKIES`. Every `/api/v1` route passes
/// [`AnonymousSource`](medium_client::http::AnonymousSource), which cannot. Making
/// the source an argument rather than reading `state.source` here means the choice
/// is visible at each call site, where a reviewer can see it, instead of being
/// made once in a helper that both paths share and neither one owns.
///
/// # The interlock, and why it is *here*
///
/// Placed **after** the cache has been given its chance and **before** the fetch,
/// because it is a genuine miss that must decline: a post that is cached renders
/// normally and is compared normally, which is where almost all of the evidence
/// comes from.
///
/// Why the shadow may not fetch: SPIKE-1's *pooled* gate is still open, so a
/// fetching Rust instance would go out through the same WARP exit production
/// serves from, and `RUST_REWRITE_PLAN` §2.7 is explicit that exhausting that exit
/// takes the whole site down rather than just the shadow. Decision 2 of the Fase 4
/// plan, and the reason there is a decision to make at all.
///
/// The cost, stated plainly: a post that is *not* cached cannot be compared, so a
/// difference that only shows on an uncached post is invisible to this phase. That
/// is a real blind spot, and it is the trade the pool's state forces. It is why
/// `difftest gen-shadow-seed` exists — seeding the durable cache is what puts a
/// corpus in front of the comparison at all.
pub async fn fetch_and_cache(
    state: &AppState,
    post_id: &str,
    source: &dyn PostSource,
) -> Result<PostPayload, FetchFailure> {
    if state.config.shadow_mode {
        tracing::debug!(
            post_id,
            "shadow instance: declining to fetch on a cache miss"
        );
        return Err(FetchFailure::Declined);
    }

    let value = source.fetch_post(post_id).await.map_err(|err| {
        tracing::error!(post_id, error = %err, "could not fetch the post");
        FetchFailure::Fetch(err)
    })?;

    // `core.py:206-208`: push to the durable cache only when the cache was not the
    // source. A payload that came from Postgres is already there.
    if let Err(err) = state.postgres.push(post_id, &value.to_string()).await {
        // Not fatal: the legacy's `self.cache.push` is outside the try, so a
        // failing push would propagate — but that turns a served page into a 500
        // on a cache hiccup. Logged, and the page is served.
        tracing::warn!(post_id, error = %err, "could not push the payload to cache");
    }

    PostPayload::from_value(value).map_err(|err| {
        tracing::error!(post_id, error = %err, "fetched payload is not a payload");
        FetchFailure::NotAPayload(err.to_string())
    })
}

/// `MediumParser.query` (`core.py:159-211`), for the page routes: Postgres
/// `cache`, then GraphQL.
///
/// The API does not call this. It needs the two halves separately — so that a
/// cache miss can be counted before it spends an upstream request, and so that a
/// failure can carry its cause — and `crate::api::posts` composes them itself.
///
/// Two legacy behaviours are deliberately not reproduced, both flagged in
/// `medium-client`'s docs: the `retry=2` loop that raises `MediumPostQueryError`
/// is [`PostSource`](medium_client::source::PostSource)'s own retry policy, and
/// the `reason` string that loop builds has a bug that makes it always report
/// `"Unknown"`. The *exception type* is the same, which is what the caller's
/// status code depends on.
pub async fn query(state: &AppState, post_id: &str) -> Result<PostPayload, PageError> {
    if let Some(payload) = query_cached(state, post_id).await {
        return Ok(payload);
    }

    fetch_and_cache(state, post_id, state.source.as_ref())
        .await
        .map_err(PageError::from)
}

/// The cached row as a payload, or why it is not one (`core.py:178-187`).
///
/// `core.py:172-197` does not trust the cache: it re-checks the shape of
/// whatever came back (`:178-187`) and, when a check fails, sets `reason` and
/// retries against the API. This applies those same checks to the row, so a bad
/// row costs one Medium request rather than a blank page.
///
/// One case does not line up, and it is unreachable rather than subtle. `{}` is
/// *falsy* in Python, so `query_get` discards it and refetches — same as here.
/// `{"data": {}}` is *truthy*, so it survives `query_get` and reaches the `:186`
/// check, which 404s it without a refetch. [`query_cached`] refetches instead.
/// Distinguishing the two would mean reproducing a truthiness accident for a row
/// nothing writes: `push` only ever stores a payload that `validate` accepted.
///
/// Returns a `String` rather than an error type because every caller does the
/// same thing with it — logs it and refetches — and the three sources (a decode
/// failure, [`validate`], `serde`) have three unrelated error types.
pub(crate) fn decode_cached(raw: &str) -> Result<PostPayload, String> {
    let value = decode_json(raw).map_err(|err| err.to_string())?;
    validate(&value).map_err(|err| err.to_string())?;
    PostPayload::from_value(value).map_err(|err| err.to_string())
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
    use medium_client::error::TransportError;

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
        //
        // The variant *and* the page it converts to. The variant is what
        // `/api/v1/resolve` reads (it answers 400 here, not 404); the conversion
        // is what the page renders, and it is the one under the difftest gate.
        // Asserting only one of the two would leave the other free to drift.
        let error = resolve_id(&state, "https://example.com/x")
            .await
            .unwrap_err();
        assert_eq!(error, ResolveError::InvalidUrl);
        let page: PageError = error.into();
        assert_eq!(page.status, 404);
        assert_eq!(page.message.as_deref(), Some(MSG_INVALID_URL));
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

    // ------------------------------------------------------------------ //
    // The typed errors `/api/v1` needs, and the promise that the page
    // routes did not move when they arrived. Every assertion below is a
    // byte of a page that is under the difftest gate.
    // ------------------------------------------------------------------ //

    /// A name per [`FetchError`] variant, for test messages.
    ///
    /// The `match` is **exhaustive**, and that is its job: it is what stops
    /// [`every_fetch_error`]'s list from quietly falling behind the enum. A new
    /// variant fails to compile here, so whoever adds one has to decide what the
    /// page answers for it rather than discovering it in production.
    fn fetch_error_name(error: &FetchError) -> &'static str {
        match error {
            FetchError::Transport(TransportError::Timeout) => "transport/timeout",
            FetchError::Transport(TransportError::Proxy(_)) => "transport/proxy",
            FetchError::Transport(TransportError::Other(_)) => "transport/other",
            FetchError::Status { .. } => "status",
            FetchError::NoPost => "no-post",
            FetchError::Malformed(_) => "malformed",
            FetchError::BadBody(_) => "bad-body",
            FetchError::GraphQl(_) => "graphql",
            FetchError::NoHealthyProxy => "no-healthy-proxy",
        }
    }

    fn every_fetch_error() -> Vec<FetchError> {
        vec![
            FetchError::Transport(TransportError::Timeout),
            FetchError::Transport(TransportError::Proxy("connection refused".to_string())),
            FetchError::Transport(TransportError::Other("dns failure".to_string())),
            FetchError::Status {
                status: 403,
                body: "Access denied".to_string(),
            },
            FetchError::NoPost,
            FetchError::Malformed("not a JSON object".to_string()),
            FetchError::BadBody("truncated".to_string()),
            FetchError::GraphQl("no such post".to_string()),
            FetchError::NoHealthyProxy,
        ]
    }

    /// The page's half of the table: **every** fetch failure is the same 404.
    ///
    /// This is the test that has to stay green while `/api/v1` answers 502 and
    /// 503 to four of these variants. Two tables over one typed error is the
    /// only arrangement in which both can be right; if someone later "unifies"
    /// them by having the API call this conversion, the API's error responses
    /// change and the tests in `crate::api` fail — not this one, which is why
    /// neither table is allowed to be derived from the other.
    ///
    /// The route-level half of the same claim is
    /// [`an_instance_that_is_not_a_shadow_still_fetches`], which drives a real
    /// transport failure (`RecordingSource`) through
    /// [`render_medium_post_link`] and reads the 404 off the response.
    #[test]
    fn fetch_failure_becomes_a_404_for_the_page_route() {
        let mut seen = Vec::new();

        for error in every_fetch_error() {
            seen.push(fetch_error_name(&error));
            let page: PageError = FetchFailure::Fetch(error).into();

            assert_eq!(page.status, 404, "{seen:?}");
            assert_eq!(page.message.as_deref(), Some(MSG_NO_ARTICLE), "{seen:?}");
            // Not quiet: a fetch that failed is worth an alert. The legacy's
            // quiet paths are the mistyped URL and nothing else.
            assert!(!page.quiet, "a failed fetch is worth an alert: {seen:?}");
            // Not declined: this is a real answer, and the edge skips the marked
            // ones. Marking it would take every failing post out of the
            // comparison instead of surfacing it.
            assert!(
                page.declined.is_none(),
                "only a decline carries the marker: {seen:?}"
            );
        }

        assert_eq!(
            seen.len(),
            9,
            "a variant was dropped from the list: {seen:?}"
        );
    }

    /// The decline keeps its own status and its marker, and it is the one
    /// `FetchFailure` that is not a 404. Fase 4's comparison depends on this
    /// being distinguishable from a real failure.
    #[test]
    fn the_shadow_decline_keeps_its_own_status_and_marker() {
        let page: PageError = FetchFailure::Declined.into();

        assert_eq!(page.status, DECLINED_STATUS);
        assert_eq!(page.declined, Some(SHADOW_NO_FETCH));
    }

    /// The unreachable variant, answered the way the legacy's generic
    /// `except Exception` answers rather than by a dropped connection.
    #[test]
    fn a_payload_that_is_not_a_payload_is_a_500() {
        let page: PageError = FetchFailure::NotAPayload("no data.post".to_string()).into();

        assert_eq!(page.status, 500);
        // `None` means the renderer picks from `ERROR_MSG_LIST`, which is what
        // `generate_error()` does with no `error_msg`.
        assert!(page.message.is_none());
        assert!(page.declined.is_none());
    }

    /// `handlers/post.py:72-89`, one row per exception `resolve_id` can actually
    /// produce.
    ///
    /// Three rows, not four: `InvalidMediumPostID` — the 500 at `:84-85` — is
    /// never raised by `medium_parser`, so there is no [`ResolveError`] for it
    /// and [`MSG_NO_POST_ID`] stays unreferenced. See its doc.
    ///
    /// `quiet` is a column because `NotValidMediumUrl` is the legacy's only
    /// silenced path (`utils/error.py:39-40`): a mistyped URL is routine traffic
    /// and does not reach Telegram, while the other two are alert-worthy.
    #[test]
    fn the_resolve_table_is_the_legacy_exception_table() {
        let cases = [
            (ResolveError::InvalidUrl, MSG_INVALID_URL, false),
            (ResolveError::NotValidMediumUrl, MSG_NOT_VALID_URL, true),
            (ResolveError::NoArticle, MSG_NO_ARTICLE, false),
        ];

        for (error, message, quiet) in cases {
            let page: PageError = error.into();

            assert_eq!(page.status, 404, "{error:?}");
            assert_eq!(page.message.as_deref(), Some(message), "{error:?}");
            assert_eq!(page.quiet, quiet, "{error:?}");
            assert!(page.declined.is_none(), "{error:?}");
        }
    }

    /// The table above is only load-bearing if the route goes through it, so this
    /// drives a known-bad domain end to end and reads the message out of the
    /// page.
    ///
    /// `github.com` is in `NOT_MEDIUM_DOMAINS`, which is the one branch that
    /// raises `NotValidMediumUrl` (`resolve.rs:543`). Before this test nothing
    /// asserted `MSG_NOT_VALID_URL` at all — the difftest fixtures happen not to
    /// contain a known-bad domain, so the gate has no opinion on it either.
    #[tokio::test]
    async fn a_known_bad_domain_renders_the_legacy_message() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = stub_state(recorder.clone());

        let response =
            render_medium_post_link(&state, &correlation(), "https://github.com/x", true, true)
                .await;

        let status = response.status();
        let html = body_text(response).await;

        assert_eq!(status, 404);
        assert!(html.contains(MSG_NOT_VALID_URL), "{html}");
        assert!(
            recorder.0.lock().unwrap().is_empty(),
            "a known-bad domain is rejected before any fetch"
        );
    }

    /// `fetch_and_cache` uses the source it is **handed**, not `state.source`.
    ///
    /// This is the seam `/api/v1` stands on: the API hands it the anonymous
    /// source and the page hands it `state.source`. Passing a [`FixedSource`]
    /// while the state's own source is a [`RecordingSource`] is what makes the
    /// difference observable — if `fetch_and_cache` ever read the state instead,
    /// the recorder would be non-empty and this would fail.
    ///
    /// The page's own side is covered by the two shadow tests above and by
    /// [`an_instance_that_is_not_a_shadow_still_fetches`], all of which call
    /// [`query`] and therefore this function with `state.source`.
    #[tokio::test]
    async fn fetch_and_cache_uses_the_source_it_is_given() {
        let recorder = Arc::new(RecordingSource(Mutex::new(Vec::new())));
        let state = stub_state(recorder.clone());

        let payload = fetch_and_cache(&state, "0291df856c77", &FixedSource(payload()))
            .await
            .expect("the fixed source answers");

        assert_eq!(payload.post().title, "A Test Article");
        assert!(
            recorder.0.lock().unwrap().is_empty(),
            "the state's own source was reached, so the parameter is not what decides"
        );
    }

    /// A cache read that fails is a **miss**, not an error: the stub's pool
    /// points at `.invalid`, so this is the unreachable-database case.
    ///
    /// It is the reason [`query_cached`] returns an `Option` and not a `Result`,
    /// and `/api/v1`'s miss bucket counts on it — a Postgres blip must not read
    /// as "this id is not a post", nor as a request that never happened.
    #[tokio::test]
    async fn an_unreachable_cache_is_a_miss_not_an_error() {
        let state = stub_state(Arc::new(RecordingSource(Mutex::new(Vec::new()))));

        assert!(query_cached(&state, "0291df856c77").await.is_none());
    }
}
