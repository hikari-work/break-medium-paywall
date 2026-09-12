//! Redis key naming.
//!
//! # Why a new namespace, and not the old keys
//!
//! Python stores `pickle.dumps(HtmlResult)` under the bare post id
//! (`legacy/web/server/handlers/post.py`, via `aio_redis_cache`). Rust cannot
//! read a Python pickle, so the two implementations cannot share those keys.
//! §2.4 takes the cheap way out: a `v2:` prefix, and the old keys are simply
//! left to expire (TTL is 5 hours — `CACHE_LIFE_TIME` in
//! `legacy/web/server/config.py:23`).
//!
//! The point of that is not tidiness. It is that Python and Rust can serve
//! traffic **at the same time** against one Redis without either one reading
//! the other's half-understood values. That property is what Fase 4's shadow
//! traffic (`RUST_REWRITE_PLAN.md` §5) is built on, and it would be lost by
//! reusing the bare keys.

/// Every key this crate writes starts with this. Nothing that does not is
/// ours, and nothing that does belongs to the Python instance.
pub const NAMESPACE: &str = "v2";

/// The rendered HTML for one post, as MessagePack.
///
/// Replaces the bare `{post_id}` key. Note the legacy key had no prefix at all,
/// so this cannot collide with it.
pub fn post_key(post_id: &str) -> String {
    format!("{POST_PREFIX}{post_id}")
}

/// The homepage's rendered post list, as MessagePack.
///
/// Legitimately derived from no arguments, so unlike the post key there is no
/// id to interpolate. Python's equivalent is the `aio_redis_cache` key built
/// from the function name (`legacy/web/server/utils/cache.py:18`), which is why
/// it is a constant here rather than a function.
pub const HOMEPAGE_KEY: &str = "v2:homepage";

/// The prefix every [`post_key`] shares, so a caller can page over the post rows
/// of a table that holds other keys too.
pub const POST_PREFIX: &str = "v2:post:";

/// The post id inside a [`post_key`], or `None` for a key that is not one.
///
/// The inverse of [`post_key`], and it lives here for the same reason: the prefix
/// is this module's business. `/api/v1/feed` walks the `cache` table in key order
/// and needs the id back to serve a row's metadata, and it must not be the module
/// that knows the namespace is spelled `v2:post:`.
///
/// An empty id (`"v2:post:"` exactly) is `None`: `post_key("")` is not a post.
/// The id keeps everything after the prefix, including any `:` in it — see
/// `post_key_keeps_the_id_at_the_end`.
pub fn post_id_from_key(key: &str) -> Option<&str> {
    key.strip_prefix(POST_PREFIX).filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_key_is_namespaced() {
        assert_eq!(post_key("515dd5a43948"), "v2:post:515dd5a43948");
        assert_eq!(POST_PREFIX, "v2:post:", "the prefix is spelled once, here");
    }

    /// The feed reads a row's id back out of its key, so the two must be exact
    /// inverses — including for an id that contains the separator.
    #[test]
    fn the_id_round_trips_out_of_the_key() {
        for id in ["515dd5a43948", "a:b", "a"] {
            assert_eq!(post_id_from_key(&post_key(id)), Some(id));
        }
        assert_eq!(
            post_id_from_key(&post_key("")),
            None,
            "an empty id is no post"
        );
        assert_eq!(post_id_from_key(HOMEPAGE_KEY), None);
        assert_eq!(post_id_from_key(""), None);
    }

    /// The whole point of §2.4: a Rust write must not be visible to a Python
    /// read of the same post. Python's key is the bare id.
    #[test]
    fn post_key_does_not_collide_with_the_legacy_key() {
        let id = "515dd5a43948";
        assert_ne!(post_key(id), id);
        assert!(post_key(id).starts_with(NAMESPACE));
    }

    #[test]
    fn homepage_key_is_namespaced() {
        assert!(HOMEPAGE_KEY.starts_with(NAMESPACE));
    }

    /// `post_key` interpolates, so a post id that itself contains the separator
    /// must not be able to impersonate another post's key or the homepage's.
    #[test]
    fn post_key_keeps_the_id_at_the_end() {
        assert_eq!(post_key("a:b"), "v2:post:a:b");
        assert_ne!(post_key("a:b"), post_key("a"));
    }
}
