//! The per-request correlation token: a three-word id and its numeric code.
//!
//! Ports the two halves of `middlewares/logger.py:21-23` —
//! `xp.generate_xkcdpassword(...)` for the words and
//! [`string_to_number_ascii`] (`utils/utils.py:7-13`) for the number — and
//! [`generate_words`] keeps the same shape:
//!
//! ```text
//! prompt-pluto-basis   →   sum(ord(c) for c in "PROMPT-PLUTO-BASIS") * randint(0, 100)
//! ```
//!
//! # The wordlist is not xkcdpass's, and cannot be
//!
//! `server/__init__.py:85-87` builds its wordlist from
//! `xkcdpass/static/legac`, filtered to words of 5–8 characters:
//!
//! ```python
//! xkcd_passwd = xp.generate_wordlist(wordfile=WORDS_LIST_FILE, min_length=5, max_length=8)
//! ```
//!
//! That file ships inside the `xkcdpass` wheel and is **not** in this repo. So
//! the words below are a list of our own that obeys the same constraint: three
//! words, each 5–8 letters, joined by hyphens.
//!
//! What that costs, stated plainly: **the words differ from production, so the
//! token differs.** What it does not affect is anything that reads the token as
//! an opaque string — the `X-Request-ID` header, the log line, the Telegram
//! alert. It *does* affect the error page, which prints the numeric code, so any
//! comparison of an error page has to normalise the code out. That is the same
//! normalisation `xtask/difftest` already does for the render gate's transponder
//! field.
//!
//! Vendoring the real list is a one-file change and would remove the caveat
//! entirely; it is not done here because the licence of that wordlist is not
//! established, and a correlation token is not worth a licensing question in a
//! phase whose whole point is byte parity elsewhere.

/// Words of 5–8 letters, matching `min_length=5, max_length=8`.
///
/// Roughly xkcdpass's own style — short, concrete, lowercase, no proper nouns —
/// so the ids read the same way in a log even though the vocabulary differs.
pub const WORDS: &[&str] = &[
    "abbey", "ablaze", "absorb", "acorn", "acrobat", "action", "adapter", "admiral", "advice",
    "aerial", "affair", "agency", "agenda", "agreed", "airbag", "airway", "alarm", "album",
    "alcove", "alien", "almond", "alpaca", "amber", "ambient", "amount", "amplify", "anchor",
    "android", "animal", "ankle", "answer", "antenna", "anthem", "anvil", "anyway", "apart",
    "apple", "april", "apron", "arcade", "arcane", "arctic", "arena", "argon", "argue", "arise",
    "armor", "aroma", "around", "arrange", "arrow", "artist", "ascend", "asleep", "aspect",
    "asset", "assist", "asthma", "athlete", "atlas", "atrium", "attach", "attic", "auburn",
    "audio", "august", "author", "autumn", "avatar", "avenue", "aviator", "avocado", "awake",
    "aware", "awesome", "awful", "awkward", "azalea", "azure", "bacon", "badge", "badly", "bagel",
    "baker", "balance", "balcony", "ballet", "balloon", "bamboo", "banana", "banjo", "banner",
    "barrel", "basalt", "basil", "basket", "batch", "bathe", "battle", "beach", "beacon", "beagle",
    "beaker", "beard", "beast", "beaver", "become", "bedlam", "beetle", "before", "began", "begin",
    "behalf", "behave", "behind", "being", "believe", "belly", "below", "bench", "bending",
    "beret", "berry", "beside", "bestow", "beyond", "bicycle", "bidder", "bigger", "bikini",
    "binary", "binder", "biology", "birch", "bishop", "bison", "bistro", "bitter", "black",
    "blade", "blanket", "blast", "blaze", "bleach", "bleak", "blend", "bless", "blimp", "blind",
    "blink", "bliss", "blizzard", "bloom", "blossom", "bluff", "blunt", "blush", "board", "boast",
    "bobcat", "bonus", "boulder", "bounce", "bound", "bouquet", "boxer", "brace", "brain", "brake",
    "branch", "brand", "brass", "brave", "bread", "breeze", "brick", "bridge", "brief", "bright",
    "brine", "bring", "brisk", "broad", "bronze", "brook", "broom", "brown", "brush", "bubble",
    "bucket", "budget", "buffalo", "builder", "bumper", "bunch", "bundle", "bunker", "burden",
    "bureau", "burger", "burst", "bushel", "butter", "button", "buyer", "cabin", "cabinet",
    "cable", "cactus", "cadet", "camera", "canal", "candle", "candy", "canoe", "canopy", "canvas",
    "canyon", "capable", "capital", "capsule", "captain", "carbon", "cargo", "carpet", "carrot",
    "carve", "cascade", "cashew", "castle", "catalog", "catch", "cattle", "cavern", "caviar",
    "cedar", "ceiling", "celery", "cello", "cement", "census", "center", "ceramic", "cereal",
    "certain", "chalk", "chamber", "champion", "change", "channel", "chapter", "charge", "charity",
    "charm", "chart", "chase", "cheap", "check", "cheese", "cherry", "chess", "chest", "chicken",
    "chief", "child", "chill", "chime", "china", "choice", "choir", "chord", "chorus", "chosen",
    "chrome", "chunk", "church", "cider", "cinema", "circle", "circus", "citizen", "civic",
    "civil", "clamp", "clarity", "clash", "clasp", "class", "clean", "clear", "clerk", "clever",
    "click", "client", "cliff", "climate", "climb", "clinic", "cloak", "clock", "closer", "cloth",
    "cloud", "clover", "clown", "cluster", "clutch", "coach", "coast", "cobalt", "cobra", "cocoa",
    "coconut", "coffee", "collar", "college", "colony", "color", "column", "combat", "comedy",
    "comet", "comfort", "comic", "common", "compass", "compose", "concert", "concrete", "conduct",
    "connect", "consul", "contact", "contest", "context", "control", "convert", "cookie", "copper",
    "coral", "corner", "cotton", "couch", "cough", "council", "count", "courier", "course",
    "cousin", "cover", "coyote", "crack", "cradle", "craft", "crane", "crash", "crater", "crawl",
    "crazy", "cream", "creator", "credit", "creek", "creep", "cricket", "crime", "crisp", "critic",
    "cross", "crowd", "crown", "cruise", "crumb", "crush", "crystal", "cubic", "cuisine",
    "culture", "cupid", "curious", "currency", "current", "curtain", "curve", "cushion", "custom",
    "cycle",
];

/// How many words an id has — `numwords=3` (`middlewares/logger.py:21`).
pub const WORDS_PER_ID: usize = 3;

/// The delimiter — `delimiter="-"` (`middlewares/logger.py:21`).
pub const DELIMITER: char = '-';

/// `string_to_number_ascii` (`utils/utils.py:7-13`).
///
/// Two details that look like mistakes and are not fixed:
///
/// - **The sum is over code points, not bytes.** Python's `ord` on a `str`
///   yields the code point, so this iterates `char`s rather than `u8`s. For the
///   ASCII ids this is called with it makes no difference; it matters if anyone
///   ever passes it something else.
/// - **`key_number == 0` means "pick one", not "multiply by zero".** The legacy
///   test is `if not key_number`, so `0` is falsy and gets replaced by a random
///   value — which means the code can never legitimately be `0` from a caller
///   who passed `0`. Reproduced, including the `0..=100` inclusive range.
pub fn string_to_number_ascii(input: &str, key_number: Option<u32>) -> u32 {
    let key = match key_number {
        // `if not key_number` — `None` and `Some(0)` take the same branch.
        None | Some(0) => random_below(101),
        Some(key) => key,
    };

    input
        .to_uppercase()
        .chars()
        .map(|c| c as u32)
        .sum::<u32>()
        .wrapping_mul(key)
}

/// A three-word id, hyphen-delimited.
///
/// The words come from a uniform pick with replacement, which is what
/// `random.choice` inside `generate_xkcdpassword` does. Repetition is therefore
/// possible (`able-able-able`) and is not filtered out.
pub fn generate_words() -> String {
    (0..WORDS_PER_ID)
        .map(|_| WORDS[random_below(WORDS.len() as u32) as usize])
        .collect::<Vec<_>>()
        .join(&DELIMITER.to_string())
}

/// The id and its code, as the middleware needs both.
///
/// One call so the two cannot be generated from different ids — which is the
/// only way this pair can be wrong in a way that is hard to notice.
pub fn generate() -> (String, u32) {
    let words = generate_words();
    let code = string_to_number_ascii(&words, None);
    (words, code)
}

/// A uniform index in `0..len`, for the places that need to pick one of a list
/// — [`crate::error::random_message`] is the caller.
pub fn random_index(len: usize) -> usize {
    assert!(len > 0, "there is no index into an empty list");
    // `len` is a slice length, so it cannot exceed `u32::MAX` in any realistic
    // build; the cast is guarded rather than assumed.
    let limit = u32::try_from(len).unwrap_or(u32::MAX);
    random_below(limit) as usize
}

/// A uniform integer in `0..limit` from the OS entropy pool.
///
/// Rejection sampling rather than a modulo: `%` on a raw `u32` would bias the
/// low indices, and with a 200-word list that bias is large enough to notice
/// across a few million requests. `getrandom` rather than `rand` for the same
/// reason the workspace already chose it — this needs entropy, not a PRNG API.
fn random_below(limit: u32) -> u32 {
    debug_assert!(limit > 0, "an empty list has no index to pick");

    // The largest multiple of `limit` that fits in a `u32`. Draws at or above it
    // are discarded, which is what makes the remaining ones uniform.
    let ceiling = (u32::MAX / limit) * limit;
    loop {
        let mut bytes = [0_u8; 4];
        // The only failure mode is a broken entropy source, which is not
        // something this function can recover from or a caller can act on.
        getrandom::fill(&mut bytes).expect("the OS entropy pool is available");
        let draw = u32::from_ne_bytes(bytes);
        if draw < ceiling {
            return draw % limit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The exact arithmetic of `utils.py:11-13`, with the key passed in so the
    /// result is deterministic: `sum(ord(c)) * key`, over the *uppercased* text.
    #[test]
    fn the_code_is_the_sum_times_the_key() {
        // 'A'=65, '-'=45: "AB-CD" upper is "AB-CD" → 65+66+45+67+68 = 311.
        assert_eq!(string_to_number_ascii("ab-cd", Some(2)), 622);
        assert_eq!(
            string_to_number_ascii("AB-CD", Some(2)),
            622,
            "case-insensitive"
        );
        assert_eq!(string_to_number_ascii("ab-cd", Some(1)), 311);
    }

    /// `if not key_number` means `Some(0)` is "pick one", so the result is not
    /// the zero that multiplying by zero would give.
    #[test]
    fn a_zero_key_means_random_not_multiply_by_zero() {
        // With a random key in 0..=100 the product is 0 only when the key is 0,
        // which is 1 chance in 101; across attempts it must usually differ.
        let results: HashSet<u32> = (0..40)
            .map(|_| string_to_number_ascii("ab-cd", Some(0)))
            .collect();
        assert!(
            results.len() > 1,
            "key 0 must be replaced by a random key, not used as a multiplier: {results:?}"
        );
    }

    /// The shape `middlewares/logger.py:21` asks for: three words, 5–8 letters.
    #[test]
    fn ids_have_three_words_of_the_configured_length() {
        for _ in 0..200 {
            let id = generate_words();
            let parts: Vec<&str> = id.split(DELIMITER).collect();
            assert_eq!(parts.len(), WORDS_PER_ID, "{id}");
            for part in parts {
                assert!(
                    (5..=8).contains(&part.chars().count()),
                    "{part:?} in {id:?} is outside the 5–8 the wordlist is built for"
                );
                assert!(
                    part.chars().all(|c| c.is_ascii_lowercase()),
                    "{part:?} should be lowercase so the uppercase is meaningful"
                );
            }
        }
    }

    /// Every word in the list must satisfy the filter `__init__.py:87` applies,
    /// or the list is not standing in for `legac` faithfully.
    #[test]
    fn every_word_obeys_the_min_and_max_length() {
        for word in WORDS {
            assert!(
                (5..=8).contains(&word.chars().count()),
                "{word:?} is outside min_length=5, max_length=8"
            );
            assert!(
                word.chars().all(|c| c.is_ascii_lowercase()),
                "{word:?} must be lowercase ASCII"
            );
        }
    }

    /// A duplicate would silently halve that word's probability.
    #[test]
    fn the_wordlist_has_no_duplicates() {
        let unique: HashSet<&&str> = WORDS.iter().collect();
        assert_eq!(unique.len(), WORDS.len());
    }

    /// The id and the code must describe the same string.
    ///
    /// [`generate`] draws the key internally, so the test cannot recompute the
    /// code exactly. What it can check is the invariant that makes the pair
    /// correct: the code is always `sum(uppercased id) * key` for *some* key in
    /// `0..=100`, i.e. the sum divides the code and the quotient is in range.
    /// That would fail if the code were computed from a different string than
    /// the one returned — the mistake this function exists to prevent.
    #[test]
    fn generate_returns_an_id_and_its_own_code() {
        for _ in 0..50 {
            let (id, code) = generate();
            let sum: u32 = id.to_uppercase().chars().map(|c| c as u32).sum();

            assert_eq!(
                code % sum,
                0,
                "{code} is not a multiple of {sum} for {id:?}"
            );
            let key = code / sum;
            assert!(
                key <= 100,
                "key {key} is outside randint(0, 100) for {id:?}"
            );
        }
    }

    /// `random_below` must stay in range and must not collapse to one value.
    #[test]
    fn the_pick_stays_in_range_and_varies() {
        for limit in [1_u32, 2, 3, 101] {
            let draws: HashSet<u32> = (0..200).map(|_| random_below(limit)).collect();
            assert!(draws.iter().all(|d| *d < limit), "limit {limit}: {draws:?}");
            if limit > 1 {
                assert!(draws.len() > 1, "limit {limit} never varied");
            }
        }
    }
}
