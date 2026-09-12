//! `/render_iframe/*` — `handlers/iframe.py`.
//!
//! Proxies `medium.com/media/{id}` through a proxy exit, patches one string in
//! the result, and always answers 200 with two fixed headers.
//!
//! # `prettify()` is dropped, and this is the module that says so
//!
//! `iframe.py:42-44` runs the patched HTML through BeautifulSoup and returns
//! `soup.prettify()`. `prettify` is not a formatter with a stable output that
//! could be reproduced: it reindents the whole document, and the result depends
//! on BeautifulSoup's parser and on how it happened to nest what it was given.
//! Reproducing it byte for byte would mean porting an HTML parser's error
//! recovery, for a response that is embedded in an iframe and read by a browser
//! rather than by a person.
//!
//! §5 decision 3 therefore keeps the patch — which is functional, it is what
//! makes the iframe work at all — and drops the pretty-printing, which is
//! cosmetic. **The two responses are not byte-identical**, so this route is
//! excluded from the parity gate; what it preserves is the patch, the status and
//! the headers, which is what a browser depends on.
//!
//! # The patch is a string replace, not a parse
//!
//! `iframe.py:38-40` replaces `document.domain = document.domain` with a
//! `console.log`, because Medium's embed sets `document.domain` and the iframe
//! is served from a different origin. `patch_iframe_content` does the same
//! replace on the raw text, which is why it can be exact where `prettify` cannot.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use medium_client::media::patch_iframe_content;

use crate::handlers::main::internal_server_error;
use crate::state::AppState;

/// `iframe.py:13` — `IFRAME_HEADERS`.
///
/// Unlike the copy in `miro.py`, this one is used.
pub const IFRAME_HEADERS: [(&str, &str); 2] = [
    ("access-control-allow-origin", "*"),
    ("x-frame-options", "SAMEORIGIN"),
];

/// `iframe_proxy` (`handlers/iframe.py:16-34`).
pub async fn iframe_proxy(state: &AppState, iframe_id: &str) -> Response {
    let media = match state.media.fetch_iframe(iframe_id).await {
        Ok(media) => media,
        Err(err) => {
            tracing::error!(error = %err, iframe_id, "could not fetch from medium.com/media");
            return internal_server_error();
        }
    };

    // The upstream status is not consulted, as at `iframe.py:34`: the response is
    // 200 even when Medium answered 404, and the relayed body is what the browser
    // acts on.
    tracing::debug!(
        status = media.status,
        content_type = ?media.content_type,
        "iframe passthrough"
    );

    // `await request.text()` (`iframe.py:31`), which decodes with the response's
    // charset and substitutes on error — `Media::text` is the same reading.
    let patched = patch_iframe_content(&media.text());

    let mut response = (
        StatusCode::OK,
        // `media_type="text/html"`, which Starlette answers with a charset.
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        patched,
    )
        .into_response();

    for (name, value) in IFRAME_HEADERS {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            header::HeaderValue::from_static(value),
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headers_are_the_legacys_two() {
        assert_eq!(IFRAME_HEADERS.len(), 2);
        assert_eq!(
            IFRAME_HEADERS[0],
            ("access-control-allow-origin", "*"),
            "iframe.py:13 — the embed is cross-origin, so this is what makes it load"
        );
        assert_eq!(IFRAME_HEADERS[1], ("x-frame-options", "SAMEORIGIN"));
    }
}
