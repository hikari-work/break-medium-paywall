//! `Document` → Markdown.
//!
//! # There is no oracle for this, and the plan is wrong to call it a free win
//!
//! `RUST_REWRITE_PLAN.md` §2.1 lists Markdown as a free win of having an IR. It
//! is not: `legacy/medium-parser/medium_parser/core.py:814-817` is
//! `raise NotImplementedError`. Nothing in the legacy tree has ever emitted
//! Markdown, so the differential gate cannot cover this module — the tests below
//! **are** the specification, and the only external authority is CommonMark plus
//! the one real consumer.
//!
//! # The policy, in one sentence
//!
//! **Markdown where the equivalent is portable and exact; raw HTML mirroring
//! [`crate::html`] where it is not; nothing else.**
//!
//! So a paragraph is Markdown, a `<mark>` is raw HTML (because `==…==` is
//! markdown-it-mark, not CommonMark, and the consumer's pipeline is remark
//! without that plugin), and an iframe is the *same element* `html.rs` emits.
//! That last part is deliberate rather than lazy: two renderers that disagree
//! about an element are two renderers a consumer has to special-case.
//!
//! # What Markdown cannot carry, and is therefore dropped
//!
//! | dropped | why |
//! |---|---|
//! | Tailwind classes, `drop_cap`, `margin`, `spacing` | presentation, and the caller supplies its own |
//! | `Block::Heading`'s `id` | Markdown has no anchors; the consumer derives them |
//! | `QuoteStyle` — `Inset` and `Pull` both become `>` | Markdown has one blockquote |
//! | `Link`'s `rel` and `new_tab` | no CommonMark equivalent |
//! | `Block::Embed`'s `site` | the consumer's plugin derives it from the URL's host |
//!
//! `Block::Heading`'s `id` is the one that costs something real: `/html` gives an
//! anchor and `/markdown` does not, so a table of contents built from one does
//! not exist in the other. The DTO carries `{id, text}` for exactly that reason.
//!
//! # Two traps this module exists to avoid
//!
//! 1. **Author text must not become structure.** A paragraph whose text is
//!    `# hello` must not render as a heading. [`harden_line_start`] handles the
//!    leading character; every character that can open an *inline* construct is
//!    backslash-escaped by [`escape_text`].
//! 2. **`&` is left alone.** `\&` is not a legal CommonMark escape, and
//!    `&amp;` in Markdown source is an entity that renders as `&` — i.e. the
//!    HTML renderer's output pasted into Markdown would be wrong twice over. The
//!    text is plain, always; that is the same rule [`crate::dto`] follows.
//!
//! # No front matter and no title heading
//!
//! `render_markdown` returns a **fragment**, starting at the first block and with
//! no trailing newline. The metadata is not here: it is `/meta` and `/posts/{id}`,
//! and a consumer that wants both splices them itself. Emitting a `# title` would
//! duplicate the title of an article that already has an `H2` for it, and YAML
//! front matter would make the fragment un-spliceable.

use medium_doc::ir::{Block, Document, Inline};

use crate::dto::{MIRO_IMAGE_BASE, MIRO_THUMBNAIL_BASE};

/// Characters that open a block construct when they are the first thing on a
/// line.
///
/// `*`, `_` and `` ` `` are absent because [`escape_text`] escapes them
/// everywhere, so they can never reach a line start unescaped. `=` and `|` are
/// absent because a setext heading and a GFM table both need a *second* line that
/// this module never emits — blocks are separated by blank lines and a paragraph
/// is always one line (see [`escape_text`] on newlines). `~` is here for the
/// tilde code fence.
const BLOCK_STARTERS: [char; 5] = ['#', '>', '-', '+', '~'];

/// The opening tag of a highlight, byte-identical to `inline_html::HIGHLIGHT_OPEN`
/// in `medium-doc` — the same literal `html.rs` reaches through `render_inline`.
///
/// Raw HTML because `==marked==` is markdown-it-mark, a plugin the consumer's
/// remark pipeline does not load — so `==` would reach a reader as literal
/// punctuation. Raw HTML carries the meaning *and* matches `/html`.
const HIGHLIGHT_OPEN: &str = r#"<mark class="bg-emerald-300">"#;
const HIGHLIGHT_CLOSE: &str = "</mark>";

/// Renders a document body as a Markdown fragment.
///
/// Blocks are joined by a blank line, which is what ends a paragraph, a list, a
/// fence and an HTML block in CommonMark — one separator that works for all of
/// them.
///
/// # A block that renders to nothing is dropped, not joined
///
/// An empty paragraph is reachable rather than theoretical: `unknowns.json` has a
/// `P` whose text is the empty string, and a paragraph with no inline content has
/// no Markdown form — an empty line. Joining it would leave a run of three
/// newlines, a phantom separator for a block that is not there. [`ImageRow`] with
/// no images is the other way to reach one.
///
/// [`ImageRow`]: Block::ImageRow
#[must_use]
pub fn render_markdown(document: &Document) -> String {
    render_markdown_blocks(&document.blocks)
        .into_iter()
        .filter(|block| !block.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One string per block, so a test can name the block that went wrong.
///
/// **1:1 with `blocks`, including the empty ones** — the empty string is the
/// honest rendering of a block with no content, and [`render_markdown`] is where
/// it is dropped. Folding the two together would make "which block was that?"
/// unanswerable.
#[must_use]
pub fn render_markdown_blocks(blocks: &[Block]) -> Vec<String> {
    blocks.iter().map(render_block).collect()
}

/// Renders one block.
#[must_use]
pub fn render_block(block: &Block) -> String {
    match block {
        Block::Heading { level, inline, .. } => {
            // `id` is dropped: see the module docs. Level is clamped to 1..=6
            // because a `#`-run of seven is not a heading at all.
            let hashes = "#".repeat(usize::from((*level).clamp(1, 6)));
            let body = harden_line_start(&render_inlines(inline));
            format!("{hashes} {body}")
        }

        Block::Paragraph { inline, .. } => harden_line_start(&render_inlines(inline)),

        Block::List { ordered, items } => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let body = harden_line_start(&render_inlines(item));
                if *ordered {
                    // Renumbered from 1. CommonMark ignores the source numbers,
                    // so sequential ones cost nothing and read better.
                    format!("{}. {body}", index + 1)
                } else {
                    format!("- {body}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),

        Block::Code { lang, lines } => {
            let body: Vec<String> = lines.iter().map(|line| code_text(line)).collect();
            let fence = fence_for(&body);
            // The info string is the language, or nothing. `html.rs` writes
            // `nohighlight` as a CSS class; a Markdown info string of
            // `nohighlight` would be read as a language name that does not exist.
            let info = lang
                .as_deref()
                .filter(|lang| !lang.is_empty() && !lang.contains('`'))
                .unwrap_or("");
            format!("{fence}{info}\n{}\n{fence}", body.join("\n"))
        }

        Block::BlockQuote { inline, .. } => {
            // One line, because `render_inlines` cannot produce a newline: the only
            // source of one would be a `Text` run, and `escape_text` collapses
            // those. So no prefixing loop is needed, and the absence of one is
            // pinned by `a_quote_is_one_line_however_its_content_is_built`.
            format!("> {}", harden_line_start(&render_inlines(inline)))
        }

        Block::Image { id, alt, caption } => {
            let mut out = format!("![{}]({MIRO_IMAGE_BASE}{id})", escape_text(alt));
            if let Some(caption) = caption {
                // Markdown has no `<figcaption>`. An emphasised line after the
                // image is the convention every Markdown exporter uses, and it
                // survives a round trip through the consumer's pipeline as a
                // paragraph — where raw `<figcaption>` would be an HTML block
                // that most renderers drop for lack of a `<figure>` parent.
                let text = render_inlines(caption);
                if !text.is_empty() {
                    out.push_str(&format!("\n*{}*", harden_line_start(&text)));
                }
            }
            out
        }

        // A row is a layout, and Markdown has none: the images go on their own
        // lines, each resumable as a block of its own. On one line they would
        // parse as a single paragraph of adjacent inline images, which is a
        // different rendering rather than a closer one.
        Block::ImageRow(images) => images
            .iter()
            .map(render_block)
            .collect::<Vec<_>>()
            .join("\n"),

        Block::Embed {
            url,
            title,
            description,
            thumbnail_id,
            site: _,
        } => render_embed(url, title, description, thumbnail_id.as_deref()),

        // Byte-identical to `html.rs`, so the two renderers cannot disagree
        // about the element. `an_iframe_is_the_same_element_the_html_renderer_emits`
        // is what holds that.
        Block::Iframe { src, dims } => match dims {
            Some((width, height)) => format!(
                "<div class=\"mt-7\"><div>\n    <iframe class=\"w-full\" src=\"{src}\" \
                 referrerpolicy=\"no-referrer\" width=\"{width}\" height=\"{height}\" \
                 allowfullscreen=\"\" frameborder=\"0\" scrolling=\"no\"></iframe>\n</div></div>"
            ),
            None => format!(
                "<div class=\"mt-7\"><iframe class=\"w-full\" src=\"{src}\" width=\"100%\" \
                 height=\"100%\" referrerpolicy=\"no-referrer\" allowfullscreen=\"\" \
                 frameborder=\"0\" scrolling=\"no\"></iframe></div>"
            ),
        },
    }
}

/// The consumer's MixtapeEmbed shape, or a deliberate degradation without a
/// thumbnail.
///
/// # The shape comes from the consumer's plugin, not from an idea of ours
///
/// `new-web/src/lib/utils/remark/mixtape-embed.js` matches
///
/// ```text
/// [!\[(alt)\]\((link)\)\]\((image)\)\n>(title)\n>(description)\n
/// ```
///
/// and reads the groups as `alt`, the `href` (its `siteName` is that URL's
/// hostname), the thumbnail background, the title and the description. The
/// rendered example in `new-web/src/lib/test/blog01.md:133-135` is the same four
/// lines. Both are reproduced exactly, down to `**` around the title.
///
/// # The caveat, stated rather than buried
///
/// That regex requires the four lines to be the text of **one** paragraph, with
/// a trailing newline inside it. In CommonMark a `>` line interrupts a paragraph,
/// so a remark parse of these four lines yields a paragraph plus a blockquote,
/// and the plugin would not fire. Resolving that is the consumer's side of the
/// boundary — decision 5 of Fase 6 leaves new-web's wiring for later — and the
/// alternative (inventing a form the plugin does not recognise) would be worse.
/// What this function guarantees is the bytes the plugin's regex is written
/// against; a test pins them.
///
/// `site` is not emitted: the plugin derives it from the link URL. `html.rs`
/// prints the IR's own `site`, which `parse` computed from the same URL, so the
/// two agree without being told twice.
fn render_embed(url: &str, title: &str, description: &str, thumbnail_id: Option<&str>) -> String {
    let title = harden_line_start(&escape_text(title));
    let description = harden_line_start(&escape_text(description));

    match thumbnail_id.filter(|id| !id.is_empty()) {
        Some(id) => {
            let alt = escape_text(title.trim_matches('*'));
            format!("[![{alt}]({url})]({MIRO_THUMBNAIL_BASE}{id})\n>**{title}**\n>{description}")
        }
        // No thumbnail, so the four-line form has nothing to put in its image
        // slot and the plugin's regex cannot match whatever we write. A plain
        // link plus the description as a quote keeps the information — the
        // target URL and the summary — instead of dropping the embed.
        None => match description.is_empty() {
            true => format!("[{title}]({url})"),
            false => format!("[{title}]({url})\n>{description}"),
        },
    }
}

/// Renders an inline list, applying the paragraph's escaping mode.
fn render_inlines(nodes: &[Inline]) -> String {
    let mut out = String::new();
    for node in nodes {
        render_inline(&mut out, node);
    }
    out
}

fn render_inline(out: &mut String, node: &Inline) {
    match node {
        Inline::Text(text) => out.push_str(&escape_text(text)),

        // A wrapper whose content renders empty would emit `****` or `**` — and
        // `**` is a thematic break, so an empty emphasis would silently become a
        // horizontal rule. Skip the markers instead.
        Inline::Strong(children) => wrap(out, "**", children),
        Inline::Emphasis(children) => wrap(out, "*", children),
        Inline::Code(children) => {
            // **Literal, not escaped.** A backslash escape is inert inside a code
            // span — CommonMark leaves it as a backslash — so running
            // [`escape_text`] here would print `\*` at a reader who asked for `*`.
            // Same reasoning as [`code_text`], one level up.
            let body = inline_code_text(children);
            if !body.is_empty() {
                let fence = span_fence(&body);
                // CommonMark strips one leading and one trailing space from a
                // code span only when both are present, so they are added only
                // when the fence is longer than a single backtick — which is
                // exactly when the content starts or ends with one.
                let pad = fence.len() > 1;
                out.push_str(&fence);
                if pad {
                    out.push(' ');
                }
                out.push_str(&body);
                if pad {
                    out.push(' ');
                }
                out.push_str(&fence);
            }
        }

        Inline::Link {
            href,
            title,
            children,
            ..
        } => {
            let body = render_inlines(children);
            let destination = escape_destination(href);
            if title.is_empty() {
                out.push_str(&format!("[{body}]({destination})"));
            } else {
                out.push_str(&format!(
                    "[{body}]({destination} \"{}\")",
                    escape_title(title)
                ));
            }
        }

        // No `rel`, no `title`, no `target` — the same element `html.rs` emits,
        // minus the attributes Markdown links have no syntax for. The URL is the
        // part that must match, and it is the part
        // `a_user_mention_links_to_the_same_url_the_html_renderer_uses` compares.
        Inline::UserMention { user_id, children } => {
            let body = render_inlines(children);
            out.push_str(&format!("[{body}](https://medium.com/u/{user_id})"));
        }

        Inline::Highlight(children) => {
            let body = render_inlines(children);
            // Raw HTML, so it is emitted even when empty: `<mark></mark>` is an
            // element, not a construct that can change the block structure.
            out.push_str(HIGHLIGHT_OPEN);
            out.push_str(&body);
            out.push_str(HIGHLIGHT_CLOSE);
        }
    }
}

/// `open` + children + `close`, or nothing at all if the children are empty.
fn wrap(out: &mut String, marker: &str, children: &[Inline]) {
    let body = render_inlines(children);
    if body.is_empty() {
        return;
    }
    out.push_str(marker);
    out.push_str(&body);
    out.push_str(marker);
}

/// Escapes text for a Markdown line, collapsing newlines.
///
/// # Which characters, and why not more
///
/// Backslash-escaped: `` \ `` `` ` `` `*` `_` `[` `]` `<`. Each of those can open
/// an inline construct that would change how the text reads — a stray `*` starts
/// emphasis, a stray `[` starts a link, a stray `<` starts raw HTML or an
/// autolink. All are ASCII punctuation, so `\` before them is a legal CommonMark
/// escape.
///
/// **`&` is deliberately untouched.** `\&` is not a legal escape, and writing
/// `&amp;` for an ampersand is the HTML renderer's job, not this one's: the
/// consumer's pipeline HTML-escapes on output, so an entity here would arrive at
/// a reader as a literal `&amp;`.
///
/// # Newlines collapse to one space
///
/// A browser collapses a newline inside a paragraph to a space anyway, so the
/// HTML renderer's output reads the same either way — but in Markdown source a
/// newline ends the paragraph's line and can start a block construct. Collapsing
/// also makes every paragraph exactly one line, which is what lets
/// [`harden_line_start`] be a single-character fix-up rather than a per-line one.
fn escape_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len());
    for ch in normalized.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// `\r\n` and a lone `\r` both become `\n`; nothing else changes.
///
/// A Windows line ending is one break, not two — without this the `\r` would be
/// turned into a second space by [`escape_text`], putting a double space in the
/// middle of a paragraph and a stray carriage return inside a code fence.
fn collapse_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Escapes a `Link`'s `title`, which sits inside `"…"` in the destination.
fn escape_title(title: &str) -> String {
    escape_text(title).replace('"', "\\\"")
}

/// Escapes a link destination.
///
/// Angle brackets rather than backslashes: `<…>` is CommonMark's own form for a
/// destination that contains spaces, and it is the only form that survives a
/// `)` in the URL. A literal `<` or `>` cannot appear in a valid URL unencoded,
/// so this cannot be fooled.
fn escape_destination(href: &str) -> String {
    if href.contains([' ', '(', ')', '<', '>']) {
        format!("<{}>", href.replace('>', "%3E").replace('<', "%3C"))
    } else {
        href.to_string()
    }
}

/// Stops the first character of a rendered line from opening a block construct.
///
/// # Why this is only ever the first character
///
/// [`escape_text`] collapses newlines, so a paragraph or a list item is one line
/// and a blockquote's content is one line per block-level newline. There is no
/// second line inside a paragraph to fix up — which is also why `=` (setext) and
/// `|` (GFM table) are not in [`BLOCK_STARTERS`]: both need a following line that
/// a blank-line-separated fragment never provides.
///
/// # The four-space case cannot be backslashed away
///
/// Four columns of leading whitespace open an *indented code block*, and there is
/// no backslash escape for a space — CommonMark defines them for punctuation
/// only. So the whitespace is written as entity references instead: `&#32;` and
/// `&#9;`, which CommonMark renders as the space and the tab they were. This is
/// the only place the module emits an entity, and it is the only way to keep an
/// author's indentation from silently becoming a code block.
fn harden_line_start(content: &str) -> String {
    let mut columns = 0usize;
    let mut bytes = 0usize;
    for ch in content.chars() {
        match ch {
            ' ' => columns += 1,
            '\t' => columns += 4,
            _ => break,
        }
        bytes += ch.len_utf8();
    }

    let mut out = content.to_string();

    if columns >= 4 {
        let escaped: String = content[..bytes]
            .chars()
            .map(|ch| match ch {
                '\t' => "&#9;",
                _ => "&#32;",
            })
            .collect();
        out.replace_range(..bytes, &escaped);
        // The line now starts with `&`, which opens no block construct, so no
        // block starter can follow it at the start of the line.
        return out;
    }

    let rest = &content[bytes..];
    let mut chars = rest.char_indices();
    let Some((offset, first)) = chars.next() else {
        return out;
    };
    let escape_at = if BLOCK_STARTERS.contains(&first) {
        Some(bytes + offset)
    } else {
        // The `.` or `)`, not the digit: `1\.` cannot be read as a marker and
        // renders as `1.`, whereas `\1.` would render as `\1.`.
        ordered_marker_len(rest).map(|len| bytes + offset + len - 1)
    };

    if let Some(at) = escape_at {
        out.insert(at, '\\');
    }
    out
}

/// The byte length of an ordered-list marker at the start of `text`, punctuation
/// included, or `None` if this is not one.
///
/// CommonMark's rule spelled out rather than approximated: one to nine digits,
/// then `.` or `)`, **then whitespace or the end of the line**. The last clause is
/// the one worth writing down — `1.5 miles` is not a list, and escaping it would
/// put a backslash in front of a reader's decimal point for nothing.
fn ordered_marker_len(text: &str) -> Option<usize> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    match text.as_bytes().get(digits) {
        Some(b'.' | b')') => {}
        _ => return None,
    }
    match text.as_bytes().get(digits + 1) {
        None | Some(b' ' | b'\t') => Some(digits + 1),
        _ => None,
    }
}

/// A line of a `PRE` block, as literal text.
///
/// **No escaping at all.** Backslashes are literal inside a code fence, so
/// escaping would put them in front of a reader's eyes. A newline inside a line
/// would split it, but the IR keeps one entry per source paragraph precisely so
/// that cannot happen (`parse` splits on the `PRE` paragraph boundary).
fn code_text(line: &[Inline]) -> String {
    let mut out = String::new();
    collect_text(line, &mut out);
    collapse_line_endings(&out)
}

/// The content of an inline code span, as literal text on one line.
///
/// Like [`code_text`] it does not escape, and additionally it turns a line ending
/// into a space — CommonMark converts one inside a code span, and a code span
/// cannot span two lines. Nested markup is flattened to its text, which is a
/// deliberate divergence from `/html`: CommonMark has no syntax for markup inside
/// a code span, so the alternative to dropping it is emitting the markup as
/// literal punctuation.
fn inline_code_text(children: &[Inline]) -> String {
    let mut raw = String::new();
    collect_text(children, &mut raw);
    collapse_line_endings(&raw).replace('\n', " ")
}

/// Every descendant `Inline::Text`, concatenated.
fn collect_text(nodes: &[Inline], out: &mut String) {
    for node in nodes {
        match node {
            Inline::Text(text) => out.push_str(text),
            Inline::Strong(children)
            | Inline::Emphasis(children)
            | Inline::Code(children)
            | Inline::Highlight(children)
            | Inline::Link { children, .. }
            | Inline::UserMention { children, .. } => collect_text(children, out),
        }
    }
}

/// The longest run of consecutive backticks anywhere in `text`.
fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// A code fence that cannot be closed by its own content.
///
/// A fence is closed by a run of backticks at least as long as the opening one, so
/// the opening run is one longer than the longest run inside — and never shorter
/// than three, which is the shortest run CommonMark recognises as a fence at all.
/// This is the whole reason a fence is computed rather than being a literal
/// ```` ``` ````.
fn fence_for(lines: &[String]) -> String {
    let longest = lines
        .iter()
        .map(|line| longest_backtick_run(line))
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

/// The delimiter for an inline code span.
fn span_fence(body: &str) -> String {
    "`".repeat(longest_backtick_run(body) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use medium_doc::ir::{HeadingSpacing, ParagraphMargin, QuoteStyle};
    use medium_doc::parse::{self, PostPayload};
    use serde_json::json;

    /// The render gate's corpus, read where it lives rather than copied.
    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../xtask/difftest/fixtures");

    fn text(value: &str) -> Inline {
        Inline::Text(value.to_string())
    }

    fn paragraph(inline: Vec<Inline>) -> Block {
        Block::Paragraph {
            inline,
            drop_cap: false,
            margin: ParagraphMargin::Mt7,
        }
    }

    /// One paragraph, rendered.
    fn of(inline: Vec<Inline>) -> String {
        render_markdown(&Document {
            meta: Default::default(),
            blocks: vec![paragraph(inline)],
        })
    }

    fn document_of(post: serde_json::Value) -> Document {
        let payload = PostPayload::from_value(json!({ "data": { "post": post } }))
            .expect("the fixture is a valid payload");
        parse::parse(&payload, "https://freedium.test")
    }

    // ---------------------------------------------------------------------
    // The escaping rule, which is the whole reason this file has tests
    // ---------------------------------------------------------------------

    /// **The gate on the escaping rule.**
    ///
    /// The same input must come out plain here and entity-escaped from the HTML
    /// renderer. If someone ever "unifies" the two escapers, this is what fails —
    /// and the failure matters, because `&amp;` in Markdown source reaches a
    /// reader as a literal `&amp;` once the consumer's pipeline escapes it again.
    #[test]
    fn text_is_escaped_for_markdown_not_for_html() {
        let inline = vec![text("it's a <b>test</b> & more")];

        let markdown = of(inline.clone());
        assert_eq!(markdown, "it's a \\<b>test\\</b> & more");
        assert!(
            !markdown.contains("&#39;"),
            "apostrophes are not entities here"
        );
        assert!(
            !markdown.contains("&amp;"),
            "`&` is not escaped for Markdown"
        );

        let html = crate::html::render_block(&paragraph(inline));
        assert!(
            html.contains("it&#39s"),
            "the page escapes the apostrophe, and does so without the semicolon \
             — the inherited `&#39` bug pinned by `metadata.rs`: {html}"
        );
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(html.contains(" &amp; more"), "{html}");
    }

    /// The characters that can open an inline construct are backslashed, and
    /// nothing else is.
    #[test]
    fn only_construct_opening_characters_are_escaped() {
        assert_eq!(
            of(vec![text("a*b_c`d[e]f\\g<h>i&j\"k'l,m.n")]),
            "a\\*b\\_c\\`d\\[e\\]f\\\\g\\<h>i&j\"k'l,m.n",
            "`&`, `\"`, `'`, `.`, `,`, `>` and `-` are not inline constructs"
        );
    }

    /// A newline inside a text run collapses to a space, so a paragraph is always
    /// one line and cannot smuggle a block construct in on a second one.
    ///
    /// `\r\n` is **one** break, not two: a Windows line ending that produced two
    /// spaces would put a visible double space in the middle of a sentence.
    #[test]
    fn a_newline_in_text_collapses_to_a_space() {
        assert_eq!(of(vec![text("one\ntwo\r\nthree")]), "one two three");
        assert_eq!(of(vec![text("one\r\ntwo")]), "one two", "CRLF is one break");
        assert_eq!(
            of(vec![text("one\rtwo")]),
            "one two",
            "a lone CR is one too"
        );
        assert!(!of(vec![text("one\ntwo")]).contains('\n'));
    }

    // ---------------------------------------------------------------------
    // Author text must not become structure
    // ---------------------------------------------------------------------

    /// **Every block-starter an author could type, neutralised.**
    ///
    /// Each of these would otherwise be read as a block construct: a heading, a
    /// quote, a list, a fence. The assertion is that rendering the result again
    /// yields a paragraph — the cheapest way to say "this is still text".
    #[test]
    fn a_paragraph_that_looks_like_a_block_stays_text() {
        for (input, expected) in [
            ("# heading", "\\# heading"),
            ("## heading", "\\## heading"),
            ("> quote", "\\> quote"),
            ("- item", "\\- item"),
            ("+ item", "\\+ item"),
            ("~~~ fence", "\\~~~ fence"),
            ("1. item", "1\\. item"),
            ("12) item", "12\\) item"),
            ("* item", "\\* item"),
            ("``` fence", "\\`\\`\\` fence"),
        ] {
            let inline = vec![text(input)];
            assert_eq!(of(inline.clone()), expected, "input = {input:?}");

            // A `Text` that is already escaped must not change the block
            // structure: whatever else it does, the output is one line with no
            // blank line in it.
            assert!(
                !of(inline).contains("\n\n"),
                "{input:?} produced a block separator"
            );
        }
    }

    /// The hardening follows the text *after* the leading spaces, because up to
    /// three columns of indentation are allowed before a block marker.
    #[test]
    fn a_block_starter_after_up_to_three_spaces_is_still_hardened() {
        assert_eq!(of(vec![text("   # x")]), "   \\# x");
        assert_eq!(of(vec![text(" 1. x")]), " 1\\. x");
    }

    /// **Four columns of leading whitespace cannot be backslashed away.**
    ///
    /// There is no `\ ` escape in CommonMark, so the indentation is written as
    /// entities. Without this the paragraph would become an indented code block —
    /// a silent change of meaning, which is the failure mode this whole section
    /// exists for.
    #[test]
    fn four_spaces_of_indent_are_written_as_entities() {
        assert_eq!(of(vec![text("    four")]), "&#32;&#32;&#32;&#32;four");
        assert_eq!(of(vec![text("\tone tab")]), "&#9;one tab");
        assert!(!of(vec![text("    four")]).starts_with(' '));
    }

    /// A paragraph whose text is *not* a block starter is left exactly alone —
    /// the hardening is not a blanket prefix.
    #[test]
    fn ordinary_text_is_not_hardened() {
        assert_eq!(of(vec![text("Hello, world.")]), "Hello, world.");
        assert_eq!(of(vec![text("a - b")]), "a - b");
        assert_eq!(
            of(vec![text("1.5 miles")]),
            "1.5 miles",
            "not a list marker"
        );
    }

    // ---------------------------------------------------------------------
    // Inline
    // ---------------------------------------------------------------------

    #[test]
    fn emphasis_and_strong_use_the_star_forms() {
        assert_eq!(
            of(vec![Inline::Strong(vec![text("b")])]),
            "**b**",
            "the IR's nesting is preserved, not flattened"
        );
        assert_eq!(of(vec![Inline::Emphasis(vec![text("i")])]), "*i*");
        assert_eq!(
            of(vec![Inline::Strong(vec![Inline::Emphasis(vec![text(
                "bi"
            )])])]),
            "***bi***"
        );
    }

    /// **An empty wrapper must not become a thematic break.**
    ///
    /// `**` alone on a line is `<hr>`, and `****` is too. An `Emphasis` with no
    /// content is what `parse` produces for a markup whose range collapsed, so
    /// this is reachable rather than hypothetical.
    #[test]
    fn an_empty_wrapper_emits_nothing() {
        assert_eq!(of(vec![Inline::Emphasis(vec![])]), "");
        assert_eq!(of(vec![Inline::Strong(vec![])]), "");
        assert_eq!(of(vec![Inline::Code(vec![])]), "");
        assert_eq!(of(vec![text("a"), Inline::Strong(vec![])]), "a");
    }

    #[test]
    fn a_code_span_grows_its_fence_around_backticks() {
        assert_eq!(of(vec![Inline::Code(vec![text("x = 1")])]), "`x = 1`");
        assert_eq!(
            of(vec![Inline::Code(vec![text("a `b` c")])]),
            "`` a `b` c ``",
            "the fence is longer than the longest run inside, and padded"
        );
        assert_eq!(
            of(vec![Inline::Code(vec![text("``")])]),
            "``` `` ```",
            "two backticks inside need three outside"
        );
    }

    /// **The content of a code span is literal.**
    ///
    /// A backslash escape is inert inside a code span, so escaping here would show
    /// a reader `\*` where the article said `*`. The newline is the other half:
    /// a code span cannot span two lines, so CommonMark reads a line ending as a
    /// space — and a raw `\n` would break the paragraph in two.
    #[test]
    fn a_code_span_holds_its_content_literally() {
        assert_eq!(
            of(vec![Inline::Code(vec![text("a*b_c[d]e\\f<g")])]),
            "`a*b_c[d]e\\f<g`"
        );
        assert_eq!(
            of(vec![Inline::Code(vec![text("one\ntwo")])]),
            "`one two`",
            "a code span is one line"
        );
        assert_eq!(
            of(vec![Inline::Code(vec![Inline::Strong(vec![text("b")])])]),
            "`b`",
            "markup inside a code span flattens to its text"
        );
    }

    #[test]
    fn a_link_carries_its_href_and_title() {
        assert_eq!(
            of(vec![Inline::Link {
                href: "https://x.test/a".into(),
                rel: "noopener".into(),
                title: String::new(),
                new_tab: true,
                children: vec![text("label")],
            }]),
            "[label](https://x.test/a)",
            "`rel` and `new_tab` have no Markdown form"
        );

        assert_eq!(
            of(vec![Inline::Link {
                href: "https://x.test/a".into(),
                rel: String::new(),
                title: "a \"titled\" link".into(),
                new_tab: false,
                children: vec![text("label")],
            }]),
            "[label](https://x.test/a \"a \\\"titled\\\" link\")"
        );
    }

    /// A destination with a space or a bracket uses CommonMark's angle-bracket
    /// form, which is the only one that survives both.
    #[test]
    fn a_destination_with_a_space_uses_angle_brackets() {
        assert_eq!(
            of(vec![Inline::Link {
                href: "https://x.test/a b".into(),
                rel: String::new(),
                title: String::new(),
                new_tab: false,
                children: vec![text("t")],
            }]),
            "[t](<https://x.test/a b>)"
        );
        assert_eq!(
            escape_destination("https://x.test/plain"),
            "https://x.test/plain"
        );
    }

    /// The mention's URL is the same one `html.rs` writes, since that is the only
    /// thing about a mention that carries meaning.
    #[test]
    fn a_user_mention_links_to_the_same_url_the_html_renderer_uses() {
        let mention = Inline::UserMention {
            user_id: "u1".into(),
            children: vec![text("@ada")],
        };

        assert_eq!(of(vec![mention.clone()]), "[@ada](https://medium.com/u/u1)");

        let html = crate::html::render_block(&paragraph(vec![mention]));
        assert!(
            html.contains("https://medium.com/u/u1"),
            "the HTML renderer's URL moved: {html}"
        );
    }

    /// The highlight is raw HTML, **byte-identical** to the HTML renderer's
    /// `<mark>` — so the two representations cannot disagree about it — and not
    /// the `==` form, which is a plugin the consumer does not load.
    #[test]
    fn a_highlight_is_the_same_mark_the_html_renderer_emits() {
        let highlighted = vec![Inline::Highlight(vec![text("marked")])];
        let markdown = of(highlighted.clone());
        assert_eq!(markdown, "<mark class=\"bg-emerald-300\">marked</mark>");
        assert!(!markdown.contains("=="), "markdown-it-mark is not loaded");

        let html = crate::html::render_block(&paragraph(highlighted));
        assert!(html.contains(&markdown), "{html}");
    }

    // ---------------------------------------------------------------------
    // Blocks
    // ---------------------------------------------------------------------

    /// Levels map to hash runs, and the `id` is dropped because Markdown has no
    /// anchors.
    #[test]
    fn a_heading_is_a_hash_run() {
        for (level, expected) in [(2, "## H"), (3, "### H"), (4, "#### H")] {
            let block = Block::Heading {
                level,
                id: "some-anchor".into(),
                spacing: HeadingSpacing::Pt12,
                inline: vec![text("H")],
            };
            assert_eq!(render_block(&block), expected);
            assert!(
                !render_block(&block).contains("some-anchor"),
                "the id has no Markdown form and must not leak in"
            );
        }
    }

    #[test]
    fn lists_use_dashes_and_numbers() {
        let unordered = Block::List {
            ordered: false,
            items: vec![vec![text("one")], vec![text("two")]],
        };
        assert_eq!(render_block(&unordered), "- one\n- two");

        let ordered = Block::List {
            ordered: true,
            items: vec![vec![text("one")], vec![text("two")]],
        };
        assert_eq!(
            render_block(&ordered),
            "1. one\n2. two",
            "renumbered from 1; CommonMark ignores the source numbers"
        );
    }

    /// An item whose text looks like a bullet keeps its text.
    #[test]
    fn a_list_item_that_looks_like_a_bullet_is_hardened() {
        let block = Block::List {
            ordered: false,
            items: vec![vec![text("- nested")]],
        };
        assert_eq!(render_block(&block), "- \\- nested");
    }

    /// A quote is one line, whatever its content is built from.
    ///
    /// This is what makes a single `> ` prefix correct rather than a lucky
    /// omission: if any inline ever started emitting a newline, half the quote
    /// would escape it — and the `Text` inside a `Code` span is the most likely
    /// way for that to happen, so it is what the test uses.
    #[test]
    fn a_quote_is_one_line_however_its_content_is_built() {
        let block = Block::BlockQuote {
            style: QuoteStyle::Inset,
            inline: vec![
                text("one\ntwo"),
                Inline::Code(vec![text("a\r\nb`c")]),
                Inline::Highlight(vec![text("x\ny")]),
            ],
        };
        let rendered = render_block(&block);
        assert_eq!(rendered.lines().count(), 1, "{rendered:?}");
        assert_eq!(
            rendered,
            "> one two`` a b`c ``<mark class=\"bg-emerald-300\">x y</mark>"
        );
    }

    /// Both quote styles become `>`, which is the whole of what Markdown has.
    #[test]
    fn both_quote_styles_render_the_same() {
        let inline = vec![text("q")];
        for style in [QuoteStyle::Inset, QuoteStyle::Pull] {
            let block = Block::BlockQuote {
                style,
                inline: inline.clone(),
            };
            assert_eq!(render_block(&block), "> q");
        }
    }

    /// **The fence grows around its content.** Without this, a code block
    /// containing a triple backtick — a Markdown example inside a Markdown
    /// article, which is exactly what this file's fixtures are — would close the
    /// fence early and turn its own remainder into prose.
    #[test]
    fn a_code_fence_grows_around_its_content() {
        let plain = Block::Code {
            lang: Some("rust".into()),
            lines: vec![vec![text("let x = 1;")]],
        };
        assert_eq!(render_block(&plain), "```rust\nlet x = 1;\n```");

        let nested = Block::Code {
            lang: None,
            lines: vec![vec![text("```")], vec![text("inner")]],
        };
        assert_eq!(
            render_block(&nested),
            "````\n```\ninner\n````",
            "the opening run is one longer than the longest inside"
        );

        let no_lang = Block::Code {
            lang: None,
            lines: vec![vec![text("x")]],
        };
        assert_eq!(
            render_block(&no_lang),
            "```\nx\n```",
            "no info string at all, rather than `nohighlight`"
        );
    }

    /// A fence's info string cannot contain a backtick — CommonMark forbids it,
    /// and a language name from the payload is not trusted to be sane.
    #[test]
    fn a_backtick_in_the_language_drops_the_info_string() {
        let block = Block::Code {
            lang: Some("ru`st".into()),
            lines: vec![vec![text("x")]],
        };
        assert_eq!(render_block(&block), "```\nx\n```");
    }

    /// Code content is literal: escaping it would put backslashes in front of a
    /// reader who is trying to copy the code.
    #[test]
    fn code_content_is_not_escaped() {
        let block = Block::Code {
            lang: None,
            lines: vec![vec![text("*not emphasis* `nor code` \\a\\b <tag>")]],
        };
        assert!(render_block(&block).contains("*not emphasis* `nor code` \\a\\b <tag>"));
    }

    /// The image URL is the same one the HTML renderer and the DTO build.
    #[test]
    fn an_image_url_is_the_same_base_as_the_html_renderer() {
        let block = Block::Image {
            id: "1*abc.png".into(),
            alt: "a picture".into(),
            caption: None,
        };
        assert_eq!(
            render_block(&block),
            "![a picture](https://miro.medium.com/v2/resize:fit:700/1*abc.png)"
        );

        let html = crate::html::render_block(&block);
        assert!(
            html.contains(&format!("{MIRO_IMAGE_BASE}1*abc.png")),
            "{html}"
        );
    }

    /// A caption becomes an emphasised line, and an alt with a bracket stays
    /// escaped so it cannot end the image syntax early.
    #[test]
    fn an_image_caption_is_an_emphasised_line() {
        let with_caption = Block::Image {
            id: "i".into(),
            alt: String::new(),
            caption: Some(vec![text("a caption")]),
        };
        assert_eq!(
            render_block(&with_caption),
            ["![](", MIRO_IMAGE_BASE, "i)\n*a caption*"].concat()
        );

        let bracketed = Block::Image {
            id: "i".into(),
            alt: "[not a link]".into(),
            caption: None,
        };
        assert_eq!(
            render_block(&bracketed),
            ["![\\[not a link\\]](", MIRO_IMAGE_BASE, "i)"].concat(),
            "an unescaped `[` inside the alt would end the alt text early"
        );

        let empty_caption = Block::Image {
            id: "i".into(),
            alt: String::new(),
            caption: Some(vec![]),
        };
        assert_eq!(
            render_block(&empty_caption),
            ["![](", MIRO_IMAGE_BASE, "i)"].concat(),
            "an empty caption adds no line"
        );
    }

    /// A row's images each get their own line — Markdown has no row.
    #[test]
    fn an_image_row_puts_each_image_on_its_own_line() {
        let row = Block::ImageRow(vec![
            Block::Image {
                id: "1*a.png".into(),
                alt: "a".into(),
                caption: None,
            },
            Block::Image {
                id: "1*b.png".into(),
                alt: "b".into(),
                caption: None,
            },
        ]);
        let rendered = render_block(&row);
        assert_eq!(rendered.lines().count(), 2);
        assert!(rendered.contains("1*a.png") && rendered.contains("1*b.png"));
    }

    /// **The iframe is byte-identical to the HTML renderer's element**, in both
    /// of its structurally different branches.
    #[test]
    fn an_iframe_is_the_same_element_the_html_renderer_emits() {
        for dims in [Some((640, 360)), None] {
            let block = Block::Iframe {
                src: "https://www.youtube.com/embed/x".into(),
                dims,
            };
            assert_eq!(
                render_block(&block),
                crate::html::render_block(&block),
                "the two renderers disagree about dims = {dims:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // The embed, which is the consumer's format rather than ours
    // ---------------------------------------------------------------------

    /// **The consumer's plugin regex, matched against what we emit.**
    ///
    /// The regex is copied verbatim from `new-web/src/lib/utils/remark/mixtape-embed.js`.
    /// This is the test that keeps the embed shape honest: the bytes are the
    /// contract, because a plugin written against them will not be changed when
    /// this module is.
    #[test]
    fn an_embed_matches_the_consumers_regex() {
        let block = Block::Embed {
            url: "https://www.example.com/post".into(),
            title: "An Article".into(),
            description: "A description".into(),
            // Deliberately a string that appears nowhere else, so "is the site
            // emitted?" is a question this test can actually answer — the URL
            // already contains `example.com`, which is what the plugin derives the
            // site from.
            site: "sitename.invalid".into(),
            thumbnail_id: Some("1*t.png".into()),
        };
        let rendered = render_block(&block);
        assert_eq!(
            rendered,
            "[![An Article](https://www.example.com/post)]\
             (https://miro.medium.com/v2/resize:fit:320/1*t.png)\n\
             >**An Article**\n>A description"
        );

        // `/\[!\[(.*?)\]\((.*?)\)\]\((.*?)\)\n>(.*?)\n>(.*?)\n/`, hand-applied:
        // the captures are alt, link, image, title and description, across the
        // **three** lines `>**title**` / `>description` end with — the regex's
        // trailing `\n` is the one the consumer's paragraph text has and ours
        // does not (see the function's docs).
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("[!["), "{rendered}");
        assert!(lines[0].ends_with(")"), "{rendered}");
        assert!(
            lines[1].starts_with(">**") && lines[1].ends_with("**"),
            "{rendered}"
        );
        assert!(lines[2].starts_with('>'), "{rendered}");
        assert!(
            !rendered.contains("sitename.invalid"),
            "the site is not emitted: the plugin derives it from the URL"
        );
    }

    /// The thumbnail uses the **320** base, the same one `html.rs` and the DTO
    /// use for an embed — not the 700 one, which is the image size.
    #[test]
    fn an_embed_thumbnail_uses_the_320_base() {
        let block = Block::Embed {
            url: "https://www.example.com/post".into(),
            title: "T".into(),
            description: "D".into(),
            site: "example.com".into(),
            thumbnail_id: Some("1*t.png".into()),
        };
        assert!(render_block(&block).contains(&format!("{MIRO_THUMBNAIL_BASE}1*t.png")));
        assert!(!render_block(&block).contains(MIRO_IMAGE_BASE));
    }

    /// **Without a thumbnail the four-line shape is unavailable**, so the embed
    /// degrades to a link plus the description as a quote — the information is
    /// kept rather than dropped.
    #[test]
    fn an_embed_without_a_thumbnail_degrades_to_a_link_and_a_quote() {
        let block = Block::Embed {
            url: "https://www.example.com/post".into(),
            title: "An Article".into(),
            description: "A description".into(),
            site: "example.com".into(),
            thumbnail_id: None,
        };
        assert_eq!(
            render_block(&block),
            "[An Article](https://www.example.com/post)\n>A description"
        );

        let no_description = Block::Embed {
            url: "https://www.example.com/post".into(),
            title: "An Article".into(),
            description: String::new(),
            site: "example.com".into(),
            thumbnail_id: Some(String::new()),
        };
        assert_eq!(
            render_block(&no_description),
            "[An Article](https://www.example.com/post)",
            "an empty thumbnail id is no thumbnail, and an empty description no quote"
        );
    }

    /// **A newline in the title or the description is discarded, not emitted.**
    ///
    /// The plugin's regex is terminated by `\n`, so a description that carried one
    /// would end the match early and leave the rest of the text as stray prose.
    #[test]
    fn a_newline_in_the_embed_text_does_not_break_the_shape() {
        let block = Block::Embed {
            url: "https://www.example.com/post".into(),
            title: "Two\nlines".into(),
            description: "First\nsecond".into(),
            site: "example.com".into(),
            thumbnail_id: Some("t".into()),
        };
        let rendered = render_block(&block);
        assert_eq!(rendered.lines().count(), 3, "{rendered}");
        assert!(rendered.contains(">**Two lines**"), "{rendered}");
        assert!(rendered.contains(">First second"), "{rendered}");
    }

    // ---------------------------------------------------------------------
    // The fragment as a whole
    // ---------------------------------------------------------------------

    /// **A block that renders to nothing contributes no separator.**
    ///
    /// `unknowns.json` holds a `P` whose text is the empty string, so this is a
    /// shape Medium really sends. Joining it blindly would put three newlines
    /// between two paragraphs — a blank line for a block that is not there.
    #[test]
    fn a_block_that_renders_to_nothing_is_dropped() {
        let document = Document {
            meta: Default::default(),
            blocks: vec![
                paragraph(vec![text("first")]),
                paragraph(vec![]),
                Block::ImageRow(vec![]),
                paragraph(vec![text("second")]),
            ],
        };

        let blocks = render_markdown_blocks(&document.blocks);
        assert_eq!(
            blocks[1], "",
            "the empty block keeps its place in the 1:1 view"
        );
        assert_eq!(blocks[2], "", "a row with no images has no Markdown form");
        assert_eq!(
            render_markdown(&document),
            "first\n\nsecond",
            "and is absent from the fragment"
        );
    }

    /// Blocks are separated by exactly one blank line and the fragment has no
    /// trailing newline — the separator that ends a paragraph, a list, a fence and
    /// an HTML block at once.
    #[test]
    fn blocks_are_separated_by_one_blank_line() {
        let document = Document {
            meta: Default::default(),
            blocks: vec![
                paragraph(vec![text("first")]),
                paragraph(vec![text("second")]),
            ],
        };
        let rendered = render_markdown(&document);
        assert_eq!(rendered, "first\n\nsecond");
        assert!(!rendered.ends_with('\n'), "a fragment, not a file");
    }

    /// **No front matter and no title heading.** `/markdown` is a fragment a
    /// consumer splices; a `---` block would be read as a thematic break by
    /// anything that does not expect front matter, and a `# title` would duplicate
    /// the article's own heading.
    #[test]
    fn the_fragment_has_no_front_matter_and_no_title() {
        let document = document_of(json!({
            "title": "A Post Title",
            "previewContent": { "subtitle": "sub" },
            "content": { "bodyModel": { "paragraphs": [
                { "type": "P", "text": "Body text.", "name": "b1", "markups": [] }
            ] } }
        }));
        let rendered = render_markdown(&document);
        assert_eq!(rendered, "Body text.");
        assert!(!rendered.starts_with("---"));
        assert!(!rendered.contains("A Post Title"));
        assert!(!rendered.contains("# "), "no title heading");
    }

    /// Every block the IR can hold renders and produces no blank line, which
    /// would silently split one block into two.
    #[test]
    fn every_block_variant_renders_without_a_blank_line() {
        let blocks = vec![
            Block::Heading {
                level: 2,
                id: "h".into(),
                spacing: HeadingSpacing::None,
                inline: vec![text("H")],
            },
            paragraph(vec![text("p")]),
            Block::List {
                ordered: true,
                items: vec![vec![text("i")]],
            },
            Block::Code {
                lang: Some("rust".into()),
                lines: vec![vec![text("c")]],
            },
            Block::BlockQuote {
                style: QuoteStyle::Inset,
                inline: vec![text("q")],
            },
            Block::Image {
                id: "i".into(),
                alt: "a".into(),
                caption: None,
            },
            Block::ImageRow(vec![Block::Image {
                id: "i".into(),
                alt: "a".into(),
                caption: None,
            }]),
            Block::Embed {
                url: "https://e.test/p".into(),
                title: "T".into(),
                description: "D".into(),
                site: "e.test".into(),
                thumbnail_id: Some("t".into()),
            },
            Block::Iframe {
                src: "s".into(),
                dims: None,
            },
        ];

        let rendered = render_markdown_blocks(&blocks);
        assert_eq!(rendered.len(), blocks.len());
        for block in &rendered {
            assert!(!block.contains("\n\n"), "{block:?} contains a blank line");
            assert!(!block.is_empty(), "a block rendered to nothing");
        }
    }

    /// **Every real fixture, rendered.**
    ///
    /// The `Block` literals above cover the shapes this module thought of. These
    /// are the render gate's own 23 fixtures — real Medium responses, already
    /// compiled by `parse` for the HTML gate — and `article.json` in particular
    /// exists to touch every branch at once. They are read from the gate's
    /// directory rather than copied, so the two cannot drift.
    ///
    /// What is asserted is the invariants that must hold for **any** document, not
    /// any particular output: there is no oracle for Markdown, so a golden file
    /// would only be this module's opinion written down twice.
    #[test]
    fn every_real_fixture_renders_to_a_clean_fragment() {
        let entries = {
            let dir = std::fs::read_dir(FIXTURES)
                .expect("the fixture corpus the render gate uses is in the tree");
            let mut paths: Vec<std::path::PathBuf> = dir
                .map(|entry| entry.expect("a readable directory entry").path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect();
            paths.sort();
            paths
        };
        assert_eq!(entries.len(), 23, "the corpus the render gate compares");

        for path in entries {
            let raw: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("a readable fixture"))
                    .expect("a JSON fixture");
            let name = raw["name"].as_str().unwrap_or("<unnamed>").to_owned();
            let host = raw["host_address"]
                .as_str()
                .unwrap_or("https://freedium.test");
            let payload = PostPayload::from_value(raw["post_data"].clone())
                .expect("a fixture is a valid GraphQL envelope");
            let document = parse::parse(&payload, host);

            let blocks = render_markdown_blocks(&document.blocks);
            assert_eq!(
                blocks.len(),
                document.blocks.len(),
                "{name}: the 1:1 mapping is what makes a failure nameable"
            );
            for block in &blocks {
                assert!(
                    !block.contains("\n\n"),
                    "{name}: a blank line inside a block splits it in two: {block:?}"
                );
            }

            let fragment = render_markdown(&document);
            assert!(
                !fragment.contains("\n\n\n"),
                "{name}: a block that rendered to nothing left a phantom separator"
            );
            // **The plain-text rule, checked against real content.** This renderer
            // must not *introduce* an entity — the consumer escapes on output, so
            // one here reaches a reader as literal punctuation.
            //
            // An entity is not forbidden outright, because the article text can
            // contain one: `escaping.json` exists to prove the page escapes
            // `&amp;` and nothing un-escapes it back, so its own body holds the
            // entity as typed. The check is therefore that every entity in the
            // fragment is one the payload already had.
            let source = raw.to_string();
            for (at, _) in fragment.match_indices('&') {
                let tail = &fragment[at..];
                let Some(semi) = tail.find(';') else { break };
                if semi > 12 {
                    continue;
                }
                let body = &tail[1..semi];
                if body.is_empty() || !body.chars().all(|c| c.is_ascii_alphanumeric() || c == '#') {
                    continue;
                }
                let entity = &tail[..=semi];
                assert!(
                    source.contains(entity),
                    "{name}: the renderer introduced {entity}, which is not in the payload"
                );
            }
            assert_eq!(
                fragment.trim(),
                fragment,
                "{name}: the fragment has padding at its edge"
            );
        }
    }
}
