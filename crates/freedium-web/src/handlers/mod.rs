//! The route handlers, one module per legacy file.
//!
//! | here | `legacy/web/server/` |
//! |---|---|
//! | [`main`] | `handlers/main.py` — the catch-all dispatcher |
//! | [`post`] | `handlers/post.py` — `/` and the article pages |
//! | [`miro`] | `handlers/miro.py` — `/@miro/*` |
//! | [`iframe`] | `handlers/iframe.py` — `/render_iframe/*` |
//! | [`misc`] | `handlers/misc.py` — the two POST routes |
//!
//! Every handler takes `&AppState` and an optional [`Correlation`] rather than
//! reaching for globals, which is the one structural change from the legacy. The
//! correlation is optional only because [`crate::error::json_error`] does not
//! need one; a handler that renders a page always has it, because
//! [`crate::middleware::correlation`] put it in the request extensions.

use axum::response::IntoResponse;

pub mod iframe;
pub mod main;
pub mod miro;
pub mod misc;
pub mod post;

/// An HTML body with the content type Starlette's `HTMLResponse` sends.
///
/// A bare `String` implements `IntoResponse` as `text/plain; charset=utf-8`, so
/// `html.into_response()` compiles, returns 200, and hands the browser its own
/// markup as source text. Nothing about the status code, the length or the
/// bytes says so — the page is byte-identical to the right answer — which is how
/// the homepage answered `text/plain` through a whole session of green tests
/// and was caught by the first container that was actually curled.
///
/// `handlers/post.py:105` returns `HTMLResponse`, and everything that renders a
/// page here goes through this so that the content type is decided once.
pub fn html(body: impl Into<String>) -> axum::response::Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        body.into(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::html;

    /// The bug this function exists to prevent: `String`'s `IntoResponse` is
    /// `text/plain`, and it is one keystroke shorter than calling this.
    #[test]
    fn an_html_body_is_labelled_as_html() {
        let response = html("<p>a page</p>");
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .expect("html() sets a content type"),
            "text/html; charset=utf-8"
        );
    }
}
