//! `/api/v1/resolve` — the id behind a Medium URL.
//!
//! # The input is handed to the page's resolver unchanged
//!
//! [`resolve_id`] is `MediumParser.resolve`, and the API calls it with the
//! caller's `url` verbatim. Not stripped, not normalised, not validated first:
//! the resolver's own first step is `correct_url`, which is a ported no-op, and
//! the order of its checks (`is_valid_url` on the raw input, then
//! `is_valid_medium_url` on the "sanitised" one, then the hex fallback) is the
//! behaviour the page is pinned to. A second guessing layer here would be a
//! second implementation of the same rule.
//!
//! That the parameter is called `url` and also accepts a bare hex id is the
//! resolver's own fallback (`core.py:76-86`), not something this endpoint adds.
//!
//! # The global fetch budget is charged before the resolution, always
//!
//! Not conditionally on the URL shape. A `link.medium.com/...` URL spends a real
//! upstream request; `https://example.com/x` spends nothing, because the domain
//! check rejects it before the resolver is reached. Charging everything is
//! imprecise in the direction that fails safe — it can refuse a resolution that
//! would have been free, never allow one that would have cost — and the
//! alternative is predicting, at the charge site, which of `resolve_id`'s
//! branches the network is reached from. That prediction would have to be kept in
//! step with the resolver, and it is exactly the kind of coupling this codebase
//! avoids by charging in one place.
//!
//! # Not the article's canonical URL
//!
//! [`ResolveDto`] carries `resolved_url` = `https://medium.com/p/{id}`. The
//! article's own canonical URL is `MetaDto::medium_url` and needs a fetch. The
//! DTO's docs say so; the name is the other half of that.

use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::Response;
use freedium_dto::problem::Problem;
use freedium_dto::resolve::ResolveDto;

use crate::api::http_cache::{JSON, respond};
use crate::api::problem::ApiError;
use crate::api::query_param;
use crate::handlers::post::resolve_id;
use crate::middleware::Correlation;
use crate::state::AppState;

/// `GET /api/v1/resolve?url=`
#[utoipa::path(
    get,
    path = "/api/v1/resolve",
    params(
        ("url" = String, Query, description = "A Medium URL, or a bare post id."),
    ),
    responses(
        (status = 200, description = "The id the input resolved to.", body = ResolveDto),
        (status = 400, description = "Not an absolute URL, or not a Medium URL.", body = Problem),
        (status = 401, description = "X-API-TOKEN did not match.", body = Problem),
        (status = 404, description = "A Medium URL with no id in it.", body = Problem),
        (status = 429, description = "The request bucket or the fetch budget is empty.", body = Problem),
        (status = 500, description = "A bug on our side.", body = Problem),
        (status = 503, description = "This instance does not fetch, or has no upstream.", body = Problem),
        (status = 504, description = "The upstream request timed out.", body = Problem),
    ),
    tag = "resolve"
)]
pub async fn resolve(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    // A missing parameter is not a `400` from an extractor but the empty string,
    // which `resolve_id` answers `InvalidUrl` for — the same 400, from the same
    // table, with a detail that names the input rather than the extractor's
    // complaint about it.
    let url = query_param(&uri, "url").unwrap_or_default();

    // Charged unconditionally: see the module docs. The returned value is not
    // logged here — `spend_fetch`'s callers that actually fetch log it, and a
    // resolution that spends nothing would make the line misleading.
    if let Err(error) = state.limits.spend_fetch() {
        return error.resolve(&uri, Some(&correlation));
    }

    match resolve_id(&state, &url).await {
        Ok(post_id) => {
            let dto = ResolveDto::new(post_id);
            let bytes = serde_json::to_vec(&dto).expect("a DTO always serialises");
            respond(
                &headers,
                bytes,
                JSON,
                // No `stale-while-revalidate`: unlike a post, this answer is a
                // pure function of its input and of a static domain list, so a
                // stale copy is never *usefully* stale — it is only old.
                &format!("public, max-age={}", state.config.api_cache_seconds),
            )
        }
        Err(error) => ApiError::from(error).resolve(&uri, Some(&correlation)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::post::ResolveError;
    use axum::http::StatusCode;
    use freedium_dto::problem::ProblemKind;

    /// The three rows §2.7 gives the API, and the statuses are what this endpoint
    /// answers. The page's table over the same errors is 404 for all three —
    /// `crate::api::problem`'s `the_api_resolve_table_differs_from_the_pages` is
    /// the other half of this.
    #[test]
    fn the_resolve_statuses_are_the_api_ones() {
        let table = [
            (ResolveError::InvalidUrl, ProblemKind::InvalidUrl, 400),
            (
                ResolveError::NotValidMediumUrl,
                ProblemKind::NotAMediumUrl,
                400,
            ),
            (ResolveError::NoArticle, ProblemKind::NotFound, 404),
        ];
        for (error, kind, status) in table {
            let api = ApiError::from(error);
            assert_eq!(api.kind, kind, "{error:?}");
            assert_eq!(
                api.resolve(&axum::http::Uri::from_static("/api/v1/resolve"), None)
                    .status(),
                StatusCode::from_u16(status).unwrap(),
                "{error:?}"
            );
        }
    }

    /// A missing `url` is the empty string, and the resolver's table answers it
    /// `InvalidUrl` rather than anything about a missing extractor field — which
    /// is what makes `query_param` returning an `Option` rather than a `Result`
    /// the right shape here.
    #[test]
    fn a_missing_url_is_an_invalid_url() {
        assert_eq!(
            query_param(&"http://x/api/v1/resolve".parse().unwrap(), "url"),
            None
        );
        assert_eq!(
            query_param(&"http://x/api/v1/resolve?url=".parse().unwrap(), "url"),
            Some(String::new())
        );
    }
}
