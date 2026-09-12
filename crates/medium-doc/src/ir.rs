//! The document IR: the single representation every output format is rendered
//! from.
//!
//! `Document` is what `RUST_REWRITE_PLAN.md` §2.1 calls the core of the
//! rewrite. The Python pipeline renders HTML twice and splices the results into
//! one string while maintaining a position matrix (`rl_string_helper`); here
//! the structure is built once and rendered once. Nothing in this module
//! mutates a string.
//!
//! Two deliberate departures from the §2.1 sketch, both because the legacy
//! renderer's output depends on facts that are not recoverable from the block
//! list alone:
//!
//! - [`HeadingSpacing`] and [`ParagraphMargin`] are *stored*, not recomputed.
//!   `core.py:415` reads `paragraphs[current_pos - 1]` to choose `mt-3` vs
//!   `mt-7`, which for the first paragraph wraps around to the **last** source
//!   paragraph. A renderer walking the IR cannot see that paragraph if it was
//!   skipped, so the decision has to be made where the source order is known.
//! - [`Block::Image`] carries its caption. The legacy code appends the
//!   `<figcaption>` as a separate list entry; concatenating them at render time
//!   produces the same bytes.

/// A parsed Medium post: metadata plus the ordered body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub meta: PostMeta,
    pub blocks: Vec<Block>,
}

/// Post-level metadata.
///
/// `title` and `subtitle` are the values *after* the parse-time de-duplication
/// pass, matching what `_parse_and_render_content_html_post` returns. Note that
/// the legacy `_render_as_html` then discards both and uses the raw ones from
/// `generate_metadata` instead (`core.py:758-784`) — a quirk that matters in
/// Fase 3, not here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PostMeta {
    pub post_id: String,
    pub title: String,
    pub subtitle: String,
    /// `previewImage.id`; the first `IMG` paragraph repeating it is dropped.
    pub preview_image_id: Option<String>,
    /// `tags[].displayTitle`, used to drop an `H4` that merely repeats a tag.
    pub tags: Vec<String>,
    pub reading_time_minutes: u32,
    pub is_locked: bool,
    /// `mediumUrl` as returned by the GraphQL API.
    pub medium_url: String,
    /// Kept as raw JSON until Fase 6 defines the public DTO (§2.7).
    pub creator: serde_json::Value,
    pub collection: serde_json::Value,
}

/// One body block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        /// 2, 3 or 4.
        level: u8,
        /// The Medium paragraph `name`, emitted as the `id` attribute.
        id: String,
        spacing: HeadingSpacing,
        inline: Vec<Inline>,
    },
    Paragraph {
        inline: Vec<Inline>,
        drop_cap: bool,
        margin: ParagraphMargin,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Inline>>,
    },
    Code {
        /// `codeBlockMetadata.lang`, `None` renders as `nohighlight`.
        lang: Option<String>,
        /// One entry per `PRE` paragraph. The legacy code escapes each
        /// paragraph on its own and only then joins them with `\n`
        /// (`core.py:502-513`), so the line split has to survive into the IR —
        /// joining earlier would move characters across an escaping boundary.
        lines: Vec<Vec<Inline>>,
    },
    BlockQuote {
        style: QuoteStyle,
        inline: Vec<Inline>,
    },
    Image {
        /// The Medium image id, interpolated into the `miro.medium.com` URL.
        id: String,
        alt: String,
        caption: Option<Vec<Inline>>,
    },
    /// A run of `OUTSET_ROW` / `OUTSET_ROW_CONTINUE` paragraphs.
    ImageRow(Vec<Block>),
    Embed {
        url: String,
        title: String,
        description: String,
        site: String,
        thumbnail_id: Option<String>,
    },
    /// `dims` is `None` for the no-dimensions fallback branch, which renders a
    /// structurally different element (`core.py:666-676`).
    Iframe {
        src: String,
        dims: Option<(u32, u32)>,
    },
}

/// Top padding for a heading.
///
/// The legacy rule is "add padding only if something was already emitted"
/// (`core.py:326`), with `pt-8` for `H4` and `pt-12` for `H2`/`H3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadingSpacing {
    None,
    Pt8,
    Pt12,
}

impl HeadingSpacing {
    pub const fn class(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Pt8 => "pt-8",
            Self::Pt12 => "pt-12",
        }
    }
}

/// Top margin for a paragraph, from the *source* type of the preceding
/// paragraph (`core.py:415`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParagraphMargin {
    Mt3,
    Mt7,
}

impl ParagraphMargin {
    /// `paragraphs[current_pos - 1]` is an `H3`/`H4`; for the first paragraph
    /// this is a wrap-around read of the last one.
    pub const fn from_previous_type(previous_type: &str) -> Self {
        match previous_type.as_bytes() {
            b"H3" | b"H4" => Self::Mt3,
            _ => Self::Mt7,
        }
    }
}

/// Which of the two blockquote shapes to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStyle {
    /// Medium `BQ` — the inset box-shadow bar.
    Inset,
    /// Medium `PQ` — the large pull quote.
    Pull,
}

/// Inline content. Children hold **raw** text: HTML escaping happens at render
/// time only, which is what removes the position bookkeeping the Python
/// pipeline needed (§2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Code(Vec<Inline>),
    Link {
        href: String,
        rel: String,
        title: String,
        /// `target="_blank"`, false for same-page `#` anchors.
        new_tab: bool,
        children: Vec<Inline>,
    },
    /// A Medium `@user` mention, which renders as a bare link with no `rel`,
    /// `title` or `target` attributes.
    UserMention {
        user_id: String,
        children: Vec<Inline>,
    },
    Highlight(Vec<Inline>),
}

impl Inline {
    /// True for nodes that carry no markup of their own.
    pub fn is_plain_text(&self) -> bool {
        matches!(self, Self::Text(_))
    }
}

impl Block {
    /// True for the block kinds that carry inline content and therefore need
    /// the paragraph-level escaping mode.
    pub fn inline(&self) -> Option<&[Inline]> {
        match self {
            Self::Heading { inline, .. }
            | Self::Paragraph { inline, .. }
            | Self::BlockQuote { inline, .. } => Some(inline),
            _ => None,
        }
    }
}
