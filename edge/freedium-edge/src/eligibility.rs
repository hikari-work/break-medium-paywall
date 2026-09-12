//! Which requests get mirrored, and why the rest do not.
//!
//! Every rule here is a *deliberate* narrowing of the corpus, and each one is
//! recorded rather than applied silently — a request this module excludes leaves
//! a `RecordOutcome::Excluded` line with the reason token, so `shadow-report` can
//! print what was skipped and how much. §5's gate is "seven consecutive days with
//! no semantic difference on real traffic"; a rule that quietly swallowed most of
//! the traffic would make that counter easy to reach and worthless.
//!
//! # The exclusions, and what each one is for
//!
//! | Reason | What it excludes | Why |
//! |---|---|---|
//! | `shadow-disabled` | everything, when `SHADOW_ENABLED` is unset | The kill switch. Recorded rather than absent so a soak accidentally run with it off reads as an empty run, not a clean one. |
//! | `method` | anything but `GET` | The two `POST` routes are admin key-gated and write; mirroring a write is not something a shadow should do, and the legacy's `GET` routes are the whole of what this phase compares. |
//! | `homepage` | `/` | Decision 3. Python samples with `ORDER BY RANDOM()` plus a ten-minute Redis fragment, Rust with `TABLESAMPLE SYSTEM`, and the two caches are independent by design — two homepages legitimately differ, so a difference there is not evidence and a match there is a coincidence. |
//! | `miro` | `/@miro/*` | Decision 8: a passthrough that makes its *own* outbound request through the proxy pool. Mirroring it doubles WARP traffic for bytes that are not rendered output. The render gate is what this phase is about. |
//! | `iframe` | `/render_iframe/*` | The same, plus it is a raw HTML echo rather than a rendered page. |
//! | `bypass-params` | `?no-redis`, `?no-db-cache` | Both force a code path the caches exist to avoid, and both are admin-key-gated. Comparing them compares different work, and the responses are *supposed* to differ from a cached one. |
//! | `sampled-out` | all but `SHADOW_SAMPLE` of the rest | The cost cap. Deterministic per path — see [`in_sample`]. |
//! | `primary-encoded` | responses with a `Content-Encoding` | The edge buffers the primary's bytes *as they go on the wire*. If those are gzipped, comparing them against a decoded shadow body is comparing a compressor's output against HTML, and the difference would be enormous and meaningless. Detected and skipped rather than misconcluded. |
//! | `body-too-large` | responses over `SHADOW_MAX_BODY` | The edge buffers on the request path. See the config docs. |
//!
//! # What is deliberately *not* excluded
//!
//! Non-HTML responses — stylesheets, images, the `robots.txt` — are eligible.
//! They are not skipped here because [`compare`](page_canonical::compare) already
//! handles them: two matching statuses and a non-HTML content type is
//! `StatusOnly`. One rule, in the place that can see both sides, rather than a
//! path-pattern guess here that would have to be kept in step with it.

use crate::config::Config;

/// The reason tokens. They are the values of `ShadowRecord::reason` and what
/// `shadow-report` groups by, so they are constants rather than literals.
pub const DISABLED: &str = "shadow-disabled";
pub const METHOD: &str = "method";
pub const HOMEPAGE: &str = "homepage";
pub const MIRO: &str = "miro";
pub const IFRAME: &str = "iframe";
pub const BYPASS_PARAMS: &str = "bypass-params";
pub const SAMPLED_OUT: &str = "sampled-out";
pub const PRIMARY_ENCODED: &str = "primary-encoded";
pub const BODY_TOO_LARGE: &str = "body-too-large";

/// `handlers/main.py:44` and `:45`, spelled as the routes spell them.
const NO_REDIS: &str = "no-redis";
const NO_DB_CACHE: &str = "no-db-cache";

/// The prefix checks, as `handlers/main.py` applies them.
const MIRO_PREFIX: &str = "@miro/";
const IFRAME_PREFIX: &str = "render_iframe/";

/// Whether this request may be mirrored, or the reason it may not.
///
/// Taken before the request goes upstream, so an excluded request costs the
/// `fnv1a` hash and nothing else — no buffering, no second request.
pub fn exclusion(
    method: &str,
    path: &str,
    query: Option<&str>,
    config: &Config,
) -> Option<&'static str> {
    if !config.shadow_enabled {
        return Some(DISABLED);
    }
    if method != "GET" {
        return Some(METHOD);
    }

    // The legacy's routes carry a leading slash and the prefix checks are on the
    // *stripped* path (`handlers/main.py:34` strips the origin). `trim_start_matches`
    // rather than `strip_prefix('/')` so that a doubled slash — which reaches the
    // app as a path — cannot slip a `/@miro/` request past the check below.
    let stripped = path.trim_start_matches('/');

    if stripped.is_empty() {
        return Some(HOMEPAGE);
    }
    if stripped.starts_with(MIRO_PREFIX) {
        return Some(MIRO);
    }
    if stripped.starts_with(IFRAME_PREFIX) {
        return Some(IFRAME);
    }
    if let Some(query) = query
        && (query_has_key(query, NO_REDIS) || query_has_key(query, NO_DB_CACHE))
    {
        return Some(BYPASS_PARAMS);
    }
    if !in_sample(path, config.shadow_sample) {
        return Some(SAMPLED_OUT);
    }

    None
}

/// Whether a query string carries a key, matching `"no-redis" not in
/// query_params` (`handlers/main.py:22`).
///
/// Membership of the *key*, not a truthy value: `?no-redis`, `?no-redis=`,
/// `?no-redis=0` and `?a=1&no-redis&b=2` all bypass the cache in Python, as
/// `crates/freedium-web/src/handlers/main.rs`'s `query_has` records and tests.
/// The edge has to agree with both, and it agrees by asking the same question.
fn query_has_key(query: &str, key: &str) -> bool {
    query.split('&').any(|pair| {
        let name = pair.split('=').next().unwrap_or(pair);
        // Starlette does not percent-decode the name for this lookup either; a
        // `%6Eo-redis` is a different key on both sides, which is the agreement
        // that matters.
        name == key
    })
}

/// Whether a path is in the sample.
///
/// # Why the hash is FNV-1a and not `DefaultHasher`
///
/// The point of hashing the path rather than drawing a random number is that a
/// path is *consistently* in or out: a difference found on one request can be
/// reproduced by requesting it again, and the same page does not flip between
/// compared and not across a soak. `DefaultHasher` would give the right shape and
/// the wrong guarantee — its output is explicitly not stable across Rust
/// versions, so an edge rebuilt on a new toolchain would silently reshuffle which
/// paths are compared. FNV-1a is ten lines, fixed forever, and has no dependency.
///
/// `rate >= 1.0` and `rate <= 0.0` are handled without hashing so that the two
/// ends of the range are exact rather than nearly-always.
pub fn in_sample(path: &str, rate: f64) -> bool {
    if rate >= 1.0 {
        return true;
    }
    if rate <= 0.0 {
        return false;
    }
    // Ten thousand buckets: fine enough that a 1% sample is 1% and not
    // "whichever paths happened to hash low".
    const BUCKETS: u64 = 10_000;
    let bucket = fnv1a(path.as_bytes()) % BUCKETS;
    (bucket as f64) / (BUCKETS as f64) < rate
}

/// FNV-1a, 64-bit. The constants are the spec's; see [`in_sample`] for why this
/// is hand-written rather than taken from the standard library.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Whether the primary's response is one the edge can compare.
///
/// A `Content-Encoding` the edge did not ask for is a body it is holding
/// compressed. `identity` is the one value that means "as-is"; everything else,
/// including anything unrecognised, is a skip. Treating an unknown value as
/// "probably fine" is how a compressed body gets compared against a decoded one
/// and reported as a renderer difference.
pub fn encoding_allows_comparison(content_encoding: Option<&str>) -> bool {
    match content_encoding {
        None => true,
        Some(value) => value.trim().eq_ignore_ascii_case("identity"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            listen: "0.0.0.0:6755".into(),
            upstream: "127.0.0.1:7080".into(),
            shadow_upstream: "127.0.0.1:7081".into(),
            shadow_enabled: true,
            shadow_sample: 1.0,
            shadow_timeout_ms: 5_000,
            shadow_max_body: 1024,
            shadow_log: "shadow.jsonl".into(),
            shadow_declarations: None,
            threads: 2,
        }
    }

    fn why(method: &str, path: &str, query: Option<&str>) -> Option<&'static str> {
        exclusion(method, path, query, &config())
    }

    /// The ordinary case: an article path is mirrored.
    #[test]
    fn an_article_path_is_eligible() {
        assert_eq!(why("GET", "/0291df856c77", None), None);
        assert_eq!(why("GET", "/@someone/a-post-0291df856c77", None), None);
        assert_eq!(why("GET", "/medium.com/foo-0291df856c77", None), None);
    }

    /// The kill switch records rather than disappears — see the module docs.
    #[test]
    fn the_kill_switch_excludes_everything_and_says_so() {
        let mut config = config();
        config.shadow_enabled = false;
        assert_eq!(
            exclusion("GET", "/0291df856c77", None, &config),
            Some(DISABLED)
        );
    }

    /// A write is never mirrored, whatever its path looks like.
    #[test]
    fn only_get_is_mirrored() {
        for method in ["POST", "PUT", "DELETE", "HEAD", "get"] {
            assert_eq!(why(method, "/0291df856c77", None), Some(METHOD), "{method}");
        }
    }

    /// Decision 3. `/` is uncomparable by construction, and every spelling of it
    /// is excluded — including a doubled slash, which reaches the app as a path.
    #[test]
    fn the_homepage_is_excluded_in_every_spelling() {
        for path in ["/", "", "//"] {
            assert_eq!(why("GET", path, None), Some(HOMEPAGE), "{path:?}");
        }
        assert_eq!(why("GET", "/", Some("no-redis")), Some(HOMEPAGE));
    }

    /// Decision 8: the two passthroughs, which would double WARP traffic.
    #[test]
    fn the_passthroughs_are_excluded() {
        assert_eq!(
            why("GET", "/@miro/v2/resize:fit:1400/1*abc.png", None),
            Some(MIRO)
        );
        assert_eq!(
            why("GET", "/render_iframe/0291df856c77", None),
            Some(IFRAME)
        );

        // The prefix must include its separator: `/@mirofoo` is not under
        // `/@miro/`, and neither is a path that merely starts with the letters.
        assert_eq!(why("GET", "/@mirofoo", None), None);
        assert_eq!(why("GET", "/render_iframes", None), None);
        // A doubled slash must not slip past the trim.
        assert_eq!(why("GET", "//@miro/x", None), Some(MIRO));
    }

    /// The cache-bypass parameters, as *key membership* — the same question
    /// `handlers/main.py:22` asks and `query_has` answers.
    #[test]
    fn the_cache_bypass_parameters_are_excluded_by_key_not_value() {
        for query in [
            "no-redis",
            "no-redis=",
            "no-redis=0",
            "a=1&no-redis&b=2",
            "no-db-cache",
            "no-db-cache=yes",
        ] {
            assert_eq!(
                why("GET", "/0291df856c77", Some(query)),
                Some(BYPASS_PARAMS),
                "{query}"
            );
        }

        for query in ["no-redisx", "no_redis", "cache=no-redis", "", "a=1"] {
            assert_eq!(why("GET", "/0291df856c77", Some(query)), None, "{query:?}");
        }
    }

    /// Sampling is per *path*, so a path is consistently in or out. This is what
    /// makes a difference reproducible: requesting the same article again lands
    /// in the same bucket rather than a coin flip.
    #[test]
    fn sampling_is_deterministic_per_path() {
        let config = Config {
            shadow_sample: 0.5,
            ..config()
        };
        for path in ["/a", "/b", "/0291df856c77", "/@someone/post"] {
            let first = exclusion("GET", path, None, &config);
            for _ in 0..20 {
                assert_eq!(exclusion("GET", path, None, &config), first, "{path}");
            }
        }
    }

    /// The two ends of the range are exact, so `SHADOW_SAMPLE=1` really does mean
    /// everything and `0` really does mean nothing.
    #[test]
    fn the_ends_of_the_sample_range_are_exact() {
        for path in ["/a", "/b", "/c", "/", "/0291df856c77"] {
            assert!(in_sample(path, 1.0), "{path} at 1.0");
            assert!(!in_sample(path, 0.0), "{path} at 0.0");
        }
        assert!(in_sample("/a", 2.0), "above 1.0 is still everything");
        assert!(!in_sample("/a", -1.0), "below 0.0 is still nothing");
    }

    /// A sample rate is a rate, not a switch: about half the paths land in it.
    /// Without this the "sampling" could be a function that returns true for one
    /// path and false for another and still pass every test above.
    #[test]
    fn a_half_sample_takes_roughly_half() {
        let taken = (0..2_000)
            .filter(|i| in_sample(&format!("/post-{i}"), 0.5))
            .count();
        assert!(
            (900..=1_100).contains(&taken),
            "0.5 took {taken} of 2000, which is not a rate"
        );
    }

    /// The hash is fixed, not `DefaultHasher` — see [`in_sample`]. These values
    /// are the ones this implementation produced when it was written; a change
    /// here means every path's bucket moved, which would reshuffle a running
    /// soak's sample mid-flight.
    #[test]
    fn the_hash_is_the_fixed_fnv_spec() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x85944171f73967e8);
    }

    /// An encoded primary is skipped rather than misconcluded — the difference
    /// against a decoded body would be enormous, meaningless, and read as a
    /// renderer bug.
    #[test]
    fn an_encoded_primary_is_not_comparable() {
        assert!(encoding_allows_comparison(None));
        assert!(encoding_allows_comparison(Some("identity")));
        assert!(encoding_allows_comparison(Some("  IDENTITY ")));

        for value in ["gzip", "br", "deflate", "zstd", "gzip, br", ""] {
            assert!(!encoding_allows_comparison(Some(value)), "{value:?}");
        }
    }
}
