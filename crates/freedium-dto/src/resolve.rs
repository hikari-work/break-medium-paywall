//! The URL-resolution contract.

use serde::{Deserialize, Serialize};

/// The id behind a Medium URL the caller was given.
///
/// # `resolved_url` is not the article's canonical URL
///
/// It is always `https://medium.com/p/{post_id}` — the id-only form, which is
/// valid for every post and requires no fetch. The article's *own* canonical URL
/// is `MetaDto::medium_url`, and it is only knowable after fetching the payload;
/// a client that wants it should call `/posts/{id}`.
///
/// The name says `resolved_url` and not `canonical_url` precisely so nobody reads
/// it the other way.
///
/// # What it deliberately does not do
///
/// The legacy `correct_url` applies a set of cleaning transformations, and what
/// exactly they should do is an unresolved open question in the plan (§8 item 3).
/// This endpoint must not silently pick a side: it resolves an id and hands back
/// a URL built from that id, and echoes nothing about the input URL's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResolveDto {
    pub schema_version: u8,
    /// The 12-hex post id the input resolved to.
    pub post_id: String,
    /// `https://medium.com/p/{post_id}`.
    pub resolved_url: String,
}

impl ResolveDto {
    #[must_use]
    pub fn new(post_id: impl Into<String>) -> Self {
        let post_id = post_id.into();
        Self {
            resolved_url: format!("https://medium.com/p/{post_id}"),
            schema_version: crate::SCHEMA_VERSION,
            post_id,
        }
    }
}
