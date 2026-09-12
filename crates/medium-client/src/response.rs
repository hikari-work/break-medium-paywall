//! Deciding whether a GraphQL payload actually contains a post.
//!
//! A 200 from `medium.com/_/graphql` is not success. The endpoint answers 200
//! with `{"data":{"post":null}}` for a deleted post, and with an `errors`
//! array for a rejected query. `MediumApi.query_post_graphql` only checks
//! `status_code != 200` (`api.py:78`), so the real check lives in
//! `MediumParser.query` (`core.py:178-187`) — five truthiness tests that
//! produce a `reason` string.
//!
//! Those five tests are ported here as one function returning a `Result`,
//! which is the §7 item 3 cleanup applied to the check rather than to the loop:
//! the legacy version builds a `reason` string that the caller then has to
//! remember to reset. Here there is nothing to reset, because the outcome of
//! one attempt is a value rather than a variable that outlives it.
//!
//! The *order* is load-bearing and kept: `error` is reported before a missing
//! `data`, so a query Medium rejected is not misreported as an empty post.

use serde_json::Value;

use crate::error::FetchError;

/// Checks that `payload` is a 200 response carrying a post.
///
/// The counterpart of the `reason` chain at `core.py:178-187`, in the same
/// order.
pub fn validate(payload: &Value) -> Result<(), FetchError> {
    let Value::Object(root) = payload else {
        // `core.py:180-181`. Reachable in practice: a `null` or bare-array body
        // is a valid JSON document and a perfectly fine HTTP 200.
        return Err(FetchError::Malformed(describe(payload)));
    };

    // `core.py:182-183`. Checked before `data` on purpose — see the module doc.
    if let Some(error) = root.get("error") {
        return Err(FetchError::GraphQl(describe(error)));
    }

    // `core.py:184-187`. `data` and `data.post` are separate checks in the
    // legacy code and collapse to one here, because which of the two is missing
    // does not change what the caller does: there is no post, and retrying will
    // not produce one.
    let post = root.get("data").and_then(|data| data.get("post"));
    match post {
        Some(Value::Null) | None => Err(FetchError::NoPost),
        Some(_) => Ok(()),
    }
}

/// A short rendering of a value for an error message.
///
/// Bounded because the input is arbitrary up to a few hundred kilobytes of
/// article JSON, and an error message is not a place to put that. Counted in
/// characters, not bytes, so the cut cannot land inside a multi-byte character
/// — the payload is full of them.
fn describe(value: &Value) -> String {
    const LIMIT: usize = 200;

    let mut rendered = value.to_string();
    if rendered.chars().count() > LIMIT {
        let cut = rendered
            .char_indices()
            .nth(LIMIT)
            .map(|(index, _)| index)
            .expect("counted more than LIMIT characters");
        rendered.truncate(cut);
        rendered.push('…');
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_payload_with_a_post_passes() {
        let payload = json!({"data": {"post": {"id": "abc"}}});
        assert_eq!(validate(&payload), Ok(()));
    }

    /// The common real-world miss: Medium answers 200 with a null post for a
    /// deleted or unpublished article.
    #[test]
    fn a_null_post_is_no_post() {
        let payload = json!({"data": {"post": null}});
        assert_eq!(validate(&payload), Err(FetchError::NoPost));
    }

    #[test]
    fn a_missing_post_key_is_no_post() {
        assert_eq!(validate(&json!({"data": {}})), Err(FetchError::NoPost));
    }

    #[test]
    fn a_missing_data_key_is_no_post() {
        assert_eq!(
            validate(&json!({"extensions": {}})),
            Err(FetchError::NoPost)
        );
    }

    /// `core.py` checks `error` before `data`, so a rejected query is reported
    /// as a GraphQL error even when the body also has no `data`.
    #[test]
    fn an_error_is_reported_before_a_missing_data() {
        let payload = json!({"error": {"message": "nope"}});
        assert!(matches!(validate(&payload), Err(FetchError::GraphQl(_))));
    }

    /// Order, pinned from the other side: when both are present the error wins
    /// too, so a partially-populated body cannot mask a rejection.
    #[test]
    fn an_error_wins_over_a_present_post() {
        let payload = json!({"error": "rate limited", "data": {"post": {"id": "abc"}}});
        assert!(matches!(validate(&payload), Err(FetchError::GraphQl(_))));
    }

    #[test]
    fn a_non_object_payload_is_malformed() {
        for payload in [json!(null), json!([1, 2, 3]), json!("a string")] {
            assert!(
                matches!(validate(&payload), Err(FetchError::Malformed(_))),
                "{payload} should be malformed"
            );
        }
    }

    /// A short value is reproduced whole.
    #[test]
    fn describe_keeps_short_values_intact() {
        assert_eq!(describe(&json!(null)), "null");
        assert_eq!(describe(&json!({"a": 1})), r#"{"a":1}"#);
    }

    /// The bound exists because these strings reach error messages and logs. A
    /// byte slice at 200 would panic here, since every character is 4 bytes.
    #[test]
    fn describe_bounds_long_values_without_splitting_characters() {
        let long = json!("😀".repeat(500));
        let described = describe(&long);

        // The 200 kept characters are of the JSON *rendering*, so they include
        // the opening quote; the ellipsis is appended after the cut.
        assert_eq!(described.chars().count(), 201);
        assert!(described.starts_with('"'));
        assert!(described.ends_with('…'));
    }
}
