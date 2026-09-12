//! A rendered post, packaged for storage.
//!
//! This is the payload that goes into Redis. It exists in `medium-render`
//! rather than in `freedium-cache` so that the cache crate holds no opinion
//! about rendering and this crate holds no opinion about Redis — the two only
//! meet at this type, which is plain data.
//!
//! It is the counterpart of `HtmlResult`
//! (`legacy/medium-parser/medium_parser/models/html_result.py:4-9`), with one
//! deliberate difference: the fourth field is `html`, where the Python dataclass
//! calls it `data`. The name `data` says nothing about what is in it, and
//! nothing reads the two side by side — the Python value is a pickle in an
//! unprefixed keyspace and this one is MessagePack under `v2:`.
//!
//! # The wire format is positional, and that is load-bearing
//!
//! `rmp_serde`'s default struct encoding is a MessagePack **array**, not a map:
//! field *names* never reach the wire, only their order and types. Two
//! consequences:
//!
//! - Renaming a field cannot break a cached entry; reordering one does, and
//!   silently. All four fields are `String`, so a swap would decode cleanly into
//!   the wrong fields and serve a post whose title was its HTML. Entry TTL is
//!   five hours (`config.CACHE_LIFE_TIME`), so it would also heal five hours
//!   later — a bug that appears and disappears on its own.
//! - A test on the JSON rendering would not catch it, because JSON uses names.
//!   The layout is pinned as bytes instead, in `freedium-cache`, which is where
//!   the encoding actually happens.
//!
//! Adding a field is safe for reads (an old entry is short and fails to
//! deserialize, giving a cache miss rather than a wrong answer) provided it goes
//! at the end.
//!
//! There are no tests here because there is no behaviour to test: the type is
//! four owned strings and two derives. Its one real contract — the byte layout —
//! is pinned in `freedium-cache`, next to the code that encodes and decodes it
//! and the only place `rmp_serde` is a dependency.

use serde::{Deserialize, Serialize};

/// A post rendered to HTML, ready to be cached and served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedPost {
    /// The article title, unescaped — the template escapes it.
    pub title: String,

    /// The meta description. Also the `content` of the description tag, so the
    /// template escapes it too.
    pub description: String,

    /// The canonical URL the post was requested by.
    ///
    /// Stored for parity with `HtmlResult` and, in the legacy server, never
    /// read: `handlers/post.py` passes `title`, `description` and the HTML to
    /// the template and drops this. Kept because it is cheap and a canonical
    /// URL is the sort of thing a `<link rel="canonical">` will eventually
    /// want — not because anything consumes it today.
    pub url: String,

    /// The **whole page**, not a fragment: `base.html` with `post.html` spliced
    /// into it, which is what `handlers/post.py:88-98` caches and serves.
    ///
    /// The article on its own is [`crate::page::render_post_body`]. Serving that
    /// out of this field would ship the page chrome with it.
    pub html: String,
}
