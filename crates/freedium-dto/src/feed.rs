//! The feed contract.

use serde::{Deserialize, Serialize};

use crate::post::MetaDto;

/// A page of posts, without bodies.
///
/// # The order is stable but arbitrary, and that is a decision rather than an
/// oversight
///
/// `cursor` is the opaque key of the last post returned, and rows come back in
/// key order — a 12-hex FNV-1a hash of the post id (`freedium-cache::keys`), not
/// a date. So the feed **is** genuinely paginable, but it is **not**
/// chronological, and no amount of it will look like a timeline.
///
/// Both alternatives are closed off rather than merely unchosen. A random sample
/// has no stable order, so `cursor` could not mean anything there. A
/// chronological feed needs an `updated_at` column, and the cache table has
/// exactly two columns — `key` and `value`, with the payload as a JSON blob —
/// which the plan's no-migrations rule keeps it at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FeedDto {
    pub schema_version: u8,
    pub posts: Vec<MetaDto>,
    /// Pass back as `?cursor=`. `None` at the end of the feed, which is the
    /// signal to stop — an empty `posts` with a `Some` cursor would mean "keep
    /// asking", and that distinction is the whole reason this is an `Option`.
    pub next_cursor: Option<String>,
}

impl FeedDto {
    #[must_use]
    pub fn new(posts: Vec<MetaDto>, next_cursor: Option<String>) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            posts,
            next_cursor,
        }
    }
}
