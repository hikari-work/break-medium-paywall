//! The four post representations: JSON, HTML fragment, markdown fragment, and
//! metadata.
//!
//! # One fetch, four renderings
//!
//! Every route here goes through [`load`], so the cache lookup, the miss bucket,
//! the global fetch budget and the shadow interlock are decided in exactly one
//! place. A route that grew its own copy of that sequence would be a route that
//! forgot the budget.
//!
//! # What is cached and what is not
//!
//! Only the payload — the durable cache row — is reused. `/html` re-parses and
//! re-renders even on a Postgres hit, because the artefact the page route caches
//! in Redis is a whole HTML document rather than a fragment, so there is nothing
//! to reuse. Fase 6 adds **no Redis key at all**: the volume is bounded by the
//! token buckets, an API representation is cheap once the payload is warm, and
//! every new key is a new invalidation surface while the only invalidation path
//! there is (`/delete-from-cache`) removes a Postgres row.
//!
//! If that ever needs revisiting, the key is `v2:api:{id}:{repr}` and it must
//! **never** be `v2:post:{id}`: the value there is a positional msgpack array of
//! `RenderedPost`'s four fields, so a DTO written to it would decode
//! *successfully* as a rendered post with every field wrong. Silent corruption
//! rather than an error.
//!
//! # `{id}` is checked before anything is spent
//!
//! [`basic_hex_check`] runs first, so a malformed id costs a string comparison
//! and no bucket cell. That is what makes the global budget mean something: the
//! budget exists to bound the *expensive* request, and the cheap ones never reach
//! it.
//!
//! It is deliberately the stricter of the two checks available.
//! [`is_has_valid_medium_post_id`] searches for a hex run *inside* a longer
//! string, so `foo-12345678` would pass it and then be fetched as an id. A path
//! segment is an id or it is nothing.

use axum::Extension;
use axum::extract::{OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use freedium_dto::post::{MetaDto, PostDto};
use freedium_dto::problem::{Problem, ProblemKind};
use medium_doc::metadata;
use medium_doc::parse::{PostPayload, parse};
use medium_doc::resolve::basic_hex_check;
use medium_render::dto::{meta_dto, to_dto};
use medium_render::markdown::render_markdown;
use medium_render::page::render_post_body;

use crate::api::http_cache::{HTML, JSON, MARKDOWN, respond};
use crate::api::limit::ApiClient;
use crate::api::problem::ApiError;
use crate::handlers::post::{fetch_and_cache, query_cached};
use crate::middleware::Correlation;
use crate::state::AppState;

/// The id in `{id}` is not a post id.
fn invalid_post_id(id: &str) -> ApiError {
    ApiError::new(
        ProblemKind::InvalidPostId,
        format!("{id} is not a Medium post id"),
    )
}

/// The cache, the buckets and the fetch — once, for all four routes.
///
/// # The two charges, in the order they can be spent
///
/// The miss bucket first: it is per-IP and it is the one a client controls, so a
/// client that has been misbehaving is refused before the shared budget is
/// touched. The global budget second, because it is charged only when an upstream
/// request is genuinely about to be made.
///
/// A cache **hit** charges neither. That is the point of splitting them: reading
/// a post that is already cached costs the process nothing, and the request
/// bucket is what bounds how often a client may do it.
async fn load(
    state: &AppState,
    client: &ApiClient,
    post_id: &str,
) -> Result<PostPayload, ApiError> {
    if !basic_hex_check(post_id) {
        return Err(invalid_post_id(post_id));
    }

    if let Some(payload) = query_cached(state, post_id).await {
        return Ok(payload);
    }

    state.limits.spend_miss(client)?;
    let remaining = state.limits.spend_fetch()?;

    // Every miss, with the budget it just spent. This line is the source the
    // shipped number is supposed to be replaced by: the correct global budget is
    // a measurement of what one WARP exit takes, and this is the data.
    tracing::info!(post_id, remaining, "api cache miss");

    fetch_and_cache(state, post_id, state.api_source.inner().as_ref())
        .await
        .map_err(ApiError::from)
}

/// The `Cache-Control` for a post representation.
///
/// `API_CACHE_SECONDS` and not `CACHE_LIFE_TIME`: the latter is five hours, and
/// **nothing purges**. `/delete-from-cache` removes a Postgres row; no code
/// invalidates Redis and none can recall a CDN's copy. A five-hour `max-age` on
/// this URL would outlive a deploy, a template fix and — during a soak — a
/// corrected parser, and the symptom would be a consumer reporting stale content
/// that nobody can flush.
fn cache_control(state: &AppState) -> String {
    format!(
        "public, max-age={}, stale-while-revalidate=60",
        state.config.api_cache_seconds
    )
}

/// Serialises, then validates — never the other way round.
///
/// The tag has to cover the bytes the client receives, so the JSON is produced
/// here rather than by an extracted `Json` wrapper. `serde_json::to_vec` over a
/// DTO whose every field is a `String`, an integer or an `Option` of those cannot
/// fail, and the `expect` says so.
fn json_response<T: serde::Serialize>(
    headers: &HeaderMap,
    value: &T,
    cache_control: &str,
) -> Response {
    let bytes = serde_json::to_vec(value).expect("a DTO always serialises");
    respond(headers, bytes, JSON, cache_control)
}

/// `GET /api/v1/posts/{id}` — the structured article.
#[utoipa::path(
    get,
    path = "/api/v1/posts/{id}",
    params(("id" = String, Path, description = "The Medium post id: 8 to 12 ASCII alphanumerics.")),
    responses(
        (status = 200, description = "The article.", body = PostDto),
        (status = 400, description = "Not a post id.", body = Problem),
        (status = 401, description = "X-API-TOKEN did not match.", body = Problem),
        (status = 404, description = "No such post.", body = Problem),
        (status = 429, description = "A rate-limit bucket is empty.", body = Problem),
        (status = 500, description = "A bug on our side.", body = Problem),
        (status = 502, description = "Upstream answered with something unusable.", body = Problem),
        (status = 503, description = "No upstream, or this instance does not fetch.", body = Problem),
        (status = 504, description = "Upstream timed out.", body = Problem),
    ),
    tag = "posts"
)]
pub async fn post(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    Extension(client): Extension<ApiClient>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match load(&state, &client, &id).await {
        Ok(payload) => {
            let document = parse(&payload, &state.config.host_address);
            let dto = to_dto(&document, &payload, &id);
            json_response(&headers, &dto, &cache_control(&state))
        }
        Err(error) => error.resolve(&uri, Some(&correlation)),
    }
}

/// `GET /api/v1/posts/{id}/meta` — the metadata, without parsing the body.
///
/// `meta_dto` needs no `Document` and no template, which is what makes this the
/// cheapest endpoint here: on a cache hit it is a Postgres read and a projection.
#[utoipa::path(
    get,
    path = "/api/v1/posts/{id}/meta",
    params(("id" = String, Path, description = "The Medium post id.")),
    responses(
        (status = 200, description = "The article's metadata.", body = MetaDto),
        (status = 400, description = "Not a post id.", body = Problem),
        (status = 401, description = "X-API-TOKEN did not match.", body = Problem),
        (status = 404, description = "No such post.", body = Problem),
        (status = 429, description = "A rate-limit bucket is empty.", body = Problem),
        (status = 500, description = "A bug on our side.", body = Problem),
        (status = 502, description = "Upstream answered with something unusable.", body = Problem),
        (status = 503, description = "No upstream, or this instance does not fetch.", body = Problem),
        (status = 504, description = "Upstream timed out.", body = Problem),
    ),
    tag = "posts"
)]
pub async fn meta(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    Extension(client): Extension<ApiClient>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match load(&state, &client, &id).await {
        Ok(payload) => {
            let dto = meta_dto(&payload.post(), &id);
            json_response(&headers, &dto, &cache_control(&state))
        }
        Err(error) => error.resolve(&uri, Some(&correlation)),
    }
}

/// `GET /api/v1/posts/{id}/html` — the article fragment.
///
/// The fragment and not the page: `RenderedPost.html` is a whole document with a
/// `<head>` and a `<title>`, and what a consumer embedding an article wants is
/// what `base.html` splices in. [`render_post_body`] is that, and the page route
/// calls the same function, so the two cannot drift.
#[utoipa::path(
    get,
    path = "/api/v1/posts/{id}/html",
    params(("id" = String, Path, description = "The Medium post id.")),
    responses(
        (status = 200, description = "The article body, as the page embeds it.", body = String, content_type = "text/html"),
        (status = 400, description = "Not a post id.", body = Problem),
        (status = 401, description = "X-API-TOKEN did not match.", body = Problem),
        (status = 404, description = "No such post.", body = Problem),
        (status = 429, description = "A rate-limit bucket is empty.", body = Problem),
        (status = 500, description = "A bug on our side.", body = Problem),
        (status = 502, description = "Upstream answered with something unusable.", body = Problem),
        (status = 503, description = "No upstream, or this instance does not fetch.", body = Problem),
        (status = 504, description = "Upstream timed out.", body = Problem),
    ),
    tag = "posts"
)]
pub async fn html(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    Extension(client): Extension<ApiClient>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let payload = match load(&state, &client, &id).await {
        Ok(payload) => payload,
        Err(error) => return error.resolve(&uri, Some(&correlation)),
    };

    let document = parse(&payload, &state.config.host_address);
    let post_metadata = metadata::from_payload(&payload, &id);

    match render_post_body(state.templates(), &document, &post_metadata) {
        Ok(body) => respond(&headers, body.into_bytes(), HTML, &cache_control(&state)),
        Err(err) => {
            tracing::error!(id, error = %err, "could not render the article fragment");
            ApiError::new(
                ProblemKind::Internal,
                "the article could not be rendered".to_string(),
            )
            .resolve(&uri, Some(&correlation))
        }
    }
}

/// `GET /api/v1/posts/{id}/markdown` — the article as markdown.
///
/// A fragment, with no front matter and no heading for the title: the metadata
/// lives in `/meta` and `/posts/{id}`, and duplicating it here would create a
/// second place for a title to be wrong.
#[utoipa::path(
    get,
    path = "/api/v1/posts/{id}/markdown",
    params(("id" = String, Path, description = "The Medium post id.")),
    responses(
        (status = 200, description = "The article body, as markdown.", body = String, content_type = "text/markdown"),
        (status = 400, description = "Not a post id.", body = Problem),
        (status = 401, description = "X-API-TOKEN did not match.", body = Problem),
        (status = 404, description = "No such post.", body = Problem),
        (status = 429, description = "A rate-limit bucket is empty.", body = Problem),
        (status = 500, description = "A bug on our side.", body = Problem),
        (status = 502, description = "Upstream answered with something unusable.", body = Problem),
        (status = 503, description = "No upstream, or this instance does not fetch.", body = Problem),
        (status = 504, description = "Upstream timed out.", body = Problem),
    ),
    tag = "posts"
)]
pub async fn markdown(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    Extension(client): Extension<ApiClient>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match load(&state, &client, &id).await {
        Ok(payload) => {
            let document = parse(&payload, &state.config.host_address);
            respond(
                &headers,
                render_markdown(&document).into_bytes(),
                MARKDOWN,
                &cache_control(&state),
            )
        }
        Err(error) => error.resolve(&uri, Some(&correlation)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    /// The validator round trip, at the level the property actually lives: the
    /// bytes and the tag. A router would add a request builder to every
    /// assertion without testing anything more.
    #[tokio::test]
    async fn a_validator_round_trip_returns_the_same_bytes() {
        let first = json_response(
            &HeaderMap::new(),
            &serde_json::json!({"a": 1}),
            "public, max-age=300",
        );
        assert_eq!(first.status(), StatusCode::OK);
        let tag = first
            .headers()
            .get("etag")
            .expect("a validator is set")
            .to_str()
            .unwrap()
            .to_string();
        let body = to_bytes(first.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], br#"{"a":1}"#);

        let mut again = HeaderMap::new();
        again.insert("if-none-match", tag.parse().unwrap());
        let second = json_response(&again, &serde_json::json!({"a": 1}), "public, max-age=300");
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);

        // One byte of difference is a different representation, so the stale
        // validator must not match it.
        let changed = json_response(&again, &serde_json::json!({"a": 2}), "public, max-age=300");
        assert_eq!(changed.status(), StatusCode::OK);
    }

    /// A malformed id is refused before any bucket is touched, which is what
    /// makes the global fetch budget mean something.
    #[test]
    fn a_bad_id_costs_nothing() {
        for id in [
            "",
            "zz",
            "not-an-id",
            "1234567",
            "1234567890123",
            "abc/def",
            "../etc",
        ] {
            let error = invalid_post_id(id);
            assert_eq!(error.kind, ProblemKind::InvalidPostId);
            assert_eq!(error.kind.status(), 400);
        }

        for id in [
            "",
            "zz",
            "not-an-id",
            "1234567",
            "1234567890123",
            "foo-12345678",
        ] {
            assert!(!basic_hex_check(id), "{id} passed the gate");
        }
        for id in ["a1b2c3d4e5f6", "aaaaaaaaaaaa", "12345678"] {
            assert!(basic_hex_check(id), "{id} failed the gate");
        }
    }
}
