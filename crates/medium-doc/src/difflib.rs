//! Port of CPython's `difflib.SequenceMatcher` — Ratcliff/Obershelp.
//!
//! Scope is exactly what Freedium needs, from `medium-parser/medium_parser/utils.py:110`:
//!
//! ```python
//! def getting_percontage_of_match(string: str, matched_string: str) -> float:
//!     if string is None or matched_string is None:
//!         return 0.0
//!     return difflib.SequenceMatcher(None, string, matched_string).ratio() * 100
//! ```
//!
//! ...called from `core.py:261` and `core.py:276` as `... > 80`.
//!
//! # Why this is a line-by-line port and not "a better algorithm"
//!
//! The acceptance gate (RUST_REWRITE_PLAN §3.2) is **identical boolean
//! decisions at the `> 80` threshold**, not merely similar ratios. A ratio that
//! differs in the last bit flips the decision whenever a pair lands exactly on
//! the boundary, and a flipped decision either drops a heading or prints the
//! title twice. So the goal here is bug-for-bug equivalence, including the
//! parts that look like accidents:
//!
//! * **Elements are Unicode code points.** Both arguments are Python `str`, so
//!   `difflib` compares code points — `str::chars()` in Rust, not bytes and not
//!   UTF-16 units. (Medium's *markup offsets* are UTF-16 units, which is a
//!   different problem handled in the parser, not here.)
//! * **`isjunk` is `None` at every call site.** That leaves `bjunk` empty,
//!   which is load-bearing: the two "extend by non-junk elements" loops in
//!   [`SequenceMatcher::find_longest_match`] therefore always run, while the
//!   two "extend by junk" loops never do. This is how a *popular* element —
//!   purged from `b2j` by autojunk — can still end up inside a match.
//! * **`autojunk` stays `true`, and applies to `b`.** At our call site `b` is
//!   the *title or subtitle*, not the paragraph (`a` is the paragraph text).
//!   It only kicks in once `len(b) >= 200`, and then drops any element
//!   occurring more than `len(b) / 100 + 1` times.
//! * **`_calculate_ratio` returns `1.0` when the total length is 0**, i.e. two
//!   empty strings are a perfect match — not `0.0`.
//! * **Float operation order is preserved** so results are bit-identical:
//!   `(2.0 * matches) / length`, and the caller then `* 100.0`.
//!
//! `strsim` and friends are deliberately *not* used: they implement
//! Levenshtein/Jaro, which answer a different question and would silently
//! change which paragraphs are dropped.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// A matching block: `a[a .. a+size] == b[b .. b+size]`.
///
/// Field order matters — CPython sorts `(i, j, k)` tuples lexicographically
/// when collapsing adjacent blocks, so the derived `Ord` must compare `a`, then
/// `b`, then `size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Match {
    pub a: usize,
    pub b: usize,
    pub size: usize,
}

/// Port of `difflib.SequenceMatcher`.
pub struct SequenceMatcher<'a, T> {
    a: &'a [T],
    b: &'a [T],
    /// Always `None` at our call sites. Kept as a plain `fn` pointer (rather
    /// than a boxed closure) so it stays `Copy` and adds no lifetime.
    isjunk: Option<fn(&T) -> bool>,
    autojunk: bool,
    /// `b2j[element] = positions in b`, ascending. Junk and popular elements
    /// are purged in [`SequenceMatcher::chain_b`].
    b2j: HashMap<&'a T, Vec<usize>>,
    /// Empty whenever `isjunk` is `None`.
    bjunk: HashSet<&'a T>,
    matching_blocks: Option<Vec<Match>>,
}

impl<'a, T: Eq + Hash> SequenceMatcher<'a, T> {
    /// `SequenceMatcher(None, a, b)` — `autojunk` on, no junk predicate.
    pub fn new(a: &'a [T], b: &'a [T]) -> Self {
        Self::with_options(a, b, None, true)
    }

    /// `SequenceMatcher(isjunk, a, b, autojunk=autojunk)`.
    pub fn with_options(
        a: &'a [T],
        b: &'a [T],
        isjunk: Option<fn(&T) -> bool>,
        autojunk: bool,
    ) -> Self {
        let mut matcher = Self {
            a,
            b,
            isjunk,
            autojunk,
            b2j: HashMap::new(),
            bjunk: HashSet::new(),
            matching_blocks: None,
        };
        matcher.chain_b();
        matcher
    }

    /// Port of `SequenceMatcher.__chain_b`.
    ///
    /// Builds `b2j` ignoring junk (cheaper: no predicate calls), then purges
    /// junk elements, then purges *popular* ones. Popular elements go into
    /// `bpopular` and are **not** added to `bjunk` — CPython keeps the two sets
    /// separate, and the distinction is observable through the extension loops
    /// in `find_longest_match`.
    fn chain_b(&mut self) {
        let mut b2j: HashMap<&'a T, Vec<usize>> = HashMap::new();
        for (i, elt) in self.b.iter().enumerate() {
            b2j.entry(elt).or_default().push(i);
        }

        let mut bjunk: HashSet<&'a T> = HashSet::new();
        if let Some(isjunk) = self.isjunk {
            // Collect first, then delete: mutating while iterating keys is what
            // the two separate loops in CPython avoid.
            let junk: Vec<&'a T> = b2j.keys().copied().filter(|elt| isjunk(elt)).collect();
            for elt in junk {
                bjunk.insert(elt);
                b2j.remove(elt);
            }
        }

        if self.autojunk && self.b.len() >= 200 {
            let ntest = self.b.len() / 100 + 1;
            let popular: Vec<&'a T> = b2j
                .iter()
                .filter(|(_, idxs)| idxs.len() > ntest)
                .map(|(elt, _)| *elt)
                .collect();
            for elt in popular {
                b2j.remove(elt);
            }
        }

        self.b2j = b2j;
        self.bjunk = bjunk;
    }

    /// Port of `SequenceMatcher.find_longest_match`.
    ///
    /// Returns the earliest-in-`a`, then earliest-in-`b`, longest match, with
    /// the four extension loops applied in CPython's order.
    pub fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
        let (a, b) = (self.a, self.b);
        let mut besti = alo;
        let mut bestj = blo;
        let mut bestsize = 0usize;

        // `j2len[j]` = length of the longest match ending at `a[i-1]`, `b[j]`.
        // Rebuilt per `i`, exactly as CPython discards `newj2len` into `j2len`.
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, a_i) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(positions) = self.b2j.get(a_i) {
                for &j in positions {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        // `positions` is ascending, so nothing later can fit.
                        break;
                    }
                    // CPython looks up `j2len.get(j - 1, 0)`; for `j == 0` that
                    // is the key `-1`, which is never present. Using
                    // `saturating_sub` here would wrongly read key `0`.
                    let prev = if j == 0 {
                        0
                    } else {
                        j2len.get(&(j - 1)).copied().unwrap_or(0)
                    };
                    let k = prev + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }

        // Extend over non-junk elements. With `isjunk = None` the `bjunk` set is
        // empty, so `!is_junk(..)` is always true and these two loops are the
        // ones that actually run.
        while besti > alo
            && bestj > blo
            && !self.is_junk(&b[bestj - 1])
            && a[besti - 1] == b[bestj - 1]
        {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && !self.is_junk(&b[bestj + bestsize])
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }

        // Extend over junk. Dead code while `isjunk = None`, kept so the port
        // stays faithful if a junk predicate is ever introduced.
        while besti > alo
            && bestj > blo
            && self.is_junk(&b[bestj - 1])
            && a[besti - 1] == b[bestj - 1]
        {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.is_junk(&b[bestj + bestsize])
            && a[besti + bestsize] == b[bestj + bestsize]
        {
            bestsize += 1;
        }

        Match {
            a: besti,
            b: bestj,
            size: bestsize,
        }
    }

    /// Port of `SequenceMatcher.get_matching_blocks`, memoised like CPython's.
    ///
    /// The final dummy block `(len(a), len(b), 0)` is included, so callers
    /// summing `size` over the result get the true match count.
    pub fn get_matching_blocks(&mut self) -> &[Match] {
        if self.matching_blocks.is_none() {
            self.matching_blocks = Some(self.compute_matching_blocks());
        }
        self.matching_blocks.as_deref().unwrap_or_default()
    }

    fn compute_matching_blocks(&self) -> Vec<Match> {
        let la = self.a.len();
        let lb = self.b.len();

        // Iterative instead of recursive: CPython made the same change after
        // users hit the recursion limit. `pop()` from the end is LIFO, matching
        // CPython's `queue.pop()`.
        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        let mut matching_blocks: Vec<Match> = Vec::new();

        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(alo, ahi, blo, bhi);
            let (i, j, k) = (m.a, m.b, m.size);
            if k != 0 {
                matching_blocks.push(m);
                // a[alo..i] vs b[blo..j] is still unknown.
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                // a[i+k..ahi] vs b[j+k..bhi] is still unknown.
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        matching_blocks.sort();

        // Collapse adjacent equal blocks (added in Python 2.5).
        let mut i1 = 0usize;
        let mut j1 = 0usize;
        let mut k1 = 0usize;
        let mut non_adjacent: Vec<Match> = Vec::new();
        for m in &matching_blocks {
            let (i2, j2, k2) = (m.a, m.b, m.size);
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 != 0 {
                    non_adjacent.push(Match {
                        a: i1,
                        b: j1,
                        size: k1,
                    });
                }
                i1 = i2;
                j1 = j2;
                k1 = k2;
            }
        }
        if k1 != 0 {
            non_adjacent.push(Match {
                a: i1,
                b: j1,
                size: k1,
            });
        }

        non_adjacent.push(Match {
            a: la,
            b: lb,
            size: 0,
        });
        non_adjacent
    }

    /// Port of `SequenceMatcher.ratio` — `2.0 * M / T` over all matching blocks.
    pub fn ratio(&mut self) -> f64 {
        let matches: usize = self.get_matching_blocks().iter().map(|m| m.size).sum();
        calculate_ratio(matches, self.a.len() + self.b.len())
    }

    fn is_junk(&self, elt: &T) -> bool {
        self.bjunk.contains(elt)
    }
}

/// Port of `difflib._calculate_ratio`.
///
/// Note the `length == 0` branch returning `1.0`: two empty strings are a
/// perfect match. The multiply-then-divide order is deliberate.
fn calculate_ratio(matches: usize, length: usize) -> f64 {
    if length != 0 {
        2.0 * (matches as f64) / (length as f64)
    } else {
        1.0
    }
}

/// `SequenceMatcher(None, a, b).ratio()`, with `a`/`b` as `&str`.
///
/// Compares Unicode code points, like `difflib` does for Python `str`.
pub fn ratio_of_str(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    SequenceMatcher::new(&a, &b).ratio()
}

/// Port of `medium_parser.utils.getting_percontage_of_match` (sic — the
/// misspelling is upstream's).
///
/// Returns `0.0` for a `None` argument, which is the Rust `None` case here.
/// The `* 100.0` is part of the port, not a convenience: the caller compares
/// the product against `80`, and `(ratio * 100.0) > 80.0` is not always the
/// same boolean as `ratio > 0.8` in floating point.
pub fn percentage_of_match(string: Option<&str>, matched_string: Option<&str>) -> f64 {
    match (string, matched_string) {
        (Some(a), Some(b)) => ratio_of_str(a, b) * 100.0,
        _ => 0.0,
    }
}

/// The decision actually made at `core.py:261` and `core.py:276`.
///
/// Strictly greater: a pair landing exactly on `80.0` is **not** treated as a
/// duplicate.
pub fn is_match_over_80(string: Option<&str>, matched_string: Option<&str>) -> bool {
    percentage_of_match(string, matched_string) > 80.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every doctest in CPython's `find_longest_match` and
    /// `get_matching_blocks`, transcribed.
    #[test]
    fn doctest_find_longest_match() {
        let a: Vec<char> = " abcd".chars().collect();
        let b: Vec<char> = "abcd abcd".chars().collect();
        let m = SequenceMatcher::new(&a, &b);
        assert_eq!(
            m.find_longest_match(0, 5, 0, 9),
            Match {
                a: 0,
                b: 4,
                size: 5
            }
        );

        let m = SequenceMatcher::with_options(&a, &b, Some(|c: &char| *c == ' '), true);
        assert_eq!(
            m.find_longest_match(0, 5, 0, 9),
            Match {
                a: 1,
                b: 0,
                size: 4
            }
        );

        let a: Vec<char> = "ab".chars().collect();
        let b: Vec<char> = "c".chars().collect();
        let m = SequenceMatcher::new(&a, &b);
        assert_eq!(
            m.find_longest_match(0, 2, 0, 1),
            Match {
                a: 0,
                b: 0,
                size: 0
            }
        );
    }

    #[test]
    fn doctest_get_matching_blocks() {
        let a: Vec<char> = "abxcd".chars().collect();
        let b: Vec<char> = "abcd".chars().collect();
        let got = SequenceMatcher::new(&a, &b).get_matching_blocks().to_vec();
        assert_eq!(
            got,
            vec![
                Match {
                    a: 0,
                    b: 0,
                    size: 2
                },
                Match {
                    a: 3,
                    b: 2,
                    size: 2
                },
                Match {
                    a: 5,
                    b: 4,
                    size: 0
                },
            ]
        );
    }

    #[test]
    fn doctest_ratio() {
        // SequenceMatcher(None, "abcd", "bcde").ratio() == 0.75
        assert!((ratio_of_str("abcd", "bcde") - 0.75).abs() < f64::EPSILON);
    }

    /// `_calculate_ratio` returns 1.0 — not 0.0 — when both sequences are empty.
    #[test]
    fn empty_sequences_are_a_perfect_match() {
        assert_eq!(ratio_of_str("", ""), 1.0);
        assert_eq!(percentage_of_match(Some(""), Some("")), 100.0);
        assert!(is_match_over_80(Some(""), Some("")));
    }

    /// One empty side means `length != 0` and `matches == 0`, so ratio is 0.0.
    #[test]
    fn one_sided_empty_is_zero() {
        assert_eq!(ratio_of_str("abc", ""), 0.0);
        assert_eq!(ratio_of_str("", "abc"), 0.0);
        assert!(!is_match_over_80(Some("abc"), Some("")));
    }

    /// `utils.py:111` short-circuits `None` to 0.0 before reaching difflib.
    #[test]
    fn none_arguments_are_zero() {
        assert_eq!(percentage_of_match(None, Some("abc")), 0.0);
        assert_eq!(percentage_of_match(Some("abc"), None), 0.0);
        assert_eq!(percentage_of_match(None, None), 0.0);
        assert!(!is_match_over_80(None, None));
    }

    #[test]
    fn identical_strings_are_100() {
        let s = "The quick brown fox jumps over the lazy dog";
        assert_eq!(percentage_of_match(Some(s), Some(s)), 100.0);
    }

    /// Multibyte and astral characters must count as one element each, matching
    /// Python's code-point iteration.
    #[test]
    fn counts_code_points_not_bytes() {
        // "📊" is one char but 4 UTF-8 bytes and 2 UTF-16 units.
        assert_eq!(ratio_of_str("📊", "📊"), 1.0);
        assert_eq!(ratio_of_str("a📊b", "a📊b"), 1.0);
        // 𝙗 is U+1D657, astral plane.
        assert_eq!(ratio_of_str("𝙗𝙤𝙩𝙝", "𝙗𝙤𝙩𝙝"), 1.0);
    }

    /// A repeated single character in a title of length >= 200 trips autojunk:
    /// the character is purged from `b2j`, but the non-junk extension loop
    /// recovers the full match anyway.
    #[test]
    fn autojunk_purges_popular_but_extension_recovers() {
        let b: Vec<char> = "a".repeat(300).chars().collect();
        let a: Vec<char> = "aaaa".chars().collect();
        let m = SequenceMatcher::new(&a, &b);
        assert!(m.b2j.is_empty(), "popular element must be purged from b2j");
        assert!(
            m.bjunk.is_empty(),
            "popular elements are not junk; the sets stay separate"
        );
        assert_eq!(m.find_longest_match(0, 4, 0, 300).size, 4);

        // 300 > 300/100 + 1 == 4, so autojunk really did engage here.
        assert!(ratio_of_str("aaaa", "a".repeat(300).as_str()) > 0.0);
    }

    /// Below 200 elements autojunk is inert, so a popular character stays in
    /// `b2j`. This is the boundary the plan calls out as easy to miss.
    #[test]
    fn autojunk_is_inert_below_200() {
        // `a` is only probed for its length here; its contents never match `b`.
        let a: Vec<char> = "xyz".chars().collect();

        let b: Vec<char> = "a".repeat(199).chars().collect();
        let m = SequenceMatcher::new(&a, &b);
        assert_eq!(m.b2j.len(), 1, "no purge below the 200-element threshold");

        let b: Vec<char> = "a".repeat(200).chars().collect();
        let m = SequenceMatcher::new(&a, &b);
        assert!(m.b2j.is_empty(), "purge starts at exactly 200");
    }

    /// The threshold at the call site is `> 80` on a `[0, 100]` scale, so an
    /// exact 80.0 must *not* count as a duplicate.
    #[test]
    fn threshold_is_strictly_greater() {
        // 1000 identical-otherwise chars with 200 substituted -> M = 800,
        // T = 2000, ratio = 0.8 exactly, so *100 == 80.0.
        let (a, b) = controlled_pair(1000, 200);
        let pct = percentage_of_match(Some(&a), Some(&b));
        assert_eq!(pct, 80.0, "construction must land exactly on the boundary");
        assert!(!is_match_over_80(Some(&a), Some(&b)), "80.0 is not > 80.0");

        // One fewer substitution -> 80.1 -> duplicate.
        let (a, b) = controlled_pair(1000, 199);
        assert!(is_match_over_80(Some(&a), Some(&b)));
    }

    /// Builds a pair with a *known exact* match count.
    ///
    /// `a` is `n` distinct CJK code points, and `b` replaces the first `m` of
    /// them with code points that appear nowhere else — so the only matching
    /// block is the `n - m` long suffix, giving `M == n - m` and
    /// `ratio == (n - m) / n` exactly.
    fn controlled_pair(n: usize, m: usize) -> (String, String) {
        // 0x4E00.. is CJK Unified Ideographs; stays clear of the 0xD800
        // surrogate range for any n used here.
        let base = 0x4E00u32;
        // Replacement alphabet lives far above the base block.
        let alt = 0x20000u32;
        let a: String = (0..n)
            .map(|i| char::from_u32(base + i as u32).unwrap())
            .collect();
        let b: String = (0..n)
            .map(|i| {
                if i < m {
                    char::from_u32(alt + i as u32).unwrap()
                } else {
                    char::from_u32(base + i as u32).unwrap()
                }
            })
            .collect();
        (a, b)
    }

    #[test]
    fn controlled_pairs_have_exact_ratios() {
        for (n, m, expected_pct) in [
            (100, 0, 100.0),
            (100, 20, 80.0),
            (100, 50, 50.0),
            (1000, 100, 90.0),
            (4, 1, 75.0),
        ] {
            let (a, b) = controlled_pair(n, m);
            assert_eq!(
                percentage_of_match(Some(&a), Some(&b)),
                expected_pct,
                "n={n} m={m}"
            );
        }
    }
}
