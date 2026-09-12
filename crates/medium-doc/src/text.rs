//! Text normalisation applied before any markup is resolved.
//!
//! Ports `quote_symbol` from `rl_string_helper/utils.py:20-26`, which
//! `RLStringHelper.__init__` runs over every paragraph before building its
//! position matrix.
//!
//! Medium's editor emits typographic quotes; the site renders straight ones.
//! Each substitution is one character for one character, so it does not shift
//! any offset — but it must happen *before* the UTF-16 map is built, because
//! the map is indexed against the text that is actually rendered.

/// Replaces curly quotation marks with their ASCII equivalents.
///
/// Note this is applied to paragraphs and to nothing else: the title and
/// subtitle used for de-duplication comparison are compared in their raw form
/// (`core.py:261`, `core.py:276`).
pub fn quote_symbol(text: &str) -> String {
    if !text.contains(['\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}']) {
        return text.to_string();
    }
    text.chars()
        .map(|ch| match ch {
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::quote_symbol;

    #[test]
    fn curly_quotes_become_straight() {
        assert_eq!(quote_symbol("\u{201c}hi\u{201d}"), "\"hi\"");
        assert_eq!(quote_symbol("\u{2018}x\u{2019}"), "'x'");
    }

    #[test]
    fn straight_quotes_are_untouched() {
        assert_eq!(quote_symbol("\"hi\" 'x'"), "\"hi\" 'x'");
    }

    #[test]
    fn other_text_passes_through() {
        assert_eq!(
            quote_symbol("plain \u{2014} text 😀"),
            "plain \u{2014} text 😀"
        );
        assert_eq!(quote_symbol(""), "");
    }

    /// One character in, one character out — the property that lets this run
    /// before the offset map without invalidating Medium's indices.
    #[test]
    fn length_is_preserved() {
        for text in ["\u{201c}a\u{201d}", "\u{2018}\u{2019}", "no quotes here"] {
            assert_eq!(
                quote_symbol(text).chars().count(),
                text.chars().count(),
                "{text:?}"
            );
        }
    }
}
