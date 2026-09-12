//! `/@miro/*` — `handlers/miro.py`.
//!
//! A byte-for-byte passthrough of `miro.medium.com`, and one of the two places
//! where the legacy discards the upstream status.
//!
//! # The page never learns whether the fetch worked
//!
//! `miro.py:33` is `Response(content=request_content, media_type=content_type)`.
//! FastAPI's `Response` defaults to 200, and `raise_for_status=False`
//! (`miro.py:27`) means a 404 from Medium is not an exception either — its body
//! is simply relayed under a 200. So a request for an image that does not exist
//! gets a 200 whose body says `Not Found`.
//!
//! That is reproduced, because the alternative is a visible behaviour change on
//! a hot path: every article image goes through here, and turning a missing
//! image into a non-200 could change what a browser renders. Note that
//! `Media::status` is what carries the discarded value, so a caller that wants
//! it can have it — this one logs it at `DEBUG` and drops it, as the legacy does.
//!
//! # `IFRAME_HEADERS` is defined in this file and never used
//!
//! `miro.py:11` declares `IFRAME_HEADERS = {"Access-Control-Allow-Origin": "*",
//! "X-Frame-Options": "SAMEORIGIN"}`, and `miro_proxy` does not pass it. It is
//! dead code in the legacy, and dead code that *would* matter if someone wired it
//! up: `X-Frame-Options: SAMEORIGIN` on an image is harmless, but the same
//! header on this route would be a change. Left out here so the omission is
//! visible rather than copied.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::handlers::main::internal_server_error;
use crate::state::AppState;

/// What `Content-Type` a response with no usable upstream one gets.
///
/// `miro.py:31` is `content_type = request.headers["Content-Type"]`, which
/// **raises `KeyError`** when the header is absent, turning the request into a
/// 500. That is not reproduced: an upstream that omits the header is a
/// transport accident, not a reason to fail an image, and §3.3's preference is
/// that a malformed field costs the field rather than the whole response. The
/// value here is what aiohttp's own default would effectively have been.
const FALLBACK_CONTENT_TYPE: &str = "application/octet-stream";

/// `miro_proxy` (`handlers/miro.py:14-33`), with `use_proxy` left at its default.
///
/// `use_proxy=False` is the legacy's default and the only value any caller
/// passes, so the `random.choice(config.PROXY_LIST)` branch at `:20-23` is
/// unreachable in production — [`medium_client::media::MediaFetcher::fetch_miro`]
/// goes out directly for that reason. The branch is not ported; if it is ever
/// wanted, it is a parameter on the fetcher and not a change here.
pub async fn miro_proxy(state: &AppState, miro_data: &str) -> Response {
    let media = match state.media.fetch_miro(miro_data).await {
        Ok(media) => media,
        Err(err) => {
            // `miro.py` catches nothing, so this reached FastAPI's error
            // middleware and answered a plain-text 500. See
            // [`internal_server_error`].
            tracing::error!(error = %err, miro_data, "could not fetch from miro.medium.com");
            return internal_server_error();
        }
    };

    // Logged rather than sent — see the module docs.
    tracing::debug!(
        status = media.status,
        content_type = ?media.content_type,
        "miro passthrough"
    );

    let content_type = media
        .content_type
        .as_deref()
        .map_or_else(|| FALLBACK_CONTENT_TYPE.to_string(), with_charset);

    response_with_content_type(StatusCode::OK, content_type, media.body)
}

/// Builds a response, falling back to the default type if the upstream value is
/// not a legal header.
pub(crate) fn response_with_content_type(
    status: StatusCode,
    content_type: String,
    body: Vec<u8>,
) -> Response {
    let content_type = header::HeaderValue::from_str(&content_type)
        .unwrap_or_else(|_| header::HeaderValue::from_static(FALLBACK_CONTENT_TYPE));

    (
        status,
        [(header::CONTENT_TYPE, content_type)],
        Body::from(body),
    )
        .into_response()
}

/// Starlette's rule for `media_type` (`starlette/responses.py`): a `text/*` type
/// without a `charset` parameter gets `; charset=utf-8` appended.
///
/// It applies here because `Content-Type` is copied verbatim from upstream
/// (`miro.py:31`) and then handed to `Response(media_type=...)`, which is where
/// Starlette would append it. Medium serves images, so this rarely fires — but
/// `miro.medium.com` does serve a few `text/*` resources, and a missing charset
/// is a visible header difference for exactly those.
fn with_charset(content_type: &str) -> String {
    let is_text = content_type.starts_with("text/");
    let has_charset = content_type.contains("charset=");
    if is_text && !has_charset {
        format!("{content_type}; charset=utf-8")
    } else {
        content_type.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_type_gets_a_charset_and_an_image_does_not() {
        assert_eq!(with_charset("text/html"), "text/html; charset=utf-8");
        assert_eq!(with_charset("image/png"), "image/png");
        // Already specified: appending a second one would be a malformed header.
        assert_eq!(
            with_charset("text/html; charset=latin-1"),
            "text/html; charset=latin-1"
        );
        assert_eq!(
            with_charset("image/svg+xml"),
            "image/svg+xml",
            "not a text/* type, whatever the +xml suffix suggests"
        );
    }

    /// An upstream `Content-Type` that is not a legal header value must not panic
    /// inside a response builder — it degrades to the fallback.
    #[test]
    fn an_illegal_content_type_falls_back() {
        let response = response_with_content_type(
            StatusCode::OK,
            "image/png\nX-Injected: 1".to_string(),
            Vec::new(),
        );
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            FALLBACK_CONTENT_TYPE
        );
    }

    /// The status is always 200 however the fetch went — the whole point of the
    /// module. `Media::status` carries the real one and is not consulted here.
    #[test]
    fn the_response_status_is_always_ok() {
        let response = response_with_content_type(StatusCode::OK, "image/png".to_string(), vec![1]);
        assert_eq!(response.status(), StatusCode::OK);
    }
}
