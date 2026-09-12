//! The request Medium is sent — headers, body, and the query itself.
//!
//! Transcribed from `MediumApi.query_post_graphql`
//! (`legacy/medium-parser/medium_parser/api.py:28-60`). Header *order* is kept
//! as Python's dict had it, because header order is part of what a bot check
//! can look at.
//!
//! # One source of truth for the query, enforced
//!
//! [`FULL_POST_QUERY`] is `include_str!`-ed from `query.graphql`. The legacy
//! string therefore exists twice in this repository, and a query that drifts
//! between them would be a silent, expensive bug: the Rust client would ask a
//! slightly different question than production, and the difference would show
//! up as missing fields rather than as an error.
//!
//! [`tests::the_query_matches_the_legacy_client`] closes that gap by reading
//! `api.py` and comparing. `xtask/spike-impersonate/baseline_curl_cffi.py`
//! solves the same problem the same way — it lifts the query out of `api.py`
//! with `ast` rather than re-declaring it, for exactly this reason.
//!
//! # Runtime-varying headers
//!
//! Two headers change per request: `X-APOLLO-OPERATION-ID` (a fresh hash) and
//! `X-Client-Date` (now, in milliseconds). They are parameters of
//! [`headers`] rather than computed inside it, so a test can pin the resulting
//! header list without mocking a clock.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The GraphQL endpoint (`api.py:70`).
pub const ENDPOINT: &str = "https://medium.com/_/graphql";

/// The operation name, sent both as a header and in the body (`api.py:38`,
/// `api.py:54`).
pub const OPERATION_NAME: &str = "FullPostQuery";

/// Copied verbatim from `api.py:44`, including the trailing `;` and the absent
/// closing parenthesis after `YandexMobileBot/3.0;`. That string is part of the
/// fingerprint; "fixing" it is not a cleanup.
pub const USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 15_4_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1 (compatible; YandexMobileBot/3.0;";

/// The `FullPostQuery` document, from `src/query.graphql`.
pub const FULL_POST_QUERY: &str = include_str!("query.graphql");

/// The request body (`api.py:53-60`).
///
/// Built rather than pasted, so the post id cannot be interpolated into the
/// query text: it travels as a GraphQL variable, which is what the legacy code
/// does (`variables.postId`).
pub fn body(post_id: &str) -> Value {
    json!({
        "operationName": OPERATION_NAME,
        "variables": {
            "postId": post_id,
            "postMeteringOptions": {},
        },
        "query": FULL_POST_QUERY,
    })
}

/// The header list, in the order `api.py:36-48` declares them.
///
/// `auth_cookies` is the `Cookie` header, added only when configured
/// (`api.py:50-51`). See `RUST_REWRITE_PLAN.md` §2.7 warning 2 before giving
/// this a value: it is an account's session, and the metering it unlocks is
/// quota-bound to that account.
///
/// `Connection: Keep-Alive` is not sent. It is a hop-by-hop header that HTTP/2
/// forbids, and the transport speaks HTTP/2 where the server offers it. The
/// legacy client sets it while also negotiating HTTP/2, so it was already being
/// dropped by curl in the same situation — omitting it here is not a change in
/// what Medium receives.
pub fn headers(
    operation_id: &str,
    client_date_ms: u64,
    auth_cookies: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "X-APOLLO-OPERATION-ID".to_string(),
            operation_id.to_string(),
        ),
        (
            "X-APOLLO-OPERATION-NAME".to_string(),
            OPERATION_NAME.to_string(),
        ),
        (
            "Accept".to_string(),
            "multipart/mixed; deferSpec=20220824, application/json, application/json".to_string(),
        ),
        ("Accept-Language".to_string(), "en-US".to_string()),
        ("X-Obvious-CID".to_string(), "android".to_string()),
        ("X-Xsrf-Token".to_string(), "1".to_string()),
        ("X-Client-Date".to_string(), client_date_ms.to_string()),
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        (
            "Cache-Control".to_string(),
            "public, max-age=-1".to_string(),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];

    if let Some(cookies) = auth_cookies {
        headers.push(("Cookie".to_string(), cookies.to_string()));
    }

    headers
}

/// A fresh `X-APOLLO-OPERATION-ID`: SHA-256 over 32 random bytes, hex-encoded.
///
/// Port of `generate_random_sha256_hash` (`medium_parser/utils.py:117`), which
/// hashes `secrets.token_bytes()` — 32 bytes, cryptographically random. The
/// value is not a secret and nothing verifies it; it is sent so the request
/// looks like the app's own. `getrandom` rather than `rand` because entropy,
/// not a PRNG API, is all that is wanted here — and because the one place this
/// repository needed reproducible randomness (`xtask/difftest/src/prng.rs`)
/// wrote its own generator rather than take the dependency.
pub fn operation_id() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS entropy source is available");
    hex::encode(Sha256::digest(bytes))
}

/// Milliseconds since the Unix epoch, for `X-Client-Date`.
///
/// Port of `get_unix_ms` (`medium_parser/time.py:31`). Note the legacy function
/// goes through `datetime.now().timestamp()`, i.e. the *local* clock, which is
/// the same instant as the epoch-relative one — the epoch is UTC but the
/// difference does not depend on the zone.
pub fn client_date_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The anti-drift gate. See the module doc for why the query is stored
    /// twice and why that is tolerable only with this test.
    #[test]
    fn the_query_matches_the_legacy_client() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../legacy/medium-parser/medium_parser/api.py");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));

        let line = source
            .lines()
            .find(|line| line.trim_start().starts_with("\"query\":"))
            .expect("api.py still declares a \"query\" key");

        // `"query": "<the document>",` on one line. The document contains no
        // double quote — GraphQL arguments here are all enum values and
        // integers — so the last quote on the line is the closing one.
        let after_key = line
            .split_once("\"query\": \"")
            .expect("the key is followed by a quoted value")
            .1;
        let legacy = after_key.rsplit_once('"').expect("the value is quoted").0;

        assert_eq!(
            FULL_POST_QUERY.trim(),
            legacy.trim(),
            "query.graphql has drifted from api.py:59; the Rust client would \
             ask Medium a different question than the Python one"
        );
    }

    /// The file is `include_str!`-ed, so a missing trailing newline would be
    /// invisible — but this also pins that nothing else was appended to it.
    #[test]
    fn the_query_is_the_full_post_query_document() {
        assert!(FULL_POST_QUERY.starts_with("query FullPostQuery($postId: ID!"));
        assert!(FULL_POST_QUERY.trim_end().ends_with('}'));
    }

    #[test]
    fn body_carries_the_id_as_a_variable_not_as_query_text() {
        let body = body("515dd5a43948");
        assert_eq!(body["operationName"], OPERATION_NAME);
        assert_eq!(body["variables"]["postId"], "515dd5a43948");
        assert_eq!(body["variables"]["postMeteringOptions"], json!({}));
        assert_eq!(body["query"], FULL_POST_QUERY);
        // The id must not appear in the document itself.
        assert!(!body["query"].as_str().unwrap().contains("515dd5a43948"));
    }

    #[test]
    fn headers_match_the_legacy_order() {
        let headers = headers("op-id", 1_700_000_000_000, None);
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();

        assert_eq!(
            names,
            [
                "X-APOLLO-OPERATION-ID",
                "X-APOLLO-OPERATION-NAME",
                "Accept",
                "Accept-Language",
                "X-Obvious-CID",
                "X-Xsrf-Token",
                "X-Client-Date",
                "User-Agent",
                "Cache-Control",
                "Content-Type",
            ],
            "api.py:36-48 declares them in this order"
        );
    }

    #[test]
    fn headers_carry_the_expected_values() {
        let headers = headers("op-id", 1_700_000_000_000, None);
        let find = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("{name} is missing"))
        };

        assert_eq!(find("X-APOLLO-OPERATION-ID"), "op-id");
        assert_eq!(find("X-APOLLO-OPERATION-NAME"), OPERATION_NAME);
        assert_eq!(
            find("Accept"),
            "multipart/mixed; deferSpec=20220824, application/json, application/json"
        );
        assert_eq!(find("X-Obvious-CID"), "android");
        assert_eq!(find("X-Xsrf-Token"), "1");
        assert_eq!(find("X-Client-Date"), "1700000000000");
        assert_eq!(find("Cache-Control"), "public, max-age=-1");
        assert_eq!(find("Content-Type"), "application/json");
    }

    /// `Connection` is deliberately absent — see `headers`.
    #[test]
    fn connection_header_is_not_sent() {
        let headers = headers("op-id", 0, None);
        assert!(!headers.iter().any(|(name, _)| name == "Connection"));
    }

    /// The cookie is opt-in, and appended rather than replacing anything.
    #[test]
    fn cookie_is_only_present_when_configured() {
        assert!(
            !headers("op-id", 0, None)
                .iter()
                .any(|(name, _)| name == "Cookie")
        );

        let with_cookie = headers("op-id", 0, Some("uid=1; sid=2"));
        assert_eq!(
            with_cookie.last(),
            Some(&("Cookie".to_string(), "uid=1; sid=2".to_string()))
        );
    }

    /// Every call must produce a distinct id, or the header is decorative.
    #[test]
    fn operation_ids_are_unique_and_hex() {
        let first = operation_id();
        let second = operation_id();

        assert_ne!(first, second);
        assert_eq!(first.len(), 64, "sha256 as hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// The clock is read once per request and must be plausible, not zero —
    /// `unwrap_or(0)` is there for a system clock before 1970, which would
    /// otherwise be a panic on a request path.
    #[test]
    fn client_date_is_a_plausible_epoch_millisecond_value() {
        // 2020-01-01, comfortably in the past.
        assert!(client_date_ms() > 1_577_836_800_000);
    }
}
