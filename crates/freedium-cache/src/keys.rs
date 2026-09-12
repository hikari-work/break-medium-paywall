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
    format!("{NAMESPACE}:post:{post_id}")
}

/// The homepage's rendered post list, as MessagePack.
///
/// Legitimately derived from no arguments, so unlike the post key there is no
/// id to interpolate. Python's equivalent is the `aio_redis_cache` key built
/// from the function name (`legacy/web/server/utils/cache.py:18`), which is why
/// it is a constant here rather than a function.
pub const HOMEPAGE_KEY: &str = "v2:homepage";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_key_is_namespaced() {
        assert_eq!(post_key("515dd5a43948"), "v2:post:515dd5a43948");
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
