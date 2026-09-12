//! The catch-all route — `handlers/main.py`.
//!
//! One handler answers every page path, because the legacy registers one route
//! and dispatches inside it:
//!
//! ```text
//! GET|HEAD /{path:path}  →  ""            → the homepage
//!                           "@miro/"      → miro_proxy
//!                           "render_iframe/" → iframe_proxy
//!                           anything else → render_medium_post_link
//! ```
//!
//! # The path is read off the URI, not from a wildcard capture
//!
//! `handlers/main.py:27` computes `path = path.removeprefix("/")` on the string
//! FastAPI's `{path:path}` converter produced, which is the request path with no
//! leading slash. [`dispatch`] recomputes exactly that from `request.uri()`
//! instead of taking `Path<String>`, so the two agree by construction rather than
//! by two different frameworks happening to strip the same way.
//!
//! # Two different strings, and only one of them is the path
//!
//! `handlers/main.py:34-35` strips the origin off the full URL and passes the
//! remainder to `render_medium_post_link`, so what `resolve` ultimately sees is
//! **path plus query** — `"@ada/my-post-0291df856c77"` or
//! `"0291df856c77?no-redis"`. The prefix checks above, though, run against the
//! path alone (`:43-48`), so a query string cannot smuggle a request into the
//! miro route. Both strings are built here, and keeping them apart is the point
//! of [`Path`].

use axum::Extension;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};

use crate::error::json_error;
use crate::handlers::{iframe, miro, post};
use crate::middleware::Correlation;
use crate::state::AppState;

/// The header `handlers/main.py:38` reads, spelled as the legacy spells it.
pub const ADMIN_SECRET_KEY_HEADER: &str = "ADMIN_SECRET_KEY";

/// The two cache-bypass query parameters, as `_`-separated keys.
pub const NO_REDIS: &str = "no-redis";
pub const NO_DB_CACHE: &str = "no-db-cache";

/// The request path, in the two forms the legacy uses.
struct Path {
    /// `handlers/main.py:27` — no leading slash, no query. Drives the dispatch.
    only: String,
    /// `handlers/main.py:34-35` — the same, plus `?query`. What gets resolved.
    with_query: String,
}

impl Path {
    fn of(request: &Request) -> Self {
        let only = request
            .uri()
            .path()
            .strip_prefix('/')
            .unwrap_or_else(|| request.uri().path())
            .to_string();

        let with_query = match request.uri().query() {
            Some(query) => format!("{only}?{query}"),
            None => only.clone(),
        };

        Self { only, with_query }
    }
}

/// `route_processing` (`handlers/main.py:16-50`).
pub async fn dispatch(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    request: Request,
) -> Response {
    let path = Path::of(&request);

    // `handlers/main.py:18-19`: the empty path is the homepage, and it is
    // dispatched *before* the query parameters are read — so `/` ignores
    // `no-redis` entirely rather than 403ing on a missing admin key.
    if path.only.is_empty() {
        return post::render_index(&state, &correlation).await;
    }

    let query = request.uri().query().unwrap_or_default();
    // `"no-redis" not in query_params` — membership of the *key*, so `?no-redis`
    // and `?no-redis=0` both disable the cache, as they do in Starlette.
    let use_redis = !query_has(query, NO_REDIS);
    let use_db_cache = !query_has(query, NO_DB_CACHE);

    // `handlers/main.py:37-41`. Note that either parameter alone requires the
    // key — they are not independently gated.
    if (!use_db_cache || !use_redis)
        && let Some(denial) = check_admin_key(&state, &request)
    {
        return denial;
    }

    // `:43-48`. `removeprefix` on a string that does not start with the prefix
    // is a no-op, so the `if` is what guards the strip and the order matters:
    // `@miro/render_iframe/x` is a miro request, as it is in the legacy.
    if let Some(miro_data) = path.only.strip_prefix("@miro/") {
        return miro::miro_proxy(&state, miro_data).await;
    }
    if let Some(iframe_id) = path.only.strip_prefix("render_iframe/") {
        return iframe::iframe_proxy(&state, iframe_id).await;
    }

    post::render_medium_post_link(
        &state,
        &correlation,
        &path.with_query,
        use_db_cache,
        use_redis,
    )
    .await
}

/// Whether `query` contains `key`, matching Starlette's `key in request.query_params`.
///
/// The value is discarded: `handlers/main.py:22-23` only ever asks whether the
/// key is present. `form_urlencoded` rather than a split on `&` so that a
/// percent-encoded key is matched the way Starlette's parser matches it.
fn query_has(query: &str, key: &str) -> bool {
    form_urlencoded::parse(query.as_bytes()).any(|(name, _)| name == key)
}

/// `handlers/main.py:38-41`, or `None` when the key is right.
///
/// The comparison is constant-time — §7 item 7, and the reason `subtle` is a
/// dependency. The legacy's `!=` on two strings short-circuits at the first
/// differing byte, which leaks the length of the shared prefix of the secret to
/// anyone who can time the endpoint.
///
/// # The rejection message echoes the presented key, deliberately
///
/// `handlers/main.py:41` builds `f"Wrong secret key: {key_data}"`, and that
/// value goes back to the caller. It is the caller's *own* input rather than the
/// secret, so echoing it discloses nothing they did not send — and it is what
/// makes a misconfigured client debuggable. Kept, including the literal `None`
/// for a missing header, because that is what the f-string prints.
fn check_admin_key(state: &AppState, request: &Request) -> Option<Response> {
    let presented = request.headers().get(ADMIN_SECRET_KEY_HEADER);

    let Some(presented) = presented else {
        // `f"Wrong secret key: {None}"` — the f-string prints `None`.
        return Some(json_error("Wrong secret key: None".to_string(), 403));
    };

    // `HeaderValue` is not required to be UTF-8; a non-UTF-8 value cannot equal
    // the key, and the legacy would have decoded it with `latin-1` and echoed
    // that. The invalid case is reported as unreadable rather than echoed.
    let Ok(text) = presented.to_str() else {
        return Some(json_error(
            "Wrong secret key: <not valid UTF-8>".to_string(),
            403,
        ));
    };

    let matches = {
        let presented = text.as_bytes();
        let expected = state.config.admin_secret_key.as_bytes();
        // `ct_eq` on unequal lengths returns 0 without reading past either end,
        // so the length check is part of the comparison and not a shortcut.
        presented.len() == expected.len()
            && bool::from(subtle::ConstantTimeEq::ct_eq(presented, expected))
    };

    if matches {
        None
    } else {
        Some(json_error(format!("Wrong secret key: {text}"), 403))
    }
}

/// Starlette's `ServerErrorMiddleware` body: `PlainTextResponse("Internal Server
/// Error", 500)`.
///
/// The shares of the route handlers that are not pages — `@miro/` and
/// `render_iframe/` — have no error page in the legacy, because the exception
/// escapes the handler and the middleware described in
/// [`crate::middleware`]'s module docs never sees it: FastAPI's own error
/// middleware answers first. An image request that fails must not receive HTML,
/// so this is what those two return.
pub fn internal_server_error() -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "Internal Server Error",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn path_of(uri: &str) -> (String, String) {
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let path = Path::of(&request);
        (path.only, path.with_query)
    }

    /// `handlers/main.py:27` and `:34-35`: the dispatch key has no leading slash
    /// and no query, and the value handed to `resolve` has both the query and no
    /// leading slash. Getting these the same way round is the whole point of
    /// keeping two.
    #[test]
    fn the_two_path_forms_match_the_legacy() {
        assert_eq!(
            path_of("/@ada/my-post-0291df856c77"),
            (
                "@ada/my-post-0291df856c77".to_string(),
                "@ada/my-post-0291df856c77".to_string()
            )
        );
        assert_eq!(
            path_of("/0291df856c77?no-redis"),
            (
                "0291df856c77".to_string(),
                "0291df856c77?no-redis".to_string()
            )
        );
        // The root, with and without a query: `only` is empty either way, which
        // is what makes `/` the homepage before the parameters are read.
        assert_eq!(path_of("/"), (String::new(), String::new()));
        assert_eq!(
            path_of("/?no-redis"),
            (String::new(), "?no-redis".to_string())
        );
        // A trailing slash is not stripped from the middle of the path.
        assert_eq!(path_of("/@miro/").0, "@miro/");
        assert_eq!(path_of("/a//b").0, "a//b");
    }

    /// Membership is by key, and a key is present whether or not it has a value.
    #[test]
    fn the_bypass_parameters_are_read_by_key() {
        assert!(query_has("no-redis", NO_REDIS));
        assert!(query_has("no-redis=", NO_REDIS));
        assert!(query_has("no-redis=0", NO_REDIS));
        assert!(query_has("a=1&no-redis&b=2", NO_REDIS));
        assert!(query_has("a=1&no-db-cache=yes", NO_DB_CACHE));

        assert!(!query_has("", NO_REDIS));
        assert!(!query_has("no-redisx", NO_REDIS));
        assert!(!query_has("no_redis", NO_REDIS));
        // A *value* that mentions the key is not the key.
        assert!(!query_has("cache=no-redis", NO_REDIS));
    }

    /// A miro path is checked before an iframe one, as the two `if`s are ordered
    /// in the legacy — so this is a miro request, with `render_iframe/x` as its
    /// target, and not an iframe request.
    #[test]
    fn the_miro_prefix_wins_over_the_iframe_one() {
        let only = "@miro/render_iframe/x";
        assert_eq!(
            only.strip_prefix("@miro/"),
            Some("render_iframe/x"),
            "the miro branch is taken first, matching handlers/main.py:43"
        );
    }
}
