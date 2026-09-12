//! Turning a row of the `cache` table back into JSON.
//!
//! `cache` is `(key TEXT PRIMARY KEY, value TEXT)` and
//! `PostgreSQLCacheBackend.push` writes whatever `orjson` produced, so the
//! common case is a plain `serde_json` parse. Some rows are not that.
//!
//! `CacheData.workaround_decode_json`
//! (`legacy/database-lib/database_lib/main.py:37`) exists because a subset of
//! the table holds values that were stored as hex-encoded bytes with a literal
//! `\x` in front — the marker Postgres uses for a `bytea` literal, which is the
//! likely origin. Those rows fail a JSON parse and need decoding first.
//!
//! This is not a hypothetical path being carried along for completeness: the
//! corpus in §4 is *dumped from this table*, so a reader that cannot decode
//! these rows cannot read part of its own regression corpus.
//!
//! # Two quirks, reproduced deliberately
//!
//! 1. **The fallback is reached on any parse failure**, not only on a `\x`
//!    prefix. A row that is neither JSON nor hex is hex-decoded anyway and
//!    fails there instead. The legacy code has no branch that handles only the
//!    prefixed case, so narrowing it here would change which error a corrupt
//!    row produces — and, worse, would silently succeed on rows that are
//!    unprefixed hex, which is quirk 2.
//! 2. **Unprefixed hex decodes too.** `if startswith("\\x"): s = data[2:] else:
//!    s = data` — the else branch does not bail out, it hex-decodes the raw
//!    value. Kept.
//!
//! # What is not reproduced
//!
//! Python logs `logger.warning` on *every* fallback. Here the fallback is
//! silent and only a genuine failure is reported by the caller, because falling
//! back is routine for these rows — a warning per read would be noise at the
//! volume this table sees.

use serde_json::Value;

use crate::error::CacheError;

/// The two characters backslash and `x`, as Python's `"\\x"` denotes them.
/// Not an escape sequence as far as the value is concerned — it is a literal
/// prefix on the stored text.
const HEX_PREFIX: &str = "\\x";

/// How much of a failing value to name in the log line, in characters.
///
/// Python slices `self.data[:100]`, i.e. 100 *characters*. Byte-slicing the
/// same amount would panic on a value whose 101st byte is a continuation byte,
/// which for this table is likely rather than exotic — the rows are JSON
/// containing article text.
const PREVIEW_CHARS: usize = 100;

/// Decodes a stored `cache.value` into JSON.
///
/// Tries plain JSON first — that is the shape almost every row has, and trying
/// it first is also what makes a value that merely *looks* hex-ish still parse
/// as the JSON it is.
pub fn decode_json(raw: &str) -> Result<Value, CacheError> {
    match serde_json::from_str(raw) {
        Ok(value) => Ok(value),
        Err(first_error) => match decode_hex_encoded_json(raw) {
            Ok(value) => Ok(value),
            Err(hex_error) => {
                // Name both failures: the first says what the value looked like
                // (so a bad row can be recognised), the second says why the
                // fallback did not rescue it.
                tracing::warn!(
                    preview = %preview(raw),
                    json_error = %first_error,
                    hex_error = %hex_error,
                    "cache value is neither JSON nor hex-encoded JSON"
                );
                Err(hex_error)
            }
        },
    }
}

/// The fallback: strip the `\x` marker if present, hex-decode, then parse.
fn decode_hex_encoded_json(raw: &str) -> Result<Value, CacheError> {
    let digits = raw.strip_prefix(HEX_PREFIX).unwrap_or(raw);
    let bytes = hex::decode(digits)?;
    let text = String::from_utf8(bytes)?;
    Ok(serde_json::from_str(&text)?)
}

/// A character-safe prefix of `raw`, for logging.
///
/// `char_indices` rather than `&raw[..n]` so a multi-byte character straddling
/// the cut cannot panic.
fn preview(raw: &str) -> String {
    match raw.char_indices().nth(PREVIEW_CHARS) {
        Some((byte_index, _)) => format!("{}…", &raw[..byte_index]),
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_json_passes_through() {
        let decoded = decode_json(r#"{"data":{"post":{"id":"abc"}}}"#).unwrap();
        assert_eq!(decoded["data"]["post"]["id"], "abc");
    }

    /// The row shape the workaround exists for: `\x` + hex of the JSON text.
    #[test]
    fn hex_prefixed_row_decodes() {
        let text = r#"{"data":{"post":{"id":"abc"}}}"#;
        let stored = format!("\\x{}", hex::encode(text));

        let decoded = decode_json(&stored).unwrap();
        assert_eq!(decoded["data"]["post"]["id"], "abc");
    }

    /// Quirk 2, pinned as a test so a future "cleanup" has to argue with it.
    /// Unprefixed hex is decoded, not rejected.
    #[test]
    fn unprefixed_hex_also_decodes() {
        let text = r#"{"ok":true}"#;
        let stored = hex::encode(text);

        assert_eq!(decode_json(&stored).unwrap(), json!({"ok": true}));
    }

    /// Quirk 1: the fallback is reached on any JSON failure. A value that fails
    /// both is an error, and specifically the *hex* error — that is what
    /// distinguishes "corrupt row" from "parse failed".
    #[test]
    fn value_that_is_neither_json_nor_hex_is_an_error() {
        let err = decode_json("not json and not hex").unwrap_err();
        assert!(matches!(err, CacheError::Hex(_)), "got {err:?}");
    }

    /// Odd digit count: `binascii.unhexlify` rejects it, and so must we, rather
    /// than decoding the even prefix and parsing the remainder's half.
    #[test]
    fn odd_length_hex_is_rejected() {
        let err = decode_json("abc").unwrap_err();
        assert!(matches!(err, CacheError::Hex(_)), "got {err:?}");
    }

    /// Valid hex whose bytes are not UTF-8. Distinct from the `Hex` variant:
    /// the hex was fine, the decoded text was not.
    #[test]
    fn hex_that_is_not_utf8_is_rejected() {
        let err = decode_json("fffe").unwrap_err();
        assert!(matches!(err, CacheError::Utf8(_)), "got {err:?}");
    }

    /// The ordering matters: valid JSON wins even when a hex decoding would
    /// also have been possible. Here the value *is* a JSON string that happens
    /// to hold hex-looking text, so a hex-first reader would return something
    /// else entirely.
    #[test]
    fn json_is_tried_before_hex() {
        // 4 hex digits, so `hex::decode` would happily produce 2 bytes.
        let stored = r#""abcd""#;
        assert_eq!(decode_json(stored).unwrap(), json!("abcd"));
    }

    /// `binascii.unhexlify` rejects whitespace, and so does `hex::decode`.
    /// Worth pinning because "be lenient" is the tempting change here.
    #[test]
    fn whitespace_in_hex_is_rejected() {
        let stored = format!("\\x{}", "7b 7d");
        assert!(decode_json(&stored).is_err());
    }

    /// Empty input is not JSON, and hex-decoding it yields empty bytes, which
    /// is not JSON either. An error, not a panic.
    #[test]
    fn empty_value_is_an_error() {
        assert!(decode_json("").is_err());
    }

    #[test]
    fn preview_is_short_for_short_input() {
        assert_eq!(preview("abc"), "abc");
    }

    /// The reason `preview` walks char boundaries: a 100-*byte* slice of this
    /// string would panic, since every character here is 4 bytes.
    #[test]
    fn preview_does_not_split_a_multibyte_character() {
        let raw = "😀".repeat(200);
        let out = preview(&raw);
        // 100 chars kept, plus the ellipsis.
        assert_eq!(out.chars().count(), PREVIEW_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn preview_cuts_at_exactly_the_limit_without_ellipsis() {
        let raw = "a".repeat(PREVIEW_CHARS);
        assert_eq!(preview(&raw), raw);
    }
}
