//! The post contract: [`PostDto`], its metadata, and the block tree.

use serde::{Deserialize, Serialize};

/// A whole post: metadata plus the ordered body.
///
/// The root of `/api/v1/posts/{id}`, which is the only representation that
/// carries both halves. `/meta` serves [`MetaDto`] alone and `/markdown` serves
/// the body alone, so a client that wants one does not pay for the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PostDto {
    pub schema_version: u8,
    pub meta: MetaDto,
    pub blocks: Vec<BlockDto>,
}

impl PostDto {
    #[must_use]
    pub fn new(meta: MetaDto, blocks: Vec<BlockDto>) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            meta,
            blocks,
        }
    }
}

/// Everything about a post except its body.
///
/// # Every string here is plain text
///
/// These values come from the **raw payload**, not from `medium-doc`'s
/// `PostMetadata` — whose fields are pre-escaped for HTML and whose
/// `description` is escaped twice by a legacy bug (`&#39;` becomes `&amp;#39;`).
/// A JSON consumer wants `it's`, not `it&#39;s`, and the double-escaped variant
/// is not something a client could unescape correctly without knowing which of
/// several passes produced it.
///
/// # `title` is the raw title, not the de-duplicated one
///
/// `Document::meta.title` has been through the parse-time pass that drops a
/// subtitle that merely repeats the title, and the legacy renderer then discards
/// it in favour of the raw value (`core.py:758-784`) — which is why the `dedup`
/// fixture renders a `<title>` that still contains the duplication. The DTO
/// takes the same source as the page, so the API and the page never disagree
/// about a post's title. The divergence worth knowing: this is also why the DTO
/// does **not** apply `metadata::quote_symbol` (curly → straight), so it carries
/// the author's real typography where the page's `<title>` does not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MetaDto {
    /// The contract version.
    ///
    /// It is here because `/api/v1/posts/{id}/meta` serves this type as its
    /// **root**, and the rule is that a client can read the version off any JSON
    /// body without knowing which endpoint answered. So the same struct is a
    /// root on one route and nested on two others ([`PostDto::meta`] and
    /// [`crate::FeedDto::posts`]), where the field repeats.
    ///
    /// The repetition is deliberate and cannot lie: one process serialises the
    /// whole tree in one pass, so a nested copy disagreeing with its root would
    /// mean two different binaries produced one response. The alternative — a
    /// second wrapper type that exists only to hold a version — buys a few bytes
    /// in a feed at the cost of a permanent extra concept.
    pub schema_version: u8,

    /// The post id, always supplied by the caller.
    ///
    /// `PostMeta::post_id` is empty by construction (`parse.rs` leaves it to the
    /// caller, pinned by `the_post_id_is_left_to_the_caller`) and Medium's `Post`
    /// has no id field at all — the GraphQL query does not select one. So this
    /// comes from the URL path or from the resolver, exactly as the cache keys do.
    pub post_id: String,

    pub title: String,

    /// `preview_content.subtitle`, `None` when the payload has no subtitle.
    ///
    /// `Some("")` and `None` are different here: the first means Medium sent an
    /// empty subtitle, the second that it sent none. Collapsing them would throw
    /// away the only signal.
    pub subtitle: Option<String>,

    /// The short summary as plain text.
    pub description: String,

    /// The `resize:fit:700` image URL, or `None`.
    pub preview_image_url: Option<String>,

    pub reading_time_minutes: u32,

    /// `is_locked`, straight from the payload — **not** a `free_access: bool`.
    ///
    /// The legacy's `PostMetadata::free_access` is the string `"Yes"`/`"No"`, and
    /// the template reads it under that inverted name. Mirroring that shape into
    /// a public contract would hand every consumer a negated boolean to get
    /// backwards; the payload's own field is already the right polarity.
    pub is_locked: bool,

    /// `mediumUrl` — the canonical URL on medium.com.
    ///
    /// The one absolute URL that belongs in the contract, because it is the
    /// article's own identity rather than a property of this deployment.
    pub medium_url: Option<String>,

    /// Milliseconds since the Unix epoch, in the field name because a bare `i64`
    /// timestamp is a two-year support tail of "seconds or milliseconds?".
    pub first_published_at_unix_ms: Option<i64>,
    pub updated_at_unix_ms: Option<i64>,

    pub tags: Vec<TagDto>,

    /// `None` when the payload carries no creator, or one with no `id`.
    pub creator: Option<CreatorDto>,
    pub collection: Option<CollectionDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TagDto {
    pub display_title: String,
    /// `normalizedTagSlug`; `None` when the payload omits it.
    pub slug: Option<String>,
}

/// The author.
///
/// `image_url` is here because a client cannot construct it — the `miro.medium.com`
/// resize parameters are not derivable from an image id. The author's *profile*
/// URL is deliberately absent for the opposite reason: it is
/// `https://medium.com/@{username}`, which a client can build, and freezing it
/// into a versioned contract buys nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreatorDto {
    pub id: String,
    /// Empty string when the payload omits it — never `None`. A missing name and
    /// an empty name are the same thing to every consumer, and a field that is
    /// sometimes absent for one reason only adds a branch.
    pub name: String,
    pub username: String,
    pub bio: String,
    pub image_url: Option<String>,
}

/// A Medium publication.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CollectionDto {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub avatar_url: Option<String>,
}

/// One body block.
///
/// The tags are **Freedium's vocabulary**, not Medium's. Medium calls these
/// paragraphs `"H2"`, `"P"`, `"PRE"`, `"BQ"` and so on; those are the names of
/// its own storage format, they are not a stable interface, and a consumer that
/// switches on them inherits every rename Medium makes. `imageRow` is camelCase
/// while the field names below are snake_case — deliberate and consistent
/// throughout: a tag is a name, a key is a key, and `schema_version` fixes the
/// keys as snake_case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum BlockDto {
    Heading {
        /// 2, 3 or 4. Medium's `H2`/`H3`/`H4`, which is the one place its
        /// numbering survives — as a number, not as a tag name.
        level: u8,
        /// The anchor `id` the HTML renderer emits, so a client can build links.
        id: String,
        /// The heading's text with all inline markup flattened.
        ///
        /// The one derived field in the contract, and it is here for a named
        /// consumer: a table of contents is exactly `{id, text}`, and the
        /// alternative is every consumer walking `content` to flatten it.
        text: String,
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },
    Paragraph {
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
        /// The legacy `first-letter:` drop-cap utilities.
        drop_cap: bool,
    },
    List {
        ordered: bool,
        /// One entry per `<li>`. The IR's lists are flat, so there is no
        /// nesting to represent.
        items: Vec<Vec<InlineDto>>,
    },
    Code {
        /// `codeBlockMetadata.lang`; `None` renders as `nohighlight`.
        language: Option<String>,
        /// The IR holds `Vec<Vec<Inline>>`; this flattens it, because a `PRE`
        /// block's content is plain text. If a markup ever appeared there the
        /// HTML renderer would emit it and this would not, so the mapping logs
        /// once per dropped markup rather than losing it silently.
        lines: Vec<String>,
    },
    Blockquote {
        style: QuoteStyleDto,
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },
    Image {
        /// The full `resize:fit:700` URL.
        url: String,
        alt: String,
        caption: Option<Vec<InlineDto>>,
    },
    /// A run of `OUTSET_ROW` paragraphs.
    ImageRow { images: Vec<ImageDto> },
    Embed {
        url: String,
        title: String,
        description: String,
        site: String,
        /// The `resize:fit:320` thumbnail — a **different** size from
        /// [`BlockDto::Image`], because that is what the embed element uses.
        thumbnail_url: Option<String>,
    },
    Iframe {
        src: String,
        /// `None` for the no-dimensions fallback branch, which the HTML renderer
        /// emits as `100%` placeholders. Kept as `None` rather than `100` so a
        /// consumer can tell a real dimension from a placeholder.
        width: Option<u32>,
        height: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ImageDto {
    pub url: String,
    pub alt: String,
}

/// Which of the two `blockquote` elements the HTML renderer emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum QuoteStyleDto {
    /// `core.py:521` — an inset box shadow.
    Inset,
    /// `core.py:528` — larger, muted, indented.
    Pull,
}

/// One node of inline content.
///
/// A nested tree rather than the legacy's flat markup list, because the legacy's
/// flat list has to be spliced into an already-escaped string through a position
/// matrix and emits *several adjacent elements* when two markups overlap — see
/// `medium-render`'s `html` module. A JSON consumer has no reason to inherit
/// that; the tree says what it means.
///
/// # The schema refers to itself, and it has to
///
/// Every `content` field below carries `#[schema(no_recursion)]`, which makes
/// `utoipa` emit a `$ref` to `InlineDto` instead of expanding it. Without it the
/// `ToSchema` derive expands `InlineDto` inside `InlineDto` inside `InlineDto`,
/// and the process dies of a stack overflow while *building the schema* — not at
/// run time, and not with a compile error a reader could act on. The same
/// attribute is on `BlockDto`'s `content` fields, where the expansion is merely
/// wasteful: a copy of a definition that is about to `$ref` itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum InlineDto {
    /// Raw text — **not** HTML-escaped.
    Text { text: String },

    Strong {
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },

    /// Renamed from the variant name: the tag is the HTML element, matching
    /// [`InlineDto::Strong`]'s relationship to `<strong>`.
    #[serde(rename = "em")]
    Emphasis {
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },

    Code {
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },

    Link {
        href: String,
        /// Empty when the payload carried none. Not `Option`, because the HTML
        /// renderer emits the attribute either way.
        rel: String,
        title: String,
        /// `target="_blank"`; false for same-page `#` anchors.
        new_tab: bool,
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },

    /// A Medium `@user` mention, which renders as a bare link with no `rel`,
    /// `title` or `target`. The href is `https://medium.com/u/{user_id}`, so it
    /// is not carried separately — it is the only URL this can be.
    UserMention {
        user_id: String,
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },

    Highlight {
        // `$ref` rather than an inlined copy of `InlineDto`. Inside `InlineDto`
        // itself the inline is infinite — the derive recurses until the test
        // binary overflows its stack — and outside it the inline duplicates a
        // definition that is about to refer to itself anyway. See the note on
        // the enum.
        #[cfg_attr(feature = "openapi", schema(no_recursion))]
        content: Vec<InlineDto>,
    },
}
