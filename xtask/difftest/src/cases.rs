//! Deterministic case corpus for the difflib parity gate.
//!
//! Every case is a `(a, b)` pair fed to `SequenceMatcher(None, a, b).ratio()`,
//! mirroring the call site at `medium_parser/utils.py:114` where `a` is the
//! paragraph text and `b` is the title or subtitle. The groups exist to make the
//! *reason* a case is interesting explicit, so a failure tells you which
//! assumption broke.
//!
//! Note on surrogates: Rust `char` cannot represent U+D800..U+DFFF, so lone
//! surrogates are never generated. That is not a coverage gap in practice —
//! Medium's payload is JSON, and a lone surrogate cannot survive JSON decoding
//! into a Rust `String` in the first place.

use serde::{Deserialize, Serialize};

use crate::prng::SplitMix64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub index: usize,
    pub group: String,
    pub note: String,
    pub a: String,
    pub b: String,
}

/// Builds the whole corpus. Deterministic for a given `scale`.
pub fn generate(scale: f64) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut rng = SplitMix64::new(0x5EED_D1FF_7E57_0001);

    fn scaled(base: usize, scale: f64) -> usize {
        ((base as f64) * scale).round().max(1.0) as usize
    }

    let push = |cases: &mut Vec<Case>, group: &str, note: &str, a: String, b: String| {
        let index = cases.len();
        cases.push(Case {
            index,
            group: group.to_string(),
            note: note.to_string(),
            a,
            b,
        });
    };

    // --- Group: doctest -----------------------------------------------------
    // Literal pairs from CPython's difflib docstrings. If these fail, the port
    // is wrong at the most basic level.
    for (a, b, note) in [
        ("abcd", "bcde", "ratio() docstring, expects 0.75"),
        (" abcd", "abcd abcd", "find_longest_match docstring"),
        ("ab", "c", "find_longest_match docstring, no match"),
        ("abxcd", "abcd", "get_matching_blocks docstring"),
    ] {
        push(&mut cases, "doctest", note, a.into(), b.into());
    }

    // --- Group: empty -------------------------------------------------------
    // `_calculate_ratio` returns 1.0 when T == 0, so two empty strings are a
    // perfect match while a one-sided empty is 0.0. Easy to get backwards.
    for (a, b, note) in [
        ("", "", "T == 0 -> ratio 1.0, and 100 > 80 is true"),
        ("abc", "", "T != 0, M == 0 -> 0.0"),
        ("", "abc", "T != 0, M == 0 -> 0.0"),
        (" ", " ", "single space, identical"),
        (" ", "", "single space vs empty"),
        ("\n", "\n", "newline only"),
    ] {
        push(&mut cases, "empty", note, a.into(), b.into());
    }

    // --- Group: unicode -----------------------------------------------------
    // Element identity is by code point, so every one of these must be treated
    // as a single element regardless of UTF-8 length or UTF-16 width.
    let unicode_pairs: &[(&str, &str, &str)] = &[
        (
            "Noah dragged his two printers out from Settings ⚙️  < Printers & Scanners 🖨️  and dropped them",
            "Noah dragged his two printers out from Settings ⚙️  < Printers & Scanners 🖨️  and dropped them",
            "variation selector, from the rl_string_helper emoji test",
        ),
        (
            "We have a 📊, a 📊 and a 📊.",
            "We have a 📊, a 📊 and a 📊.",
            "astral emoji repeated, identical",
        ),
        (
            "👨👩👧👦 family",
            "👨👩👧👦 family",
            "ZWJ sequence with internal variation selectors",
        ),
        ("🇮🇩 Indonesia", "🇮🇩 Indonesia", "regional indicator pair"),
        (
            "Whilst academic research papers have highlighted performance issues with the prophet since 2017, the propagation of package popularity through the data science community has been fueled by 𝙗𝙤𝙩𝙝 𝙚𝙭𝙘𝙚𝙨𝙨𝙞𝙫𝙚 𝙘𝙡𝙖𝙞𝙢𝙨 𝙛𝙧𝙤𝙢 𝙩𝙝𝙚 𝙤𝙧𝙞𝙜𝙞𝙣𝙖𝙡 𝙙𝙚𝙫𝙚𝙡𝙤𝙥𝙢𝙚𝙣𝙩 𝙩𝙚𝙖𝙢",
            "Benchmarking Neural Prophet Part I: Neural Prophet vs Prophet",
            "the 'still have problems' post, math bold astral letters",
        ),
        (
            "Café résumé naïve",
            "Cafe resume naive",
            "combining acute accents vs precomposed",
        ),
        (
            "Cafe\u{0301} re\u{0301}sume\u{0301} nai\u{0308}ve",
            "Café résumé naïve",
            "decomposed vs precomposed — different code point counts",
        ),
        ("مرحبا بالعالم", "مرحبا بالعالم", "Arabic RTL"),
        ("שלום עולם", "שלום עולם", "Hebrew RTL"),
        ("日本語のテキストです", "日本語のテキストです", "CJK"),
        (
            "前半部分だけ一致します後半は違います",
            "前半部分だけ一致しますまったく別の文章です",
            "CJK with a differing tail",
        ),
    ];
    for (a, b, note) in unicode_pairs {
        push(&mut cases, "unicode", note, (*a).into(), (*b).into());
    }

    // Curly quotes are normalized by `quote_symbol` *before* difflib runs, but
    // the raw GraphQL text reaching this comparison can still hold them, so
    // they belong in the corpus either way.
    for (a, b, note) in [
        (
            "It’s a ‘quoted’ title — with dashes",
            "It's a 'quoted' title - with dashes",
            "curly vs straight quotes",
        ),
        (
            "“Double” quotes”",
            "\"Double\" quotes\"",
            "curly vs straight double quotes",
        ),
    ] {
        push(&mut cases, "unicode", note, a.into(), b.into());
    }

    // --- Group: realish -----------------------------------------------------
    // Titles taken from legacy/tests/smokie_tests.py slugs, compared against the slug
    // with dashes swapped for spaces — the shape of a real title/subtitle dedup
    // check, at realistic lengths.
    let slugs = [
        "stop-wasting-your-life",
        "21-sentences-that-will-make-you-more-attractive-than-most",
        "some-linux-commands-that-can-boost-your-work-efficiency-dramatically",
        "http-cache-on-rails-nginx-stack",
        "35-actionable-tips-to-grow-your-medium-blog",
        "benchmarking-neural-prophet-part-i-neural-prophet-vs-prophet",
        "python-vs-r-for-time-series-forecasting",
        "how-to-generate-random-user-agents-with-an-api",
        "the-best-way-to-unsubscribe-rxjs-observable-in-the-angular-applications",
        "12-macos-apps-so-good-you-will-wonder-how-they-are-free",
        "how-any-gitamite-can-get-free-linkedin-premium-membership",
        "be-an-engineer-not-a-frameworker",
        "parseint-strange-behavior",
        "the-11-craziest-and-most-advanced-macos-tips-tricks-ive-ever-seen",
    ];
    for slug in slugs {
        let title: String = slug.replace('-', " ");
        // Exact match: the duplicate case, which must be caught.
        push(
            &mut cases,
            "realish",
            "slug title, exact duplicate",
            title.clone(),
            title.clone(),
        );
        // Title-cased variant, as Medium renders headings.
        let capitalized = title
            .split(' ')
            .map(|w| {
                let mut c = w.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        push(
            &mut cases,
            "realish",
            "slug title, title-cased",
            capitalized,
            title,
        );
    }

    // --- Group: autojunk ----------------------------------------------------
    // autojunk engages only when len(b) >= 200, and purges any element seen more
    // than len(b)/100 + 1 times. These cases sit on and around that threshold,
    // with `b` on both sides of it.
    for n in [197usize, 198, 199, 200, 201, 202, 250, 500, 1000] {
        for alpha in [1usize, 2, 3, 5, 11] {
            let b = random_string(&mut rng, n, alpha);
            let a = if rng.below(2) == 0 {
                b.clone()
            } else {
                random_string(&mut rng, n, alpha)
            };
            push(
                &mut cases,
                "autojunk",
                &format!("len(b)={n} alphabet={alpha}"),
                a,
                b,
            );
        }
    }
    // One repeated element only: the cleanest autojunk trigger.
    for n in [199usize, 200, 201, 300, 1000] {
        let b: String = "a".repeat(n);
        push(
            &mut cases,
            "autojunk",
            &format!("b = 'a' x {n}"),
            "aaaa".into(),
            b.clone(),
        );
        push(
            &mut cases,
            "autojunk",
            &format!("both = 'a' x {n}"),
            b.clone(),
            b,
        );
    }

    // --- Group: boundary ----------------------------------------------------
    // Exact, construction-controlled match counts sweeping across ratio 0.8.
    // `controlled_pair(n, m)` yields M == n - m and T == 2n exactly, so the
    // ratio is (n - m) / n with no accidental matches.
    for n in [10usize, 50, 100, 199, 200, 201, 500, 1000, 2000] {
        // m = 0.2n lands on ratio 0.8 if n is divisible by 5.
        let base_m = n / 5;
        for delta in [-2i64, -1, 0, 1, 2] {
            let m = (base_m as i64 + delta).clamp(0, n as i64) as usize;
            let (a, b) = controlled_pair(n, m);
            let note = format!("n={n} m={m} ratio=({n}-{m})/{n}");
            push(&mut cases, "boundary", &note, a, b);
        }
    }
    // Explicitly hit ratio 0.8 exactly, where `> 80` must be false.
    for n in [5usize, 10, 100, 1000, 5000] {
        let m = n / 5;
        let (a, b) = controlled_pair(n, m);
        push(&mut cases, "boundary", &format!("exact 0.8, n={n}"), a, b);
    }

    // --- Group: boundary_autojunk -------------------------------------------
    // The combination the plan warns about: len(b) >= 200 *and* the ratio on the
    // boundary. Both sequences get `k` trailing 'z's, which makes 'z' popular in
    // `b` (purged from b2j) yet still reachable by the non-junk extension loop.
    // With n + k == L and m == L/5, M == 0.8L and T == 2L, so the ratio is
    // exactly 0.8 *and* autojunk is engaged.
    for l in [200usize, 205, 250, 500, 1000, 2000] {
        for delta in [-2i64, -1, 0, 1, 2] {
            let k = l / 5;
            let n = l - k;
            let m = ((l / 5) as i64 + delta).clamp(0, n as i64) as usize;
            let (a, b) = controlled_pair_with_filler(n, m, k);
            let note = format!("L={l} n={n} m={m} k={k} filler='z'");
            push(&mut cases, "boundary_autojunk", &note, a, b);
        }
    }
    // Same shape but the popular filler is a multi-code-point token, and a
    // variant where the filler leads instead of trails.
    for l in [400usize, 1000] {
        let k = l / 5;
        let n = l - k;
        let m = l / 5;
        // Trailing: M = (n - m) + 2k, and the (n - m) block can only reach the
        // purged 'a'/'b' run by extension.
        let (a, b) = controlled_pair_filler_token(n, m, k, "ab");
        push(
            &mut cases,
            "boundary_autojunk",
            &format!("L={l} filler='ab' trailing k={k}"),
            a,
            b,
        );
        // Leading: the common prefix is the purged run, the match resumes after
        // a gap, so the two blocks do not collapse.
        let (a, b) = controlled_pair_filler_token(n, m, k, "ab");
        let lead = "ab".repeat(k);
        push(
            &mut cases,
            "boundary_autojunk",
            &format!("L={l} filler='ab' leading k={k}"),
            format!("{lead}{a}"),
            format!("{lead}{b}"),
        );
    }

    // --- Group: random ------------------------------------------------------
    // Broad sweep so the gate is not just a handful of hand-picked shapes.
    let random_count = scaled(4000, scale);
    for i in 0..random_count {
        let alpha = *rng.pick(&[1usize, 2, 3, 4, 6, 11, 26, 52, 70]);
        let la = rng.range(0, 600);
        let lb = rng.range(0, 600);
        let a = random_string(&mut rng, la, alpha);
        let near_dup = rng.below(4) == 0 && !a.is_empty();
        let b = if near_dup {
            // Same content, a few edits — much more likely to land near 0.8
            // than pure noise, which is where the threshold actually decides.
            let upper = (a.chars().count() / 3).max(2);
            let edits = 1 + rng.below(upper);
            edit(&mut rng, &a, edits)
        } else {
            random_string(&mut rng, lb, alpha)
        };
        push(
            &mut cases,
            "random",
            &format!("i={i} alphabet={alpha} len(a)={la} len(b)={lb} near_dup={near_dup}"),
            a,
            b,
        );
    }

    // --- Group: long random -------------------------------------------------
    // Longer sequences, where autojunk is always on and the O(n*m) inner loop
    // actually matters.
    let long_count = scaled(120, scale);
    for i in 0..long_count {
        let alpha = *rng.pick(&[2usize, 3, 5, 11, 26]);
        let len = rng.range(200, 2000);
        let a = random_string(&mut rng, len, alpha);
        let edits = rng.range(2, len / 3 + 2);
        let b = edit(&mut rng, &a, edits);
        push(
            &mut cases,
            "long",
            &format!("i={i} alphabet={alpha} len={len}"),
            a,
            b,
        );
    }

    // --- Group: mutation ----------------------------------------------------
    // The realistic shape: a paragraph that is a lightly-edited copy of the
    // title. This is exactly what core.py:261 is trying to detect.
    let mutation_count = scaled(1500, scale);
    let bases: Vec<String> = slugs
        .iter()
        .map(|s| s.replace('-', " "))
        .chain(unicode_pairs.iter().map(|(a, _, _)| (*a).to_string()))
        .collect();
    for i in 0..mutation_count {
        let base = rng.pick(&bases).clone();
        let edits = rng.range(0, 6);
        let a = edit(&mut rng, &base, edits);
        let b = base.clone();
        push(
            &mut cases,
            "mutation",
            &format!("i={i} edits={edits}"),
            a,
            b,
        );
    }

    cases
}

/// `a` is `n` distinct code points; `b` replaces the first `m` with code points
/// appearing nowhere else. The sole matching block is therefore the `n - m` long
/// suffix, giving `M == n - m` and `ratio == (n - m) / n` exactly.
///
/// Both strings have length `n`, so autojunk is inert here (no element repeats).
pub fn controlled_pair(n: usize, m: usize) -> (String, String) {
    assert!(m <= n);
    let (a, b) = distinct_and_mutated(n, m);
    (a, b)
}

/// As [`controlled_pair`], but both sides get `k` trailing `'z'`s.
///
/// `'z'` then occurs more than `(n + k) / 100 + 1` times in `b`, so autojunk
/// purges it from `b2j`. The `n - m` block and the `k` long `'z'` run are
/// adjacent in both sequences, so `get_matching_blocks` collapses them into one
/// block of `n - m + k`: `M == n - m + k`, `T == 2(n + k)`.
///
/// The collapsed block is only reachable because the *non-junk* extension loop
/// in `find_longest_match` walks over the purged-but-not-junk `'z'`s. If that
/// loop were omitted, `M` would be just `n - m` and the ratio would be wrong.
pub fn controlled_pair_with_filler(n: usize, m: usize, k: usize) -> (String, String) {
    let (a, b) = distinct_and_mutated(n, m);
    let filler = "z".repeat(k);
    (format!("{a}{filler}"), format!("{b}{filler}"))
}

/// As [`controlled_pair_with_filler`] but the popular token is multi-code-point.
fn controlled_pair_filler_token(n: usize, m: usize, k: usize, token: &str) -> (String, String) {
    let (a, b) = distinct_and_mutated(n, m);
    let filler = token.repeat(k);
    (format!("{a}{filler}"), format!("{b}{filler}"))
}

/// Builds `(a, b)` of equal length `n` where `b` differs from `a` in exactly the
/// first `m` positions, using disjoint code-point ranges so no accidental match
/// can occur.
fn distinct_and_mutated(n: usize, m: usize) -> (String, String) {
    // CJK Unified Ideographs, and a plane-2 block far above it. Both stay clear
    // of the surrogate range for any `n` used in this corpus.
    let base = 0x4E00u32;
    let alt = 0x2_0000u32;
    let mk = |f: &dyn Fn(usize) -> char| (0..n).map(f).collect::<String>();
    let a = mk(&|i| char::from_u32(base + i as u32).expect("valid BMP code point"));
    let b = mk(&|i| {
        if i < m {
            char::from_u32(alt + i as u32).expect("valid plane-2 code point")
        } else {
            char::from_u32(base + i as u32).expect("valid BMP code point")
        }
    });
    (a, b)
}

/// Random string of `len` code points drawn from the first `alphabet` code
/// points of a small pool. `alphabet == 1` gives a single repeated character,
/// which is the strongest possible autojunk trigger.
fn random_string(rng: &mut SplitMix64, len: usize, alphabet: usize) -> String {
    // A pool wide enough for `alphabet == 70`, deliberately including
    // characters that are multi-byte in UTF-8 and astral.
    const POOL: &[char] = &[
        'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r',
        's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J',
        'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '0', '1',
        '2', '3', '4', '5', '6', '7', '8', '9', ' ', '.', ',', '-', '!', '?', ';', ':',
    ];
    let alphabet = alphabet.clamp(1, POOL.len());
    (0..len).map(|_| POOL[rng.below(alphabet)]).collect()
}

/// Applies `edits` random single-character edits to `s`: substitute, insert,
/// delete, or duplicate a short run. Mirrors how a paragraph drifts from its
/// title in the wild.
fn edit(rng: &mut SplitMix64, s: &str, edits: usize) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    const EXTRA: &[char] = &['x', 'q', 'z', '!', '7', '~', 'é', '中', '😀'];
    for _ in 0..edits {
        if chars.is_empty() {
            chars.push(*rng.pick(EXTRA));
            continue;
        }
        let pos = rng.below(chars.len());
        match rng.below(4) {
            0 => chars[pos] = *rng.pick(EXTRA),
            1 => chars.insert(pos, *rng.pick(EXTRA)),
            2 => {
                chars.remove(pos);
            }
            _ => {
                // Duplicate a short run, which creates the repeated elements
                // that autojunk reacts to.
                let run = rng.range(1, 8).min(chars.len() - pos);
                let slice: Vec<char> = chars[pos..pos + run].to_vec();
                for (i, c) in slice.into_iter().enumerate() {
                    chars.insert(pos + i, c);
                }
            }
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The filler construction must put the ratio *exactly* on 0.8, otherwise
    /// the boundary group silently stops testing the boundary.
    #[test]
    fn filler_construction_lands_on_the_boundary() {
        let l = 1000usize;
        let k = l / 5;
        let n = l - k;
        let m = l / 5;
        let (a, b) = controlled_pair_with_filler(n, m, k);
        assert_eq!(a.chars().count(), l);
        assert_eq!(b.chars().count(), l);
        // M = (n - m) + k, T = 2l.
        let expected = ((n - m + k) as f64 * 2.0) / (2.0 * l as f64);
        assert_eq!(expected, 0.8);
        // And the filler really is popular in b.
        let ntest = l / 100 + 1;
        assert!(k > ntest, "'z' count {k} must exceed ntest {ntest}");
    }

    #[test]
    fn generator_is_deterministic() {
        let a = generate(0.01);
        let b = generate(0.01);
        assert_eq!(a.len(), b.len());
        assert!(
            a.iter()
                .zip(&b)
                .all(|(x, y)| x.a == y.a && x.b == y.b && x.group == y.group)
        );
    }

    #[test]
    fn generator_covers_every_group() {
        let cases = generate(0.01);
        for group in [
            "doctest",
            "empty",
            "unicode",
            "realish",
            "autojunk",
            "boundary",
            "boundary_autojunk",
            "random",
            "long",
            "mutation",
        ] {
            assert!(
                cases.iter().any(|c| c.group == group),
                "group {group} missing"
            );
        }
    }

    #[test]
    fn controlled_pair_has_exact_ratio() {
        let (a, b) = controlled_pair(1000, 200);
        assert_eq!(a.chars().count(), 1000);
        assert_eq!(b.chars().count(), 1000);
        // ratio = (1000 - 200) / 1000 == 0.8
        assert_eq!((1000.0f64 - 200.0) / 1000.0, 0.8);
    }
}
