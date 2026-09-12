//! `Document` → HTML fragments.
//!
//! Every block literal here is copied from `legacy/medium-parser/medium_parser/core.py`
//! at commit `75d1433`. The Tailwind class strings, the attribute order, and even
//! the inconsistent quoting (`class="..."` versus `class='...'`) are
//! load-bearing: the differential harness compares this output against the
//! Python renderer's byte for byte. The *inline* literals — `<strong>`, the
//! anchor, the `<mark>` — live in [`medium_doc::inline_html`], because the
//! parser needs them too.
//!
//! One structural difference from the legacy renderer is deliberate. Python
//! splices markup into an already-escaped string through a position matrix, so
//! a link that is interrupted by another markup is emitted as several adjacent
//! `<a>` elements with identical attributes and the interrupting markup hoisted
//! *outside* them:
//!
//! ```text
//! LINK[0,10) + STRONG[2,5)  ->  <a>ab</a><strong><a>cde</a></strong><a>fghij</a>
//! ```
//!
//! Rendering from the IR gives one `<a>` with the `<strong>` inside it. The two
//! are semantically identical — same target, same emphasised text — which is
//! why the harness normalises to a canonical form before comparing rather than
//! comparing markup.

use medium_doc::escape::{EscapeMode, mode_for};
use medium_doc::inline_html::render_inlines;
use medium_doc::ir::{Block, Document, ParagraphMargin, QuoteStyle};

/// Renders a document body to one HTML fragment per block.
///
/// The `Vec<String>` shape mirrors the legacy `out_paragraphs` list, so the
/// differential harness can compare element by element and report *which*
/// block diverged instead of just "the page differs".
pub fn render_blocks(document: &Document) -> Vec<String> {
    document.blocks.iter().map(render_block).collect()
}

/// Renders a single block.
pub fn render_block(block: &Block) -> String {
    let mut out = String::new();
    match block {
        Block::Heading {
            level,
            id,
            spacing,
            inline,
        } => {
            // `core.py:328-362`. The class list ends with a space before the
            // spacing class, so a heading with no spacing keeps a trailing
            // space inside the attribute. Reproduced as-is.
            let (tag, size) = match level {
                2 => ("h2", "text-1xl md:text-2xl"),
                3 => ("h3", "text-1xl md:text-2xl"),
                4 => ("h4", "text-l md:text-xl"),
                // Unreachable via `parse`, which only builds 2/3/4.
                _ => ("h4", "text-l md:text-xl"),
            };
            // `id={{ id }}` is unquoted in the source template.
            out.push_str(&format!(
                "<{tag} id={id} class=\"font-bold font-sans break-normal text-gray-900 \
                 dark:text-gray-100 {size} {}\">",
                spacing.class()
            ));
            render_inlines(&mut out, inline, EscapeMode::Full);
            out.push_str(&format!("</{tag}>"));
        }

        Block::Paragraph {
            inline,
            drop_cap,
            margin,
        } => {
            // `core.py:407-422`. Order matters: `leading-8`, then the drop-cap
            // utilities, then the margin.
            out.push_str("<p class=\"leading-8");
            if *drop_cap {
                out.push_str(
                    " first-letter:text-7xl first-letter:float-left first-letter:mr-2 \
                     first-letter:pt-2",
                );
            }
            out.push(' ');
            out.push_str(margin_class(*margin));
            out.push_str("\">");
            render_inlines(&mut out, inline, mode_for(inline));
            out.push_str("</p>");
        }

        Block::List { ordered, items } => {
            let (tag, class) = if *ordered {
                ("ol", "pl-8 mt-2 list-decimal")
            } else {
                ("ul", "pl-8 mt-2 list-disc")
            };
            out.push_str(&format!("<{tag} class=\"{class}\">"));
            for item in items {
                // `core.py:427` — single quotes around the class here.
                out.push_str("<li class='mt-3'>");
                render_inlines(&mut out, item, mode_for(item));
                out.push_str("</li>");
            }
            out.push_str(&format!("</{tag}>"));
        }

        Block::Code { lang, lines } => {
            // `core.py:478-483`. Code is always minimally escaped, and
            // `has_code_block` in `core.py:232` is what forces the same mode on
            // a paragraph that merely contains an inline `CODE`.
            let class = match lang {
                Some(lang) => format!("language-{lang}"),
                None => "nohighlight".to_string(),
            };
            out.push_str(
                "<pre class=\"flex flex-col justify-center border mt-7 \
                 dark:border-gray-700\"><code class=\"p-2 bg-gray-100 dark:bg-gray-900 \
                 overflow-x-auto ",
            );
            out.push_str(&class);
            out.push_str("\">");
            // `core.py:511-512` escapes each `PRE` paragraph and then joins the
            // results with a newline, so each line gets its own escaping pass
            // and its own `&` lookahead.
            for (index, line) in lines.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                render_inlines(&mut out, line, EscapeMode::Minimal);
            }
            out.push_str("</code></pre>");
        }

        Block::BlockQuote { style, inline } => match style {
            // `core.py:521-523`.
            QuoteStyle::Inset => {
                out.push_str(
                    "<blockquote style=\"box-shadow: inset 3px 0 0 0 rgb(209 207 239 / \
                     var(--tw-bg-opacity));\" class=\"px-5 pt-3 pb-3 mt-5\"><p \
                     class=\"font-italic\">",
                );
                render_inlines(&mut out, inline, mode_for(inline));
                out.push_str("</p></blockquote>");
            }
            // `core.py:528-530`.
            QuoteStyle::Pull => {
                out.push_str(
                    "<blockquote class=\"ml-5 text-2xl text-gray-600 mt-7 \
                     dark:text-gray-300\"><p>",
                );
                render_inlines(&mut out, inline, mode_for(inline));
                out.push_str("</p></blockquote>");
            }
        },

        Block::Image { id, alt, caption } => {
            render_image_element(&mut out, id, alt);
            if let Some(caption) = caption {
                // `core.py:368` — single quotes, and appended as its own list
                // entry in Python.
                out.push_str(
                    "<figcaption class='mt-3 text-sm text-center text-gray-500 \
                     dark:text-gray-200'>",
                );
                render_inlines(&mut out, caption, mode_for(caption));
                out.push_str("</figcaption>");
            }
        }

        Block::ImageRow(images) => {
            out.push_str("<div class=\"mx-5\"><div class=\"flex flex-row justify-center\">");
            for image in images {
                match image {
                    Block::Image { id, alt, .. } => render_image_element(&mut out, id, alt),
                    // `parse` only ever puts images in a row; anything else
                    // would be a bug in the row builder.
                    other => out.push_str(&render_block(other)),
                }
            }
            out.push_str("</div></div>");
        }

        Block::Embed {
            url,
            title,
            description,
            site,
            thumbnail_id,
        } => {
            // `core.py:536-554`. The source is a triple-quoted string that
            // begins with a newline, so the fragment does too. Every
            // interpolation uses the non-escaping environment.
            out.push_str(
                "\n<div class=\"items-center p-2 overflow-hidden border border-gray-300 mt-7\">\n    \
                 <a rel=\"noopener follow\" href=\"",
            );
            out.push_str(url);
            out.push_str("\" target=\"_blank\">\n        <div class=\"flex flex-row justify-between p-2 overflow-hidden\">\n            <div class=\"flex flex-col justify-center p-2\">\n                <h2 class=\"text-base font-bold text-black dark:text-gray-100\">");
            out.push_str(title);
            out.push_str("</h2>\n                <div class=\"block mt-2\">\n                    <h3 class=\"text-sm text-grey-darker\">");
            out.push_str(description);
            out.push_str("</h3>\n                </div>\n                <div class=\"mt-5\">\n                    <p class=\"text-xs text-grey-darker\">");
            out.push_str(site);
            out.push_str("</p>\n                </div>\n            </div>\n            <div class=\"relative flex h-40 flew-row w-60\">\n                <div class=\"absolute inset-0 bg-center bg-cover\" style=\"background-image: url('https://miro.medium.com/v2/resize:fit:320/");
            out.push_str(thumbnail_id.as_deref().unwrap_or(""));
            out.push_str("'); background-repeat: no-repeat;\" referrerpolicy=\"no-referrer\"></div>\n            </div>\n        </div>\n    </a>\n</div>");
        }

        Block::Iframe { src, dims } => match dims {
            // `core.py:654-658` — the wrapper is a nested `<div>` with a
            // newline and indentation before the `<iframe>`.
            Some((width, height)) => {
                out.push_str("<div class=\"mt-7\"><div>\n    <iframe class=\"w-full\" src=\"");
                out.push_str(src);
                out.push_str("\" referrerpolicy=\"no-referrer\" width=\"");
                out.push_str(&width.to_string());
                out.push_str("\" height=\"");
                out.push_str(&height.to_string());
                out.push_str(
                    "\" allowfullscreen=\"\" frameborder=\"0\" scrolling=\"no\"></iframe>\n</div></div>",
                );
            }
            // `core.py:668-669` — a different attribute order, no nested
            // wrapper, and `100%` placeholders.
            None => {
                out.push_str("<div class=\"mt-7\"><iframe class=\"w-full\" src=\"");
                out.push_str(src);
                out.push_str(
                    "\" width=\"100%\" height=\"100%\" referrerpolicy=\"no-referrer\" \
                     allowfullscreen=\"\" frameborder=\"0\" scrolling=\"no\"></iframe></div>",
                );
            }
        },
    }
    out
}

/// Renders the `<div class="mt-7"><img ...></div>` element shared by
/// `Block::Image` and `Block::ImageRow` (`core.py:364-366`).
///
/// `alt` is interpolated without escaping, exactly as the legacy template does,
/// and a null `alt` in the payload arrives here as the literal string `"None"`
/// because that is what Jinja renders for `None`.
fn render_image_element(out: &mut String, id: &str, alt: &str) {
    out.push_str("<div class=\"mt-7\"><img loading=\"eager\" alt=\"");
    out.push_str(alt);
    out.push_str("\" class=\"pt-5 m-auto\" role=\"presentation\" referrerpolicy=\"no-referrer\" src=\"https://miro.medium.com/v2/resize:fit:700/");
    out.push_str(id);
    out.push_str("\"></div>");
}

const fn margin_class(margin: ParagraphMargin) -> &'static str {
    match margin {
        ParagraphMargin::Mt3 => "mt-3",
        ParagraphMargin::Mt7 => "mt-7",
    }
}

#[cfg(test)]
mod tests {
    use super::render_block;
    use medium_doc::inline::{Markup, MarkupKind, build_inlines};
    use medium_doc::ir::{Block, Inline, ParagraphMargin, QuoteStyle};

    fn inline_text(text: &str) -> Vec<Inline> {
        vec![Inline::Text(text.into())]
    }

    fn paragraph(inline: Vec<Inline>, drop_cap: bool) -> Block {
        Block::Paragraph {
            inline,
            drop_cap,
            margin: ParagraphMargin::Mt7,
        }
    }

    fn link(href: &str) -> MarkupKind {
        MarkupKind::Link {
            href: href.into(),
            rel: "noopener".into(),
            title: "T".into(),
        }
    }

    #[test]
    fn paragraph_without_drop_cap() {
        let block = paragraph(vec![Inline::Text("Hello world".into())], false);
        assert_eq!(
            render_block(&block),
            "<p class=\"leading-8 mt-7\">Hello world</p>"
        );
    }

    /// Verified against the legacy renderer, including the class order.
    #[test]
    fn paragraph_with_drop_cap() {
        let block = paragraph(vec![Inline::Text("Hello".into())], true);
        assert_eq!(
            render_block(&block),
            "<p class=\"leading-8 first-letter:text-7xl first-letter:float-left \
             first-letter:mr-2 first-letter:pt-2 mt-7\">Hello</p>"
        );
    }

    #[test]
    fn text_is_escaped_by_default() {
        let block = paragraph(vec![Inline::Text("a & b <c> \"d\" 'e'".into())], false);
        assert!(render_block(&block).contains("a &amp; b &lt;c&gt; &quot;d&quot; &#39e&#39"));
    }

    /// A `CODE` markup switches the whole paragraph to minimal escaping.
    #[test]
    fn code_markup_forces_minimal_escaping_for_the_paragraph() {
        let block = paragraph(
            vec![
                Inline::Text("say \"hi\" then ".into()),
                Inline::Code(vec![Inline::Text("x < y".into())]),
            ],
            false,
        );
        let html = render_block(&block);
        assert!(
            html.contains("say \"hi\" then"),
            "quotes must survive: {html}"
        );
        assert!(html.contains("x &lt; y"), "angles still escaped: {html}");
        assert!(html.contains("<code class='p-1.5 bg-gray-300 dark:bg-gray-600'>"));
    }

    /// Verified against the legacy renderer: overlapping link and strong.
    #[test]
    fn overlapping_link_and_strong_nests_instead_of_splitting() {
        let block = paragraph(
            vec![Inline::Link {
                href: "https://x.test".into(),
                rel: String::new(),
                title: String::new(),
                new_tab: true,
                children: vec![
                    Inline::Text("ab".into()),
                    Inline::Strong(vec![Inline::Text("cde".into())]),
                    Inline::Text("fghij".into()),
                ],
            }],
            false,
        );
        assert_eq!(
            render_block(&block),
            "<p class=\"leading-8 mt-7\"><a style=\"text-decoration: underline;\" rel=\"\" \
             title=\"\" href=\"https://x.test\" target=\"_blank\">ab<strong>cde</strong>fghij\
             </a></p>"
        );
    }

    /// The anchor the legacy renderer emits, with `rel`/`title` from the payload.
    const LEGACY_ANCHOR: &str = "<a style=\"text-decoration: underline;\" rel=\"noopener\" \
                                  title=\"T\" href=\"https://x.test\" target=\"_blank\">";

    fn legacy_paragraph(text: &str, markups: &[Markup]) -> String {
        let block = paragraph(build_inlines(text, markups), false);
        let html = render_block(&block);
        html.strip_prefix("<p class=\"leading-8 mt-7\">")
            .and_then(|rest| rest.strip_suffix("</p>"))
            .expect("paragraph wrapper")
            .to_string()
    }

    /// Contained markup stays outer whichever position it holds in the array,
    /// so both payload orders render the same thing. The legacy output split
    /// the anchor in two and re-opened it three times; see
    /// `medium_doc::inline`'s module docs.
    #[test]
    fn link_and_strong_merge_into_one_anchor_in_either_order() {
        let expected = format!("{LEGACY_ANCHOR}ab<strong>cde</strong>fghij</a>");
        let link_first = legacy_paragraph(
            "abcdefghij",
            &[
                Markup {
                    start: 0,
                    end: 10,
                    kind: link("https://x.test"),
                },
                Markup {
                    start: 2,
                    end: 5,
                    kind: MarkupKind::Strong,
                },
            ],
        );
        let strong_first = legacy_paragraph(
            "abcdefghij",
            &[
                Markup {
                    start: 2,
                    end: 5,
                    kind: MarkupKind::Strong,
                },
                Markup {
                    start: 0,
                    end: 10,
                    kind: link("https://x.test"),
                },
            ],
        );
        assert_eq!(link_first, expected);
        assert_eq!(strong_first, expected);
    }

    /// Byte-identical to the legacy renderer, which produced
    /// `<strong><a ...>abcdefghij</a></strong>`.
    #[test]
    fn identical_ranges_wrap_the_anchor_in_the_strong() {
        let html = legacy_paragraph(
            "abcdefghij",
            &[
                Markup {
                    start: 0,
                    end: 10,
                    kind: link("https://x.test"),
                },
                Markup {
                    start: 0,
                    end: 10,
                    kind: MarkupKind::Strong,
                },
            ],
        );
        assert_eq!(
            html,
            format!("<strong>{LEGACY_ANCHOR}abcdefghij</a></strong>")
        );
    }

    /// Verified byte for byte against the legacy renderer.
    #[test]
    fn emoji_offsets_render_the_emoji_inside_the_markup() {
        let html = legacy_paragraph(
            "hi \u{1f600} there",
            &[Markup {
                start: 3,
                end: 5,
                kind: MarkupKind::Strong,
            }],
        );
        assert_eq!(html, "hi <strong>\u{1f600}</strong> there");
    }

    /// The legacy renderer escaped a paragraph in one pass and then spliced
    /// markup into it, so an entity was recognised across a markup boundary.
    /// Splitting the paragraph into one `Text` per segment must not lose that.
    /// Verified against the legacy renderer.
    #[test]
    fn entity_recognition_survives_a_markup_boundary() {
        // The `Text` before the markup ends in `&`; escaped on its own it would
        // become `&amp;`, which is not what the legacy renderer emitted.
        let html = legacy_paragraph(
            "x&amp;y",
            &[Markup {
                start: 2,
                end: 7,
                kind: MarkupKind::Strong,
            }],
        );
        assert_eq!(
            html, "x&<strong>amp;y</strong>",
            "the ampersand starts the entity `&amp;`, so it must not be escaped"
        );

        // Same string, markup starting one character later so the entity is
        // inside it rather than split by it.
        let inside = legacy_paragraph(
            "a&amp;b",
            &[Markup {
                start: 1,
                end: 7,
                kind: MarkupKind::Strong,
            }],
        );
        assert_eq!(inside, "a<strong>&amp;b</strong>");
    }

    #[test]
    fn same_page_anchor_has_no_target() {
        let block = paragraph(
            vec![Inline::Link {
                href: "#section".into(),
                rel: String::new(),
                title: String::new(),
                new_tab: false,
                children: vec![Inline::Text("jump".into())],
            }],
            false,
        );
        assert!(render_block(&block).contains("target=\"\">jump</a>"));
    }

    #[test]
    fn user_mention_omits_rel_and_title() {
        let block = paragraph(
            vec![Inline::UserMention {
                user_id: "abc123".into(),
                children: vec![Inline::Text("someone".into())],
            }],
            false,
        );
        assert!(render_block(&block).contains(
            "<a style=\"text-decoration: underline;\" href=\"https://medium.com/u/abc123\">someone</a>"
        ));
    }

    #[test]
    fn unordered_list() {
        let block = Block::List {
            ordered: false,
            items: vec![
                vec![Inline::Text("one".into())],
                vec![Inline::Text("two".into())],
            ],
        };
        assert_eq!(
            render_block(&block),
            "<ul class=\"pl-8 mt-2 list-disc\"><li class='mt-3'>one</li><li class='mt-3'>two</li></ul>"
        );
    }

    #[test]
    fn code_block_with_language_and_without() {
        let with = Block::Code {
            lang: Some("python".into()),
            lines: vec![inline_text("x < y && z")],
        };
        assert_eq!(
            render_block(&with),
            "<pre class=\"flex flex-col justify-center border mt-7 dark:border-gray-700\">\
             <code class=\"p-2 bg-gray-100 dark:bg-gray-900 overflow-x-auto language-python\">\
             x &lt; y &amp;&amp; z</code></pre>"
        );

        let without = Block::Code {
            lang: None,
            lines: vec![inline_text("plain")],
        };
        assert!(render_block(&without).contains("overflow-x-auto nohighlight\">plain</code>"));
    }

    /// `core.py:511-512` joins the escaped `PRE` paragraphs with a newline.
    #[test]
    fn code_block_joins_its_lines_with_newlines() {
        let block = Block::Code {
            lang: None,
            lines: vec![inline_text("first"), inline_text("second")],
        };
        assert!(render_block(&block).contains(">first\nsecond</code>"));
    }

    /// Each `PRE` paragraph is escaped on its own (`core.py:502-513`), so a
    /// partial entity on one line is not completed by the next.
    #[test]
    fn code_block_escapes_each_line_independently() {
        let block = Block::Code {
            lang: None,
            lines: vec![inline_text("&am"), inline_text("p;")],
        };
        assert!(render_block(&block).contains(">&amp;am\np;</code>"));
    }

    #[test]
    fn both_blockquote_styles() {
        let inset = Block::BlockQuote {
            style: QuoteStyle::Inset,
            inline: vec![Inline::Text("quoted".into())],
        };
        assert_eq!(
            render_block(&inset),
            "<blockquote style=\"box-shadow: inset 3px 0 0 0 rgb(209 207 239 / \
             var(--tw-bg-opacity));\" class=\"px-5 pt-3 pb-3 mt-5\">\
             <p class=\"font-italic\">quoted</p></blockquote>"
        );

        let pull = Block::BlockQuote {
            style: QuoteStyle::Pull,
            inline: vec![Inline::Text("pulled".into())],
        };
        assert_eq!(
            render_block(&pull),
            "<blockquote class=\"ml-5 text-2xl text-gray-600 mt-7 dark:text-gray-300\">\
             <p>pulled</p></blockquote>"
        );
    }

    #[test]
    fn image_with_and_without_caption() {
        let bare = Block::Image {
            id: "img1".into(),
            alt: "an image".into(),
            caption: None,
        };
        assert_eq!(
            render_block(&bare),
            "<div class=\"mt-7\"><img loading=\"eager\" alt=\"an image\" class=\"pt-5 m-auto\" \
             role=\"presentation\" referrerpolicy=\"no-referrer\" \
             src=\"https://miro.medium.com/v2/resize:fit:700/img1\"></div>"
        );

        let captioned = Block::Image {
            id: "img2".into(),
            alt: "pic".into(),
            caption: Some(vec![Inline::Text("A caption".into())]),
        };
        let html = render_block(&captioned);
        assert!(html.contains("</div><figcaption class='mt-3 text-sm text-center text-gray-500"));
        assert!(html.ends_with("A caption</figcaption>"));
    }

    /// A null `alt` reaches the renderer as the literal `"None"`, matching what
    /// Jinja emits for `{{ paragraph.metadata.alt }}` when the field is null.
    #[test]
    fn null_alt_renders_as_none() {
        let block = Block::Image {
            id: "img".into(),
            alt: "None".into(),
            caption: None,
        };
        assert!(render_block(&block).contains("alt=\"None\""));
    }

    #[test]
    fn image_row_wraps_its_images() {
        let row = Block::ImageRow(vec![
            Block::Image {
                id: "a".into(),
                alt: String::new(),
                caption: None,
            },
            Block::Image {
                id: "b".into(),
                alt: String::new(),
                caption: None,
            },
        ]);
        let html = render_block(&row);
        assert!(html.starts_with(
            "<div class=\"mx-5\"><div class=\"flex flex-row justify-center\"><div class=\"mt-7\">"
        ));
        assert!(html.ends_with("</div></div></div>"));
        assert_eq!(html.matches("resize:fit:700/").count(), 2);
    }

    /// The embed fragment starts with a newline because the Python source is a
    /// triple-quoted string whose first character is one.
    #[test]
    fn embed_starts_with_a_newline() {
        let block = Block::Embed {
            url: "https://example.com/post".into(),
            title: "Title".into(),
            description: "Desc".into(),
            site: "example.com".into(),
            thumbnail_id: Some("thumb".into()),
        };
        let html = render_block(&block);
        assert!(html.starts_with('\n'), "{html:?}");
        assert!(html.contains("href=\"https://example.com/post\""));
        assert!(html.contains("resize:fit:320/thumb"));
        assert!(html.contains(">example.com</p>"));
    }

    #[test]
    fn iframe_with_dimensions_keeps_the_nested_wrapper() {
        let block = Block::Iframe {
            src: "https://www.youtube.com/embed/x".into(),
            dims: Some((640, 360)),
        };
        assert_eq!(
            render_block(&block),
            "<div class=\"mt-7\"><div>\n    <iframe class=\"w-full\" \
             src=\"https://www.youtube.com/embed/x\" referrerpolicy=\"no-referrer\" \
             width=\"640\" height=\"360\" allowfullscreen=\"\" frameborder=\"0\" \
             scrolling=\"no\"></iframe>\n</div></div>"
        );
    }

    #[test]
    fn iframe_without_dimensions_uses_placeholders() {
        let block = Block::Iframe {
            src: "https://example.com/embed".into(),
            dims: None,
        };
        assert_eq!(
            render_block(&block),
            "<div class=\"mt-7\"><iframe class=\"w-full\" src=\"https://example.com/embed\" \
             width=\"100%\" height=\"100%\" referrerpolicy=\"no-referrer\" \
             allowfullscreen=\"\" frameborder=\"0\" scrolling=\"no\"></iframe></div>"
        );
    }

    #[test]
    fn heading_spacing_classes() {
        for (spacing, expected) in [
            (medium_doc::ir::HeadingSpacing::None, ""),
            (medium_doc::ir::HeadingSpacing::Pt8, "pt-8"),
            (medium_doc::ir::HeadingSpacing::Pt12, "pt-12"),
        ] {
            let block = Block::Heading {
                level: 3,
                id: "n1".into(),
                spacing,
                inline: vec![Inline::Text("Heading".into())],
            };
            let html = render_block(&block);
            assert!(
                html.contains(&format!("md:text-2xl {expected}\">")),
                "spacing {expected:?}: {html}"
            );
        }
    }

    /// The heading template interpolates `id` without quotes and leaves a space
    /// before an empty spacing class. Both quirks are visible in the output.
    #[test]
    fn heading_id_is_unquoted_and_empty_spacing_leaves_a_space() {
        let block = Block::Heading {
            level: 3,
            id: "n2".into(),
            spacing: medium_doc::ir::HeadingSpacing::None,
            inline: vec![Inline::Text("A heading".into())],
        };
        assert_eq!(
            render_block(&block),
            "<h3 id=n2 class=\"font-bold font-sans break-normal text-gray-900 \
             dark:text-gray-100 text-1xl md:text-2xl \">A heading</h3>"
        );
    }

    #[test]
    fn highlight_wraps_its_children() {
        let block = paragraph(
            vec![
                Inline::Text("before ".into()),
                Inline::Highlight(vec![Inline::Text("marked".into())]),
            ],
            false,
        );
        assert!(
            render_block(&block)
                .contains("before <mark class=\"bg-emerald-300\">marked</mark></p>")
        );
    }
}
