//! Inline content → the HTML string the legacy pipeline produced for it.
//!
//! This is `RLStringHelper.get_text()`: the paragraph text escaped, with the
//! `markups.py` tag literals spliced in at their resolved offsets. The legacy
//! code called it in two places, and both need the same bytes:
//!
//! - the renderer, whose block wrappers exist only to hold this string
//!   (`core.py:324-683`), and
//! - the parser, whose highlight check compares a highlight's text against the
//!   **rendered** paragraph (`core.py:309`).
//!
//! It lives in `medium-doc` rather than next to the block templates because the
//! parser needs it and `medium-render` depends on `medium-doc`, not the other
//! way round. Two implementations would be two things to keep in step, and a
//! divergence would show up as a highlight silently applying on one side only.
//!
//! ## Why the escaping needs a cursor
//!
//! The legacy renderer escaped a whole paragraph in one pass and only then
//! spliced markup into it, so its `&` lookahead saw the entire paragraph:
//! `x&amp;y` with a markup at `[2,7)` came out as `x&<strong>amp;y</strong>`, the
//! ampersand surviving because `amp;` follows it. The IR splits that same
//! paragraph into one [`Inline::Text`] per segment, so the escaper is handed the
//! concatenated paragraph text and a cursor into it, and [`escape_range`]
//! restores the original lookahead. Concatenating the text nodes in emission
//! order reconstructs the paragraph exactly, which is the invariant
//! [`TextContext::render`] asserts.

use crate::escape::{EscapeMode, escape_range};
use crate::ir::Inline;

/// `markups.py:45` — the `<strong>` template.
pub const STRONG_OPEN: &str = "<strong>";
pub const STRONG_CLOSE: &str = "</strong>";

/// `markups.py:47`.
pub const EMPHASIS_OPEN: &str = "<em>";
pub const EMPHASIS_CLOSE: &str = "</em>";

/// `markups.py:49-51` — note the single quotes around the class.
pub const CODE_OPEN: &str = "<code class='p-1.5 bg-gray-300 dark:bg-gray-600'>";
pub const CODE_CLOSE: &str = "</code>";

/// `core.py:314-316` — the highlight template.
pub const HIGHLIGHT_OPEN: &str = "<mark class=\"bg-emerald-300\">";
pub const HIGHLIGHT_CLOSE: &str = "</mark>";

/// `markups.py:27`. `rel`, `title` and `href` are spliced in raw, never escaped,
/// because `raw_render` wrapped them in `{% raw %}` (`markups.py:6-11`).
pub fn link_open(rel: &str, title: &str, href: &str, new_tab: bool) -> String {
    format!(
        "<a style=\"text-decoration: underline;\" rel=\"{rel}\" title=\"{title}\" \
         href=\"{href}\" target=\"{}\">",
        if new_tab { "_blank" } else { "" }
    )
}

/// `markups.py:39` — a mention has no `rel`, `title` or `target`.
pub fn user_mention_open(user_id: &str) -> String {
    format!("<a style=\"text-decoration: underline;\" href=\"https://medium.com/u/{user_id}\">")
}

/// Renders one inline list into `out`, appending to whatever is already there.
pub fn render_inlines(out: &mut String, inline: &[Inline], mode: EscapeMode) {
    let mut full = String::new();
    collect_text(inline, &mut full);
    let mut context = TextContext {
        full: &full,
        cursor: 0,
        mode,
    };
    context.render_all(out, inline);
}

/// [`render_inlines`] as a fresh string.
pub fn render_to_string(inline: &[Inline], mode: EscapeMode) -> String {
    let mut out = String::new();
    render_inlines(&mut out, inline, mode);
    out
}

/// Collects the text of every [`Inline::Text`] node, in emission order.
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

/// Carries the paragraph text alongside the escape mode, so a `Text` node can be
/// escaped with the same lookahead the legacy renderer had.
struct TextContext<'a> {
    /// Every text node of the current inline list, concatenated.
    full: &'a str,
    /// Byte offset into `full` where the next `Text` node begins.
    cursor: usize,
    mode: EscapeMode,
}

impl TextContext<'_> {
    fn render_all(&mut self, out: &mut String, nodes: &[Inline]) {
        for node in nodes {
            self.render(out, node);
        }
    }

    fn render(&mut self, out: &mut String, node: &Inline) {
        match node {
            Inline::Text(text) => {
                let start = self.cursor;
                let end = start + text.len();
                debug_assert_eq!(
                    self.full.get(start..end),
                    Some(text.as_str()),
                    "text nodes must concatenate back into the paragraph text"
                );
                escape_range(out, self.full, start, end, self.mode);
                self.cursor = end;
            }

            Inline::Strong(children) => {
                out.push_str(STRONG_OPEN);
                self.render_all(out, children);
                out.push_str(STRONG_CLOSE);
            }
            Inline::Emphasis(children) => {
                out.push_str(EMPHASIS_OPEN);
                self.render_all(out, children);
                out.push_str(EMPHASIS_CLOSE);
            }
            Inline::Code(children) => {
                out.push_str(CODE_OPEN);
                self.render_all(out, children);
                out.push_str(CODE_CLOSE);
            }

            Inline::Link {
                href,
                rel,
                title,
                new_tab,
                children,
            } => {
                out.push_str(&link_open(rel, title, href, *new_tab));
                self.render_all(out, children);
                out.push_str("</a>");
            }

            Inline::UserMention { user_id, children } => {
                out.push_str(&user_mention_open(user_id));
                self.render_all(out, children);
                out.push_str("</a>");
            }

            Inline::Highlight(children) => {
                out.push_str(HIGHLIGHT_OPEN);
                self.render_all(out, children);
                out.push_str(HIGHLIGHT_CLOSE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{STRONG_OPEN, render_to_string};
    use crate::escape::EscapeMode;
    use crate::inline::{Markup, MarkupKind, build_inlines};
    use crate::ir::Inline;

    fn rendered(text: &str, markups: &[Markup], mode: EscapeMode) -> String {
        render_to_string(&build_inlines(text, markups), mode)
    }

    fn strong(start: usize, end: usize) -> Markup {
        Markup {
            start,
            end,
            kind: MarkupKind::Strong,
        }
    }

    #[test]
    fn plain_text_is_escaped() {
        assert_eq!(
            rendered("a & b <c>", &[], EscapeMode::Full),
            "a &amp; b &lt;c&gt;"
        );
    }

    /// Verified byte for byte against the legacy renderer.
    #[test]
    fn markup_literals_match_the_legacy_templates() {
        assert_eq!(
            rendered("abcde", &[strong(1, 4)], EscapeMode::Full),
            "a<strong>bcd</strong>e"
        );
        assert_eq!(STRONG_OPEN, "<strong>");
    }

    /// The case that motivates the cursor: the legacy renderer escaped the whole
    /// paragraph before splicing, so an entity split across a markup boundary
    /// was still recognised.
    #[test]
    fn entity_recognition_survives_a_markup_boundary() {
        assert_eq!(
            rendered("x&amp;y", &[strong(2, 7)], EscapeMode::Full),
            "x&<strong>amp;y</strong>",
            "the ampersand starts the entity `&amp;`, so it must not be escaped"
        );
    }

    /// The entity here straddles the markup boundary: the `Text` before it ends
    /// in `&` and the `amp;` continues inside the `<strong>`. Escaping each node
    /// on its own — the mistake the cursor exists to prevent — would produce
    /// `ab<strong>&amp;am</strong>p;cd`.
    #[test]
    fn concatenating_the_text_nodes_reconstructs_the_paragraph() {
        let inline = build_inlines("ab&amp;cd", &[strong(2, 5)]);
        assert_eq!(
            render_to_string(&inline, EscapeMode::Full),
            "ab<strong>&am</strong>p;cd",
            "the ampersand starts the entity `&amp;`, so it must not be escaped"
        );
    }

    /// Minimal mode leaves quotes alone; the paragraph-level `CODE` rule that
    /// selects it lives in [`crate::escape::mode_for`].
    #[test]
    fn minimal_mode_keeps_quotes() {
        assert_eq!(
            rendered("say \"hi\"", &[], EscapeMode::Minimal),
            "say \"hi\""
        );
    }

    #[test]
    fn a_highlight_renders_its_mark() {
        let inline = vec![Inline::Highlight(vec![Inline::Text("marked".into())])];
        assert_eq!(
            render_to_string(&inline, EscapeMode::Full),
            "<mark class=\"bg-emerald-300\">marked</mark>"
        );
    }

    #[test]
    fn empty_content_renders_nothing() {
        assert_eq!(render_to_string(&[], EscapeMode::Full), "");
    }
}
