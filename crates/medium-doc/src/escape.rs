//! HTML escaping, and the rule that decides which mode applies.
//!
//! Ports `rl_string_helper/utils.py:1-41` and the `has_code_block` hotfix at
//! `core.py:232-237`. In the Python pipeline the replacements were computed on
//! the paragraph text *before* markup templates were spliced in, and then
//! applied to the spliced string through the position matrix — so that escaped
//! entities would land inside tags rather than in the markup itself. Rendering
//! from the IR makes that bookkeeping unnecessary: only [`Inline::Text`] is
//! escaped, and markup is emitted as literal strings.
//!
//! This module lives in `medium-doc` rather than next to the HTML templates
//! because two callers need it: the renderer, obviously, and the parser, whose
//! highlight check (`core.py:309`) compares a highlight's text against the
//! *escaped* paragraph.

use crate::ir::Inline;

/// Which replacement set to apply.
///
/// `core.py:232-237` picks `minimal` for code — and for any paragraph that
/// merely *contains* a `CODE` markup — and `full` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeMode {
    /// `&`, `<`, `>` only.
    Minimal,
    /// `&`, `<`, `>` plus the two quote characters.
    Full,
}

/// The escaping mode for a paragraph's inline content.
///
/// `core.py:232` forces `minimal` when *any* markup in the paragraph is a
/// `CODE`, so code inside a sentence does not get its quotes turned into
/// entities. Code blocks pass `Minimal` directly.
pub fn mode_for(inline: &[Inline]) -> EscapeMode {
    if contains_code(inline) {
        EscapeMode::Minimal
    } else {
        EscapeMode::Full
    }
}

/// True when a `CODE` markup appears anywhere in the tree.
pub fn contains_code(inline: &[Inline]) -> bool {
    inline.iter().any(|item| match item {
        Inline::Code(_) => true,
        Inline::Strong(children)
        | Inline::Emphasis(children)
        | Inline::Highlight(children)
        | Inline::Link { children, .. }
        | Inline::UserMention { children, .. } => contains_code(children),
        Inline::Text(_) => false,
    })
}

/// Entity prefixes that suppress escaping of a leading `&`.
///
/// `MINIMAL_QUOTE_PATTERN` is `([&<>])(?!(amp|lt|gt|quot|#39);)`, so an
/// ampersand already starting a known entity is left alone. Note this does not
/// make the escaper idempotent in general: `&nbsp;` is *not* in the list, so it
/// becomes `&amp;nbsp;`.
const KNOWN_ENTITY_PREFIXES: [&str; 5] = ["amp;", "lt;", "gt;", "quot;", "#39;"];

/// Appends `text` to `out` with HTML entities substituted.
///
/// The `&` lookahead only sees `text`, so an entity split across two calls is
/// not recognised. Use [`escape_range`] when the pieces are slices of one
/// original string.
pub fn escape_into(out: &mut String, text: &str, mode: EscapeMode) {
    escape_range(out, text, 0, text.len(), mode);
}

/// Escapes `full[start..end]`, resolving `&` against the rest of `full`.
///
/// The legacy renderer escaped a whole paragraph in one pass and only then
/// spliced markup into it (`string_helper.py:355-362`), so the entity lookahead
/// crossed markup boundaries. The IR splits the same paragraph into one
/// `Inline::Text` per segment, so the lookahead has to be told about the text
/// that follows — passing the paragraph text here restores the original
/// behaviour.
pub fn escape_range(out: &mut String, full: &str, start: usize, end: usize, mode: EscapeMode) {
    let text = &full[start..end];
    for (offset, ch) in text.char_indices() {
        match ch {
            '&' => {
                // `start + offset + 1` is a char boundary: `&` is one byte.
                let rest = &full[start + offset + 1..];
                if KNOWN_ENTITY_PREFIXES
                    .iter()
                    .any(|prefix| rest.starts_with(prefix))
                {
                    out.push('&');
                } else {
                    out.push_str("&amp;");
                }
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if mode == EscapeMode::Full => out.push_str("&quot;"),
            // `utils.py:14` is `"'": "&#39"` — no terminating semicolon. That is
            // a latent bug, but it is the current output and Fase 1's job is
            // parity, so it is reproduced deliberately. Fixing it is a
            // post-cutover change; see the plan's follow-up notes.
            '\'' if mode == EscapeMode::Full => out.push_str("&#39"),
            other => out.push(other),
        }
    }
}

/// Convenience wrapper around [`escape_into`].
pub fn escape(text: &str, mode: EscapeMode) -> String {
    let mut out = String::with_capacity(text.len());
    escape_into(&mut out, text, mode);
    out
}

#[cfg(test)]
mod tests {
    use super::{EscapeMode, escape, mode_for};
    use crate::ir::Inline;

    const FULL: EscapeMode = EscapeMode::Full;
    const MINIMAL: EscapeMode = EscapeMode::Minimal;

    #[test]
    fn a_paragraph_containing_code_switches_to_minimal() {
        assert_eq!(mode_for(&[Inline::Text("plain".into())]), FULL);
        assert_eq!(
            mode_for(&[
                Inline::Text("say \"hi\" and ".into()),
                Inline::Code(vec![Inline::Text("x < y".into())]),
            ]),
            MINIMAL,
            "`has_code_block` looks at the whole paragraph, not just the code"
        );
    }

    /// The `CODE` can be anywhere in the tree, however deeply nested.
    #[test]
    fn nested_code_still_forces_minimal() {
        assert_eq!(
            mode_for(&[Inline::Strong(vec![Inline::Emphasis(vec![Inline::Code(
                vec![Inline::Text("x".into())]
            )])])]),
            MINIMAL
        );
    }

    #[test]
    fn minimal_escapes_only_angle_brackets_and_ampersand() {
        assert_eq!(
            escape("a & b <c> \"d\" 'e'", MINIMAL),
            "a &amp; b &lt;c&gt; \"d\" 'e'"
        );
    }

    /// Verified against the legacy renderer.
    #[test]
    fn full_escapes_quotes_too() {
        assert_eq!(
            escape("a & b <c> \"d\" 'e'", FULL),
            "a &amp; b &lt;c&gt; &quot;d&quot; &#39e&#39"
        );
    }

    /// The missing semicolon is the legacy behaviour, not a typo here.
    #[test]
    fn apostrophe_entity_has_no_terminating_semicolon() {
        assert_eq!(escape("it's", FULL), "it&#39s");
    }

    #[test]
    fn existing_entities_are_not_double_escaped() {
        for entity in ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;"] {
            assert_eq!(escape(entity, FULL), entity);
        }
    }

    /// Only the five listed entities are recognised; anything else gets its
    /// ampersand escaped. This asymmetry is inherited, not invented.
    #[test]
    fn unknown_entities_are_escaped() {
        assert_eq!(escape("&nbsp;", FULL), "&amp;nbsp;");
        assert_eq!(escape("&amp", FULL), "&amp;amp", "no semicolon, no match");
        assert_eq!(escape("&AMP;", FULL), "&amp;AMP;", "case-sensitive");
    }

    #[test]
    fn bare_ampersand_is_escaped() {
        assert_eq!(escape("x && y", MINIMAL), "x &amp;&amp; y");
    }

    #[test]
    fn non_ascii_passes_through_untouched() {
        assert_eq!(escape("😀 é 中文", FULL), "😀 é 中文");
    }

    #[test]
    fn empty_string_is_empty() {
        assert_eq!(escape("", FULL), "");
        assert_eq!(escape("", MINIMAL), "");
    }

    /// Characters after a multibyte character must still be escaped — a
    /// byte-indexed implementation would corrupt them.
    #[test]
    fn escaping_works_after_multibyte_characters() {
        assert_eq!(escape("😀<", FULL), "😀&lt;");
        assert_eq!(escape("é'", FULL), "é&#39");
        assert_eq!(escape("中文 & more", MINIMAL), "中文 &amp; more");
    }
}
