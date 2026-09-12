//! `ETag`, `If-None-Match` and `Cache-Control` for the API's representations.
//!
//! # The tag is a hash of the response bytes, and that is the whole design
//!
//! Not of the `Document`, not of the DTO, and **never** of
//! `page_canonical`. The first two would be a second implementation of the
//! serialiser that has to stay in step with the real one; the third would be a
//! correctness bug rather than a shortcut — that canonicaliser is `html5ever`
//! plus a set of normalisations whose entire job is to make two *different*
//! inputs compare equal, so two genuinely different articles can canonicalise
//! the same and this API would answer `304` for an article that changed.
//!
//! # Weak, deliberately
//!
//! `CompressionLayer` wraps the whole application, so the same URL legitimately
//! produces different bytes under different `Accept-Encoding`. A strong
//! validator would be a claim we cannot keep. `Vary: Accept-Encoding` is sent
//! alongside it, and there is no `Vary: Accept`: each URL has exactly one
//! representation, so there is nothing to negotiate.
//!
//! # `If-None-Match: *` matches anything
//!
//! RFC 9110 §13.1.2: the wildcard asks "does any representation exist", and for a
//! URL that answered at all the answer is yes. `If-Match` and `If-Range` are out
//! of scope — nothing here is a write, and nothing serves ranges.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// The header a representation without a JSON root carries its schema version
/// in.
///
/// `/html` and `/markdown` have no place to put `schema_version` — one is a
/// fragment of a document, the other a fragment of a file — so the version moves
/// to a header rather than being dropped. A consumer that pins the version can
/// pin it for every representation it reads.
pub const SCHEMA_VERSION_HEADER: &str = "x-freedium-schema-version";

/// The API's schema version, in the same place `freedium-dto` keeps it.
const SCHEMA_VERSION: u8 = freedium_dto::SCHEMA_VERSION;

/// `application/json`, for the DTO roots.
pub const JSON: &str = "application/json";

/// `text/html; charset=utf-8`, matching what `handlers::html` sends for a page.
pub const HTML: &str = "text/html; charset=utf-8";

/// `text/markdown; charset=utf-8`.
///
/// `text/markdown` (RFC 7763) rather than `text/plain`: the fragment is a
/// markdown document, and a consumer deciding how to render it should not have
/// to guess.
pub const MARKDOWN: &str = "text/markdown; charset=utf-8";

/// The validator for a body: `W/"<sha256 hex>"`.
#[must_use]
pub fn etag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut tag = String::with_capacity(2 + 64 + 1);
    tag.push_str("W/\"");
    for byte in digest {
        write!(tag, "{byte:02x}").expect("writing to a String cannot fail");
    }
    tag.push('"');
    tag
}

/// Whether a caller's `If-None-Match` covers this representation.
///
/// The comparison is the **weak** one: the `W/` prefix is stripped from both
/// sides before the opaque tags are compared, because a weak validator is what
/// this API issues and RFC 9110 §13.1.2 says a recipient uses weak comparison for
/// `If-None-Match` regardless of how the tag was issued.
#[must_use]
pub fn matches(request_headers: &HeaderMap, tag: &str) -> bool {
    let Some(offered) = request_headers.get(header::IF_NONE_MATCH) else {
        return false;
    };
    let Ok(offered) = offered.to_str() else {
        return false;
    };

    let ours = weak_part(tag);
    offered
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || weak_part(candidate) == ours)
}

/// The opaque tag with any `W/` prefix removed.
fn weak_part(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag).trim()
}

/// Builds a validated representation.
///
/// `bytes` is the body as it will be sent, which is why every handler serialises
/// before calling this: the tag has to be over the bytes the client receives, not
/// over the value that produced them.
#[must_use]
pub fn respond(
    request_headers: &HeaderMap,
    bytes: Vec<u8>,
    content_type: &'static str,
    cache_control: &str,
) -> Response {
    let tag = etag(&bytes);

    if matches(request_headers, &tag) {
        // The 304 carries the validator and the freshness the 200 would have
        // carried, and no body — RFC 9110 §15.4.5.
        return builder(StatusCode::NOT_MODIFIED, tag, cache_control)
            .body(Body::empty())
            .expect("a 304 with known headers always builds");
    }

    builder(StatusCode::OK, tag, cache_control)
        .header(header::CONTENT_TYPE, content_type)
        .header(SCHEMA_VERSION_HEADER, SCHEMA_VERSION.to_string())
        .body(Body::from(bytes))
        .expect("a 200 with known headers always builds")
}

fn builder(status: StatusCode, tag: String, cache_control: &str) -> axum::http::response::Builder {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, cache_control)
        // The response varies by encoding, not by `Accept`: one representation
        // per URL.
        .header(header::VARY, "Accept-Encoding");

    if let Ok(value) = HeaderValue::from_str(&tag) {
        builder = builder.header(HeaderName::from_static("etag"), value);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    /// The shape RFC 9110 §8.8.3 gives a weak entity tag: `W/` then a quoted
    /// opaque string. Pinned as a string because a consumer's parser sees only
    /// the string.
    #[test]
    fn a_tag_is_weak_and_quoted_hex() {
        let tag = etag(b"a body");
        assert!(tag.starts_with("W/\""), "{tag}");
        assert!(tag.ends_with('"'), "{tag}");
        let hex = &tag[3..tag.len() - 1];
        assert_eq!(hex.len(), 64, "sha256 is 32 bytes");
        assert!(hex.chars().all(|ch| ch.is_ascii_hexdigit()), "{tag}");
    }

    /// **Two representations of the same post get two tags.** This is the guard
    /// against "optimising" the validator into a hash of the Postgres row: the
    /// JSON, the metadata and the fragments are different documents, and a
    /// consumer caching one must not be handed a `304` for another.
    #[test]
    fn the_etag_differs_between_representations() {
        let json = etag(br#"{"schema_version":1}"#);
        let meta = etag(br#"{"schema_version":1,"title":"x"}"#);
        let html = etag(b"<p>hello</p>");
        let markdown = etag(b"hello");

        let all = [json, meta, html, markdown];
        for (index, left) in all.iter().enumerate() {
            for right in all.iter().skip(index + 1) {
                assert_ne!(left, right, "two representations share a tag");
            }
        }
    }

    #[test]
    fn the_same_bytes_are_the_same_tag() {
        assert_eq!(etag(b"body"), etag(b"body"));
        assert_ne!(etag(b"body"), etag(b"body "));
    }

    /// Weak comparison, both directions: a client that echoes our weak tag and a
    /// client that sends the same opaque tag strongly both hit.
    #[test]
    fn if_none_match_compares_weakly() {
        let tag = etag(b"a body");
        let opaque = weak_part(&tag).to_string();

        for value in [tag.clone(), opaque.clone(), format!("W/{opaque}")] {
            assert!(
                matches(&headers(&[("if-none-match", &value)]), &tag),
                "{value}"
            );
        }

        // A list, which is what a browser sends after two representations of the
        // same URL have been seen.
        assert!(matches(
            &headers(&[("if-none-match", &format!("\"other\", {tag}"))]),
            &tag
        ));
        // And the wildcard.
        assert!(matches(&headers(&[("if-none-match", "*")]), &tag));

        for value in ["\"other\"", "W/\"other\"", "", "garbage"] {
            assert!(
                !matches(&headers(&[("if-none-match", value)]), &tag),
                "{value}"
            );
        }
        assert!(
            !matches(&HeaderMap::new(), &tag),
            "no header is not a match"
        );
    }

    /// A matching validator is a `304` with the freshness of the `200` and no
    /// body, and it carries no content type: there is no content.
    #[tokio::test]
    async fn a_matching_validator_is_a_bodyless_304() {
        let tag = etag(b"a body");
        let response = respond(
            &headers(&[("if-none-match", &tag)]),
            b"a body".to_vec(),
            JSON,
            "public, max-age=300",
        );

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert!(response.headers().get(header::CONTENT_TYPE).is_none());
        assert_eq!(
            response
                .headers()
                .get("etag")
                .map(|value| value.to_str().unwrap()),
            Some(tag.as_str())
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .map(|value| value.to_str().unwrap()),
            Some("public, max-age=300")
        );
        assert_eq!(
            response
                .headers()
                .get(header::VARY)
                .map(|value| value.to_str().unwrap()),
            Some("Accept-Encoding")
        );

        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(bytes.is_empty());
    }

    /// A fresh request gets the bytes, the content type and the schema version.
    /// The header is the only place a fragment can carry the version.
    #[tokio::test]
    async fn a_fresh_request_gets_the_body_and_the_schema_version() {
        let response = respond(&HeaderMap::new(), b"<p>hi</p>".to_vec(), HTML, "no-store");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .map(|value| value.to_str().unwrap()),
            Some(HTML)
        );
        assert_eq!(
            response
                .headers()
                .get(SCHEMA_VERSION_HEADER)
                .map(|value| value.to_str().unwrap()),
            Some("1")
        );
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"<p>hi</p>");
    }
}
