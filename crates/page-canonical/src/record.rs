//! The JSONL line one shadowed request leaves behind.
//!
//! Written by the edge (`edge/freedium-edge`), read by `difftest shadow-report`.
//! It lives here, next to the comparator, for the same reason the comparator
//! does: two definitions of "what a record looks like" would drift, and the
//! failure would be a report that silently read zero rows because a field had
//! been renamed on one side. One type, `Serialize` for the writer and
//! `Deserialize` for the reader, is the whole point.
//!
//! # Every request gets a line, including the ones that were not compared
//!
//! [`RecordOutcome::Excluded`] is the counterweight to the gate. A shadow that
//! only logged the requests it *did* compare would be indistinguishable from one
//! whose eligibility rules had quietly grown to swallow the corpus — and §5's
//! seven-day counter reads a run of nothing as success. So a request the edge
//! declines to shadow is recorded, with a reason, and the report prints the
//! counts by reason. Nothing is silently absent.
//!
//! The cost is real and worth naming: one line per request, including static
//! assets, on an instance that also serves live traffic. `SHADOW_ENABLED=false`
//! removes the feature and the I/O together, which is why the kill switch is the
//! rollback rather than a log-level tweak.
//!
//! # Why the outcome is an enum and not the comparator's `Outcome`
//!
//! [`Outcome`](crate::compare::Outcome) is what the comparison *came to*; this
//! is what the *edge* did with the request, and it has two cases the comparator
//! cannot have: `Excluded` (never compared) and `PrimaryError` (the request the
//! edge was proxying failed, so there was nothing to compare against). Keeping
//! them apart means the comparator never needs a variant it can never produce.

use serde::{Deserialize, Serialize};

use crate::compare::Outcome;

/// The header a shadow instance stamps on a response it declined to render.
///
/// # Why this lives here and not with either side
///
/// It is a two-process wire protocol: `freedium-web` writes it
/// (`crates/freedium-web/src/error.rs`, where the response is built) and the edge
/// reads it (`edge/freedium-edge/src/shadow.rs`). A rename on one side and not
/// the other would not fail to compile, would not fail a test, and would not be
/// visible in either process — the edge would simply stop recognising declines
/// and compare them, and every uncached post would become a fake difference.
/// That failure is loud, which is why this is not a crisis, but the way to make
/// it impossible is to keep the constant in the one place both already depend on.
///
/// The edge does depend on this crate, so for the reader there is exactly one
/// definition. The writer is the duplicate, and it is a deliberate one:
/// `freedium-web` does not depend on `page-canonical`, because this crate parses
/// HTML and a crate that only emits pages should not carry a parser. The two are
/// pinned by [`the_marker_matches_the_shared_protocol`] here and its twin
/// `the_marker_matches_the_shared_protocol` in `freedium-web`'s `error.rs`; each
/// names the other file. **If you change a string here, change it there.**
///
/// Lowercase, as HTTP/2 requires and as `HeaderName::from_static` asserts.
pub const SHADOW_HEADER: &str = "x-freedium-shadow";

/// The one reason token there is: the durable cache missed and the instance is
/// not allowed to fetch.
///
/// A token rather than free text because it is counted and grouped by
/// `difftest shadow-report`, and because a reason assembled per request would be
/// one more thing that can differ between two runs of the same request. A second
/// reason becomes a second constant, and the report shows the split.
///
/// Duplicated on the writer the same way [`SHADOW_HEADER`] is, and pinned by the
/// same pair of tests.
pub const SHADOW_NO_FETCH: &str = "no-fetch";

/// What the edge did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordOutcome {
    /// Both sides answered the same thing.
    Identical,
    /// Only the status could be compared, and it matched.
    StatusOnly,
    /// A difference a declaration accounts for.
    Declared,
    /// A difference nothing accounts for. What the gate fails on.
    Different,
    /// The shadow declined to render — `SHADOW_MODE` and a cache miss.
    Declined,
    /// The shadow did not answer at all.
    Unreachable,
    /// The edge did not shadow this request. [`ShadowRecord::reason`] says why,
    /// and the report counts by it.
    Excluded,
    /// The request the edge was proxying failed, so there was nothing to
    /// compare. Not a difference and not a failure — but a run of nothing else
    /// is, which the report's degeneracy check is for.
    PrimaryError,
}

impl RecordOutcome {
    /// Whether this outcome is a comparison that happened, as opposed to one
    /// that was skipped. Both `Identical` and `Different` count.
    ///
    /// The report's degeneracy check is built on this: a run with no comparable
    /// requests has produced no evidence, however few failures it has.
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Identical | Self::StatusOnly | Self::Declared | Self::Different
        )
    }

    /// Whether this outcome is a difference, declared or not.
    pub fn is_difference(self) -> bool {
        matches!(self, Self::Declared | Self::Different)
    }

    /// The outcome as it is spelled in the log, and therefore as
    /// `difftest shadow-report` prints it.
    ///
    /// Hand-written rather than derived, because the report needs a `Display`
    /// for a table column and `serde`'s name is a `String`. The two are held
    /// together by [`the_outcome_tokens_are_stable`], which asserts that this
    /// and the serialised form are the same string for every variant — so a
    /// rename in either direction fails one test rather than producing a report
    /// that prints a token the log does not use.
    pub fn token(self) -> &'static str {
        match self {
            Self::Identical => "identical",
            Self::StatusOnly => "status-only",
            Self::Declared => "declared",
            Self::Different => "different",
            Self::Declined => "declined",
            Self::Unreachable => "unreachable",
            Self::Excluded => "excluded",
            Self::PrimaryError => "primary-error",
        }
    }
}

/// One side's answer, as it is recorded.
///
/// The body is deliberately *not* recorded. A soak's log would grow by a
/// megabyte per request, and the detail of a difference is already in
/// [`ShadowRecord::detail`] — as the canonical node, which is the part a human
/// needs and a fraction of the size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Side {
    pub status: u16,
    /// From the first byte of the request to the last byte of the response, as
    /// that side's client saw it. Not comparable across the two in absolute
    /// terms — the shadow's clock starts after the primary's response is already
    /// written — which is why the report prints them side by side rather than a
    /// ratio.
    pub ms: f64,
    pub bytes: usize,
}

/// A declaration that accounted for a difference, recorded so the report can
/// find the declarations that never fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredMatch {
    /// The declaration's `path` pattern, as written in its file.
    pub path: String,
    pub reason: String,
}

/// One line of the shadow log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowRecord {
    /// Unix milliseconds. A `u64` rather than a formatted date because the
    /// formatting belongs where the report is read, and because a string date
    /// would need both sides to agree on a format as well as a value.
    pub ts_ms: u64,

    /// The request path, without its query. This is what a declaration matches
    /// on, and what the report groups by.
    pub path: String,

    /// The query string, when there was one. Recorded because two requests for
    /// the same path with different queries are different requests, and a report
    /// that dropped the query could not tell a `no-redis` bypass apart from an
    /// ordinary hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,

    pub outcome: RecordOutcome,

    /// The first difference, when there was one — in the comparator's own
    /// words, so a report and a gate failure read the same.
    ///
    /// Set for [`RecordOutcome::Different`] and for
    /// [`RecordOutcome::Declared`]: a declared difference is a difference, and
    /// the text is what lets the report show that an allowance is still
    /// excusing the same thing it was written for. Absent for every outcome
    /// that compared nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    /// Set only for [`RecordOutcome::Declared`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<DeclaredMatch>,

    /// Why this request was excluded, declined or unreachable — a short token
    /// for exclusions, the shadow's own reason for the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// The primary's answer. Always present: the edge only reaches the
    /// comparison having successfully proxied something.
    pub primary: Side,

    /// The shadow's answer. Absent exactly when there was no shadow answer —
    /// `Declined`, `Unreachable`, `Excluded`, `PrimaryError`. [`ShadowRecord::record`]
    /// asserts the correspondence, so a report can rely on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Side>,
}

impl ShadowRecord {
    /// The record for a comparison the edge completed, whatever it came to.
    ///
    /// One constructor rather than one per outcome, because the fields that
    /// describe an outcome have to agree with the outcome itself — a `Declared`
    /// with no `declared`, or a `Different` with no `detail`, would be a report
    /// that could not say what it had found. Here that cannot happen: the
    /// mapping is in one place, and the tests walk every variant.
    pub fn record(
        ts_ms: u64,
        path: &str,
        query: Option<&str>,
        primary: Side,
        shadow: Option<Side>,
        outcome: &Outcome,
    ) -> Self {
        let (record_outcome, detail, declared, reason) = match outcome {
            Outcome::Identical => (RecordOutcome::Identical, None, None, None),
            Outcome::StatusOnly => (RecordOutcome::StatusOnly, None, None, None),
            Outcome::Declared {
                path,
                reason,
                detail,
                ..
            } => (
                RecordOutcome::Declared,
                Some(detail.clone()),
                Some(DeclaredMatch {
                    path: path.clone(),
                    reason: reason.clone(),
                }),
                None,
            ),
            Outcome::Different { detail } => {
                (RecordOutcome::Different, Some(detail.clone()), None, None)
            }
            Outcome::Declined { reason } => {
                (RecordOutcome::Declined, None, None, Some(reason.clone()))
            }
            Outcome::Unreachable { reason } => {
                (RecordOutcome::Unreachable, None, None, Some(reason.clone()))
            }
        };

        // The invariant the report relies on, checked where it can be violated.
        // `Declined` and `Unreachable` are the two outcomes with no second
        // answer; every other outcome was produced by comparing two `Served`
        // values and so must have one.
        debug_assert_eq!(
            shadow.is_some(),
            !matches!(
                outcome,
                Outcome::Declined { .. } | Outcome::Unreachable { .. }
            ),
            "outcome {record_outcome:?} and the presence of a shadow answer disagree"
        );

        Self {
            ts_ms,
            path: path.to_string(),
            query: query.map(str::to_string),
            outcome: record_outcome,
            detail,
            declared,
            reason,
            primary,
            shadow,
        }
    }

    /// The record for a request the edge did not shadow.
    ///
    /// `reason` is a short token from a closed set — see the edge's
    /// `eligibility` module — because the report groups by it and a token that
    /// varied per request would make every exclusion its own category.
    pub fn excluded(
        ts_ms: u64,
        path: &str,
        query: Option<&str>,
        reason: &str,
        primary: Side,
    ) -> Self {
        Self {
            ts_ms,
            path: path.to_string(),
            query: query.map(str::to_string),
            outcome: RecordOutcome::Excluded,
            detail: None,
            declared: None,
            reason: Some(reason.to_string()),
            primary,
            shadow: None,
        }
    }

    /// The record for a request the edge failed to proxy.
    ///
    /// `primary` is the best measurement there is in that case — whatever was
    /// written before the failure — and the report counts these separately
    /// rather than treating them as comparisons.
    pub fn primary_error(
        ts_ms: u64,
        path: &str,
        query: Option<&str>,
        error: &str,
        primary: Side,
    ) -> Self {
        Self {
            ts_ms,
            path: path.to_string(),
            query: query.map(str::to_string),
            outcome: RecordOutcome::PrimaryError,
            detail: None,
            declared: None,
            reason: Some(error.to_string()),
            primary,
            shadow: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::DiffKind;

    fn side(status: u16) -> Side {
        Side {
            status,
            ms: 12.5,
            bytes: 4096,
        }
    }

    /// A record must survive the trip through a log file that the report reads
    /// back. This is the test that would have caught a `skip_serializing_if`
    /// that dropped a field the reader then required.
    #[test]
    fn a_record_round_trips_through_json() {
        let record = ShadowRecord::record(
            1_760_000_000_000,
            "/0291df856c77",
            Some("a=1"),
            side(200),
            Some(side(200)),
            &Outcome::Identical,
        );

        // Exactly one line: a record containing a newline would corrupt a JSONL
        // file, and the only string that could carry one is a detail or reason.
        let line = serde_json::to_string(&record).expect("a record serializes");
        assert!(!line.contains('\n'), "{line}");

        assert_eq!(
            serde_json::from_str::<ShadowRecord>(&line).expect("and reads back"),
            record
        );
    }

    /// Every outcome maps to its own token, and the mapping carries the field
    /// that describes it. A `Declared` that lost its declaration, or a
    /// `Different` that lost its detail, is a report that cannot say what it
    /// found.
    #[test]
    fn every_outcome_maps_to_its_token_and_its_field() {
        let cases = [
            (
                Outcome::Identical,
                RecordOutcome::Identical,
                true,
                false,
                false,
            ),
            (
                Outcome::StatusOnly,
                RecordOutcome::StatusOnly,
                true,
                false,
                false,
            ),
            (
                Outcome::Declared {
                    path: "/p".to_string(),
                    kind: DiffKind::TextOrMarkup,
                    reason: "why".to_string(),
                    detail: "canonical node #3: primary \"a\" vs shadow \"b\"".to_string(),
                },
                RecordOutcome::Declared,
                true,
                true,
                true,
            ),
            (
                Outcome::Different {
                    detail: "node #3 differs".to_string(),
                },
                RecordOutcome::Different,
                true,
                false,
                true,
            ),
            (
                Outcome::Declined {
                    reason: "no-fetch".to_string(),
                },
                RecordOutcome::Declined,
                false,
                false,
                false,
            ),
            (
                Outcome::Unreachable {
                    reason: "connection refused".to_string(),
                },
                RecordOutcome::Unreachable,
                false,
                false,
                false,
            ),
        ];

        for (outcome, expected, has_shadow, has_declared, has_detail) in cases {
            let record = ShadowRecord::record(
                0,
                "/p",
                None,
                side(200),
                has_shadow.then(|| side(200)),
                &outcome,
            );
            assert_eq!(record.outcome, expected, "{outcome:?}");
            assert_eq!(record.shadow.is_some(), has_shadow, "{outcome:?}");
            assert_eq!(record.declared.is_some(), has_declared, "{outcome:?}");
            assert_eq!(record.detail.is_some(), has_detail, "{outcome:?}");

            // And the reason travels for the two outcomes whose whole content is
            // a reason — that is what the report groups declined traffic by.
            if matches!(
                outcome,
                Outcome::Declined { .. } | Outcome::Unreachable { .. }
            ) {
                assert!(record.reason.is_some(), "{outcome:?}");
            }
        }
    }

    /// A declined record must carry the reason token verbatim, because the
    /// report counts by it and `no-fetch` is the token the shadow's own header
    /// carries.
    #[test]
    fn a_declined_record_carries_the_reason_token() {
        let record = ShadowRecord::record(
            0,
            "/p",
            None,
            side(503),
            None,
            &Outcome::Declined {
                reason: "no-fetch".to_string(),
            },
        );
        assert_eq!(record.reason.as_deref(), Some("no-fetch"));
        assert!(record.shadow.is_none());
    }

    /// A declared difference keeps its description, in the log.
    ///
    /// This is the field that makes a soak's declared outcomes reviewable. The
    /// declaration matches a *class* of differences, so a run that recorded only
    /// "the allowance fired 4311 times" would look the same whether the
    /// allowance were still excusing what it was written for or had quietly
    /// started excusing something else. `difftest shadow-report` groups these by
    /// declaration for exactly that reason.
    #[test]
    fn a_declared_record_carries_the_difference_it_excused() {
        let detail = "canonical node #3: primary \"mt-3\" vs shadow \"mt-7\"";
        let record = ShadowRecord::record(
            0,
            "/p",
            None,
            side(200),
            Some(side(200)),
            &Outcome::Declared {
                path: "/p".to_string(),
                kind: DiffKind::TextOrMarkup,
                reason: "the legacy drops a trailing space here".to_string(),
                detail: detail.to_string(),
            },
        );

        assert_eq!(record.detail.as_deref(), Some(detail));
        assert_eq!(
            record.declared.as_ref().map(|it| it.path.as_str()),
            Some("/p")
        );
        assert!(record.outcome.is_comparison());
        assert!(record.outcome.is_difference());

        // It has to survive the log, because the log is the only thing the
        // report ever sees.
        let line = serde_json::to_string(&record).expect("a record serializes");
        let read_back: ShadowRecord = serde_json::from_str(&line).expect("and reads back");
        assert_eq!(read_back.detail.as_deref(), Some(detail));
    }

    /// The classification the report's degeneracy check reads. `Declared` is a
    /// difference *and* a comparison: it must not inflate the "clean" count
    /// without being visible as a difference, and it must count as evidence.
    #[test]
    fn the_classification_separates_comparisons_from_skips() {
        assert!(RecordOutcome::Identical.is_comparison());
        assert!(RecordOutcome::StatusOnly.is_comparison());
        assert!(RecordOutcome::Declared.is_comparison());
        assert!(RecordOutcome::Different.is_comparison());

        assert!(!RecordOutcome::Declined.is_comparison());
        assert!(!RecordOutcome::Unreachable.is_comparison());
        assert!(!RecordOutcome::Excluded.is_comparison());
        assert!(!RecordOutcome::PrimaryError.is_comparison());

        assert!(RecordOutcome::Declared.is_difference());
        assert!(RecordOutcome::Different.is_difference());
        assert!(!RecordOutcome::Declined.is_difference());
        assert!(!RecordOutcome::StatusOnly.is_difference());
    }

    /// An excluded record has no shadow answer — and its `reason` is what the
    /// report prints, so that a rule which swallowed the corpus is visible
    /// rather than absent.
    #[test]
    fn an_excluded_record_says_why() {
        let record = ShadowRecord::excluded(0, "/", None, "homepage", side(200));
        assert_eq!(record.outcome, RecordOutcome::Excluded);
        assert_eq!(record.reason.as_deref(), Some("homepage"));
        assert!(record.shadow.is_none());
        assert!(!record.outcome.is_comparison());
    }

    /// A primary failure is not a difference and not a comparison — the
    /// distinction the report needs so that a broken edge cannot read as a
    /// clean gate.
    #[test]
    fn a_primary_error_is_neither_a_difference_nor_a_comparison() {
        let record = ShadowRecord::primary_error(0, "/p", None, "upstream timeout", side(504));
        assert_eq!(record.outcome, RecordOutcome::PrimaryError);
        assert!(!record.outcome.is_comparison());
        assert!(!record.outcome.is_difference());
    }

    /// The tokens are the file format. They are spelled here so that renaming a
    /// variant is a change to this test rather than a silent change to every log
    /// a soak has already written.
    ///
    /// Both directions are checked — the serialised form *and* [`token`], which
    /// is what the report's table prints. A report that spoke a different
    /// vocabulary from the log it read would be worse than no report.
    ///
    /// [`token`]: RecordOutcome::token
    #[test]
    fn the_outcome_tokens_are_stable() {
        let pairs = [
            (RecordOutcome::Identical, "\"identical\""),
            (RecordOutcome::StatusOnly, "\"status-only\""),
            (RecordOutcome::Declared, "\"declared\""),
            (RecordOutcome::Different, "\"different\""),
            (RecordOutcome::Declined, "\"declined\""),
            (RecordOutcome::Unreachable, "\"unreachable\""),
            (RecordOutcome::Excluded, "\"excluded\""),
            (RecordOutcome::PrimaryError, "\"primary-error\""),
        ];
        for (outcome, token) in pairs {
            assert_eq!(serde_json::to_string(&outcome).unwrap(), token);
            assert_eq!(format!("\"{}\"", outcome.token()), token);
        }

        // And with `Excluded`/`PrimaryError` in it, so the table above cannot
        // quietly stop covering a variant that was added.
        assert_eq!(pairs.len(), 8);
    }

    /// An unknown field is an error, for the same reason it is on a
    /// declaration: a log written by a newer edge must not be read as if the
    /// field it added were absent.
    #[test]
    fn an_unknown_field_is_rejected() {
        let line = r#"{"ts_ms":0,"path":"/p","outcome":"identical","primary":{"status":200,"ms":1.0,"bytes":2},"surprise":1}"#;
        assert!(serde_json::from_str::<ShadowRecord>(line).is_err());
    }

    /// The marker's exact spelling, pinned on the reader's side.
    ///
    /// `SHADOW_HEADER` is the name the edge looks for; its twin in
    /// `crates/freedium-web/src/error.rs` is the name the server stamps, and the
    /// two are in different cargo workspaces so nothing can compare them for us.
    /// The assertion is written as a literal so that this test is readable as the
    /// protocol rather than as a tautology — see [`SHADOW_HEADER`].
    #[test]
    fn the_marker_matches_the_shared_protocol() {
        assert_eq!(SHADOW_HEADER, "x-freedium-shadow");
        assert_eq!(SHADOW_NO_FETCH, "no-fetch");

        // The reason the edge reads out of the header is the reason that lands in
        // the record, unmodified — that is what lets `shadow-report` group
        // declines by a token instead of by prose.
        let record = ShadowRecord::record(
            0,
            "/p",
            None,
            side(503),
            None,
            &Outcome::Declined {
                reason: SHADOW_NO_FETCH.to_string(),
            },
        );
        assert_eq!(record.reason.as_deref(), Some(SHADOW_NO_FETCH));
    }
}
