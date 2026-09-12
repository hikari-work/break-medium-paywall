//! [`Document`] → [`PostDto`]: the projection that turns internal types into the
//! public contract.
//!
//! # Why the mapping lives here and not in `freedium-dto`
//!
//! `freedium-dto` has no `serde_json` (see its `Cargo.toml`), so its types cannot
//! be built from Medium's raw payload. That is deliberate — the *shape* of the
//! contract must not be able to follow the shape of the payload. This module is
//! the one place the two meet, and it is one crate away so that adding
//! `serde_json` there stays impossible rather than merely discouraged.
//!
//! # Every string here is plain text
//!
//! Nothing comes from [`PostMetadata`](medium_doc::metadata::PostMetadata): those
//! fields are pre-escaped for HTML and its `description` is escaped *twice*
//! (`&#39;` → `&amp;#39;`), which is a rendering artefact rather than a value.
//! The DTO's strings come from the payload directly, so `it's` stays `it's`.
//!
//! # What the contract deliberately drops
//!
//! | internal | dropped because |
//! |---|---|
//! | [`HeadingSpacing`], [`ParagraphMargin`] | Tailwind class names are a rendering detail, not data |
//! | `Block::Image`'s Medium id | the DTO carries the full URL, which a client cannot reconstruct |
//! | `codeBlockMetadata.mode` | the HTML renderer never reads it either |
//!
//! Each of those is a field the HTML renderer consumes. None is information a
//! consumer of `/api/v1` can act on.

use freedium_dto::{
    BlockDto, CollectionDto, CreatorDto, ImageDto, InlineDto, MetaDto, PostDto, QuoteStyleDto,
    TagDto,
};
use medium_doc::ir::{Block, Document, Inline, QuoteStyle};
use medium_doc::parse::{Post, PostPayload};

/// The `resize:fit:700` base for every `<img>` in the article.
///
/// **Not the only place this string appears**, deliberately: `html.rs:246` builds
/// the same URL by concatenation and `templates/homepage.html:7` spells it again.
/// Neither is switched over to this constant here, because the HTML side is under
/// a byte-parity gate and a refactor there is churn with a diff to re-verify for
/// no behaviour change. The convergence is post-cutover work.
/// `an_image_url_is_the_same_base_as_the_html_renderer` is what keeps the two from
/// drifting in the meantime: it compares this constant against rendered HTML.
pub const MIRO_IMAGE_BASE: &str = "https://miro.medium.com/v2/resize:fit:700/";

/// `textwrap.shorten`'s width and placeholder, matching `metadata.rs:52-53`.
///
/// Duplicated rather than shared: `metadata` measures the **escaped** subtitle
/// (so its cut point moves when the text contains quotes) and this measures the
/// plain one. Making the constant public would suggest the two agree on the
/// input, which is exactly what they do not.
/// `the_description_matches_the_page_for_plain_text` pins the case where they must.
const DESCRIPTION_WIDTH: usize = 100;
const DESCRIPTION_PLACEHOLDER: &str = "...";

/// The `resize:fit:320` base for an embed's thumbnail.
///
/// A **different size** from [`MIRO_IMAGE_BASE`], because `html.rs:203` renders
/// the embed's `background-image` at 320 and its `<img>` elements at 700.
/// Collapsing the two would silently change every embed's thumbnail.
pub const MIRO_THUMBNAIL_BASE: &str = "https://miro.medium.com/v2/resize:fit:320/";

/// A whole post, from the IR plus the payload it was parsed from.
///
/// Both inputs are needed and neither is redundant. The **IR** is the only place
/// the block tree exists — `Document` is what Fase 1 built so that every output
/// format renders from one structure. The **payload** is the only place the
/// *unescaped* metadata exists, and the DTO's whole point is that it does not
/// inherit `PostMetadata`'s HTML escaping.
///
/// `post_id` comes from the caller for the same reason `metadata::from_payload`
/// takes one: Medium's `Post` has no id field (the GraphQL query does not select
/// one) and the path segment is what the cache keys were built from.
#[must_use]
pub fn to_dto(document: &Document, payload: &PostPayload, post_id: &str) -> PostDto {
    PostDto::new(
        meta_dto(&payload.post(), post_id),
        blocks_dto(&document.blocks),
    )
}

/// The metadata alone, without parsing and without a [`Document`].
///
/// This is what makes `/meta` and `/feed` cheap: neither has to build an IR or
/// touch a template, so `/feed` can project twenty Postgres rows with no parser
/// in the path at all.
#[must_use]
pub fn meta_dto(post: &Post, post_id: &str) -> MetaDto {
    MetaDto {
        schema_version: freedium_dto::SCHEMA_VERSION,
        post_id: post_id.to_string(),
        // Raw, **not** `document.meta.title`: the parse-time de-duplication pass
        // can replace a title with the paragraph that repeated it, and the
        // legacy renderer discards that in favour of the raw value
        // (`core.py:758-784`). The API takes the same source as the page's
        // `<title>` so the two never disagree about a post's name.
        title: post.title.clone(),
        // Only the presence of the `previewContent` object is observable:
        // `nullable_string` has already collapsed an absent subtitle, a null one
        // and an empty one into `""`. So `None` means "no previewContent" and
        // `Some("")` means "one of those three" — which is the whole signal the
        // payload still carries.
        subtitle: post
            .preview_content
            .as_ref()
            .map(|content| content.subtitle.clone()),
        description: description_of(post),
        preview_image_url: post
            .preview_image
            .as_ref()
            .and_then(|image| image_url(MIRO_IMAGE_BASE, &image.id)),
        reading_time_minutes: post.reading_time.map_or(0, |minutes| minutes.ceil() as u32),
        is_locked: post.is_locked,
        medium_url: non_empty(&post.medium_url),
        first_published_at_unix_ms: post.first_published_at,
        updated_at_unix_ms: post.updated_at,
        tags: post
            .tags
            .iter()
            .map(|tag| TagDto {
                display_title: tag.display_title.clone(),
                slug: tag.normalized_tag_slug.clone(),
            })
            .collect(),
        creator: creator_dto(&post.creator),
        collection: collection_dto(&post.collection),
    }
}

/// The body, block by block.
#[must_use]
pub fn blocks_dto(blocks: &[Block]) -> Vec<BlockDto> {
    blocks.iter().map(block_dto).collect()
}

/// One block, with everything the HTML renderer reads that is not data dropped.
fn block_dto(block: &Block) -> BlockDto {
    match block {
        Block::Heading {
            level,
            id,
            inline,
            spacing: _,
        } => {
            let mut text = String::new();
            flatten_inline(inline, &mut text);
            BlockDto::Heading {
                level: *level,
                id: id.clone(),
                // The contract's one derived field, and it has a named consumer:
                // a table of contents is exactly `{id, text}`. The alternative is
                // every consumer walking `content` to flatten it, each with its
                // own idea of what to do with a nested `Link`.
                text,
                content: inlines_dto(inline),
            }
        }
        Block::Paragraph {
            inline,
            drop_cap,
            margin: _,
        } => BlockDto::Paragraph {
            content: inlines_dto(inline),
            drop_cap: *drop_cap,
        },
        Block::List { ordered, items } => BlockDto::List {
            ordered: *ordered,
            items: items.iter().map(|item| inlines_dto(item)).collect(),
        },
        Block::Code { lang, lines } => BlockDto::Code {
            language: lang.clone(),
            lines: lines.iter().map(|line| code_line(line)).collect(),
        },
        Block::BlockQuote { style, inline } => BlockDto::Blockquote {
            style: match style {
                QuoteStyle::Inset => QuoteStyleDto::Inset,
                QuoteStyle::Pull => QuoteStyleDto::Pull,
            },
            content: inlines_dto(inline),
        },
        Block::Image { id, alt, caption } => BlockDto::Image {
            url: format!("{MIRO_IMAGE_BASE}{id}"),
            alt: alt.clone(),
            caption: caption.as_ref().map(|caption| inlines_dto(caption)),
        },
        Block::ImageRow(images) => BlockDto::ImageRow {
            images: images.iter().filter_map(image_of).collect(),
        },
        Block::Embed {
            url,
            title,
            description,
            site,
            thumbnail_id,
        } => BlockDto::Embed {
            url: url.clone(),
            title: title.clone(),
            description: description.clone(),
            site: site.clone(),
            // `html.rs:204` emits the prefix with `unwrap_or("")`, so a post
            // without a thumbnail renders `url('https://…fit:320/')` — a
            // background-image pointing at a directory. The contract does not
            // reproduce that: `None` is the honest value, and a consumer that
            // wants the legacy's broken URL can build it.
            thumbnail_url: thumbnail_id
                .as_deref()
                .and_then(|id| image_url(MIRO_THUMBNAIL_BASE, id)),
        },
        Block::Iframe { src, dims } => BlockDto::Iframe {
            src: src.clone(),
            // `None` rather than `100`: the HTML renderer's no-dimensions branch
            // writes `width="100%"`, which is a *placeholder*, not a measurement
            // of 100 pixels. A consumer sizing a frame needs to tell those apart.
            width: dims.map(|(width, _)| width),
            height: dims.map(|(_, height)| height),
        },
    }
}

/// One entry of a row, or `None` for a block that is not an image.
///
/// `parse` only ever builds rows out of images — `html.rs:174-176` says so and
/// renders anything else through as a fallback for a bug that should not happen.
/// A DTO has nowhere to put a non-image inside `images`, so it is dropped and
/// logged rather than silently reinterpreted as one.
fn image_of(block: &Block) -> Option<ImageDto> {
    match block {
        Block::Image { id, alt, .. } => Some(ImageDto {
            url: format!("{MIRO_IMAGE_BASE}{id}"),
            alt: alt.clone(),
        }),
        other => {
            tracing::debug!(
                kind = block_kind(other),
                "a non-image block is inside an image row; the DTO drops it"
            );
            None
        }
    }
}

/// A block's short name, for logs. `Block` has no `Debug`-stable discriminant
/// that reads well, and this is the vocabulary `parse`'s own test helper uses.
const fn block_kind(block: &Block) -> &'static str {
    match block {
        Block::Heading { .. } => "heading",
        Block::Paragraph { .. } => "paragraph",
        Block::List { .. } => "list",
        Block::Code { .. } => "code",
        Block::BlockQuote { .. } => "blockquote",
        Block::Image { .. } => "image",
        Block::ImageRow(_) => "image-row",
        Block::Embed { .. } => "embed",
        Block::Iframe { .. } => "iframe",
    }
}

/// The plain-text rendering of a `PRE` line, with any markup logged.
///
/// The IR holds `Vec<Vec<Inline>>` because the legacy escapes each paragraph
/// separately before joining (`core.py:502-513`). A code block's content is plain
/// text, so the markup is flattened — but if one ever appeared, the HTML renderer
/// would emit `<strong>` where this emits `strong`, and that difference should
/// not be discovered by a consumer diffing the two endpoints.
fn code_line(line: &[Inline]) -> String {
    let mut text = String::new();
    for node in line {
        if !node.is_plain_text() {
            tracing::debug!(
                node = ?node,
                "a code block's line carries inline markup; the DTO keeps its text only"
            );
        }
        flatten_inline(std::slice::from_ref(node), &mut text);
    }
    text
}

fn inlines_dto(nodes: &[Inline]) -> Vec<InlineDto> {
    nodes.iter().map(inline_dto).collect()
}

fn inline_dto(node: &Inline) -> InlineDto {
    match node {
        Inline::Text(text) => InlineDto::Text { text: text.clone() },
        Inline::Strong(children) => InlineDto::Strong {
            content: inlines_dto(children),
        },
        Inline::Emphasis(children) => InlineDto::Emphasis {
            content: inlines_dto(children),
        },
        Inline::Code(children) => InlineDto::Code {
            content: inlines_dto(children),
        },
        Inline::Link {
            href,
            rel,
            title,
            new_tab,
            children,
        } => InlineDto::Link {
            href: href.clone(),
            rel: rel.clone(),
            title: title.clone(),
            new_tab: *new_tab,
            content: inlines_dto(children),
        },
        Inline::UserMention { user_id, children } => InlineDto::UserMention {
            user_id: user_id.clone(),
            content: inlines_dto(children),
        },
        Inline::Highlight(children) => InlineDto::Highlight {
            content: inlines_dto(children),
        },
    }
}

/// Every descendant `Inline::Text`, concatenated.
///
/// Recurses through the wrappers *and* through `Link`/`UserMention`, so a heading
/// whose text is a link still yields its label. Nothing is inserted between
/// nodes: the IR's sequence already is the text, and a separator would invent one.
fn flatten_inline(nodes: &[Inline], out: &mut String) {
    for node in nodes {
        match node {
            Inline::Text(text) => out.push_str(text),
            Inline::Strong(children)
            | Inline::Emphasis(children)
            | Inline::Code(children)
            | Inline::Highlight(children) => flatten_inline(children, out),
            Inline::Link { children, .. } | Inline::UserMention { children, .. } => {
                flatten_inline(children, out);
            }
        }
    }
}

/// The short summary: the subtitle, shortened on word boundaries.
///
/// Plain text, so the width is measured against what the author wrote. The page's
/// `description` measures the *escaped* subtitle, which costs columns —
/// `metadata.rs`'s tests pin that 14 escaped quotes are 97 of the 100. Both are
/// correct for their consumer, and neither is a bug in the other.
fn description_of(post: &Post) -> String {
    let subtitle = post
        .preview_content
        .as_ref()
        .map_or("", |content| content.subtitle.as_str());
    medium_doc::textwrap::shorten(subtitle, DESCRIPTION_WIDTH, DESCRIPTION_PLACEHOLDER)
}

/// `Some` for a non-empty string, `None` otherwise.
///
/// `nullable_string` makes "absent" and "empty" the same thing before this module
/// ever sees them, so an empty `mediumUrl` cannot be told from a missing one.
/// Folding it to `None` is the choice that keeps `Option` meaning "there isn't
/// one" rather than "there might be an empty one".
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// The `miro.medium.com` URL for an image id, or `None` when there is no id.
///
/// Every image URL in the contract goes through here, which is what keeps the
/// base from being spelled four different ways — and, more to the point, what
/// keeps the embed's `<img>`-sized URLs from being built with the 320 base or the
/// thumbnail with the 700 one. Both are `miro.medium.com` URLs of the same shape,
/// so the mistake is invisible without a test (`an_embed_thumbnail_uses_the_320_base`).
///
/// The id itself is opaque and already contains slashes (`1*abc.png`), so there is
/// no escaping to do.
fn image_url(base: &str, id: &str) -> Option<String> {
    (!id.is_empty()).then(|| format!("{base}{id}"))
}

/// `post.creator`, projected field by field.
///
/// `None` when the payload has no creator object **or** one with no `id`: the id
/// is the only field a consumer can key on, and a creator record without one
/// identifies nothing. The remaining fields fall back to the empty string, so a
/// client needs one branch rather than two.
///
/// The profile URL is deliberately absent: it is `https://medium.com/@{username}`,
/// which a client can build. The image URL is here for the opposite reason — the
/// resize parameters are not reconstructible.
fn creator_dto(creator: &serde_json::Value) -> Option<CreatorDto> {
    let id = creator.get("id")?.as_str()?;
    let image_id = string_field(creator, "imageId");
    Some(CreatorDto {
        id: id.to_string(),
        name: string_field(creator, "name"),
        username: string_field(creator, "username"),
        bio: string_field(creator, "bio"),
        image_url: image_url(MIRO_IMAGE_BASE, &image_id),
    })
}

/// `post.collection`, projected field by field.
///
/// Like a creator, a collection with no `id` identifies nothing and is `None`.
/// `shortDescription` becomes `description` because the field it lands in is the
/// contract's, and the payload's longer `description` (the publication's full
/// blurb) is a different value the page does not use either.
fn collection_dto(collection: &serde_json::Value) -> Option<CollectionDto> {
    let id = collection.get("id")?.as_str()?;
    Some(CollectionDto {
        id: id.to_string(),
        name: string_field(collection, "name"),
        slug: string_field(collection, "slug"),
        description: string_field(collection, "shortDescription"),
        avatar_url: collection
            .get("avatar")
            .and_then(|avatar| avatar.get("id"))
            .and_then(serde_json::Value::as_str)
            .and_then(|id| image_url(MIRO_IMAGE_BASE, id)),
    })
}

/// A string field of a raw JSON object, or the empty string.
///
/// A non-string (`name: 1`) is also the empty string rather than a parse failure:
/// the DTO's job is to serve the article, and a mistyped byline field costs the
/// byline, not the response — the same trade `metadata::from_payload` makes for
/// the page.
fn string_field(value: &serde_json::Value, field: &str) -> String {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use medium_doc::{metadata, parse};
    use serde_json::{Value, json};

    /// A `data.post` carrying one of everything `meta_dto` reads.
    fn post_json() -> Value {
        json!({
            "title": "A post",
            "previewContent": { "subtitle": "a subtitle" },
            "previewImage": { "id": "1*preview.png" },
            "mediumUrl": "https://medium.com/@ada/a-post-0291df856c77",
            "readingTime": 4.2,
            "isLocked": false,
            "updatedAt": 1_692_000_000_000i64,
            "firstPublishedAt": 1_690_000_000_000i64,
            "creator": { "id": "c1", "name": "Ada", "username": "ada", "bio": "b", "imageId": "1*c.png" },
            "collection": { "id": "p1", "name": "Coll", "slug": "coll", "shortDescription": "d",
                            "avatar": { "id": "1*col.png" } },
            "tags": [
                { "displayTitle": "Rust", "normalizedTagSlug": "rust" },
                { "displayTitle": "Web" }
            ],
            "content": { "bodyModel": { "paragraphs": [] } }
        })
    }

    fn payload_of(post: Value) -> PostPayload {
        PostPayload::from_value(json!({ "data": { "post": post } }))
            .expect("the fixture is a valid payload")
    }

    /// [`post_json`] with `content.bodyModel.paragraphs` replaced.
    fn with_body(mut post: Value, body: Value) -> Value {
        post.as_object_mut()
            .expect("the fixture is an object")
            .insert(
                "content".into(),
                json!({ "bodyModel": { "paragraphs": body } }),
            );
        post
    }

    fn dto_of(post: Value) -> PostDto {
        let payload = payload_of(post);
        to_dto(
            &parse::parse(&payload, "https://freedium.test"),
            &payload,
            "0291df856c77",
        )
    }

    /// **The escaping gate, stated as contract vs page.**
    ///
    /// The same subtitle the page renders as `it&#39s` — and whose description it
    /// renders as `it&amp;#39s` — must reach a JSON client as `it's`. If a future
    /// edit routes the DTO through `PostMetadata` to "reuse" the shortening logic,
    /// this is what fails.
    #[test]
    fn an_apostrophe_stays_an_apostrophe_where_the_page_escapes_it() {
        let payload = payload_of(with_body(
            json!({ "title": "T", "previewContent": { "subtitle": "it's" } }),
            json!([]),
        ));

        let page = metadata::from_payload(&payload, "id");
        assert_eq!(page.subtitle, "it&#39s");
        assert_eq!(page.description, "it&amp;#39s");

        let dto = meta_dto(&payload.post(), "id");
        assert_eq!(dto.subtitle.as_deref(), Some("it's"));
        assert_eq!(dto.description, "it's");
    }

    /// Where the two shortenings *must* agree: plain text, where escaping is the
    /// identity function and a column is a column for both.
    ///
    /// This is what keeps the duplicated `DESCRIPTION_WIDTH` honest. It cannot
    /// detect a wrong width in the abstract, but it fails if either side changes
    /// the width, the placeholder or the algorithm without the other.
    #[test]
    fn the_description_matches_the_page_for_plain_text() {
        let subtitles = [
            String::new(),
            "short".to_string(),
            "alpha ".repeat(15),
            "x".repeat(101),
            "alpha ".repeat(15) + "super-duper-hyphenated-word-here",
        ];

        for subtitle in &subtitles {
            let payload = payload_of(with_body(
                json!({ "title": "T", "previewContent": { "subtitle": subtitle } }),
                json!([]),
            ));
            assert_eq!(
                meta_dto(&payload.post(), "id").description,
                metadata::from_payload(&payload, "id").description,
                "subtitle = {subtitle:?}"
            );
        }
    }

    /// The description is the subtitle and nothing else — not the first
    /// paragraph and not the title.
    #[test]
    fn the_description_is_the_shortened_subtitle() {
        let dto = dto_of(with_body(
            json!({ "title": "T", "previewContent": { "subtitle": "word ".repeat(40) } }),
            json!([]),
        ));
        assert!(
            dto.meta.description.ends_with("..."),
            "{}",
            dto.meta.description
        );
        assert!(
            dto.meta.description.chars().count() <= DESCRIPTION_WIDTH,
            "{}",
            dto.meta.description
        );
        assert!(!dto.meta.description.contains('T'));

        let none = dto_of(with_body(json!({ "title": "T" }), json!([])));
        assert_eq!(none.meta.description, "");
        assert_eq!(none.meta.subtitle, None);
    }

    /// `Some("")` and `None` are different, and the payload can express both: no
    /// `previewContent` at all, versus one carrying an empty subtitle.
    #[test]
    fn an_empty_subtitle_is_not_an_absent_one() {
        let absent = dto_of(with_body(json!({ "title": "T" }), json!([])));
        assert_eq!(absent.meta.subtitle, None);

        let empty = dto_of(with_body(
            json!({ "title": "T", "previewContent": { "subtitle": "" } }),
            json!([]),
        ));
        assert_eq!(empty.meta.subtitle.as_deref(), Some(""));
    }

    /// The id is the caller's. Medium's `Post` has no id field at all, so a DTO
    /// that read one would be reading nothing.
    #[test]
    fn the_post_id_comes_from_the_caller() {
        let mut post = post_json();
        post.as_object_mut()
            .expect("an object")
            .insert("id".into(), json!("api-id"));
        let payload = payload_of(post);
        let dto = to_dto(&parse::parse(&payload, "h"), &payload, "resolved-id");
        assert_eq!(dto.meta.post_id, "resolved-id");
    }

    /// `math.ceil`, matching `metadata::from_payload` — 4.2 minutes is 5.
    #[test]
    fn reading_time_rounds_up() {
        let dto = dto_of(with_body(
            json!({ "title": "T", "readingTime": 4.2 }),
            json!([]),
        ));
        assert_eq!(dto.meta.reading_time_minutes, 5);

        let exact = dto_of(with_body(
            json!({ "title": "T", "readingTime": 4.0 }),
            json!([]),
        ));
        assert_eq!(
            exact.meta.reading_time_minutes, 4,
            "a whole number is not 5"
        );
    }

    /// The payload's own polarity, never the legacy's inverted `free_access`.
    #[test]
    fn is_locked_is_not_inverted() {
        let locked = dto_of(with_body(
            json!({ "title": "T", "isLocked": true }),
            json!([]),
        ));
        assert!(locked.meta.is_locked);

        let open = dto_of(with_body(
            json!({ "title": "T", "isLocked": false }),
            json!([]),
        ));
        assert!(!open.meta.is_locked);
    }

    /// The image URL is the `resize:fit:700` one the HTML renderer emits, because
    /// a client cannot build it — which is why it is in the contract at all.
    #[test]
    fn an_image_url_is_the_same_base_as_the_html_renderer() {
        let payload = payload_of(with_body(
            json!({ "title": "T" }),
            json!([{ "type": "IMG", "metadata": { "id": "1*abc.png" } }]),
        ));
        let document = parse::parse(&payload, "h");

        let BlockDto::Image { url, .. } = &blocks_dto(&document.blocks)[0] else {
            panic!("an IMG paragraph is an image block");
        };
        assert_eq!(url, &format!("{MIRO_IMAGE_BASE}1*abc.png"));

        let html = crate::html::render_block(&document.blocks[0]);
        assert!(
            html.contains(url),
            "the HTML renderer emits a different URL than the DTO: {html}"
        );
    }

    /// The embed's thumbnail is the **320** base, not the 700 one. Both are
    /// `miro.medium.com` URLs of the same shape, so a copy-paste between the two
    /// functions is invisible without this.
    #[test]
    fn an_embed_thumbnail_uses_the_320_base() {
        let dto = dto_of(with_body(
            json!({ "title": "T" }),
            json!([{
                "type": "MIXTAPE_EMBED",
                "text": "Example An Article A description",
                "name": "1",
                "mixtapeMetadata": {
                    "href": "https://www.example.com/post",
                    "thumbnailImageId": "1*t.png"
                },
                "markups": [
                    { "type": "A", "anchorType": "LINK", "start": 0, "end": 7 },
                    { "type": "A", "anchorType": "LINK", "start": 8, "end": 18 },
                    { "type": "A", "anchorType": "LINK", "start": 19, "end": 32 }
                ]
            }]),
        ));

        let Some(BlockDto::Embed {
            thumbnail_url,
            title,
            site,
            ..
        }) = dto.blocks.first()
        else {
            panic!("a mixtape paragraph is an embed: {:?}", dto.blocks);
        };
        assert_eq!(title, "An Article");
        assert_eq!(site, "example.com");
        assert_eq!(
            thumbnail_url.as_deref(),
            Some("https://miro.medium.com/v2/resize:fit:320/1*t.png")
        );
        assert_ne!(
            thumbnail_url.as_deref(),
            Some(&*format!("{MIRO_IMAGE_BASE}1*t.png")),
            "the embed thumbnail is not the image size"
        );
    }

    /// A row becomes `imageRow.images`, and a block that is not an image is
    /// dropped rather than misread as one.
    #[test]
    fn a_row_becomes_images_and_drops_anything_else() {
        let row = Block::ImageRow(vec![
            Block::Image {
                id: "1*a.png".into(),
                alt: "a".into(),
                caption: None,
            },
            Block::Paragraph {
                inline: vec![Inline::Text("stray".into())],
                drop_cap: false,
                margin: medium_doc::ir::ParagraphMargin::Mt7,
            },
            Block::Image {
                id: "1*b.png".into(),
                alt: String::new(),
                caption: None,
            },
        ]);

        let BlockDto::ImageRow { images } = block_dto(&row) else {
            panic!("an image row maps to an image row");
        };
        assert_eq!(images.len(), 2, "the paragraph is not an image");
        assert_eq!(images[0].url, format!("{MIRO_IMAGE_BASE}1*a.png"));
        assert_eq!(images[0].alt, "a");
        assert_eq!(images[1].url, format!("{MIRO_IMAGE_BASE}1*b.png"));
    }

    /// The heading's `text` carries its link labels, not just its bare runs —
    /// otherwise a table of contents would drop every linked heading's words.
    #[test]
    fn a_headings_text_flattens_nested_markup() {
        let heading = Block::Heading {
            level: 2,
            id: "h1".into(),
            spacing: medium_doc::ir::HeadingSpacing::Pt12,
            inline: vec![
                Inline::Text("See ".into()),
                Inline::Link {
                    href: "https://x.test".into(),
                    rel: "noopener".into(),
                    title: String::new(),
                    new_tab: true,
                    children: vec![Inline::Text("this".into())],
                },
                Inline::Text(" now".into()),
                Inline::Strong(vec![Inline::Text("!".into())]),
            ],
        };

        let BlockDto::Heading {
            text,
            content,
            level,
            id,
        } = block_dto(&heading)
        else {
            panic!("a heading maps to a heading");
        };
        assert_eq!(text, "See this now!");
        assert_eq!(level, 2);
        assert_eq!(id, "h1");
        assert_eq!(
            content.len(),
            4,
            "the tree is preserved, not flattened away"
        );
    }

    /// The code block's lines keep their per-line split, which is the whole reason
    /// the IR stores `Vec<Vec<Inline>>`: joining earlier would move characters
    /// across an escaping boundary — and, here, across lines.
    #[test]
    fn code_lines_stay_on_their_own_lines() {
        let code = Block::Code {
            lang: Some("python".into()),
            lines: vec![
                vec![Inline::Text("x = 1".into())],
                vec![Inline::Text("y = 2".into())],
            ],
        };

        let BlockDto::Code { language, lines } = block_dto(&code) else {
            panic!("a code block maps to a code block");
        };
        assert_eq!(language.as_deref(), Some("python"));
        assert_eq!(lines, vec!["x = 1".to_string(), "y = 2".to_string()]);
    }

    /// A markup inside a `PRE` line is flattened, and its text survives — the HTML
    /// renderer would emit `<strong>` here, and losing the word entirely would be
    /// worse than losing the emphasis.
    #[test]
    fn a_markup_inside_a_code_line_keeps_its_text() {
        let code = Block::Code {
            lang: None,
            lines: vec![vec![
                Inline::Text("a".into()),
                Inline::Strong(vec![Inline::Text("b".into())]),
            ]],
        };
        let BlockDto::Code { lines, .. } = block_dto(&code) else {
            panic!("a code block maps to a code block");
        };
        assert_eq!(lines, vec!["ab".to_string()]);
    }

    /// An iframe without dimensions keeps `None`, not the HTML renderer's `100`
    /// placeholder — a consumer sizing a frame must be able to tell them apart.
    #[test]
    fn an_iframe_without_dimensions_is_null_not_a_hundred() {
        let sized = Block::Iframe {
            src: "s".into(),
            dims: Some((640, 360)),
        };
        let BlockDto::Iframe { width, height, .. } = block_dto(&sized) else {
            panic!("an iframe maps to an iframe");
        };
        assert_eq!((width, height), (Some(640), Some(360)));

        let no_dims = Block::Iframe {
            src: "s".into(),
            dims: None,
        };
        let BlockDto::Iframe { width, height, .. } = block_dto(&no_dims) else {
            panic!("an iframe maps to an iframe");
        };
        assert_eq!((width, height), (None, None));
        assert_ne!(width, Some(100), "100% is a placeholder, not a measurement");
    }

    /// Both blockquote shapes reach the contract with their own name — the HTML
    /// renderer picks a structurally different element for each, so collapsing
    /// them would lose a distinction the page makes.
    #[test]
    fn both_quote_styles_survive() {
        for (style, expected) in [
            (QuoteStyle::Inset, QuoteStyleDto::Inset),
            (QuoteStyle::Pull, QuoteStyleDto::Pull),
        ] {
            let block = Block::BlockQuote {
                style,
                inline: vec![Inline::Text("q".into())],
            };
            let BlockDto::Blockquote { style, .. } = block_dto(&block) else {
                panic!("a blockquote maps to a blockquote");
            };
            assert_eq!(style, expected);
        }
    }

    /// A tag's slug survives, and an absent one is `None` rather than `""`.
    ///
    /// This is the gap that made `TagDto.slug` unimplementable until `parse::Tag`
    /// grew `normalized_tag_slug`: `Post::tags` used to carry only `displayTitle`,
    /// so this field could only ever have been `None`.
    #[test]
    fn a_tag_carries_its_slug_when_the_payload_has_one() {
        let dto = dto_of(with_body(post_json(), json!([])));
        assert_eq!(dto.meta.tags.len(), 2);
        assert_eq!(dto.meta.tags[0].display_title, "Rust");
        assert_eq!(dto.meta.tags[0].slug.as_deref(), Some("rust"));
        assert_eq!(dto.meta.tags[1].display_title, "Web");
        assert_eq!(
            dto.meta.tags[1].slug, None,
            "an absent slug is not an empty one"
        );
    }

    /// The creator and collection are projected into typed fields, and the
    /// avatar/image URLs get the same base as everything else.
    #[test]
    fn the_creator_and_collection_are_projected() {
        let dto = dto_of(with_body(post_json(), json!([])));

        let creator = dto.meta.creator.expect("the fixture has a creator");
        assert_eq!(creator.id, "c1");
        assert_eq!(creator.name, "Ada");
        assert_eq!(creator.username, "ada");
        assert_eq!(creator.bio, "b");
        assert_eq!(creator.image_url, Some(format!("{MIRO_IMAGE_BASE}1*c.png")));

        let collection = dto.meta.collection.expect("the fixture has a collection");
        assert_eq!(collection.name, "Coll");
        assert_eq!(collection.slug, "coll");
        assert_eq!(collection.description, "d");
        assert_eq!(
            collection.avatar_url,
            Some(format!("{MIRO_IMAGE_BASE}1*col.png"))
        );
    }

    /// A creator or collection with no `id` identifies nothing, so it is `None` —
    /// and so is an absent one. Jinja would render the page's byline anyway; a
    /// typed contract has nothing to render it *into*.
    #[test]
    fn a_creator_or_collection_without_an_id_is_absent() {
        let payload = payload_of(with_body(
            json!({ "title": "T", "creator": { "name": "Ada" }, "collection": { "slug": "coll" } }),
            json!([]),
        ));
        let dto = meta_dto(&payload.post(), "id");
        assert!(
            dto.creator.is_none(),
            "a creator with no id is not a creator"
        );
        assert!(dto.collection.is_none());

        let missing = payload_of(with_body(json!({ "title": "T" }), json!([])));
        let dto = meta_dto(&missing.post(), "id");
        assert!(dto.creator.is_none());
        assert!(dto.collection.is_none());
    }

    /// An empty `mediumUrl` is `None`: `nullable_string` has already collapsed
    /// "absent" and "empty" by the time this module sees them.
    #[test]
    fn an_empty_medium_url_is_absent() {
        let empty = dto_of(with_body(
            json!({ "title": "T", "mediumUrl": "" }),
            json!([]),
        ));
        assert_eq!(empty.meta.medium_url, None);

        let some = dto_of(with_body(
            json!({ "title": "T", "mediumUrl": "https://medium.com/p/abc" }),
            json!([]),
        ));
        assert_eq!(
            some.meta.medium_url.as_deref(),
            Some("https://medium.com/p/abc")
        );
    }

    /// A post with no body still serialises every key — the API's promise that a
    /// consumer can destructure without a presence check.
    #[test]
    fn a_post_without_a_body_still_serialises_every_field() {
        let dto = dto_of(with_body(json!({ "title": "Only a title" }), json!([])));
        let value = serde_json::to_value(&dto).expect("the DTO serialises");

        assert!(dto.blocks.is_empty());
        assert_eq!(value["schema_version"], json!(freedium_dto::SCHEMA_VERSION));
        assert_eq!(value["meta"]["title"], json!("Only a title"));
        for key in [
            "subtitle",
            "preview_image_url",
            "creator",
            "collection",
            "medium_url",
        ] {
            assert!(
                value["meta"]
                    .get(key)
                    .is_some_and(serde_json::Value::is_null),
                "`{key}` is missing rather than null: {}",
                value["meta"]
            );
        }
    }

    /// A payload with `data.post` missing entirely — `PostPayload::post` returns a
    /// default with a warning — must produce an empty DTO, not a panic. The page
    /// takes the same path (`an_empty_envelope_produces_empty_metadata`).
    #[test]
    fn an_empty_envelope_produces_an_empty_dto() {
        let payload = PostPayload::from_value(json!({})).expect("an empty envelope parses");
        let dto = to_dto(&parse::parse(&payload, "h"), &payload, "abc123");

        assert_eq!(dto.meta.post_id, "abc123");
        assert_eq!(dto.meta.title, "");
        assert_eq!(dto.meta.description, "");
        assert_eq!(dto.meta.reading_time_minutes, 0);
        assert!(!dto.meta.is_locked);
        assert!(dto.meta.tags.is_empty());
        assert!(dto.blocks.is_empty());
    }

    /// Every block the IR can hold has a DTO counterpart, and the mapping is
    /// total — a new `Block` variant breaks this test rather than a consumer.
    #[test]
    fn every_block_variant_maps() {
        let blocks = vec![
            Block::Heading {
                level: 3,
                id: "h".into(),
                spacing: medium_doc::ir::HeadingSpacing::None,
                inline: vec![],
            },
            Block::Paragraph {
                inline: vec![],
                drop_cap: false,
                margin: medium_doc::ir::ParagraphMargin::Mt3,
            },
            Block::List {
                ordered: false,
                items: vec![],
            },
            Block::Code {
                lang: None,
                lines: vec![],
            },
            Block::BlockQuote {
                style: QuoteStyle::Inset,
                inline: vec![],
            },
            Block::Image {
                id: "i".into(),
                alt: String::new(),
                caption: None,
            },
            Block::ImageRow(vec![]),
            Block::Embed {
                url: "u".into(),
                title: "t".into(),
                description: "d".into(),
                site: "s".into(),
                thumbnail_id: None,
            },
            Block::Iframe {
                src: "s".into(),
                dims: None,
            },
        ];

        assert_eq!(blocks_dto(&blocks).len(), blocks.len());
    }
}
