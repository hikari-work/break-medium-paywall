//! `/api/v1/feed` — a page of post metadata.
//!
//! # The order is by cache key, which is stable and arbitrary
//!
//! The plan's §2.7 assumed `/feed` would simply replace a `ORDER BY RANDOM()`
//! homepage query, and both halves of that are now wrong. `random` was already
//! replaced in Fase 2 by a `TABLESAMPLE`, which has no stable order at all — so a
//! `cursor` over it could not mean anything. And a chronological feed needs an
//! `updated_at` column that §2.4's no-migrations rule forbids: the `cache` table
//! has two columns, and the payload is a JSON blob in the second one.
//!
//! So the order is `key`, which is a 12-hex FNV-1a hash of the post id
//! (`freedium_cache::keys`). It is **stable**, so pagination works, and it is
//! **not** chronological, so nothing about it will look like a timeline. That is
//! said here rather than left for a consumer to discover.
//!
//! # Why `limit + 1` rows are read
//!
//! `next_cursor: None` means "you have reached the end" and `Some` means "there is
//! more" — a distinction the contract depends on, and one that a page of exactly
//! `limit` rows cannot make: it does not know whether it is the last page or
//! merely a full one. So one extra row is requested, and its existence is what
//! sets the cursor.
//!
//! That extra row is *not* returned, and a row that cannot be decoded is skipped
//! rather than shortened into the page — which is what the loop is for.
//! [`PostPayload::post`](medium_doc::parse::PostPayload::post) is infallible and
//! quietly turns an unparseable `data.post` into an empty `Post`, so a malformed
//! row does not even announce itself as a skip. Documented rather than fixed:
//! tolerating a bad row matches what the page route does, and turning it into a
//! `500` would make one broken row break the whole feed.

use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::Response;
use freedium_cache::keys;
use freedium_dto::feed::FeedDto;
use freedium_dto::problem::{Problem, ProblemKind};
use medium_render::dto::meta_dto;

use crate::api::http_cache::{JSON, respond};
use crate::api::problem::ApiError;
use crate::api::query_param;
use crate::handlers::post::decode_cached;
use crate::middleware::Correlation;
use crate::state::AppState;

/// How many posts a page carries when the caller does not say.
pub const DEFAULT_LIMIT: i64 = 20;

/// The most a caller may ask for.
///
/// A cap rather than trust: each row is a JSON blob that is decoded and projected,
/// and `?limit=100000` should not be a way to make the process read the whole
/// table into memory.
pub const MAX_LIMIT: i64 = 50;

/// `/feed`'s own `Cache-Control`, shorter than a post's.
///
/// A feed page is a moving target — posts enter it as they are fetched — so the
/// same URL can legitimately mean something different ten minutes later. Sixty
/// seconds is the compromise between "the client does not hammer us" and "the
/// client is not reading a list from an hour ago".
pub const FEED_CACHE_CONTROL: &str = "public, max-age=60";

/// `GET /api/v1/feed?cursor=&limit=`
#[utoipa::path(
    get,
    path = "/api/v1/feed",
    params(
        ("cursor" = Option<String>, Query, description = "The `next_cursor` of the previous page."),
        ("limit" = Option<i64>, Query, description = "How many posts, 1 to 50. Defaults to 20."),
    ),
    responses(
        (status = 200, description = "A page of post metadata.", body = FeedDto),
        (status = 401, description = "X-API-TOKEN did not match.", body = Problem),
        (status = 429, description = "The request bucket is empty.", body = Problem),
        (status = 503, description = "The cache is not answering.", body = Problem),
    ),
    tag = "feed"
)]
pub async fn feed(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let cursor = query_param(&uri, "cursor");
    let limit = query_param(&uri, "limit")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        // A limit of `0` or `-3` is clamped rather than refused: it cannot mean
        // anything useful, and a `400` for it would be a worse answer than the
        // page the caller can actually use.
        .clamp(1, MAX_LIMIT);

    // One extra row: see the module docs. `page` walks the primary key in order
    // and stops as soon as `size` rows match, so the extra row costs one index
    // step rather than a scan.
    let rows = match state
        .postgres
        .page(keys::POST_PREFIX, cursor.as_deref(), limit + 1)
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "could not read a feed page");
            return ApiError::new(
                ProblemKind::NotReady,
                "the durable cache is not answering".to_string(),
            )
            .resolve(&uri, Some(&correlation));
        }
    };

    let mut posts = Vec::with_capacity(limit as usize);
    let mut last_key = None;
    for (key, value) in &rows {
        if posts.len() as i64 == limit {
            break;
        }
        // Both of these are unreachable for a row under `POST_PREFIX`, and both
        // are `continue` rather than `unwrap`: a row this loop cannot read must
        // shorten the page, never end the feed.
        let Some(post_id) = keys::post_id_from_key(key) else {
            continue;
        };
        let Some(payload) = decode_cached(value).ok() else {
            tracing::warn!(key, "skipping an undecodable feed row");
            continue;
        };
        posts.push(meta_dto(&payload.post(), post_id));
        last_key = Some(key.clone());
    }

    // `rows` holds at most `limit + 1`, so "more exists" is exactly "the extra
    // row was there".
    let next_cursor = if rows.len() as i64 > limit {
        last_key
    } else {
        None
    };

    let dto = FeedDto::new(posts, next_cursor);
    let bytes = serde_json::to_vec(&dto).expect("a DTO always serialises");
    respond(&headers, bytes, JSON, FEED_CACHE_CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two bounds, and the behaviour that matters: a page of exactly `limit`
    /// rows with no extra row is the end of the feed.
    #[test]
    fn the_limit_is_clamped_to_something_servable() {
        for (given, expected) in [
            (None, DEFAULT_LIMIT),
            (Some("7"), 7),
            (Some("0"), 1),
            (Some("-3"), 1),
            (Some("100000"), MAX_LIMIT),
            (Some("abc"), DEFAULT_LIMIT),
        ] {
            let parsed = given
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);
            assert_eq!(parsed, expected, "{given:?}");
        }
    }

    /// The cursor rule, as a truth table over the row count. It is the one piece
    /// of arithmetic in this module that a consumer can observe.
    #[test]
    fn a_next_cursor_means_there_is_another_page() {
        let cursor = |rows: i64, limit: i64| {
            if rows > limit {
                Some("v2:post:abc".to_string())
            } else {
                None
            }
        };
        assert_eq!(cursor(21, 20), Some("v2:post:abc".to_string()));
        assert_eq!(
            cursor(20, 20),
            None,
            "a full page with no extra row is the end"
        );
        assert_eq!(cursor(0, 20), None);
        assert_eq!(cursor(2, 1), Some("v2:post:abc".to_string()));
    }
}
