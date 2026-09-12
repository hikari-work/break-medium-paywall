//! The two POST routes — `handlers/misc.py`.
//!
//! `/report-problem` is the button at the bottom of an error page; it is
//! unauthenticated and forwards whatever it is given to Telegram.
//! `/delete-from-cache` is the admin route, and the only endpoint in the server
//! that mutates state.
//!
//! # Two different sources for the same "wrong key" message
//!
//! The catch-all's denial (`handlers/main.py:38-41`) reads the key from a
//! **header**; this one reads it from the request **body**
//! (`handlers/misc.py:27`). They are different fields with the same name, and a
//! client that gets one right and the other wrong sees two different messages.
//! Both are reproduced as they are, because an operator debugging a 403 needs the
//! message from the route they actually called.
//!
//! # The body is parsed loosely, and Pydantic did not
//!
//! `BaseModel` rejects a missing field with a 422 and a JSON `detail` body. axum's
//! [`Json`] rejection is also a 422 but with a plain-text body, so the *status*
//! matches and the body does not. These routes are not in the parity gate (it
//! covers the post page), and a report form reading its own error body is not
//! something to spend a bespoke rejection on — but it is a known difference.

use axum::Json;
use axum::extract::State;
use axum::response::Response;
use serde::Deserialize;

use crate::error::{json_error, json_ok};
use crate::notify::MessageStatus;
use crate::state::AppState;

/// `handlers/misc.py:9-11`.
#[derive(Debug, Deserialize)]
pub struct ReportProblem {
    pub page: String,
    pub description: String,
}

/// `handlers/misc.py:14-16`.
///
/// The field is `ADMIN_SECRET_KEY` in the JSON, exactly as Pydantic spells it —
/// which is why the rename is here rather than a `serde(rename_all)`.
#[derive(Debug, Deserialize)]
pub struct DeleteFromCache {
    pub key: String,
    #[serde(rename = "ADMIN_SECRET_KEY")]
    pub admin_secret_key: String,
}

/// `report_problem` (`handlers/misc.py:19-22`).
///
/// No authentication and no rate limit: the legacy has neither, and this route's
/// whole purpose is to be reachable from a page that is already failing. The
/// message goes to Telegram with the default status, so it is an `ERROR` alert.
pub async fn report_problem(
    State(state): State<AppState>,
    Json(problem): Json<ReportProblem>,
) -> Response {
    // `send_message(f"New problem report: \n{description}\n\n{page}")` — the
    // description first, then a blank line, then the page.
    state
        .notifier
        .send(
            &format!(
                "New problem report: \n{}\n\n{}",
                problem.description, problem.page
            ),
            false,
            MessageStatus::Error,
        )
        .await;

    json_ok()
}

/// `delete_from_cache` (`handlers/misc.py:25-37`).
///
/// # What it deletes, and what it does not
///
/// `medium_parser.delete_from_cache` is `self.cache.delete(post_id)`
/// (`core.py:105-107`) — the **Postgres** cache. Redis is not touched at all, so
/// a key deleted here keeps serving its rendered page until the five-hour TTL
/// expires. The `banned_posts` row added at `misc.py:36` is what makes that
/// tolerable: nothing in the request path reads it today, so it is a record for
/// an operator rather than an enforcement mechanism. Both are reproduced —
/// deleting from Redis as well would be an improvement, and an improvement that
/// changes what the endpoint does.
///
/// # The comparison is constant-time
///
/// §7 item 7. `handlers/misc.py:27` is `!=` on two Python strings, which
/// compares byte by byte and stops at the first difference — a timing oracle on
/// the secret, on an endpoint that is reachable from the internet.
pub async fn delete_from_cache(
    State(state): State<AppState>,
    Json(body): Json<DeleteFromCache>,
) -> Response {
    if !constant_time_eq(&body.admin_secret_key, &state.config.admin_secret_key) {
        // The value echoed is the caller's own, as at `misc.py:28`.
        return json_error(format!("Wrong secret key: {}", body.admin_secret_key), 403);
    }

    // The key is the bare post id: the Postgres table is keyed by it
    // (`postgres.rs`), while the `v2:` prefix belongs to Redis alone. Passing a
    // prefixed key here would delete nothing and ban a key no post has.
    if let Err(err) = state.postgres.delete(&body.key).await {
        tracing::error!(error = %err, key = body.key, "could not delete from cache");
        // `misc.py:34` renders the exception into the message, which leaks a
        // Postgres error string to the caller. Kept: this route is admin-only,
        // and the message is what an operator has to debug with.
        return json_error(format!("Couldn't delete from cache: {err}"), 500);
    }

    if let Err(err) = state.postgres.ban_post(&body.key).await {
        // The legacy writes a local file here (`ban_db.set`), which cannot
        // realistically fail; a database write can. The delete has already
        // happened and is the part the client asked for, so this is logged
        // rather than reported as a failure — the alternative is a 500 that
        // claims the delete failed when it did not.
        tracing::error!(
            error = %err,
            key = body.key,
            "deleted from the cache, but could not record the ban"
        );
    } else {
        tracing::info!(key = body.key, "deleted from the cache and banned");
    }

    json_ok()
}

/// Whether two strings are equal, in time that does not depend on where they
/// first differ.
///
/// A length difference is not secret — the length of the key it was sent is the
/// caller's own — so returning early on it leaks nothing, and `ct_eq` is only
/// ever reached with two slices that are the same length.
fn constant_time_eq(presented: &str, expected: &str) -> bool {
    let presented = presented.as_bytes();
    let expected = expected.as_bytes();
    presented.len() == expected.len()
        && bool::from(subtle::ConstantTimeEq::ct_eq(presented, expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_comparison_accepts_only_the_exact_key() {
        assert!(constant_time_eq("s3cret", "s3cret"));
        // The same length, differing in the last byte — the case a short-circuit
        // comparison would leak the position of.
        assert!(!constant_time_eq("s3cret", "s3creT"));
        assert!(!constant_time_eq("s3cre", "s3cret"));
        assert!(!constant_time_eq("s3cret ", "s3cret"));
        assert!(!constant_time_eq("", "s3cret"));
        assert!(constant_time_eq("", ""));
        // Multi-byte: "é" is two bytes, so it must not match the one-byte "e"
        // even though a char-wise comparison might call both a single letter.
        assert!(constant_time_eq("é", "é"));
        assert!(!constant_time_eq("é", "e"));
    }

    /// The JSON field is spelled as Pydantic spells it, in capitals — a client
    /// sending `admin_secret_key` must be rejected the way Pydantic would reject
    /// it, not silently accepted.
    #[test]
    fn the_admin_key_field_is_the_capitalised_one() {
        let body: DeleteFromCache =
            serde_json::from_str(r#"{"key":"abc","ADMIN_SECRET_KEY":"s"}"#).unwrap();
        assert_eq!(body.key, "abc");
        assert_eq!(body.admin_secret_key, "s");

        assert!(
            serde_json::from_str::<DeleteFromCache>(r#"{"key":"abc","admin_secret_key":"s"}"#)
                .is_err(),
            "the lowercase spelling is not the legacy's field"
        );
        assert!(
            serde_json::from_str::<DeleteFromCache>(r#"{"key":"abc"}"#).is_err(),
            "a missing key must fail, as Pydantic's required field does"
        );
    }

    #[test]
    fn a_problem_report_needs_both_fields() {
        let report: ReportProblem =
            serde_json::from_str(r#"{"page":"/x","description":"broken"}"#).unwrap();
        assert_eq!(report.page, "/x");
        assert!(serde_json::from_str::<ReportProblem>(r#"{"page":"/x"}"#).is_err());
    }
}
