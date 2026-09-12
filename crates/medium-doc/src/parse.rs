//! A Medium GraphQL post payload → a [`Document`].
//!
//! Ports `_parse_and_render_content_html_post` (`core.py:213-683`). The legacy
//! function built a `list[str]` of HTML fragments; this builds the same
//! structure as data and lets `medium-render` turn it into those strings. Every
//! branching decision is preserved, including the ones that look like mistakes:
//! each is named in a comment beside the code, because the differential harness
//! compares against the legacy output and an "obvious fix" here would show up as
//! a diff rather than as an improvement.
//!
//! The quirks that are load-bearing, and the tests that pin them:
//!
//! | Quirk | Test |
//! |---|---|
//! | De-duplication covers only the first four paragraphs | [`the_dedup_window_is_the_first_four_paragraphs`] |
//! | The first paragraph's margin reads the **last** paragraph's type | [`the_first_paragraph_margin_reads_the_last_paragraph`] |
//! | Only the first *emitted* heading is unpadded | [`only_the_first_emitted_heading_is_unpadded`] |
//! | A highlight is dropped unless its text equals the *rendered* paragraph | [`a_highlight_is_dropped_when_escaping_changed_the_text`] |
//! | `IMG` `FULL_WIDTH` is not implemented and is dropped | [`a_full_width_image_is_dropped`] |
//! | `MIXTAPE_EMBED` needs exactly three markups, sliced by code point | [`a_mixtape_embed_splits_title_and_description_from_the_markups`] |
//! | A paragraph with no `text` also loses its markups | [`a_paragraph_without_text_keeps_none_of_its_markups`] |
//!
//! ## Tolerating the payload (§3.3 of the rewrite plan)
//!
//! The GraphQL response is shaped by Medium, not by us, so this module reads it
//! defensively: a field that is absent or null becomes its default, a paragraph
//! whose `type` is unrecognised is warned about and skipped (`core.py:678`), and
//! a `post` that fails to deserialise yields an empty document. An article that
//! renders with one block missing beats a 500.
//!
//! Unknown *fields* are dropped **silently** rather than warned about. The
//! query deliberately over-fetches — `ParagraphData` carries `id`, `href`,
//! `dropCapImage`, `validatedShareKey` and more that the renderer has no use for
//! — so a warning per unmodelled field would be noise on every article and would
//! drown the two warnings that do signal lost output: an unknown paragraph type
//! and an unknown markup type.

use serde::Deserialize;
use serde::de::Deserializer;
use serde_json::Value;
use tracing::{debug, warn};

use crate::difflib::is_match_over_80;
use crate::escape::mode_for;
use crate::inline::{Markup, MarkupKind, build_inlines};
use crate::inline_html::render_to_string;
use crate::ir::{Block, Document, HeadingSpacing, Inline, ParagraphMargin, PostMeta, QuoteStyle};
use crate::resolve;

/// How many leading paragraphs are checked against the title, the subtitle and
/// the tag list — `if current_pos in range(4)` (`core.py:259`).
///
/// A paragraph repeating the title further down the article survives, which is
/// presumably unintended, but it is what the parity gate compares against.
pub const DEDUP_WINDOW: usize = 4;

/// `post_data` — the whole GraphQL envelope.
///
/// `data` is kept as raw JSON rather than as a typed struct because it is two
/// things at once: `data.post`, and a flat map of media resources that an
/// `IFRAME`'s `__ref` points into (`core.py:617-622`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PostPayload {
    #[serde(default)]
    pub data: Value,
}

impl PostPayload {
    /// Parses an envelope from a JSON value.
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    /// `post_data["data"]["post"]`, or an empty post when the envelope is not
    /// shaped the way the API promises.
    pub fn post(&self) -> Post {
        let Some(post) = self.data.get("post") else {
            warn!("the payload has no `data.post`; parsing an empty document");
            return Post::default();
        };
        match serde_json::from_value(post.clone()) {
            Ok(post) => post,
            Err(error) => {
                warn!(%error, "`data.post` did not deserialise; parsing an empty document");
                Post::default()
            }
        }
    }

    /// `post_data["data"][reference]`, where a media resource `__ref` lands.
    fn media_resource(&self, reference: &str) -> Option<MediaResource> {
        let found = self.data.get(reference)?;
        match serde_json::from_value(found.clone()) {
            Ok(resource) => Some(resource),
            Err(error) => {
                warn!(%error, reference, "the referenced media resource is malformed");
                None
            }
        }
    }
}

/// A string field Medium sometimes sends as `null`.
///
/// `#[serde(default)]` alone covers a *missing* key but not an explicit `null`,
/// which would fail the whole `Post`. These helpers collapse the two cases, so a
/// null title costs the title rather than the article.
fn nullable_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// See [`nullable_string`].
fn nullable_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or_default())
}

/// A dimension, as Medium's schema types it (`Int`).
///
/// Read through [`Value::as_u64`] so that a payload sending a float or a string
/// loses the dimension rather than the article (§3.3). A whole-number float is
/// therefore treated as absent, where Python would have rendered `640.0`; the
/// schema says this cannot happen, and degrading is the documented preference.
fn nullable_dimension<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| value.as_u64()).map(|n| n as u32))
}

/// `post_data["data"]["post"]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Post {
    /// Null for a post the viewer cannot read, which is why it is an `Option`
    /// rather than a defaulted struct.
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default, deserialize_with = "nullable_string")]
    pub title: String,
    #[serde(default)]
    pub preview_content: Option<PreviewContent>,
    #[serde(default)]
    pub preview_image: Option<Image>,
    #[serde(default)]
    pub highlights: Vec<Highlight>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    /// The post's canonical Medium URL.
    #[serde(default, deserialize_with = "nullable_string")]
    pub medium_url: String,
    /// `math.ceil` is applied later (`core.py:711`), so this arrives fractional.
    #[serde(default)]
    pub reading_time: Option<f64>,
    #[serde(default, deserialize_with = "nullable_bool")]
    pub is_locked: bool,
    /// Kept as raw JSON until Fase 6 defines the public DTO (§2.7).
    #[serde(default)]
    pub creator: Value,
    #[serde(default)]
    pub collection: Value,
}

/// `post["content"]`. The whole body hangs off `bodyModel.paragraphs`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Content {
    #[serde(default, rename = "bodyModel")]
    pub body_model: BodyModel,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BodyModel {
    #[serde(default)]
    pub paragraphs: Vec<Paragraph>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreviewContent {
    #[serde(default, deserialize_with = "nullable_string")]
    pub subtitle: String,
}

/// An `IMG` paragraph's `metadata`, and the post's `previewImage`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Image {
    #[serde(default, deserialize_with = "nullable_string")]
    pub id: String,
    /// Rendered through Jinja, which tells a missing key from a null one; see
    /// [`Alt`].
    #[serde(default)]
    pub alt: Alt,
}

/// `{{ paragraph.metadata.alt }}` as Jinja renders it.
///
/// The legacy template runs under Jinja's default undefined, not
/// `StrictUndefined`, so a *missing* attribute renders as the empty string while
/// an explicit `null` renders as the literal text `None`. `Option<String>`
/// cannot tell those apart — serde maps both to `None` — so the three cases get
/// a type of their own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Alt {
    /// No `metadata` object, or no `alt` key in it. Jinja: `""`.
    #[default]
    Undefined,
    /// `"alt": null`. Jinja: `"None"`.
    Null,
    /// A real string.
    Text(String),
}

impl<'de> Deserialize<'de> for Alt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(deserializer)? {
            Some(text) => Self::Text(text),
            None => Self::Null,
        })
    }
}

impl std::fmt::Display for Alt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // A missing attribute is the empty string. The renderer interpolates
            // this without escaping, exactly as the legacy template does.
            Self::Undefined => Ok(()),
            Self::Null => f.write_str("None"),
            Self::Text(text) => f.write_str(text),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    #[serde(default, deserialize_with = "nullable_string")]
    pub display_title: String,
}

/// `post["highlights"][n]` — a reader's quote (`QuoteData` in the query).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Highlight {
    #[serde(default)]
    pub paragraphs: Vec<HighlightParagraph>,
    #[serde(default)]
    pub start_offset: usize,
    #[serde(default)]
    pub end_offset: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HighlightParagraph {
    #[serde(default, deserialize_with = "nullable_string")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub text: String,
}

/// One entry of `bodyModel.paragraphs`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paragraph {
    #[serde(rename = "type", default, deserialize_with = "nullable_string")]
    pub kind: String,
    /// `Option` because `core.py:298` tests for `None` before parsing: a
    /// paragraph with no text is parsed as empty, and its markups are dropped
    /// along with it.
    #[serde(default)]
    pub text: Option<String>,
    /// **Payload order**, which breaks ties between markups covering the same
    /// range; see [`crate::inline::build_inlines`].
    #[serde(default)]
    pub markups: Vec<RawMarkup>,
    /// The Medium paragraph name, emitted as the heading's `id` and matched
    /// against a highlight paragraph.
    #[serde(default, deserialize_with = "nullable_string")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable_string")]
    pub layout: String,
    #[serde(default)]
    pub metadata: Option<Image>,
    #[serde(default, deserialize_with = "nullable_bool")]
    pub has_drop_cap: bool,
    #[serde(default)]
    pub code_block_metadata: Option<CodeBlockMetadata>,
    #[serde(default)]
    pub mixtape_metadata: Option<MixtapeMetadata>,
    #[serde(default)]
    pub iframe: Option<IframeFields>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodeBlockMetadata {
    #[serde(default)]
    pub lang: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MixtapeMetadata {
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub thumbnail_image_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IframeFields {
    #[serde(default)]
    pub media_resource: Option<MediaResource>,
    #[serde(default, deserialize_with = "nullable_dimension")]
    pub iframe_width: Option<u32>,
    #[serde(default, deserialize_with = "nullable_dimension")]
    pub iframe_height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaResource {
    /// An indirection into `data`, used when the resource is shared between
    /// posts (`core.py:614-622`).
    #[serde(rename = "__ref", default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub iframe_src: Option<String>,
    #[serde(default, deserialize_with = "nullable_dimension")]
    pub iframe_width: Option<u32>,
    #[serde(default, deserialize_with = "nullable_dimension")]
    pub iframe_height: Option<u32>,
}

/// A markup exactly as the payload spells it.
///
/// Every field is optional because the legacy code reads them with `.get`, and
/// because a missing key is not worth failing an article over.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawMarkup {
    #[serde(rename = "type", default, deserialize_with = "nullable_string")]
    pub kind: String,
    #[serde(default)]
    pub start: usize,
    #[serde(default)]
    pub end: usize,
    #[serde(default)]
    pub anchor_type: Option<String>,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub rel: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

/// Where an `IFRAME`'s source and dimensions ended up, after `__ref`s are
/// followed.
struct ResolvedIframe {
    src: String,
    width: Option<u32>,
    height: Option<u32>,
}

/// Parses a post payload into a [`Document`].
///
/// `host_address` is the deployment's own origin, used to build the
/// `render_iframe/{id}` fallback (`core.py:632`).
///
/// The post id and the `mediumUrl`-derived fields are left to Fase 3's page
/// assembly: this returns only the content fragments plus the metadata the
/// de-duplication pass rewrites.
pub fn parse(payload: &PostPayload, host_address: &str) -> Document {
    Parser {
        payload,
        post: payload.post(),
        host_address,
    }
    .run()
}

/// The paragraph loop's state.
///
/// A struct rather than a pile of free functions because almost every helper
/// needs the post (for highlights), the payload (for media resources) or the
/// host address, and threading all three through every call obscures the port.
struct Parser<'a> {
    payload: &'a PostPayload,
    post: Post,
    host_address: &'a str,
}

impl Parser<'_> {
    fn run(mut self) -> Document {
        // `core.py:223`: `paragraphs = content["bodyModel"]["paragraphs"]`.
        let paragraphs = self
            .post
            .content
            .take()
            .unwrap_or_default()
            .body_model
            .paragraphs;

        // `title` and `subtitle` are both inputs and outputs: the de-duplication
        // pass can replace either with the paragraph that duplicated it
        // (`core.py:261-288`).
        let mut title = self.post.title.clone();
        let mut subtitle = self
            .post
            .preview_content
            .as_ref()
            .map_or_else(String::new, |preview| preview.subtitle.clone());
        let tags: Vec<String> = self
            .post
            .tags
            .iter()
            .map(|tag| tag.display_title.clone())
            .collect();
        let preview_image_id = self
            .post
            .preview_image
            .as_ref()
            .map(|image| image.id.as_str());

        let mut blocks: Vec<Block> = Vec::new();
        let mut position = 0usize;

        while position < paragraphs.len() {
            let paragraph = &paragraphs[position];

            // `core.py:259-296`: the de-duplication window. Only the first
            // `DEDUP_WINDOW` paragraphs are checked.
            if position < DEDUP_WINDOW {
                if matches!(paragraph.kind.as_str(), "H2" | "H3" | "H4")
                    && is_match_over_80(paragraph.text.as_deref(), Some(title.as_str()))
                {
                    if title.ends_with('…') {
                        // The title was a truncation and this heading is the
                        // real thing, so the heading wins.
                        debug!(position, "title placeholder replaced by the heading");
                        title = paragraph.text.clone().unwrap_or_default();
                    } else {
                        debug!(position, "paragraph duplicates the title; dropped");
                    }
                    position += 1;
                    continue;
                }
                if paragraph.kind == "H4"
                    && paragraph
                        .text
                        .as_deref()
                        .is_some_and(|text| tags.iter().any(|tag| tag == text))
                {
                    debug!(position, "paragraph duplicates a tag; dropped");
                    position += 1;
                    continue;
                }
                if matches!(paragraph.kind.as_str(), "H4" | "P") {
                    let text = paragraph.text.as_deref();
                    if is_match_over_80(text, Some(subtitle.as_str())) && !subtitle.ends_with('…')
                    {
                        // Over 80% of the subtitle, and the subtitle was not a
                        // truncation: this paragraph *is* the subtitle.
                        subtitle = text.unwrap_or_default().to_string();
                        debug!(position, "paragraph duplicates the subtitle; dropped");
                        position += 1;
                        continue;
                    }
                    if !subtitle.is_empty()
                        && subtitle.ends_with('…')
                        && text.is_some_and(|text| text.chars().count() > 100)
                    {
                        // The placeholder was a truncation and this paragraph is
                        // a full-length candidate, so the placeholder goes.
                        subtitle.clear();
                    }
                } else if paragraph.kind == "IMG" {
                    // `core.py:289-296`: the first paragraph repeating the
                    // preview image is dropped.
                    let repeats_preview = paragraph.metadata.as_ref().is_some_and(|metadata| {
                        preview_image_id.is_some_and(|id| metadata.id == id)
                    });
                    if repeats_preview {
                        debug!(position, "paragraph repeats the preview image; dropped");
                        position += 1;
                        continue;
                    }
                }
            }

            let consumed_to =
                match self.paragraph_block(paragraph, &paragraphs, position, !blocks.is_empty()) {
                    Some((block, consumed_to)) => {
                        blocks.push(block);
                        consumed_to
                    }
                    // A paragraph the legacy code `continue`s on without
                    // emitting: it consumed no source and produced no output.
                    None => position,
                };
            position = consumed_to + 1;
        }

        Document {
            meta: PostMeta {
                post_id: String::new(),
                title,
                subtitle,
                preview_image_id: self
                    .post
                    .preview_image
                    .as_ref()
                    .map(|image| image.id.clone()),
                tags,
                reading_time_minutes: self
                    .post
                    .reading_time
                    .map_or(0, |minutes| minutes.ceil() as u32),
                is_locked: self.post.is_locked,
                medium_url: self.post.medium_url.clone(),
                creator: self.post.creator.clone(),
                collection: self.post.collection.clone(),
            },
            blocks,
        }
    }

    /// Renders one paragraph, returning the block and the last source index it
    /// consumed.
    ///
    /// The caller resumes at `consumed_to + 1`, which is the legacy
    /// `current_pos = _tmp_current_pos - 1` followed by the loop's
    /// `current_pos += 1`. `None` means "no block, consume only this one" — the
    /// legacy `continue` paths.
    fn paragraph_block(
        &self,
        paragraph: &Paragraph,
        paragraphs: &[Paragraph],
        position: usize,
        has_output: bool,
    ) -> Option<(Block, usize)> {
        let text = paragraph.text.as_deref().unwrap_or_default();
        // `core.py:298-303`: a paragraph with no text is parsed as empty *with
        // no markups*, so its markups are discarded too rather than left to
        // resolve against an empty string.
        let markups = if paragraph.text.is_some() {
            resolve_markups(&paragraph.markups)
        } else {
            Vec::new()
        };
        let inline = self.paragraph_inline(paragraph, text, &markups);

        match paragraph.kind.as_str() {
            // `core.py:324-362`. Padding is added only once something has been
            // emitted, and `H4` steps down to `pt-8`.
            "H2" | "H3" | "H4" => {
                let level = paragraph.kind.as_bytes()[1] - b'0';
                Some((
                    Block::Heading {
                        level,
                        id: paragraph.name.clone(),
                        spacing: heading_spacing(level, has_output),
                        inline,
                    },
                    position,
                ))
            }

            // `core.py:363-406`.
            "IMG" => match paragraph.layout.as_str() {
                "OUTSET_ROW" => {
                    // The row swallows the `OUTSET_ROW_CONTINUE` paragraphs that
                    // follow it, and stops at the first one that is not
                    // (`core.py:378-388`).
                    let mut images = vec![image_block(paragraph)];
                    let mut next = position + 1;
                    while next < paragraphs.len() {
                        let candidate = &paragraphs[next];
                        if candidate.layout != "OUTSET_ROW_CONTINUE" {
                            break;
                        }
                        images.push(image_block(candidate));
                        next += 1;
                    }
                    Some((Block::ImageRow(images), next - 1))
                }
                "FULL_WIDTH" => {
                    warn!(position, "IMG: the FULL_WIDTH layout is not implemented");
                    None
                }
                _ => {
                    // `core.py:403`: the caption is emitted only when the
                    // paragraph has text, and it reuses the paragraph's own
                    // rendered text — including any highlight.
                    let caption = (!text.is_empty()).then(|| inline.clone());
                    Some((
                        Block::Image {
                            id: image_id(paragraph),
                            alt: image_alt(paragraph),
                            caption,
                        },
                        position,
                    ))
                }
            },

            // `core.py:407-422`. `paragraphs[current_pos - 1]` is a wrap-around
            // read of the **last** paragraph when this is the first one, which is
            // why the margin comes from the source list rather than from the
            // blocks emitted so far.
            "P" => {
                let previous = &paragraphs[(position + paragraphs.len() - 1) % paragraphs.len()];
                Some((
                    Block::Paragraph {
                        inline,
                        drop_cap: paragraph.has_drop_cap,
                        margin: ParagraphMargin::from_previous_type(&previous.kind),
                    },
                    position,
                ))
            }

            // `core.py:423-476`. The two branches differ only in the wrapper.
            // Note the run starts *at* `position`, and each item is re-parsed
            // with `parse_paragraph_text` — which is why a highlight never
            // reaches a list item.
            "ULI" | "OLI" => {
                let mut items = Vec::new();
                let mut next = position;
                while next < paragraphs.len() {
                    let candidate = &paragraphs[next];
                    if candidate.kind != paragraph.kind {
                        break;
                    }
                    items.push(build_inlines(
                        candidate.text.as_deref().unwrap_or_default(),
                        &resolve_markups(&candidate.markups),
                    ));
                    next += 1;
                }
                Some((
                    Block::List {
                        ordered: paragraph.kind == "OLI",
                        items,
                    },
                    next - 1,
                ))
            }

            // `core.py:477-519`: consecutive `PRE` paragraphs become one code
            // block, one entry per line. The language comes from the first
            // paragraph of the run.
            "PRE" => {
                let lang = paragraph
                    .code_block_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.lang.clone());
                let mut lines = Vec::new();
                let mut next = position;
                while next < paragraphs.len() {
                    let candidate = &paragraphs[next];
                    if candidate.kind != "PRE" {
                        break;
                    }
                    // `is_code=True` forces minimal escaping, which the renderer
                    // applies to every `Block::Code` line.
                    lines.push(build_inlines(
                        candidate.text.as_deref().unwrap_or_default(),
                        &resolve_markups(&candidate.markups),
                    ));
                    next += 1;
                }
                Some((Block::Code { lang, lines }, next - 1))
            }

            "BQ" => Some((
                Block::BlockQuote {
                    style: QuoteStyle::Inset,
                    inline,
                },
                position,
            )),
            "PQ" => Some((
                Block::BlockQuote {
                    style: QuoteStyle::Pull,
                    inline,
                },
                position,
            )),

            // `core.py:534-606`. The misspelling is upstream's, in both the
            // paragraph type and the payload key.
            "MIXTAPE_EMBED" => {
                let Some(mixtape) = paragraph.mixtape_metadata.as_ref() else {
                    warn!(position, "MIXTAPE_EMBED without mixtapeMetadata; skipped");
                    return None;
                };
                // The three markups are, in order: the site's, the embed title's
                // and the description's. With any other count the text cannot be
                // split.
                if paragraph.markups.len() != 3 {
                    warn!(position, "MIXTAPE_EMBED without exactly 3 markups; skipped");
                    return None;
                }
                let title_range = &paragraph.markups[1];
                let description_range = &paragraph.markups[2];
                let url = mixtape.href.clone().unwrap_or_default();
                Some((
                    Block::Embed {
                        title: slice_chars(text, title_range.start, title_range.end),
                        description: slice_chars(
                            text,
                            description_range.start,
                            description_range.end,
                        ),
                        site: embed_site(&url),
                        thumbnail_id: mixtape.thumbnail_image_id.clone(),
                        url,
                    },
                    position,
                ))
            }

            // `core.py:607-676`.
            "IFRAME" => {
                let Some(iframe) = self.resolve_iframe(paragraph) else {
                    warn!(position, "IFRAME without a source; skipped");
                    return None;
                };
                // `if iframe_width and iframe_height and iframe_width > 0`
                // (`core.py:652`): zeros were already filtered out when the
                // dimensions were resolved, so both being present is the test.
                let dims = match (iframe.width, iframe.height) {
                    (Some(width), Some(height)) => Some((width, height)),
                    _ => None,
                };
                Some((
                    Block::Iframe {
                        src: iframe.src,
                        dims,
                    },
                    position,
                ))
            }

            other => {
                warn!(position, kind = other, "unknown paragraph type; skipped");
                None
            }
        }
    }

    /// Builds a paragraph's inline content, applying a reader highlight if the
    /// legacy check accepts one.
    fn paragraph_inline(
        &self,
        paragraph: &Paragraph,
        text: &str,
        markups: &[Markup],
    ) -> Vec<Inline> {
        let inline = build_inlines(text, markups);
        let Some((start, end)) = self.matching_highlight(paragraph, &inline) else {
            return inline;
        };
        // The legacy code sets the `<mark>` template *after* the markup
        // templates, which is what makes the mark the outermost tag when the
        // ranges coincide. Appending it to the list reproduces that: the inline
        // builder's tie-break favours the last markup in the array.
        let mut with_highlight = markups.to_vec();
        with_highlight.push(Markup {
            start,
            end,
            kind: MarkupKind::Highlight,
        });
        build_inlines(text, &with_highlight)
    }

    /// `core.py:305-322`: finds the highlight covering this paragraph, if any.
    ///
    /// The gate is `highlight_paragraph["text"] != text_formater.get_text()` —
    /// it compares the highlight's text against the paragraph **as rendered**,
    /// which for a paragraph carrying markup contains tags and for one carrying
    /// an escapable character contains entities. So a highlight is applied only
    /// when it matches the rendered form exactly, and is otherwise dropped with
    /// a warning. That is reproduced rather than fixed; it is on the
    /// post-cutover list in the rewrite plan.
    ///
    /// The legacy code `break`s out of the paragraph loop on a mismatch and
    /// carries on with the next highlight — a mismatch is not the end of the
    /// search — which is what the inner `break` does here.
    fn matching_highlight(
        &self,
        paragraph: &Paragraph,
        inline: &[Inline],
    ) -> Option<(usize, usize)> {
        if self.post.highlights.is_empty() {
            return None;
        }
        let rendered = render_to_string(inline, mode_for(inline));
        for highlight in &self.post.highlights {
            for candidate in &highlight.paragraphs {
                if candidate.name != paragraph.name {
                    continue;
                }
                if candidate.text != rendered {
                    warn!(
                        name = paragraph.name.as_str(),
                        "highlighted text and paragraph text differ; highlight skipped"
                    );
                    break;
                }
                debug!(name = paragraph.name.as_str(), "applying a highlight");
                return Some((highlight.start_offset, highlight.end_offset));
            }
        }
        None
    }

    /// `core.py:607-647`: resolves an `IFRAME`'s source and dimensions.
    ///
    /// `None` means no source could be found, which the loop treats as a skip.
    fn resolve_iframe(&self, paragraph: &Paragraph) -> Option<ResolvedIframe> {
        let fields = paragraph.iframe.as_ref();
        let mut resource = fields
            .and_then(|fields| fields.media_resource.clone())
            .unwrap_or_default();

        // A `__ref` is followed only when it is not accompanied by the data it
        // points at (`core.py:615`).
        if let Some(reference) = resource.reference.clone() {
            if resource.id.is_none() && resource.iframe_src.is_none() {
                debug!(reference, "following a media resource reference");
                match self.payload.media_resource(&reference) {
                    Some(found) => resource = found,
                    None => warn!(reference, "no media resource for that reference"),
                }
            }
        }

        let src = match resource.iframe_src.clone().filter(|src| !src.is_empty()) {
            Some(src) => src,
            None => {
                let id = resource.id.clone().filter(|id| !id.is_empty())?;
                // `core.py:630-632`: with only an id, the source points back at
                // us and the deployment proxies the frame.
                format!("{}/render_iframe/{id}", self.host_address)
            }
        };

        // `core.py:640-647`: the media resource wins, the paragraph's own fields
        // are the fallback, and `if not iframe_width and ...` means a zero
        // counts as absent.
        let width = resource
            .iframe_width
            .filter(|width| *width != 0)
            .or_else(|| {
                fields
                    .and_then(|fields| fields.iframe_width)
                    .filter(|w| *w != 0)
            });
        let height = resource
            .iframe_height
            .filter(|height| *height != 0)
            .or_else(|| {
                fields
                    .and_then(|fields| fields.iframe_height)
                    .filter(|h| *h != 0)
            });

        Some(ResolvedIframe { src, width, height })
    }
}

/// Turns payload markups into [`Markup`]s, dropping the types the renderer has
/// no template for.
fn resolve_markups(raw: &[RawMarkup]) -> Vec<Markup> {
    raw.iter()
        .filter_map(|markup| {
            let kind = match markup.kind.as_str() {
                "A" => match markup.anchor_type.as_deref() {
                    Some("LINK") => MarkupKind::Link {
                        href: markup.href.clone().unwrap_or_default(),
                        rel: markup.rel.clone().unwrap_or_default(),
                        title: markup.title.clone().unwrap_or_default(),
                    },
                    Some("USER") => MarkupKind::UserMention {
                        user_id: markup.user_id.clone().unwrap_or_default(),
                    },
                    // `markups.py:42-43`: an `A` that is neither a link nor a
                    // mention has no template.
                    other => {
                        warn!(
                            anchor_type = other.unwrap_or("<missing>"),
                            "unsupported anchor markup; skipped"
                        );
                        return None;
                    }
                },
                "STRONG" => MarkupKind::Strong,
                "EM" => MarkupKind::Emphasis,
                "CODE" => MarkupKind::Code,
                // `markups.py:52-53` drops unknown types silently.
                other => {
                    debug!(kind = other, "unknown markup type; skipped");
                    return None;
                }
            };
            Some(Markup {
                start: markup.start,
                end: markup.end,
                kind,
            })
        })
        .collect()
}

/// `core.py:326`/`339`/`352`: `pt-12` for `H2`/`H3`, `pt-8` for `H4`, and
/// nothing at all on the first heading emitted.
const fn heading_spacing(level: u8, has_output: bool) -> HeadingSpacing {
    if !has_output {
        return HeadingSpacing::None;
    }
    if level == 4 {
        HeadingSpacing::Pt8
    } else {
        HeadingSpacing::Pt12
    }
}

fn image_id(paragraph: &Paragraph) -> String {
    paragraph
        .metadata
        .as_ref()
        .map_or_else(String::new, |metadata| metadata.id.clone())
}

/// `alt` as the legacy template interpolates it; see [`Alt`].
fn image_alt(paragraph: &Paragraph) -> String {
    match paragraph.metadata.as_ref().map(|metadata| &metadata.alt) {
        // No `metadata` at all: Jinja reaches into the undefined and renders
        // nothing, the same as a missing `alt` key.
        None | Some(Alt::Undefined) => String::new(),
        Some(alt) => alt.to_string(),
    }
}

fn image_block(paragraph: &Paragraph) -> Block {
    Block::Image {
        id: image_id(paragraph),
        alt: image_alt(paragraph),
        caption: None,
    }
}

/// `core.py:588-595`: the registrable domain, falling back to the bare hostname
/// when the domain has no public suffix.
///
/// One divergence to keep in mind for the parity gate: the Python reference runs
/// against the stubbed `tld.get_fld` in `xtask/difftest/py/stubs.py`, which
/// returns the last two labels for *any* two-label host rather than consulting a
/// suffix list. For a host such as `foo.internal` the stub says `internal` and
/// [`resolve::registrable_domain`] says `None`, so the fallback fires on one side
/// only. Fixtures keep their embed URLs under real public suffixes; the
/// agreement on the suffixes the stub knows is pinned by
/// `resolve::tests::the_public_suffix_list_agrees_with_the_python_stub`.
fn embed_site(url: &str) -> String {
    resolve::registrable_domain(url)
        // `urlparse(url).hostname` lowercases, so the fallback does too.
        .or_else(|| resolve::host_of(url).map(|host| host.to_ascii_lowercase()))
        // `hostname` is `None` for a URL with no host, and Jinja renders that as
        // the literal "None".
        .unwrap_or_else(|| "None".to_string())
}

/// `text_raw[start:end]` (`core.py:580-583`) — a Python slice, so by code point.
///
/// Medium's offsets are UTF-16 code units, so this is exact only while the text
/// before the range is entirely BMP; a surrogate pair would shift it. The legacy
/// code slices the Python string directly and carries the same assumption.
/// Reproduced rather than fixed, because the parity gate compares against these
/// bytes — see [`crate::utf16`] for the same class of bug handled the other way.
/// The slice endpoints come from the payload's own markup objects, not from
/// `Utf16Map`, so they are used raw here just as they are there.
fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Alt, PostPayload, parse};
    use crate::ir::{Block, HeadingSpacing, Inline, ParagraphMargin};

    const HOST: &str = "https://freedium.test";

    fn document(json: serde_json::Value) -> crate::ir::Document {
        let payload = PostPayload::from_value(json).expect("the envelope deserialises");
        parse(&payload, HOST)
    }

    fn blocks_of(json: serde_json::Value) -> Vec<Block> {
        document(json).blocks
    }

    /// Wraps paragraphs in the envelope shape the GraphQL API returns.
    fn with_paragraphs(paragraphs: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": paragraphs } },
                "title": "A Title",
                "previewContent": { "subtitle": "A Subtitle" },
                "tags": [{ "displayTitle": "Rust" }],
            } },
        })
    }

    fn rendered_types(json: serde_json::Value) -> Vec<&'static str> {
        blocks_of(json)
            .iter()
            .map(|block| match block {
                Block::Heading { .. } => "heading",
                Block::Paragraph { .. } => "paragraph",
                Block::List { .. } => "list",
                Block::Code { .. } => "code",
                Block::BlockQuote { .. } => "quote",
                Block::Image { .. } => "image",
                Block::ImageRow(_) => "image-row",
                Block::Embed { .. } => "embed",
                Block::Iframe { .. } => "iframe",
            })
            .collect()
    }

    fn paragraph_json(kind: &str, text: &str, name: &str) -> serde_json::Value {
        serde_json::json!({ "type": kind, "text": text, "name": name, "markups": [] })
    }

    /// A paragraph wrapped in the envelope, with highlights, for the cases that
    /// need to control the highlight list.
    fn with_highlights(
        paragraphs: serde_json::Value,
        highlights: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": paragraphs } },
                "title": "T",
                "previewContent": { "subtitle": "" },
                "highlights": highlights,
            } },
        })
    }

    fn highlight(name: &str, text: &str, start: usize, end: usize) -> serde_json::Value {
        serde_json::json!([{ "startOffset": start, "endOffset": end,
                             "paragraphs": [{ "name": name, "text": text }] }])
    }

    // ---- block types ------------------------------------------------------

    #[test]
    fn a_plain_paragraph_becomes_a_paragraph_block() {
        let json = with_paragraphs(serde_json::json!([paragraph_json("P", "Body text", "p1")]));
        assert_eq!(rendered_types(json), ["paragraph"]);
    }

    #[test]
    fn headings_carry_their_level_and_paragraph_name() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("H2", "Two", "h2"),
            paragraph_json("H3", "Three", "h3"),
            paragraph_json("H4", "Four", "h4"),
        ]));
        let levels: Vec<(u8, String)> = blocks_of(json)
            .iter()
            .map(|block| match block {
                Block::Heading { level, id, .. } => (*level, id.clone()),
                other => panic!("expected a heading, got {other:?}"),
            })
            .collect();
        assert_eq!(
            levels,
            [
                (2, "h2".to_string()),
                (3, "h3".to_string()),
                (4, "h4".to_string())
            ]
        );
    }

    /// `core.py:326` — only the first heading *emitted* is unpadded, and `H4`
    /// steps down to `pt-8`.
    #[test]
    fn only_the_first_emitted_heading_is_unpadded() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("H2", "First", "a"),
            paragraph_json("H4", "Second", "b"),
            paragraph_json("H2", "Third", "c"),
        ]));
        let spacings: Vec<HeadingSpacing> = blocks_of(json)
            .iter()
            .filter_map(|block| match block {
                Block::Heading { spacing, .. } => Some(*spacing),
                _ => None,
            })
            .collect();
        assert_eq!(
            spacings,
            [
                HeadingSpacing::None,
                HeadingSpacing::Pt8,
                HeadingSpacing::Pt12
            ]
        );
    }

    /// Quirk: the *last* paragraph's type decides the first paragraph's margin,
    /// because `core.py:415` reads `paragraphs[current_pos - 1]` and Python
    /// wraps a negative index.
    #[test]
    fn the_first_paragraph_margin_reads_the_last_paragraph() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("P", "First", "p1"),
            paragraph_json("P", "Second", "p2"),
            paragraph_json("H3", "Last", "h"),
        ]));
        let margins: Vec<ParagraphMargin> = blocks_of(json)
            .iter()
            .filter_map(|block| match block {
                Block::Paragraph { margin, .. } => Some(*margin),
                _ => None,
            })
            .collect();
        assert_eq!(
            margins,
            [ParagraphMargin::Mt3, ParagraphMargin::Mt7],
            "the first paragraph wraps around to the H3 at the end"
        );
    }

    /// A paragraph after a heading gets `mt-3`; after anything else, `mt-7`.
    #[test]
    fn a_paragraph_after_a_heading_gets_the_smaller_margin() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("H3", "Heading", "h"),
            paragraph_json("P", "Body", "p"),
        ]));
        let margin = blocks_of(json)
            .iter()
            .find_map(|block| match block {
                Block::Paragraph { margin, .. } => Some(*margin),
                _ => None,
            })
            .expect("one paragraph");
        assert_eq!(margin, ParagraphMargin::Mt3);
    }

    #[test]
    fn drop_cap_is_carried_through() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "Once", "name": "p", "markups": [], "hasDropCap": true }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { drop_cap, .. } => assert!(drop_cap),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn quote_types_map_to_their_two_styles() {
        use crate::ir::QuoteStyle;
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("BQ", "Inset", "1"),
            paragraph_json("PQ", "Pull", "2"),
        ]));
        let styles: Vec<QuoteStyle> = blocks_of(json)
            .iter()
            .map(|block| match block {
                Block::BlockQuote { style, .. } => *style,
                other => panic!("expected a quote, got {other:?}"),
            })
            .collect();
        assert_eq!(styles, [QuoteStyle::Inset, QuoteStyle::Pull]);
    }

    // ---- de-duplication ---------------------------------------------------

    /// Quirk: the de-duplication window covers only the first four paragraphs,
    /// so a heading repeating the title further down survives.
    #[test]
    fn the_dedup_window_is_the_first_four_paragraphs() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("P", "one", "1"),
                    paragraph_json("P", "two", "2"),
                    paragraph_json("P", "three", "3"),
                    paragraph_json("P", "four", "4"),
                    paragraph_json("H3", "A Title", "5"),
                ] } },
                "title": "A Title",
                "previewContent": { "subtitle": "" },
            } },
        });
        let types = rendered_types(json);
        assert_eq!(types.len(), 5);
        assert_eq!(types[4], "heading", "position 4 is outside the window");
    }

    /// Inside the window the same heading is dropped as a title duplicate.
    #[test]
    fn a_heading_duplicating_the_title_is_dropped_inside_the_window() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("H3", "A Title", "h"),
                    paragraph_json("P", "Body", "p"),
                ] } },
                "title": "A Title",
                "previewContent": { "subtitle": "" },
            } },
        });
        assert_eq!(rendered_types(json), ["paragraph"]);
    }

    /// `core.py:262-264`: an ellipsised title is a truncation, so the heading
    /// replaces it rather than being dropped.
    #[test]
    fn an_ellipsised_title_is_replaced_by_the_heading() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("H3", "The Real Title", "h"),
                    paragraph_json("P", "Body", "p"),
                ] } },
                "title": "The Real Tit…",
                "previewContent": { "subtitle": "" },
            } },
        });
        let document = document(json);
        assert_eq!(document.meta.title, "The Real Title");
        assert_eq!(document.blocks.len(), 1, "the heading is consumed");
    }

    /// `core.py:274-282`: a `P` repeating a *complete* subtitle is dropped, and
    /// it replaces the subtitle on the way out.
    #[test]
    fn a_paragraph_duplicating_the_subtitle_is_dropped() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("P", "The Real Subtitle", "p"),
                    paragraph_json("P", "Body", "p2"),
                ] } },
                "title": "A Title",
                "previewContent": { "subtitle": "The Real Subtitle" },
            } },
        });
        let document = document(json.clone());
        assert_eq!(rendered_types(json), ["paragraph"]);
        assert_eq!(document.meta.subtitle, "The Real Subtitle");
    }

    /// The drop fires only when the subtitle is *not* a truncation
    /// (`core.py:278`): with a `…` subtitle the paragraph is kept, and the
    /// `> 100` branch does not clear the subtitle either because a short
    /// paragraph never reaches it.
    #[test]
    fn a_truncated_subtitle_does_not_drop_the_paragraph() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("P", "The Real Subtitle", "p"),
                ] } },
                "title": "A Title",
                "previewContent": { "subtitle": "The Real Subtitl…" },
            } },
        });
        let document = document(json.clone());
        assert_eq!(rendered_types(json), ["paragraph"]);
        assert_eq!(document.meta.subtitle, "The Real Subtitl…");
    }

    /// `core.py:283-288`: a 100+ character paragraph clears a truncated
    /// subtitle, and is itself kept.
    #[test]
    fn a_long_paragraph_clears_a_truncated_subtitle() {
        let long = "x".repeat(101);
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("P", &long, "p"),
                ] } },
                "title": "A Title",
                "previewContent": { "subtitle": "Something else enti…" },
            } },
        });
        let document = document(json);
        assert_eq!(document.meta.subtitle, "");
        assert_eq!(document.blocks.len(), 1, "the paragraph is kept");
    }

    /// An `H4` repeating a tag is dropped; a `P` repeating it is not.
    #[test]
    fn an_h4_repeating_a_tag_is_dropped() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("H4", "Rust", "h"),
            paragraph_json("P", "Rust", "p"),
        ]));
        assert_eq!(rendered_types(json), ["paragraph"]);
    }

    /// `core.py:289-296`: an `IMG` repeating the preview image is dropped — and
    /// because the check runs *every* paragraph inside the window rather than
    /// only once, a second such paragraph is dropped too. There is no
    /// already-dropped-one state in the legacy code.
    #[test]
    fn every_preview_image_paragraph_inside_the_window_is_dropped() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    { "type": "IMG", "text": "", "name": "i1", "markups": [],
                      "metadata": { "id": "preview" } },
                    { "type": "IMG", "text": "", "name": "i2", "markups": [],
                      "metadata": { "id": "other" } },
                    { "type": "IMG", "text": "", "name": "i3", "markups": [],
                      "metadata": { "id": "preview" } },
                ] } },
                "title": "T",
                "previewImage": { "id": "preview" },
            } },
        });
        assert_eq!(rendered_types(json), ["image"], "only i2 survives");
    }

    /// The same paragraph past the window is kept, like every other
    /// de-duplication check here.
    #[test]
    fn a_preview_image_paragraph_outside_the_window_survives() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("P", "one", "1"),
                    paragraph_json("P", "two", "2"),
                    paragraph_json("P", "three", "3"),
                    paragraph_json("P", "four", "4"),
                    { "type": "IMG", "text": "", "name": "i", "markups": [],
                      "metadata": { "id": "preview" } },
                ] } },
                "title": "T",
                "previewContent": { "subtitle": "" },
                "previewImage": { "id": "preview" },
            } },
        });
        assert_eq!(
            rendered_types(json),
            ["paragraph", "paragraph", "paragraph", "paragraph", "image"]
        );
    }

    // ---- runs: lists, code, image rows -----------------------------------

    #[test]
    fn consecutive_uli_paragraphs_join_into_one_list() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("ULI", "one", "1"),
            paragraph_json("ULI", "two", "2"),
            paragraph_json("OLI", "three", "3"),
            paragraph_json("P", "after", "4"),
        ]));
        let blocks = blocks_of(json);
        assert_eq!(
            rendered_types(with_paragraphs(serde_json::json!([
                paragraph_json("ULI", "one", "1"),
                paragraph_json("ULI", "two", "2"),
                paragraph_json("OLI", "three", "3"),
                paragraph_json("P", "after", "4"),
            ]))),
            ["list", "list", "paragraph"]
        );
        match &blocks[0] {
            Block::List { ordered, items } => {
                assert!(!ordered);
                assert_eq!(items.len(), 2);
            }
            other => panic!("expected an unordered list, got {other:?}"),
        }
        match &blocks[1] {
            Block::List { ordered, items } => {
                assert!(ordered);
                assert_eq!(items.len(), 1);
            }
            other => panic!("expected an ordered list, got {other:?}"),
        }
    }

    /// The run stops at the first paragraph of a different type.
    #[test]
    fn a_list_run_stops_at_the_first_other_type() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("ULI", "one", "1"),
            paragraph_json("P", "break", "2"),
            paragraph_json("ULI", "two", "3"),
        ]));
        assert_eq!(rendered_types(json), ["list", "paragraph", "list"]);
    }

    /// A list item's markups survive, but they are resolved without the
    /// highlight pass.
    #[test]
    fn a_list_item_keeps_its_markups() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "ULI", "text": "abcdefghij", "name": "1", "markups": [
                { "type": "STRONG", "start": 2, "end": 5 }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::List { items, .. } => match items[0].as_slice() {
                [Inline::Text(a), Inline::Strong(b), Inline::Text(c)] => {
                    assert_eq!((a.as_str(), c.as_str()), ("ab", "fghij"));
                    assert_eq!(b.len(), 1);
                }
                other => panic!("expected three nodes, got {other:?}"),
            },
            other => panic!("expected a list, got {other:?}"),
        }
    }

    #[test]
    fn consecutive_pre_paragraphs_join_into_one_code_block() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "PRE", "text": "line one", "name": "1", "markups": [],
              "codeBlockMetadata": { "lang": "rust" } },
            { "type": "PRE", "text": "line two", "name": "2", "markups": [] },
        ]));
        let blocks = blocks_of(json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Code { lang, lines } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert_eq!(lines.len(), 2, "one entry per PRE paragraph");
            }
            other => panic!("expected a code block, got {other:?}"),
        }
    }

    /// `core.py:485-495`: no metadata, or a null `lang`, both mean
    /// `nohighlight`.
    #[test]
    fn a_code_block_without_a_language_has_no_lang() {
        for metadata in [serde_json::json!({ "lang": null }), serde_json::json!({})] {
            let json = with_paragraphs(serde_json::json!([
                { "type": "PRE", "text": "code", "name": "1", "markups": [],
                  "codeBlockMetadata": metadata }
            ]));
            match &blocks_of(json)[0] {
                Block::Code { lang, .. } => assert!(lang.is_none()),
                other => panic!("expected a code block, got {other:?}"),
            }
        }
    }

    /// The language comes from the *first* paragraph of the run.
    #[test]
    fn only_the_first_pre_paragraph_supplies_the_language() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "PRE", "text": "a", "name": "1", "markups": [] },
            { "type": "PRE", "text": "b", "name": "2", "markups": [],
              "codeBlockMetadata": { "lang": "rust" } },
        ]));
        match &blocks_of(json)[0] {
            Block::Code { lang, .. } => assert!(lang.is_none()),
            other => panic!("expected a code block, got {other:?}"),
        }
    }

    /// `core.py:370-395`: an `OUTSET_ROW` swallows the continues that follow.
    #[test]
    fn an_outset_row_swallows_the_continues_that_follow() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "", "name": "1", "markups": [],
              "layout": "OUTSET_ROW", "metadata": { "id": "a" } },
            { "type": "IMG", "text": "", "name": "2", "markups": [],
              "layout": "OUTSET_ROW_CONTINUE", "metadata": { "id": "b" } },
            paragraph_json("P", "after", "3"),
        ]));
        let blocks = blocks_of(json);
        assert_eq!(
            rendered_types(with_paragraphs(serde_json::json!([
                { "type": "IMG", "text": "", "name": "1", "markups": [],
                  "layout": "OUTSET_ROW", "metadata": { "id": "a" } },
                { "type": "IMG", "text": "", "name": "2", "markups": [],
                  "layout": "OUTSET_ROW_CONTINUE", "metadata": { "id": "b" } },
                paragraph_json("P", "after", "3"),
            ]))),
            ["image-row", "paragraph"]
        );
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::ImageRow(images) => assert_eq!(images.len(), 2),
            other => panic!("expected an image row, got {other:?}"),
        }
    }

    /// A row on its own must still advance the loop past itself.
    #[test]
    fn a_row_with_no_continues_advances_by_one() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "", "name": "1", "markups": [],
              "layout": "OUTSET_ROW", "metadata": { "id": "a" } },
            paragraph_json("P", "after", "2"),
        ]));
        let blocks = blocks_of(json);
        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            Block::ImageRow(images) => assert_eq!(images.len(), 1),
            other => panic!("expected an image row, got {other:?}"),
        }
    }

    /// A `OUTSET_ROW_CONTINUE` with no row before it is an ordinary image.
    #[test]
    fn a_lone_outset_row_continue_is_a_plain_image() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "", "name": "1", "markups": [],
              "layout": "OUTSET_ROW_CONTINUE", "metadata": { "id": "a" } },
        ]));
        assert_eq!(rendered_types(json), ["image"]);
    }

    /// `core.py:396-399`: `FULL_WIDTH` is not implemented and is dropped.
    #[test]
    fn a_full_width_image_is_dropped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "", "name": "1", "markups": [],
              "layout": "FULL_WIDTH", "metadata": { "id": "a" } },
            paragraph_json("P", "after", "2"),
        ]));
        assert_eq!(rendered_types(json), ["paragraph"]);
    }

    /// `core.py:403`: the caption is emitted only when the paragraph has text.
    #[test]
    fn an_image_caption_requires_text() {
        let with = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "A caption", "name": "1", "markups": [],
              "metadata": { "id": "a", "alt": "alt" } },
        ]));
        match &blocks_of(with)[0] {
            Block::Image { caption, .. } => assert!(caption.is_some()),
            other => panic!("expected an image, got {other:?}"),
        }

        let without = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "", "name": "1", "markups": [],
              "metadata": { "id": "a" } },
        ]));
        match &blocks_of(without)[0] {
            Block::Image { caption, .. } => assert!(caption.is_none()),
            other => panic!("expected an image, got {other:?}"),
        }
    }

    /// `{{ paragraph.metadata.alt }}` under Jinja's default undefined: a null
    /// renders as the string `None`, a missing attribute as nothing at all.
    #[test]
    fn image_alt_distinguishes_null_from_missing() {
        for (metadata, expected) in [
            (serde_json::json!({ "id": "a", "alt": null }), "None"),
            (serde_json::json!({ "id": "a" }), ""),
            (
                serde_json::json!({ "id": "a", "alt": "a caption" }),
                "a caption",
            ),
        ] {
            let json = with_paragraphs(serde_json::json!([
                { "type": "IMG", "text": "", "name": "1", "markups": [],
                  "metadata": metadata }
            ]));
            match &blocks_of(json)[0] {
                Block::Image { alt, .. } => assert_eq!(alt, expected),
                other => panic!("expected an image, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_image_without_metadata_is_empty_rather_than_skipped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IMG", "text": "", "name": "1", "markups": [] }
        ]));
        match &blocks_of(json)[0] {
            Block::Image { alt, id, .. } => {
                assert_eq!(alt, "");
                assert_eq!(id, "");
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    #[test]
    fn alt_deserialises_the_three_cases() {
        assert_eq!(Alt::default(), Alt::Undefined);
        assert_eq!(
            serde_json::from_value::<Alt>(serde_json::json!(null)).unwrap(),
            Alt::Null
        );
        assert_eq!(
            serde_json::from_value::<Alt>(serde_json::json!("x")).unwrap(),
            Alt::Text("x".into())
        );
        assert_eq!(Alt::Null.to_string(), "None");
        assert_eq!(Alt::Undefined.to_string(), "");
    }

    // ---- markups ----------------------------------------------------------

    #[test]
    fn markups_are_attached_to_the_paragraph_text() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "abcdefghij", "name": "1", "markups": [
                { "type": "STRONG", "start": 2, "end": 5 }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert_eq!(inline.len(), 3);
                assert!(matches!(inline[1], Inline::Strong(_)));
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn a_link_markup_becomes_an_anchor_with_its_attributes() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "abcdefghij", "name": "1", "markups": [
                { "type": "A", "anchorType": "LINK", "start": 0, "end": 10,
                  "href": "https://x.test", "rel": "noopener", "title": "T" }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => match &inline[0] {
                Inline::Link {
                    href,
                    rel,
                    title,
                    new_tab,
                    ..
                } => {
                    assert_eq!(href, "https://x.test");
                    assert_eq!(rel, "noopener");
                    assert_eq!(title, "T");
                    assert!(new_tab);
                }
                other => panic!("expected a link, got {other:?}"),
            },
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn a_user_mention_markup_becomes_a_mention() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "abcdefghij", "name": "1", "markups": [
                { "type": "A", "anchorType": "USER", "start": 0, "end": 3,
                  "userId": "abc123" }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => match &inline[0] {
                Inline::UserMention { user_id, .. } => assert_eq!(user_id, "abc123"),
                other => panic!("expected a mention, got {other:?}"),
            },
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// `markups.py:42-43`: an `A` that is neither a link nor a mention has no
    /// template, so it is dropped and the text stays plain.
    #[test]
    fn an_unsupported_anchor_type_is_dropped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "abcdefghij", "name": "1", "markups": [
                { "type": "A", "anchorType": "SOMETHING", "start": 2, "end": 5 }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert_eq!(inline.len(), 1, "one untouched text node: {inline:?}");
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// `markups.py:52-53` drops unknown markup types silently.
    #[test]
    fn an_unknown_markup_type_is_dropped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "abcdefghij", "name": "1", "markups": [
                { "type": "STRIKETHROUGH", "start": 2, "end": 5 }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => assert_eq!(inline.len(), 1),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    // ---- embeds -----------------------------------------------------------

    #[test]
    fn a_mixtape_embed_without_metadata_is_skipped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "MIXTAPE_EMBED", "text": "x", "name": "1", "markups": [] }
        ]));
        assert!(blocks_of(json).is_empty());
    }

    /// `core.py:567-572`: without exactly three markups the text cannot be
    /// split into title and description, so the embed is skipped.
    #[test]
    fn a_mixtape_embed_needs_exactly_three_markups() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "MIXTAPE_EMBED", "text": "x", "name": "1",
              "mixtapeMetadata": { "href": "https://example.com/a" },
              "markups": [{ "type": "A", "anchorType": "LINK", "start": 0, "end": 1 }] }
        ]));
        assert!(blocks_of(json).is_empty());
    }

    /// `core.py:574-583`: markups 0, 1 and 2 are the site, title and
    /// description, and the title/description are Python slices of the raw text.
    #[test]
    fn a_mixtape_embed_splits_title_and_description_from_the_markups() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "MIXTAPE_EMBED",
              "text": "Example An Article A description",
              "name": "1",
              "mixtapeMetadata": {
                  "href": "https://www.example.com/post",
                  "thumbnailImageId": "thumb"
              },
              "markups": [
                  { "type": "A", "anchorType": "LINK", "start": 0, "end": 7 },
                  { "type": "A", "anchorType": "LINK", "start": 8, "end": 18 },
                  { "type": "A", "anchorType": "LINK", "start": 19, "end": 32 }
              ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Embed {
                url,
                title,
                description,
                site,
                thumbnail_id,
            } => {
                assert_eq!(url, "https://www.example.com/post");
                assert_eq!(title, "An Article");
                assert_eq!(description, "A description");
                assert_eq!(site, "example.com", "the registrable domain");
                assert_eq!(thumbnail_id.as_deref(), Some("thumb"));
            }
            other => panic!("expected an embed, got {other:?}"),
        }
    }

    /// `core.py:588-595`: when `get_fld` cannot place the domain, the fallback
    /// is the bare hostname.
    #[test]
    fn an_embed_site_falls_back_to_the_hostname() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "MIXTAPE_EMBED", "text": "abcdefghijklmnopqrstuvwxyz",
              "name": "1",
              "mixtapeMetadata": { "href": "https://localhost:8080/a" },
              "markups": [
                  { "type": "A", "anchorType": "LINK", "start": 0, "end": 1 },
                  { "type": "A", "anchorType": "LINK", "start": 1, "end": 2 },
                  { "type": "A", "anchorType": "LINK", "start": 2, "end": 3 }
              ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Embed { site, .. } => assert_eq!(site, "localhost"),
            other => panic!("expected an embed, got {other:?}"),
        }
    }

    /// The multi-label suffix has to be respected, not just the last two
    /// labels.
    #[test]
    fn an_embed_site_handles_a_multi_label_suffix() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "MIXTAPE_EMBED", "text": "abcdefghijklmnopqrstuvwxyz",
              "name": "1",
              "mixtapeMetadata": { "href": "https://news.bbc.co.uk/a" },
              "markups": [
                  { "type": "A", "anchorType": "LINK", "start": 0, "end": 1 },
                  { "type": "A", "anchorType": "LINK", "start": 1, "end": 2 },
                  { "type": "A", "anchorType": "LINK", "start": 2, "end": 3 }
              ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Embed { site, .. } => assert_eq!(site, "bbc.co.uk"),
            other => panic!("expected an embed, got {other:?}"),
        }
    }

    // ---- iframes ----------------------------------------------------------

    #[test]
    fn an_iframe_with_dimensions_keeps_them() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": { "mediaResource": {
                  "iframeSrc": "https://www.youtube.com/embed/x",
                  "iframeWidth": 640, "iframeHeight": 360 } } }
        ]));
        match &blocks_of(json)[0] {
            Block::Iframe { src, dims } => {
                assert_eq!(src, "https://www.youtube.com/embed/x");
                assert_eq!(*dims, Some((640, 360)));
            }
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// `core.py:644-647`: the paragraph's own dimensions are the fallback.
    #[test]
    fn iframe_dimensions_fall_back_to_the_paragraph_fields() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": {
                  "mediaResource": { "iframeSrc": "https://example.com/e" },
                  "iframeWidth": 320, "iframeHeight": 240 } }
        ]));
        match &blocks_of(json)[0] {
            Block::Iframe { dims, .. } => assert_eq!(*dims, Some((320, 240))),
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// A width with no height cannot take the aspect-ratio branch, so both
    /// attributes become `100%` (`core.py:666-676`).
    #[test]
    fn an_iframe_with_only_one_dimension_takes_the_fallback_branch() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": { "mediaResource": {
                  "iframeSrc": "https://example.com/e", "iframeWidth": 640 } } }
        ]));
        match &blocks_of(json)[0] {
            Block::Iframe { dims, .. } => assert!(dims.is_none()),
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// `if not iframe_width and ...` (`core.py:644`): a zero is absent.
    #[test]
    fn a_zero_iframe_dimension_falls_back() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": {
                  "mediaResource": { "iframeSrc": "https://example.com/e",
                                     "iframeWidth": 0, "iframeHeight": 0 },
                  "iframeWidth": 320, "iframeHeight": 240 } }
        ]));
        match &blocks_of(json)[0] {
            Block::Iframe { dims, .. } => assert_eq!(*dims, Some((320, 240))),
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// A dimension the schema would not send costs the dimension, not the
    /// article (§3.3).
    #[test]
    fn a_non_integer_iframe_dimension_is_treated_as_absent() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": { "mediaResource": {
                  "iframeSrc": "https://example.com/e",
                  "iframeWidth": "640", "iframeHeight": "360" } } }
        ]));
        match &blocks_of(json)[0] {
            Block::Iframe { dims, .. } => assert!(dims.is_none()),
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// `core.py:614-622`: a `__ref` is followed into `post_data["data"]`.
    #[test]
    fn an_iframe_media_resource_reference_is_followed() {
        let json = serde_json::json!({
            "data": {
                "post": { "content": { "bodyModel": { "paragraphs": [
                    { "type": "IFRAME", "text": "", "name": "1", "markups": [],
                      "iframe": { "mediaResource": { "__ref": "MediaResource:1" } } }
                ] } } },
                "MediaResource:1": {
                    "iframeSrc": "https://example.com/resolved",
                    "iframeWidth": 800, "iframeHeight": 450 },
            },
        });
        match &blocks_of(json)[0] {
            Block::Iframe { src, dims } => {
                assert_eq!(src, "https://example.com/resolved");
                assert_eq!(*dims, Some((800, 450)));
            }
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// A `__ref` alongside the data it points at is not followed
    /// (`core.py:615`).
    #[test]
    fn a_reference_with_its_own_data_is_not_followed() {
        let json = serde_json::json!({
            "data": {
                "post": { "content": { "bodyModel": { "paragraphs": [
                    { "type": "IFRAME", "text": "", "name": "1", "markups": [],
                      "iframe": { "mediaResource": {
                          "__ref": "MediaResource:1",
                          "iframeSrc": "https://example.com/inline" } } }
                ] } } },
                "MediaResource:1": { "iframeSrc": "https://example.com/resolved" },
            },
        });
        match &blocks_of(json)[0] {
            Block::Iframe { src, .. } => assert_eq!(src, "https://example.com/inline"),
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    /// `core.py:630-632`: with only an id, the source points back at us.
    #[test]
    fn an_iframe_id_falls_back_to_the_render_proxy() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": { "mediaResource": { "id": "abc123" } } }
        ]));
        match &blocks_of(json)[0] {
            Block::Iframe { src, .. } => {
                assert_eq!(src, "https://freedium.test/render_iframe/abc123");
            }
            other => panic!("expected an iframe, got {other:?}"),
        }
    }

    #[test]
    fn an_iframe_without_a_source_is_skipped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": { "mediaResource": {} } }
        ]));
        assert!(blocks_of(json).is_empty());
    }

    /// A dangling `__ref` leaves the resource empty, which is a skip — the same
    /// place the legacy code ends up when the reference is not in `data`.
    #[test]
    fn a_dangling_iframe_reference_is_skipped() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "IFRAME", "text": "", "name": "1", "markups": [],
              "iframe": { "mediaResource": { "__ref": "MediaResource:404" } } }
        ]));
        assert!(blocks_of(json).is_empty());
    }

    // ---- highlights -------------------------------------------------------

    /// The common case: a plain paragraph, highlighted. The rendered paragraph
    /// is the escaped text, which is what the highlight's text has to equal.
    #[test]
    fn a_highlight_on_a_plain_paragraph_is_applied() {
        let json = with_highlights(
            serde_json::json!([paragraph_json("P", "Hello world", "p1")]),
            highlight("p1", "Hello world", 0, 5),
        );
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert!(matches!(inline[0], Inline::Highlight(_)), "{inline:?}");
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// The gate compares against the **rendered** paragraph, so a paragraph
    /// whose text is escaped no longer matches the highlight's plain text and
    /// the highlight is dropped. This is the legacy behaviour
    /// (`core.py:309-313`), not a bug introduced here.
    #[test]
    fn a_highlight_is_dropped_when_escaping_changed_the_text() {
        let json = with_highlights(
            serde_json::json!([paragraph_json("P", "a & b", "p1")]),
            highlight("p1", "a & b", 0, 5),
        );
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert!(
                    !inline
                        .iter()
                        .any(|node| matches!(node, Inline::Highlight(_))),
                    "the rendered text is `a &amp; b`, which never equals `a & b`: {inline:?}"
                );
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// A paragraph carrying markup renders with tags in it, so no plain
    /// highlight text can match either.
    #[test]
    fn a_highlight_is_dropped_on_a_paragraph_with_markup() {
        let json = with_highlights(
            serde_json::json!([
                { "type": "P", "text": "abcdefghij", "name": "p1", "markups": [
                    { "type": "STRONG", "start": 0, "end": 10 }
                ] }
            ]),
            highlight("p1", "abcdefghij", 1, 5),
        );
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert!(
                    !inline
                        .iter()
                        .any(|node| matches!(node, Inline::Highlight(_)))
                );
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// The highlight's offsets are UTF-16, like the markups', so a range can
    /// select an emoji.
    #[test]
    fn a_highlight_range_is_resolved_as_utf16() {
        let json = with_highlights(
            serde_json::json!([paragraph_json("P", "hi 😀 there", "p1")]),
            highlight("p1", "hi 😀 there", 3, 5),
        );
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert_eq!(inline.len(), 3, "{inline:?}");
                match &inline[1] {
                    Inline::Highlight(children) => {
                        assert!(matches!(children.as_slice(), [Inline::Text(t)] if t == "😀"));
                    }
                    other => panic!("expected a highlight, got {other:?}"),
                }
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// A highlight naming a different paragraph is left alone.
    #[test]
    fn a_highlight_for_another_paragraph_does_not_apply() {
        let json = with_highlights(
            serde_json::json!([paragraph_json("P", "Hello world", "p1")]),
            highlight("other", "Hello world", 0, 5),
        );
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert!(
                    !inline
                        .iter()
                        .any(|node| matches!(node, Inline::Highlight(_)))
                );
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// A mismatch for one highlight does not stop the search: the legacy code
    /// breaks out of the *inner* loop only (`core.py:313`).
    #[test]
    fn a_later_highlight_still_applies_after_a_mismatch() {
        let highlights = serde_json::json!([
            { "startOffset": 0, "endOffset": 5,
              "paragraphs": [{ "name": "p1", "text": "not the text" }] },
            { "startOffset": 6, "endOffset": 11,
              "paragraphs": [{ "name": "p1", "text": "Hello world" }] },
        ]);
        let json = with_highlights(
            serde_json::json!([paragraph_json("P", "Hello world", "p1")]),
            highlights,
        );
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => {
                assert!(matches!(inline[1], Inline::Highlight(_)), "{inline:?}");
            }
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// `core.py:434-436` re-parses a list item, so a highlight never reaches
    /// one even when its text matches.
    #[test]
    fn a_highlight_does_not_reach_a_list_item() {
        let json = with_highlights(
            serde_json::json!([paragraph_json("ULI", "Hello world", "p1")]),
            highlight("p1", "Hello world", 0, 5),
        );
        match &blocks_of(json)[0] {
            Block::List { items, .. } => {
                assert!(
                    !items[0]
                        .iter()
                        .any(|node| matches!(node, Inline::Highlight(_)))
                );
            }
            other => panic!("expected a list, got {other:?}"),
        }
    }

    /// The caption reuses the paragraph's own rendered text, highlight included
    /// (`core.py:405`).
    #[test]
    fn an_image_caption_carries_the_highlight() {
        let json = with_highlights(
            serde_json::json!([
                { "type": "IMG", "text": "Hello world", "name": "p1", "markups": [],
                  "metadata": { "id": "a" } }
            ]),
            highlight("p1", "Hello world", 0, 5),
        );
        match &blocks_of(json)[0] {
            Block::Image { caption, .. } => {
                let caption = caption.as_ref().expect("a caption");
                assert!(matches!(caption[0], Inline::Highlight(_)), "{caption:?}");
            }
            other => panic!("expected an image, got {other:?}"),
        }
    }

    // ---- envelope tolerance -----------------------------------------------

    #[test]
    fn metadata_is_read_from_the_envelope() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [] } },
                "title": "A Title",
                "previewContent": { "subtitle": "A Subtitle" },
                "previewImage": { "id": "img" },
                "tags": [{ "displayTitle": "Rust" }, { "displayTitle": "Parsing" }],
                "mediumUrl": "https://medium.com/@x/t-0291df856c77",
                "readingTime": 4.2,
                "isLocked": true,
            } },
        });
        let meta = document(json).meta;
        assert_eq!(meta.title, "A Title");
        assert_eq!(meta.subtitle, "A Subtitle");
        assert_eq!(meta.preview_image_id.as_deref(), Some("img"));
        assert_eq!(meta.tags, ["Rust", "Parsing"]);
        assert_eq!(meta.medium_url, "https://medium.com/@x/t-0291df856c77");
        assert_eq!(meta.reading_time_minutes, 5, "ceil(4.2)");
        assert!(meta.is_locked);
    }

    /// The post id belongs to the caller's page assembly, not to the body.
    #[test]
    fn the_post_id_is_left_to_the_caller() {
        let json = with_paragraphs(serde_json::json!([]));
        assert_eq!(document(json).meta.post_id, "");
    }

    /// Unknown fields anywhere in the envelope are ignored rather than
    /// rejected; the GraphQL query deliberately over-fetches (§3.3).
    #[test]
    fn unknown_fields_are_ignored() {
        let json = serde_json::json!({
            "data": { "post": {
                "brandNewField": { "nested": true },
                "content": { "bodyModel": { "paragraphs": [
                    { "type": "P", "text": "kept", "name": "1", "markups": [],
                      "anotherNewField": 7 }
                ], "extraModel": [] } },
                "title": "T",
                "previewContent": { "subtitle": "" },
            } },
        });
        assert_eq!(rendered_types(json), ["paragraph"]);
    }

    /// A null title or subtitle costs the title, not the article.
    #[test]
    fn a_null_title_does_not_lose_the_article() {
        let json = serde_json::json!({
            "data": { "post": {
                "content": { "bodyModel": { "paragraphs": [
                    paragraph_json("P", "kept", "1")
                ] } },
                "title": null,
                "previewContent": { "subtitle": null },
                "mediumUrl": null,
            } },
        });
        let document = document(json.clone());
        assert_eq!(document.meta.title, "");
        assert_eq!(document.meta.subtitle, "");
        assert_eq!(rendered_types(json), ["paragraph"]);
        assert_eq!(document.blocks.len(), 1, "the body survives");
    }

    /// A locked post has no readable content; the document is empty rather than
    /// an error.
    #[test]
    fn a_null_content_yields_an_empty_document() {
        let json = serde_json::json!({
            "data": { "post": { "content": null, "title": "T", "isLocked": true } },
        });
        let document = document(json);
        assert!(document.blocks.is_empty());
        assert!(document.meta.is_locked);
    }

    /// A payload of the wrong shape yields an empty document instead of an
    /// error, so a schema change degrades rather than 500s.
    #[test]
    fn a_malformed_envelope_yields_an_empty_document() {
        for data in [
            serde_json::json!(7),
            serde_json::json!({ "post": 7 }),
            serde_json::json!({}),
        ] {
            let payload = PostPayload::from_value(serde_json::json!({ "data": data })).unwrap();
            assert!(parse(&payload, HOST).blocks.is_empty());
        }
    }

    #[test]
    fn a_paragraph_without_text_keeps_none_of_its_markups() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": null, "name": "1", "markups": [
                { "type": "STRONG", "start": 0, "end": 3 }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => assert!(inline.is_empty(), "{inline:?}"),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    /// The `text` key may be absent altogether, not just null.
    #[test]
    fn a_paragraph_missing_its_text_key_is_empty() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "name": "1", "markups": [] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => assert!(inline.is_empty()),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_paragraph_type_is_skipped() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("P", "kept", "1"),
            paragraph_json("SOMETHING_NEW", "dropped", "2"),
            paragraph_json("P", "also kept", "3"),
        ]));
        assert_eq!(rendered_types(json), ["paragraph", "paragraph"]);
    }

    /// The loop must not stall on a skipped paragraph.
    #[test]
    fn skipping_a_paragraph_still_advances() {
        let json = with_paragraphs(serde_json::json!([
            paragraph_json("SOMETHING_NEW", "dropped", "1"),
            paragraph_json("P", "kept", "2"),
        ]));
        assert_eq!(rendered_types(json), ["paragraph"]);
    }

    /// A paragraph `name` is unquoted in the heading template, so it must
    /// survive verbatim.
    #[test]
    fn a_heading_keeps_its_paragraph_name() {
        let json = with_paragraphs(serde_json::json!([paragraph_json("H2", "Two", "n123")]));
        match &blocks_of(json)[0] {
            Block::Heading { id, .. } => assert_eq!(id, "n123"),
            other => panic!("expected a heading, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_payload_is_an_empty_document() {
        let document = document(serde_json::json!({}));
        assert!(document.blocks.is_empty());
        assert_eq!(document.meta.title, "");
    }

    /// Curly quotes are folded to ASCII before the offsets are resolved, and
    /// the fold is one character for one, so Medium's indices stay valid.
    #[test]
    fn curly_quotes_are_folded_before_offsets_resolve() {
        let json = with_paragraphs(serde_json::json!([
            { "type": "P", "text": "\u{201c}abcdef\u{201d}", "name": "1", "markups": [
                { "type": "STRONG", "start": 1, "end": 7 }
            ] }
        ]));
        match &blocks_of(json)[0] {
            Block::Paragraph { inline, .. } => match inline.as_slice() {
                [
                    Inline::Text(open),
                    Inline::Strong(inner),
                    Inline::Text(close),
                ] => {
                    assert_eq!((open.as_str(), close.as_str()), ("\"", "\""));
                    assert!(matches!(inner.as_slice(), [Inline::Text(t)] if t == "abcdef"));
                }
                other => panic!("expected three nodes, got {other:?}"),
            },
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }
}
