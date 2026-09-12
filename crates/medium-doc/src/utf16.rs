//! UTF-16 offset mapping — the piece that replaces `rl_string_helper`.
//!
//! Medium addresses markup ranges in UTF-16 code units, because that is what
//! JavaScript string indices are. Python's workaround was to insert a "bang
//! char" (`R`) after every character that occupies two code units, so that
//! Medium's indices lined up with indices into the mutated string, then to
//! delete the bang chars at the very end while dragging a position matrix
//! along. Every historical render bug the rewrite is meant to remove lived in
//! that machinery.
//!
//! Here the same mapping is built once, as data, and never mutated:
//! [`Utf16Map::boundary`] turns a UTF-16 unit index into a byte offset.
//!
//! ## The mapping, derived from the legacy behaviour
//!
//! For a character occupying two UTF-16 units at indices `k` and `k+1`, the
//! legacy matrix assigned index `k` to the character itself and index `k+1` to
//! the inserted bang char, i.e. to the character's end.
//!
//! A template range `[start, end)` was sliced as `[matrix[start],
//! matrix[end - 1] + 1)` in bang-augmented coordinates (`string_helper.py:158-161`).
//! Collapsing that to byte offsets gives a single rule: map both endpoints
//! through `boundary`. It is the same rule, because `matrix[end - 1] + 1` is
//! exactly "the position after the character that unit `end - 1` belongs to",
//! and `boundary(end)` is "where unit `end` starts" — which lands in the same
//! place whether the preceding character was one unit wide or two.
//!
//! Verified against the real implementation, which renders `STRONG[3,5)` over
//! `"hi 😀 there"` as `hi <strong>😀</strong> there`. So a range that *ends* on
//! either of an emoji's units includes the whole emoji, while a range that
//! *starts* on the second unit selects the text between it and the next
//! character boundary (nothing, for `[4,5)`).
//!
//! ## Where the legacy renderer disagreed with itself
//!
//! An endpoint landing on the second unit of a pair makes the legacy code print
//! different bytes from what it selected — the *same* text, corrupted. The bang
//! char it leaves behind is deleted by index *after* the splice has moved
//! everything along, which eats the first character of the tag being spliced in.
//!
//! Measured on the real implementation, one range per row over `"hi 😀 there"`.
//! Unit 3 is the emoji's first unit and unit 4 its second:
//!
//! ```text
//! range   legacy output                       what it was aiming for
//! [0,4)   <strong>hi 😀/strong>R there       <strong>hi 😀</strong> there
//! [2,4)   …<strong> 😀/strong>R there        …<strong> 😀</strong> there
//! [3,4)   …<strong>😀/strong>R there         …<strong>😀</strong> there
//! [4,5)   …😀strong>R</strong> there          …😀 there   (empty range, skipped)
//! [4,7)   …😀strong>R t</strong>there         …😀<strong> t</strong>there
//! [3,5)   …<strong>😀</strong> there          …<strong>😀</strong> there
//! [5,7)   …<strong> t</strong>there           …<strong> t</strong>there
//! ```
//!
//! The split is exactly "is an endpoint on unit 4", and it does not matter
//! whether that endpoint is the start or the end — `[4,5)` and `[4,7)` corrupt
//! just as `[3,4)` does. Every range whose endpoints are both on character
//! boundaries agrees.
//!
//! This module returns the text the legacy code was aiming for. The corruption
//! is one of the render bugs the rewrite exists to remove, so it is deliberately
//! not reproduced, and `xtask/difftest/fixtures/utf16-mid-surrogate.json`
//! declares the difference rather than hiding it.
//! [`Utf16Map::is_mid_surrogate`] exists so callers can log when a payload lands
//! there.

use std::ops::Range;

/// Maps UTF-16 code-unit indices to byte offsets, for one paragraph's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utf16Map {
    /// `boundary[i]` is the byte offset of UTF-16 unit index `i`, for
    /// `i` in `0..=utf16_len()`. Monotonically non-decreasing.
    boundary: Vec<usize>,
    byte_len: usize,
}

impl Utf16Map {
    pub fn new(text: &str) -> Self {
        let mut boundary = Vec::with_capacity(text.len() + 1);
        for (byte_offset, ch) in text.char_indices() {
            // The character's own unit points at its start.
            boundary.push(byte_offset);
            if ch.len_utf16() == 2 {
                // Its second unit points past it — this is where the legacy
                // code put the bang char.
                boundary.push(byte_offset + ch.len_utf8());
            }
        }
        boundary.push(text.len());
        Self {
            boundary,
            byte_len: text.len(),
        }
    }

    /// Number of UTF-16 code units in the text — the index space Medium uses.
    pub fn utf16_len(&self) -> usize {
        self.boundary.len() - 1
    }

    /// Byte offset for a UTF-16 unit index.
    ///
    /// Indices past the end clamp to the end of the text, mirroring the
    /// legacy `end = len(string_pos_matrix)` fixup (`string_helper.py:153`).
    pub fn boundary(&self, utf16_index: usize) -> usize {
        self.boundary
            .get(utf16_index)
            .copied()
            .unwrap_or(self.byte_len)
    }

    /// Resolves a markup range to a byte range.
    ///
    /// Returns `None` for ranges the legacy renderer skipped, so the caller can
    /// warn and move on rather than emit a malformed tag. Those are
    /// `string_helper.py:146-165`:
    ///
    /// - `start` at or past the end of the text
    /// - empty ranges (`start == end`)
    /// - inverted ranges (`matrix[end - 1] + 1 < matrix[start]`)
    pub fn resolve(&self, start: usize, end: usize) -> Option<Range<usize>> {
        if start >= self.utf16_len() || start == end {
            return None;
        }
        // The `end` bound is inclusive-of-last-unit in UTF-16 terms, so `end`
        // itself is already the exclusive boundary.
        let byte_start = self.boundary(start);
        let byte_end = self.boundary(end);
        if byte_end < byte_start {
            return None;
        }
        Some(byte_start..byte_end)
    }

    /// True when `utf16_index` names the *second* unit of a surrogate pair,
    /// i.e. a position inside a single character.
    ///
    /// The legacy code silently produced one of two behaviours here depending
    /// on which endpoint it was; [`Self::resolve`] reproduces that, and this
    /// exists so callers can log when a payload depends on it.
    pub fn is_mid_surrogate(&self, utf16_index: usize) -> bool {
        if utf16_index + 1 >= self.boundary.len() {
            return false;
        }
        // The second unit of a two-unit character maps to the offset just past
        // it, which is also where the next character begins — so the boundary
        // repeats.
        self.boundary[utf16_index] == self.boundary[utf16_index + 1]
    }
}

#[cfg(test)]
mod tests {
    use super::Utf16Map;

    /// "hi 😀 there" — the emoji is one code point, two UTF-16 units, four
    /// bytes. Used throughout because it is the shortest string where code
    /// point count, UTF-16 length and byte length all differ.
    const EMOJI: &str = "hi 😀 there";

    #[test]
    fn ascii_boundaries_are_identity() {
        let map = Utf16Map::new("abcd");
        assert_eq!(map.utf16_len(), 4);
        for i in 0..=4 {
            assert_eq!(map.boundary(i), i);
        }
    }

    #[test]
    fn emoji_lengths_differ_by_encoding() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(EMOJI.len(), 13, "bytes");
        assert_eq!(EMOJI.chars().count(), 10, "code points");
        assert_eq!(map.utf16_len(), 11, "UTF-16 units");
    }

    /// The second unit of the emoji maps past it, not to its start — this is
    /// the bang char's position and the reason `start` and `end` behave
    /// asymmetrically.
    #[test]
    fn second_unit_of_surrogate_maps_past_the_character() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(map.boundary(3), 3, "first unit -> emoji start");
        assert_eq!(map.boundary(4), 7, "second unit -> past the emoji");
        assert_eq!(map.boundary(5), 7, "next char starts where the emoji ends");
    }

    /// Verified against the legacy implementation: `STRONG[3,5)` produced
    /// `hi <strong>😀</strong> there`.
    #[test]
    fn range_covering_both_units_selects_the_emoji() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(map.resolve(3, 5), Some(3..7));
        assert_eq!(&EMOJI[3..7], "😀");
    }

    /// A range that stops after the first unit still takes the whole character:
    /// the legacy slice ran up to the bang char and then dropped it.
    ///
    /// The legacy *renderer* corrupted its output for this range (see the
    /// module docs), but the text it selected was the emoji, which is what this
    /// asserts.
    #[test]
    fn range_ending_on_first_unit_selects_the_emoji() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(map.resolve(3, 4), Some(3..7));
    }

    /// Starting on the second unit excludes the character: the legacy slice
    /// began *at* the bang char, which was then deleted.
    #[test]
    fn range_starting_on_second_unit_excludes_the_emoji() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(map.resolve(4, 5), Some(7..7));
        assert_eq!(&EMOJI[7..7], "");
    }

    /// A range straddling the second unit selects text, it just starts after the
    /// emoji. The legacy renderer corrupted this one too (it ate the `<` of the
    /// opening tag), which is why the fixture for it declares a divergence.
    #[test]
    fn range_from_the_second_unit_to_later_text() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(map.resolve(4, 7), Some(7..9));
        assert_eq!(&EMOJI[7..9], " t");
    }

    #[test]
    fn range_after_the_emoji_still_maps_correctly() {
        let map = Utf16Map::new(EMOJI);
        // Units 5..6 are the space following the emoji.
        assert_eq!(map.resolve(5, 6), Some(7..8));
        assert_eq!(&EMOJI[7..8], " ");
    }

    #[test]
    fn end_past_the_text_clamps() {
        let map = Utf16Map::new(EMOJI);
        // 999 is well past utf16_len; the legacy code clamped `end` to the end
        // of the position matrix rather than failing.
        let resolved = map.resolve(0, 999).expect("clamped rather than skipped");
        assert_eq!(resolved, 0..EMOJI.len());
    }

    #[test]
    fn start_past_the_text_is_skipped() {
        let map = Utf16Map::new(EMOJI);
        assert_eq!(map.resolve(11, 12), None, "start == utf16_len");
        assert_eq!(map.resolve(99, 100), None);
    }

    #[test]
    fn empty_range_is_skipped() {
        let map = Utf16Map::new("abcd");
        assert_eq!(map.resolve(2, 2), None);
    }

    #[test]
    fn inverted_range_is_skipped() {
        let map = Utf16Map::new("abcd");
        assert_eq!(map.resolve(3, 1), None);
    }

    #[test]
    fn mid_surrogate_detection() {
        let map = Utf16Map::new(EMOJI);
        assert!(!map.is_mid_surrogate(3), "first unit is a character start");
        assert!(map.is_mid_surrogate(4), "second unit is mid-character");
        assert!(!map.is_mid_surrogate(5), "next character start");
        assert!(!map.is_mid_surrogate(0));
        assert!(!map.is_mid_surrogate(map.utf16_len()));
    }

    /// Every boundary must land on a character boundary, or slicing the text
    /// would panic. This is the property the whole module exists to guarantee.
    #[test]
    fn all_boundaries_are_char_boundaries() {
        for text in ["", "a", EMOJI, "😀", "😀😀", "a😀b😀c", "ñ\u{0303}", "🇮🇩"] {
            let map = Utf16Map::new(text);
            assert_eq!(map.utf16_len(), text.encode_utf16().count());
            for i in 0..=map.utf16_len() {
                let byte = map.boundary(i);
                assert!(
                    text.is_char_boundary(byte),
                    "{text:?}: utf16 index {i} -> byte {byte} is not a char boundary"
                );
            }
        }
    }

    /// Boundary offsets must never go backwards, since ranges are resolved by
    /// mapping the two endpoints independently.
    #[test]
    fn boundaries_are_monotonic() {
        for text in ["", "abc", EMOJI, "😀a😀", "🇮🇩x🇮🇩"] {
            let map = Utf16Map::new(text);
            let offsets: Vec<usize> = (0..=map.utf16_len()).map(|i| map.boundary(i)).collect();
            assert!(
                offsets.windows(2).all(|w| w[0] <= w[1]),
                "{text:?}: {offsets:?}"
            );
            assert_eq!(offsets[0], 0);
            assert_eq!(*offsets.last().unwrap(), text.len());
        }
    }
}
