//! CPython's `textwrap.shorten`, ported.
//!
//! # Why this exists
//!
//! `generate_metadata` derives a post's `description` with
//! `textwrap.shorten(subtitle, width=100, placeholder="...")`
//! (`legacy/medium-parser/medium_parser/core.py:706`), and the description ends
//! up in the page's `<meta name="description">` and its OpenGraph tags. Getting
//! it a character wrong is a visible parity difference, so it is ported rather
//! than approximated by `&s[..100]`.
//!
//! This follows [`crate::difflib`], which ports CPython's
//! `difflib.SequenceMatcher` for the same reason: the behaviour of the standard
//! library *is* the specification here, quirks included.
//!
//! # What a naive port gets wrong
//!
//! Four behaviours, each pinned by a test below:
//!
//! 1. **The text is collapsed before the width is measured.** `shorten` is
//!    `TextWrapper(width, max_lines=1).fill(' '.join(text.strip().split()))`,
//!    so `"  a   b  "` is measured as `"a b"` and the width applies to the
//!    collapsed form.
//! 2. **`break_on_hyphens` is on**, so `wordsep_re` breaks a line after a
//!    hyphen between letters: a 100-column line can end mid-word at `"super-"`.
//!    Splitting on whitespace alone truncates in the wrong place.
//! 3. **An oversized word yields the bare placeholder.** `"x" * 101` shortens
//!    to `"..."`, not to 100 `x`s and not to a prefix.
//! 4. **Python's whitespace set is not Rust's.** `str.split()` treats
//!    `\x1c`–`\x1f` as whitespace; [`char::is_whitespace`] does not.
//!
//! # Scope
//!
//! Only what `shorten` reaches is ported, with the wrapper's configuration
//! hard-coded to the one call site: `width = 100`, `placeholder = "..."`,
//! `max_lines = 1`, empty indents, `drop_whitespace = break_long_words =
//! break_on_hyphens = true`. The knobs are not parameters because nothing would
//! pass anything else, and a half-ported `TextWrapper` would be a worse
//! foundation than this deliberately narrow one. Notably absent is
//! `textwrap.wrap`'s multi-line path, which `max_lines = 1` never takes, and
//! `wordsep_simple_re`, which only `break_on_hyphens = False` would select.
//!
//! # Character counting
//!
//! Python measures strings in code points, not bytes and not grapheme clusters,
//! so every length and every slice here is over `char`s. Slicing a `&str` by
//! byte index would panic on a subtitle containing an emoji.
//!
//! # Unicode predicates
//!
//! [`is_word`], [`is_letter`], and [`is_word_punct`] stand in for the regex
//! classes `\w`, `[^\d\W]`, and `[\w!"'&.,?]`. `\w` and `\d` are Unicode-aware
//! in a Python `str` pattern, and Rust has no exact equivalents, so these use
//! [`char::is_alphanumeric`] / [`char::is_numeric`]. The two sets differ on a
//! handful of punctuation-adjacent code points (Unicode's `Alphabetic` property
//! is slightly wider than Python's `str.isalpha`, and `No`/`Nl` characters are
//! digits to Rust but not to Python's `\d`). That is a deliberate approximation:
//! for a Medium subtitle it is unreachable, and it is recorded here rather than
//! discovered later.

use std::cmp::min;

/// The character class the chunker's regex uses for `\s` and `\S`.
///
/// This is `textwrap`'s private `_whitespace` constant — the *narrow* ASCII
/// set, deliberately not the same as what [`py_isspace`] accepts. `\s` in a
/// Python `str` pattern would be Unicode-aware, but `wordsep_re` builds its
/// class from this literal string instead, so `\xa0` is a *non*-whitespace
/// character to the chunker.
const REGEX_WHITESPACE: [char; 6] = ['\t', '\n', '\u{b}', '\u{c}', '\r', ' '];

fn is_regex_whitespace(c: char) -> bool {
    REGEX_WHITESPACE.contains(&c)
}

/// Python's whitespace set, as used by `str.split()` and `str.strip()`.
///
/// `Py_UNICODE_ISSPACE` is the Unicode `White_Space` property plus the four
/// C0 information separators `\x1c`–`\x1f`, which Rust's
/// [`char::is_whitespace`] omits. That difference is behaviour, not trivia:
/// `"a\x1cb"` collapses to `"a b"` in Python and to `"a\x1cb"` here without
/// this.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// `\w`: word characters, i.e. alphanumerics and the underscore.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `[^\d\W]`: a word character that is not a digit.
///
/// The `\W` half is load-bearing and easy to drop. The class is used to check
/// that a hyphen sits between *letters*, so `"1-2"` must not be a hyphenation
/// point while `"a-b"` must be.
fn is_letter(c: char) -> bool {
    is_word(c) && !c.is_numeric()
}

/// `[\w!"'&.,?]`: what may sit immediately before an em-dash.
fn is_word_punct(c: char) -> bool {
    is_word(c) || matches!(c, '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}

/// `chunk.strip() == ''`, over Python's whitespace set.
fn is_blank(chunk: &[char]) -> bool {
    chunk.iter().all(|c| py_isspace(*c))
}

/// `''.join(chunks)`.
fn join_chunks(chunks: &[Vec<char>]) -> String {
    chunks.iter().flat_map(|chunk| chunk.iter()).collect()
}

/// `str.strip()` / `str.rstrip()`, over Python's whitespace set.
fn py_trim_end(s: &str) -> &str {
    s.trim_end_matches(py_isspace)
}

/// `str.lstrip()`, over Python's whitespace set.
fn py_trim_start(s: &str) -> &str {
    s.trim_start_matches(py_isspace)
}

/// `' '.join(text.strip().split())`.
///
/// `strip` is redundant — `str.split()` with no separator already drops the
/// empty pieces that leading and trailing whitespace would produce — but it is
/// what `shorten` does, so the composition is kept visible.
fn collapse(text: &str) -> Vec<char> {
    let mut out = String::with_capacity(text.len());
    for (i, word) in text.split(py_isspace).filter(|w| !w.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.chars().collect()
}

/// A `-{2,}` run that the regex's em-dash alternative matches at `pos`.
///
/// This is a *top-level* alternative in `wordsep_re`, tried before the word
/// branch, so it wins over it at the same start position. That ordering is
/// observable: in `"a---b"` Python produces the chunks `["a", "---", "b"]`, and
/// a chunker that only looked for continuations inside the word branch would
/// produce `["a", "---b"]`. The two rejoin identically on a line, so it only
/// shows up when the *placeholder* replaces chunks from the end — which is
/// exactly the truncation this module exists for.
fn em_dash_at(chars: &[char], pos: usize) -> Option<usize> {
    if pos == 0 || chars[pos] != '-' || !is_word_punct(chars[pos - 1]) {
        return None;
    }

    let mut end = pos;
    while end < chars.len() && chars[end] == '-' {
        end += 1;
    }

    // `-{2,}` is greedy, and a shorter match would have to end on a `-`, which
    // cannot satisfy `(?=\w)`. So either the whole run works or nothing does.
    if end - pos >= 2 && end < chars.len() && is_word(chars[end]) {
        Some(end)
    } else {
        None
    }
}

/// Where the regex's word branch ends when its group matches at `i`, or `None`
/// when it cannot match there.
///
/// `i` is the position just past the lazy `\S+?`, and is always `> 0`. Because
/// the lazy quantifier tries the shortest prefix first, the *first* `i` for
/// which this returns `Some` gives the chunk boundary — so the caller grows `i`
/// one character at a time and stops at the first hit.
///
/// The start of the word never matters: the end-of-word branch below fires at
/// the first whitespace character, so `\S+?` can never grow across one and the
/// predicate needs only `i`.
///
/// **The returned end is not always `i`.** The hyphenated-word branch consumes
/// one `-` character as part of the match, so the chunk ends at `i + 1` and the
/// hyphen belongs to the *preceding* chunk: `"goof-ball"` is `"goof-"` then
/// `"ball"`, not `"goof"` then `"-ball"`. The other two branches are zero-width
/// lookarounds and end exactly at `i`.
fn word_branch_end(chars: &[char], i: usize) -> Option<usize> {
    let n = chars.len();

    // Hyphenated word: `-` such that the chunk may break after it, provided a
    // letter follows the break.
    if i < n && chars[i] == '-' {
        let j = i;
        // `(?<=[^\d\W]{2}-) | (?<=[^\d\W]-[^\d\W]-)` — the lookbehind runs over
        // the whole text, so an already-emitted `-b-` before this chunk counts.
        // It also has to fit: a hyphen at index 1 has no two characters before
        // it, which is why `"a-b"` is one chunk and `"goof-ball"` is two.
        let two_letters = j >= 2 && is_letter(chars[j - 2]) && is_letter(chars[j - 1]);
        let letter_hyphen_letter_hyphen =
            j >= 3 && is_letter(chars[j - 3]) && chars[j - 2] == '-' && is_letter(chars[j - 1]);
        if two_letters || letter_hyphen_letter_hyphen {
            // `(?=[^\d\W]-?[^\d\W])`
            if j + 1 < n && is_letter(chars[j + 1]) {
                let after = if j + 2 < n && chars[j + 2] == '-' {
                    j + 3
                } else {
                    j + 2
                };
                if after < n && is_letter(chars[after]) {
                    return Some(j + 1);
                }
            }
        }
    }

    // End of word: `(?=\s|\Z)`.
    if i == n || is_regex_whitespace(chars[i]) {
        return Some(i);
    }

    // Em-dash continuation: `(?<=[\w!"'&.,?])(?=-{2,}\w)`. The chunk ends just
    // before a dash run, leaving the dashes to the em-dash alternative.
    if chars[i] == '-' && is_word_punct(chars[i - 1]) {
        let mut end = i;
        while end < n && chars[end] == '-' {
            end += 1;
        }
        if end - i >= 2 && end < n && is_word(chars[end]) {
            return Some(i);
        }
    }

    None
}

/// `TextWrapper._split`, for `break_on_hyphens = True`.
///
/// `re.split` with a capturing group returns the separators interleaved with
/// the text between them; the group here spans every alternative, and the word
/// branch matches at any non-whitespace position, so the string is fully
/// partitioned and the empty pieces are the only thing `[c for c in chunks if
/// c]` removes.
///
/// Returns chunks in forward order; [`wrap_chunks`] reverses them, as CPython
/// does, so that the line-filling loop pops from the end of a stack.
fn split_chunks(chars: &[char]) -> Vec<Vec<char>> {
    let n = chars.len();
    let mut chunks: Vec<Vec<char>> = Vec::new();
    let mut pos = 0;

    while pos < n {
        if is_regex_whitespace(chars[pos]) {
            let mut end = pos + 1;
            while end < n && is_regex_whitespace(chars[end]) {
                end += 1;
            }
            chunks.push(chars[pos..end].to_vec());
            pos = end;
            continue;
        }

        if let Some(end) = em_dash_at(chars, pos) {
            chunks.push(chars[pos..end].to_vec());
            pos = end;
            continue;
        }

        let mut end = pos + 1;
        let word_end = loop {
            if let Some(end) = word_branch_end(chars, end) {
                break end;
            }
            end += 1;
            debug_assert!(end <= n, "the end-of-word branch must terminate the word");
        };
        chunks.push(chars[pos..word_end].to_vec());
        pos = word_end;
    }

    chunks
}

/// `TextWrapper._handle_long_word`, for `break_long_words = break_on_hyphens =
/// true`.
///
/// Only reached when the next chunk is wider than the whole line, so there is
/// nothing on `cur_line` to work around: the chunk is cut at the last hyphen
/// that fits, or at the column limit when there is none.
fn handle_long_word(
    chunks: &mut Vec<Vec<char>>,
    cur_line: &mut Vec<Vec<char>>,
    cur_len: usize,
    width: usize,
) {
    // "Figure out when indent is larger than the specified width, and make sure
    // at least one character is stripped off on every pass."
    let space_left = if width < 1 { 1 } else { width - cur_len };

    if space_left > 0 {
        let chunk = chunks.last().expect("caller checked for a chunk").clone();
        let mut end = space_left;

        // `chunk.len() > space_left` is implied by the caller's `> width`
        // check, but the hyphen search below would panic on `chunk[..space_left]`
        // without it, so the guard is kept.
        if chunk.len() > space_left {
            // "break after last hyphen, but only if there are non-hyphens
            // before it" — so `"---"` finds no break and `"a--"` breaks only
            // after the `a`.
            let hyphen = chunk[..space_left]
                .iter()
                .rposition(|c| *c == '-')
                .filter(|h| *h > 0 && chunk[..*h].iter().any(|c| *c != '-'));
            if let Some(h) = hyphen {
                end = h + 1;
            }
        }

        // `end` is always `<= chunk.len()`: the caller only gets here when
        // `chunk.len() > width >= space_left`. Clamping is belt and braces.
        let end = min(end, chunk.len());
        cur_line.push(chunk[..end].to_vec());
        *chunks.last_mut().expect("checked above") = chunk[end..].to_vec();
    } else if cur_line.is_empty() {
        // "Otherwise, we have to preserve the long word intact. Only add it to
        // the current line if there's nothing already there."
        cur_line.push(chunks.pop().expect("caller checked for a chunk"));
    }
}

/// `TextWrapper(width, max_lines=1, placeholder).fill`, for empty indents and
/// `drop_whitespace = true`.
///
/// Returns the single line, or `""` when the text was empty.
fn wrap_chunks(chunks: Vec<Vec<char>>, width: usize, placeholder: &str) -> String {
    debug_assert!(width > 0, "shorten's width is a positive constant");
    debug_assert!(
        placeholder.chars().count() <= width,
        "the wrapper raises when the placeholder does not fit"
    );

    // "Arrange in reverse order so items can be efficiently popped from a stack
    // of chucks."
    let mut chunks: Vec<Vec<char>> = chunks;
    chunks.reverse();

    let placeholder: Vec<char> = placeholder.chars().collect();
    let placeholder_text: String = placeholder.iter().collect();
    let mut lines: Vec<String> = Vec::new();

    while !chunks.is_empty() {
        let mut cur_line: Vec<Vec<char>> = Vec::new();
        let mut cur_len = 0usize;

        // `indent` is empty for both the initial and the subsequent line, so
        // `width` is the full width on every pass.

        // "First chunk on line is whitespace -- drop it, unless this is the
        // very beginning of the text."
        if !lines.is_empty() && is_blank(chunks.last().expect("loop condition")) {
            chunks.pop();
        }

        while let Some(chunk) = chunks.last() {
            if cur_len + chunk.len() <= width {
                cur_len += chunk.len();
                cur_line.push(chunks.pop().expect("just matched"));
            } else {
                break;
            }
        }

        // "The current line is full, and the next chunk is too big to fit on
        // *any* line (not just this one)."
        if chunks.last().is_some_and(|chunk| chunk.len() > width) {
            handle_long_word(&mut chunks, &mut cur_line, cur_len, width);
            cur_len = cur_line.iter().map(Vec::len).sum();
        }

        // "If the last chunk on this line is all whitespace, drop it."
        if let Some(last) = cur_line.last().filter(|last| is_blank(last)) {
            let last_len = last.len();
            cur_line.pop();
            cur_len -= last_len;
        }

        if cur_line.is_empty() {
            continue;
        }

        // `max_lines = 1`, so `len(lines) + 1 < max_lines` is never true and
        // the only way to accept is having consumed everything that matters.
        let exhausted = chunks.is_empty()
            || (chunks.len() == 1 && is_blank(chunks.first().expect("length checked")));
        if exhausted && cur_len <= width {
            lines.push(join_chunks(&cur_line));
            continue;
        }

        // The line overflowed and cannot be kept as it is: trim whole chunks
        // off the end until the placeholder fits.
        let mut placed = false;
        while let Some(last) = cur_line.last() {
            if !is_blank(last) && cur_len + placeholder.len() <= width {
                cur_line.push(placeholder.clone());
                lines.push(join_chunks(&cur_line));
                placed = true;
                break;
            }
            let last_len = last.len();
            cur_line.pop();
            cur_len -= last_len;
        }

        if !placed {
            // Nothing of this line survives. In the multi-line case the
            // placeholder would go on the previous line — unreachable here,
            // since `max_lines = 1` means there is no previous line — so what
            // is left is the bare placeholder as the only line.
            if let Some(previous) = lines.last() {
                let trimmed = py_trim_end(previous);
                if trimmed.chars().count() + placeholder.len() <= width {
                    let mut joined = trimmed.to_string();
                    joined.push_str(&placeholder_text);
                    *lines.last_mut().expect("just matched") = joined;
                    break;
                }
            }
            lines.push(py_trim_start(&placeholder_text).to_string());
        }

        break;
    }

    lines.join("\n")
}

/// `textwrap.shorten(text, width, placeholder=...)`, with `max_lines = 1`.
///
/// The one behaviour worth calling out from the call site's point of view: an
/// input that is already short enough comes back *collapsed* but otherwise
/// unchanged, so this is not a pure function of the width threshold.
///
/// # Panics
///
/// If `width` is zero, or the placeholder does not fit in `width` — both of
/// which raise `ValueError` in Python.
pub fn shorten(text: &str, width: usize, placeholder: &str) -> String {
    assert!(width > 0, "invalid width (must be > 0)");
    assert!(
        placeholder.chars().count() <= width,
        "placeholder too large for max width"
    );

    let collapsed = collapse(text);
    wrap_chunks(split_chunks(&collapsed), width, placeholder)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `textwrap.shorten(text, width=100, placeholder="...")`.
    fn shorten100(text: &str) -> String {
        shorten(text, 100, "...")
    }

    /// Cases captured from CPython, assertions written from the outputs.
    ///
    /// Regenerate with, and compare against, a real interpreter — the point of
    /// the table is that the expected values are not this module's author's
    /// opinion of what `shorten` should do:
    ///
    /// ```text
    /// python3 -c "
    /// import textwrap, json, sys
    /// w = textwrap.TextWrapper(width=100, max_lines=1, placeholder='...')
    /// for s in json.load(sys.stdin):
    ///     print(json.dumps(w.fill(' '.join(s.strip().split())), ensure_ascii=False))"
    /// ```
    ///
    /// The wrapping algorithm is identical in CPython 3.12 and 3.14 (verified
    /// by diffing `TextWrapper._wrap_chunks`'s source between them), which
    /// matters because `legacy/` runs on 3.12 and the gate runs on whatever
    /// `python3` is on `PATH`.
    ///
    /// A function rather than a plain `const` only because the length-boundary
    /// cases are repeats: `String::repeat` is not const.
    fn golden() -> Vec<(String, String)> {
        let mut cases: Vec<(String, String)> = GOLDEN
            .iter()
            .map(|(input, expected)| (input.to_string(), expected.to_string()))
            .collect();

        // Exactly at the limit: no placeholder, no truncation. One over, and a
        // single word that cannot fit: the bare placeholder — not a
        // 100-character prefix, and not a `"..."`-suffixed one.
        for n in [99usize, 100] {
            cases.push(("x".repeat(n), "x".repeat(n)));
        }
        cases.push(("x".repeat(101), "...".to_string()));

        cases
    }

    const GOLDEN: &[(&str, &str)] = &[
        // Width is measured against the collapsed text, so the boundary sits
        // at 100 code points *after* collapsing.
        ("x", "x"),
        ("a and\tb\nc  ", "a and b c"),
        ("multi   space   collapse", "multi space collapse"),
        ("trailing space ", "trailing space"),
        ("", ""),
        ("   ", ""),
        // `\x1c`-`\x1f` are whitespace to Python and not to Rust.
        ("\u{1c}unit\u{1d}sep\u{1e}here\u{1f}!", "unit sep here !"),
        // U+00A0 is whitespace to Python's `str.split()`...
        ("\u{a0}nbsp run\u{a0}here", "nbsp run here"),
        // ...and a *non*-whitespace character to the chunker's `\S`, which is
        // the narrow ASCII set. It survives collapsing only because `split`
        // already removed it; this case pins that it does not reappear.
        ("emoji \u{1f600} tail", "emoji \u{1f600} tail"),
        // Truncation lands on a word boundary, and the trailing chunk that no
        // longer fits takes its preceding space with it.
        (
            "word word word word word word word word word word word word word word word word word word word word word word word word word word word word word word ",
            "word word word word word word word word word word word word word word word word word word word...",
        ),
        // 50 + 1 + 60: the second word cannot fit on the line at all, and the
        // space before it is dropped with it.
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...",
        ),
        // Hyphens are live: `super-duper-...` is a single whitespace-delimited
        // word that the chunker splits further, so the line can end mid-word. A
        // whitespace split would have truncated after `alpha`.
        (
            "alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha super-duper-hyphenated-word-here tail",
            "alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha alpha super-...",
        ),
        // The hyphenated chunks interact with the placeholder: the dropped
        // `kata` takes the count back under the limit, so `kata-` survives.
        (
            "kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata panjang-sekali-ini-ya",
            "kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-kata kata-...",
        ),
        (
            "one-two-three-one-two-three-one-two-three-one-two-three-one-two-three-one-two-three-one-two-three-one-two-three-",
            "one-two-three-one-two-three-one-two-three-one-two-three-one-two-three-one-two-three-one-two-...",
        ),
        (
            "well-known fact well-known fact well-known fact well-known fact well-known fact well-known fact well-known fact well-known fact well-known fact well-known fact ",
            "well-known fact well-known fact well-known fact well-known fact well-known fact well-known fact...",
        ),
        // Short inputs are returned collapsed but otherwise untouched, whatever
        // punctuation or hyphens they contain.
        ("it's", "it's"),
        ("a & b", "a & b"),
        ("say \"hi\"", "say \"hi\""),
        (
            "\u{201c}curly\u{201d} quotes",
            "\u{201c}curly\u{201d} quotes",
        ),
        (
            "Look, goof-ball -- use the -b option!",
            "Look, goof-ball -- use the -b option!",
        ),
        ("self-evident", "self-evident"),
        ("-", "-"),
        ("--", "--"),
        ("a-", "a-"),
        ("-a", "-a"),
        ("a-b", "a-b"),
        ("a--b", "a--b"),
        ("a---b", "a---b"),
        ("word---next", "word---next"),
        ("a -- b", "a -- b"),
    ];

    #[test]
    fn matches_cpython() {
        for (input, expected) in golden() {
            assert_eq!(
                shorten100(&input),
                expected,
                "shorten({input:?}) diverged from CPython"
            );
        }
    }

    /// `shorten` is called on an *escaped* subtitle, so its input can be full
    /// of `&#39` and `&quot` — strings whose `;`-less and `&`-heavy shape is
    /// nothing like prose. They must not trip the chunker's word characters.
    #[test]
    fn escaped_text_is_wrapped_as_ordinary_characters() {
        // `'` escaped in Full mode, then the whole thing escaped again by the
        // caller — the double escape `description` actually receives.
        assert_eq!(shorten100("it&#39s fine"), "it&#39s fine");
        assert_eq!(
            shorten100(&"&amp;#39 ".repeat(20)),
            "&amp;#39 &amp;#39 &amp;#39 &amp;#39 &amp;#39 &amp;#39 &amp;#39 &amp;#39 &amp;#39 &amp;#39..."
        );
    }

    /// Collapsing is the first step, so whether a subtitle truncates depends on
    /// its width *after* collapsing. This is the case a byte-length check, or a
    /// check against the raw input, gets wrong.
    #[test]
    fn width_applies_after_collapsing() {
        // 60 `a`s separated by single spaces: 119 columns, truncates.
        let spaced = "a ".repeat(60);
        assert_eq!(spaced.trim_end().chars().count(), 119);
        assert!(shorten100(&spaced).ends_with("..."));

        // The same text padded with whitespace collapses to the same string, so
        // the padding changes neither the output nor the truncation.
        assert_eq!(shorten100(&format!("  {spaced}  ")), shorten100(&spaced));

        // 50 + 49 words are 101 columns raw but exactly 100 once the tab and
        // newline collapse to single spaces — so this must *not* truncate.
        let wide = format!("{}\t\n{}", "a".repeat(50), "b".repeat(49));
        assert_eq!(wide.chars().count(), 101);
        assert_eq!(
            shorten100(&wide),
            format!("{} {}", "a".repeat(50), "b".repeat(49))
        );
    }

    /// The chunker, pinned directly against CPython's `wordsep_re`.
    ///
    /// The wrapper only reveals chunk boundaries when it truncates, so a
    /// disagreement that happens to fit on one line is invisible in
    /// [`golden`]. These are `wordsep_re.split(text)` from a real interpreter,
    /// filtered of the empty strings `_split` drops.
    const CHUNKS: &[(&str, &[&str])] = &[
        (
            "Look, goof-ball -- use the -b option!",
            &[
                "Look,", " ", "goof-", "ball", " ", "--", " ", "use", " ", "the", " ", "-b", " ",
                "option!",
            ],
        ),
        // The hyphenated-word branch consumes the hyphen, so it ends the
        // *preceding* chunk: `goof-` then `ball`.
        ("goof-ball", &["goof-", "ball"]),
        (
            "alpha-beta gamma-delta",
            &["alpha-", "beta", " ", "gamma-", "delta"],
        ),
        (
            "super-duper-hyphenated-word-here",
            &["super-", "duper-", "hyphenated-", "word-", "here"],
        ),
        ("self-evident", &["self-", "evident"]),
        // Both lookbehinds need characters that are not there — a hyphen at
        // index 1 has no two letters before it — so a two-letter word does not
        // break, while `goof-ball` does.
        ("a-b", &["a-b"]),
        // `[^\d\W]` needs *letters* around the hyphen, so digits do not break.
        ("1-2", &["1-2"]),
        ("-a", &["-a"]),
        ("a-", &["a-"]),
        ("--", &["--"]),
        // The top-level em-dash alternative wins over the word branch at the
        // same position, which is what splits the run out as its own chunk.
        ("a---b", &["a", "---", "b"]),
        ("word---next", &["word", "---", "next"]),
        ("a -- b", &["a", " ", "--", " ", "b"]),
        // The em-dash needs a word character after it, so a trailing run stays
        // inside the word.
        ("a --", &["a", " ", "--"]),
    ];

    #[test]
    fn chunking_matches_cpython_wordsep_re() {
        for (input, expected) in CHUNKS {
            let chunks: Vec<String> = split_chunks(&collapse(input))
                .iter()
                .map(|c| c.iter().collect())
                .collect();
            assert_eq!(&chunks, expected, "chunking {input:?} diverged");
        }
    }

    /// Every chunk must rejoin into the collapsed input: chunking decides where
    /// the placeholder may cut, not what the text says.
    #[test]
    fn chunking_is_lossless() {
        let golden = golden();
        let inputs: Vec<&str> = golden
            .iter()
            .map(|(input, _)| input.as_str())
            .chain(CHUNKS.iter().map(|(input, _)| *input))
            .collect();

        for input in inputs {
            let collapsed: String = collapse(input).iter().collect();
            let rejoined: String = split_chunks(&collapse(input))
                .iter()
                .map(|c| c.iter().collect::<String>())
                .collect();
            assert_eq!(rejoined, collapsed, "chunking {input:?} lost or added text");
        }
    }

    /// An emoji is one `char` but four bytes, so any length measured in bytes —
    /// or any slice taken by byte index — either miscounts or panics here.
    #[test]
    fn lengths_are_counted_in_code_points_not_bytes() {
        let emoji = "\u{1f600}".repeat(100);
        assert_eq!(emoji.chars().count(), 100);
        assert_eq!(emoji.len(), 400, "the byte length is what must not be used");

        assert_eq!(shorten100(&emoji), emoji, "100 code points fit in 100");
        assert_eq!(
            shorten100(&"\u{1f600}".repeat(101)),
            "...",
            "a single 101-code-point word is replaced whole"
        );
    }

    /// The bounds Python enforces with `ValueError`. Both are unreachable from
    /// the one call site, which is exactly why they are worth a test: a future
    /// caller passing a width from configuration should find out at the call,
    /// not by silently getting `""`.
    #[test]
    #[should_panic(expected = "invalid width")]
    fn a_zero_width_is_rejected() {
        shorten("text", 0, "...");
    }

    #[test]
    #[should_panic(expected = "placeholder too large")]
    fn an_oversized_placeholder_is_rejected() {
        shorten("text", 2, "...");
    }
}
