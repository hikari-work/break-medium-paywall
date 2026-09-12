//! What each server answered, and whether the two answers disagree.
//!
//! [`canonical`](crate::canonical) reduces one page to a comparable form. This
//! module is the layer above it: it takes both servers' answers and decides
//! whether they are the same, accounting for the three cases a fixture-based
//! gate never sees.
//!
//! # The three cases, and why each one is not "a difference"
//!
//! | Case | Outcome | Why |
//! |---|---|---|
//! | The shadow declined to render — `SHADOW_MODE` and a Postgres miss | [`Outcome::Declined`] | The Rust instance is configured **not** to fetch (`RUST_REWRITE_PLAN` §3.1: SPIKE-1's pooled gate is open, and the exit IP is the one production serves from). There is no second opinion to compare against, so counting it as a failure would make the gate red for a decision that was deliberate. |
//! | The shadow could not be reached | [`Outcome::Unreachable`] | Kept apart from `Declined` on purpose. A shadow that is simply down produces a run of skipped comparisons, and a gate that reads *that* as clean is not evidence — the report's degeneracy check is built on this distinction. |
//! | Both sides answered an error | [`Outcome::StatusOnly`] | `error.rs` picks one of fifteen messages at random and the transponder code is random too, so two 404s **never** have equal bodies. The status is the only part that can be compared. |
//!
//! # Why non-HTML is status-only rather than a byte comparison
//!
//! A response that is not `text/html` is not a rendered page, and this gate is
//! about rendered pages. Comparing it byte-for-byte would be a different and
//! much stricter contract, and it would fail on things nobody cares about — a
//! `Set-Cookie` that moved, a compression that differed. Stated as a
//! limitation: a redirect whose `Location` differs **is** a real difference
//! this comparator does not catch.
//!
//! # Declarations are deliberately coarse
//!
//! A [`Declaration`] matches on the path and the *kind* of difference, not on
//! the exact text. That is the same coarseness the render gate accepts for
//! `expected_divergence` (`xtask/difftest/src/render.rs`), and it is safe for
//! the same reason: a declaration carries a mandatory `reason`, a difference
//! nothing declares fails the run, and a declaration that stops firing is
//! reported. What it does not do is let a *new* difference hide behind an old
//! declaration — a second, different difference on the same path and kind
//! would be absorbed. That is why [`Outcome::Declared`] carries the difference's
//! own description and `difftest shadow-report` prints each declaration's
//! distinct ones rather than a bare count: a count cannot show that the
//! difference an allowance was written for has been replaced by another.

use serde::{Deserialize, Serialize};

use crate::canonical::{CanonicalNode, canonicalize};

/// One server's answer, ready to compare.
///
/// `body` is a `String`: both servers serve `text/html; charset=utf-8`, and a
/// caller holding raw bytes is expected to have gone through
/// `String::from_utf8_lossy` — a body that is not valid UTF-8 cannot be a page
/// either server renders, and the lossy conversion keeps it comparable rather
/// than dropping the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub status: u16,
    /// `Content-Type` with its parameters stripped and lowercased — `text/html`,
    /// not `text/html; charset=utf-8`. `None` for a response that sent no
    /// `Content-Type` at all.
    pub content_type: Option<String>,
    pub body: String,
}

impl Served {
    pub fn new(
        status: u16,
        content_type: Option<impl Into<String>>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            status,
            content_type: content_type.map(Into::into).map(|value| normalize(&value)),
            body: body.into(),
        }
    }

    /// Whether this is a page the canonical form applies to.
    pub fn is_html(&self) -> bool {
        self.content_type.as_deref() == Some("text/html")
    }
}

/// `Content-Type` reduced to its media type: parameters dropped, lowercased,
/// trimmed. `TEXT/HTML; charset=utf-8` and `text/html` are the same type.
fn normalize(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}

/// What the shadow said.
///
/// Only the shadow has these cases — the primary is the server currently
/// answering real users, so it always serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Served(Served),
    /// The shadow declined to render this request. See the module docs.
    Declined {
        reason: String,
    },
    /// The shadow did not answer at all. See the module docs.
    Unreachable {
        reason: String,
    },
}

/// How two answers differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffKind {
    /// The two statuses disagree.
    Status,
    /// Both sides have a node at this position and their text or annotations
    /// differ — the ordinary case.
    TextOrMarkup,
    /// The primary had a node the shadow did not.
    MissingInShadow,
    /// The shadow had a node the primary did not.
    MissingInPrimary,
}

/// The first place two canonical sequences disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Which node, in canonical order. Reported so a human can find it.
    pub index: usize,
    pub kind: DiffKind,
    /// The primary's rendering of that node, or `None` where it had none.
    pub primary: Option<String>,
    /// The shadow's rendering of that node, or `None`.
    pub shadow: Option<String>,
}

impl Difference {
    /// One line for a log or a report.
    pub fn detail(&self) -> String {
        let position = format!("canonical node #{}", self.index);
        match self.kind {
            DiffKind::Status => format!(
                "status: primary {} vs shadow {}",
                self.primary.as_deref().unwrap_or("?"),
                self.shadow.as_deref().unwrap_or("?")
            ),
            DiffKind::TextOrMarkup => format!(
                "{position}: primary {} vs shadow {}",
                self.primary.as_deref().unwrap_or("(none)"),
                self.shadow.as_deref().unwrap_or("(none)")
            ),
            DiffKind::MissingInShadow => format!(
                "{position}: primary has {} and the shadow has nothing there",
                self.primary.as_deref().unwrap_or("(none)")
            ),
            DiffKind::MissingInPrimary => format!(
                "{position}: shadow has {} and the primary has nothing there",
                self.shadow.as_deref().unwrap_or("(none)")
            ),
        }
    }
}

/// The first difference between two canonical sequences, or `None`.
///
/// Walks both in step and stops at the first disagreement rather than
/// collecting all of them: one difference makes the outcome what it is, and a
/// report that named every downstream consequence of a single dropped element
/// would be much longer and no more informative.
pub fn difference(primary: &[CanonicalNode], shadow: &[CanonicalNode]) -> Option<Difference> {
    for index in 0..primary.len().max(shadow.len()) {
        match (primary.get(index), shadow.get(index)) {
            (Some(left), Some(right)) if left == right => continue,
            (Some(left), Some(right)) => {
                return Some(Difference {
                    index,
                    kind: DiffKind::TextOrMarkup,
                    primary: Some(left.describe()),
                    shadow: Some(right.describe()),
                });
            }
            (Some(left), None) => {
                return Some(Difference {
                    index,
                    kind: DiffKind::MissingInShadow,
                    primary: Some(left.describe()),
                    shadow: None,
                });
            }
            (None, Some(right)) => {
                return Some(Difference {
                    index,
                    kind: DiffKind::MissingInPrimary,
                    primary: None,
                    shadow: Some(right.describe()),
                });
            }
            // `index` is below the longer length, so one of the two is present.
            (None, None) => unreachable!("index is within the longer slice"),
        }
    }
    None
}

/// What the comparison came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Both sides answered the same thing.
    Identical,
    /// Only the status could be compared, and it matched. Error pages and
    /// non-HTML responses — see the module docs.
    StatusOnly,
    /// A difference a declaration accounts for.
    ///
    /// Carries the matched declaration so the report can say which one fired,
    /// and therefore which ones never did — and carries `detail`, the same
    /// description [`Outcome::Different`] would have held, so a declared
    /// difference is still visible rather than merely counted. A declaration
    /// absorbs a *class* of differences, so the only way to tell an allowance
    /// that is still excusing what it was written for from one that is now
    /// excusing something else is to keep the text.
    Declared {
        path: String,
        kind: DiffKind,
        reason: String,
        detail: String,
    },
    /// A difference nothing accounts for. This is what the gate fails on.
    Different { detail: String },
    /// No comparison was possible because the shadow declined to render.
    Declined { reason: String },
    /// No comparison was possible because the shadow did not answer.
    Unreachable { reason: String },
}

impl Outcome {
    /// Whether this outcome should fail a run.
    ///
    /// `Declined` and `Unreachable` do not fail here — a run of them is caught
    /// by the report's degeneracy check instead, which is the only place that
    /// can see it is the *whole* run rather than one request.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Different { .. })
    }
}

/// Where a declaration applies.
///
/// `path` is either an exact path or a prefix ending in `*`; `kind` is the
/// difference it accounts for. Both must match — a declaration written for a
/// missing image must not silently absorb a text difference on the same page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// An exact path (`/0291df856c77`) or a prefix (`/@miro/*`).
    pub path: String,
    pub kind: DiffKind,
    /// Why this difference is expected and accepted. Required: an unexplained
    /// declaration is indistinguishable from a suppressed bug.
    pub reason: String,
}

impl Declaration {
    /// Whether this declaration covers a difference at `path` of `kind`.
    fn covers(&self, path: &str, kind: DiffKind) -> bool {
        self.kind == kind
            && match self.path.strip_suffix('*') {
                Some(prefix) => path.starts_with(prefix),
                None => self.path == path,
            }
    }
}

/// The declarations file, and the lookup against it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declarations {
    #[serde(default)]
    pub declarations: Vec<Declaration>,
}

impl Declarations {
    pub fn new(declarations: Vec<Declaration>) -> Self {
        Self { declarations }
    }

    /// Parses the JSON a declarations file holds.
    ///
    /// `deny_unknown_fields` on both this and [`Declaration`] means a typo'd key
    /// is an error rather than an ignored line: a declarations file that
    /// silently stopped applying would turn every declared difference back into
    /// a failure, and the failure would be reported against the *page* rather
    /// than against the file that broke.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    pub fn all(&self) -> &[Declaration] {
        &self.declarations
    }

    /// The declaration covering this difference, if there is one. The first
    /// match wins, so ordering in the file is meaningful.
    pub fn find(&self, path: &str, kind: DiffKind) -> Option<&Declaration> {
        self.declarations
            .iter()
            .find(|declaration| declaration.covers(path, kind))
    }

    /// Whether the file declares nothing, so the caller does not have to care
    /// about the difference between "no declarations" and "empty file".
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }

    /// Turns a difference into an outcome, declared or not.
    fn judge(&self, path: &str, difference: Difference) -> Outcome {
        match self.find(path, difference.kind) {
            Some(declaration) => Outcome::Declared {
                path: declaration.path.clone(),
                kind: declaration.kind,
                reason: declaration.reason.clone(),
                detail: difference.detail(),
            },
            None => Outcome::Different {
                detail: difference.detail(),
            },
        }
    }
}

/// Compares the primary's answer against the shadow's.
///
/// `path` is the request path, which is what a declaration is matched against —
/// not the URL, which carries a query the same page is served under.
pub fn compare(
    primary: &Served,
    shadow: &Answer,
    declarations: &Declarations,
    path: &str,
) -> Outcome {
    let served = match shadow {
        Answer::Declined { reason } => {
            return Outcome::Declined {
                reason: reason.clone(),
            };
        }
        Answer::Unreachable { reason } => {
            return Outcome::Unreachable {
                reason: reason.clone(),
            };
        }
        Answer::Served(served) => served,
    };

    // Order matters. The status is compared first because it is the one thing
    // that holds for every response: a 404 against a 200 is a difference
    // whether or not either body could be canonicalised.
    if primary.status != served.status {
        let difference = Difference {
            index: 0,
            kind: DiffKind::Status,
            primary: Some(primary.status.to_string()),
            shadow: Some(served.status.to_string()),
        };
        return declarations.judge(path, difference);
    }

    // Equal statuses, and nothing canonicalisable to compare: a redirect, a
    // JSON body, a response with no `Content-Type`. See the module docs for
    // what this does not catch.
    if !primary.is_html() || !served.is_html() {
        return Outcome::StatusOnly;
    }

    // Equal non-success statuses are as far as this can go — `error.rs` picks
    // the message at random, so the bodies differ by construction.
    if !(200..300).contains(&primary.status) {
        return Outcome::StatusOnly;
    }

    match difference(&canonicalize(&primary.body), &canonicalize(&served.body)) {
        None => Outcome::Identical,
        Some(difference) => declarations.judge(path, difference),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(status: u16, body: &str) -> Served {
        Served::new(status, Some("text/html; charset=utf-8"), body)
    }

    fn shadow(body: &str) -> Answer {
        Answer::Served(html(200, body))
    }

    fn none() -> Declarations {
        Declarations::default()
    }

    /// The ordinary case, and the one the whole phase is aiming at.
    #[test]
    fn the_same_page_is_identical() {
        let page = "<div class=\"mt-7\"><p>Hello</p></div>";
        assert_eq!(
            compare(&html(200, page), &shadow(page), &none(), "/p"),
            Outcome::Identical
        );
    }

    /// The property inherited from the canonical form, asserted through this
    /// layer so a future change to `compare` cannot quietly bypass it: the
    /// legacy splices a link into siblings, the IR nests it, and both render
    /// the same.
    #[test]
    fn sibling_and_nested_markup_are_identical() {
        let legacy = r#"<a rel="r" href="h">ab</a><strong><a rel="r" href="h">cde</a></strong>"#;
        let rust = r#"<a rel="r" href="h">ab<strong>cde</strong></a>"#;
        assert_eq!(
            compare(&html(200, legacy), &shadow(rust), &none(), "/p"),
            Outcome::Identical
        );
    }

    /// A real difference, undeclared, is what fails the gate.
    #[test]
    fn a_different_class_is_a_difference() {
        let outcome = compare(
            &html(200, "<p class=\"mt-3\">x</p>"),
            &shadow("<p class=\"mt-7\">x</p>"),
            &none(),
            "/p",
        );
        let Outcome::Different { detail } = outcome else {
            panic!("expected a difference, got {outcome:?}");
        };
        assert!(detail.contains("mt-3"), "{detail}");
        assert!(detail.contains("mt-7"), "{detail}");
    }

    /// A status difference is a difference even when neither body is a page —
    /// this is the case a body-only comparison would miss entirely.
    #[test]
    fn a_status_difference_is_caught_even_without_html() {
        let primary = Served::new(200, Some("application/json"), "{}");
        let shadow = Answer::Served(Served::new(404, Some("application/json"), "{}"));
        let outcome = compare(&primary, &shadow, &none(), "/p");
        assert!(matches!(outcome, Outcome::Different { .. }), "{outcome:?}");
    }

    /// Two 404s are not compared by body: `error.rs` picks the message at
    /// random and the transponder code is random, so equal bodies are not
    /// something to expect — or to require.
    #[test]
    fn two_error_pages_compare_on_status_alone() {
        let primary = html(404, "<p>one of fifteen messages, code 8812</p>");
        let shadow = Answer::Served(html(404, "<p>a different message, code 4417</p>"));
        assert_eq!(
            compare(&primary, &shadow, &none(), "/p"),
            Outcome::StatusOnly
        );
    }

    /// A 200 against a 404 is a difference, not a `StatusOnly` — the check
    /// that runs before the error-page shortcut.
    #[test]
    fn a_success_against_an_error_is_a_difference() {
        let outcome = compare(
            &html(200, "<p>x</p>"),
            &Answer::Served(html(404, "<p>y</p>")),
            &none(),
            "/p",
        );
        assert!(matches!(outcome, Outcome::Different { .. }), "{outcome:?}");
    }

    /// Non-HTML with matching statuses is status-only, and deliberately does
    /// not compare bytes — see the module docs for what that gives up.
    #[test]
    fn non_html_is_status_only() {
        let primary = Served::new(200, Some("image/png"), "aaa");
        let shadow = Answer::Served(Served::new(200, Some("image/png"), "bbb"));
        assert_eq!(
            compare(&primary, &shadow, &none(), "/p"),
            Outcome::StatusOnly
        );
    }

    /// A response with no `Content-Type` is not a page either, and must not
    /// panic its way to a canonicalisation.
    #[test]
    fn a_missing_content_type_is_not_html() {
        let primary = Served::new(200, None::<String>, "hello");
        let shadow = Answer::Served(Served::new(200, None::<String>, "hello"));
        assert_eq!(
            compare(&primary, &shadow, &none(), "/p"),
            Outcome::StatusOnly
        );
    }

    /// The shadow declining is a decision, not a failure — the interlock that
    /// keeps the Rust instance off the WARP exit.
    #[test]
    fn a_declined_shadow_is_not_a_difference() {
        let answer = Answer::Declined {
            reason: "SHADOW_MODE: no fetch on a cache miss".to_string(),
        };
        let outcome = compare(&html(200, "<p>x</p>"), &answer, &none(), "/p");
        assert_eq!(
            outcome,
            Outcome::Declined {
                reason: "SHADOW_MODE: no fetch on a cache miss".to_string()
            }
        );
        assert!(!outcome.is_failure());
    }

    /// An unreachable shadow is kept apart from a declining one, because a run
    /// of these must not read as a clean gate.
    #[test]
    fn an_unreachable_shadow_is_distinct_from_a_declining_one() {
        let outcome = compare(
            &html(200, "<p>x</p>"),
            &Answer::Unreachable {
                reason: "connection refused".to_string(),
            },
            &none(),
            "/p",
        );
        assert!(
            matches!(outcome, Outcome::Unreachable { .. }),
            "{outcome:?}"
        );
        assert!(
            !outcome.is_failure(),
            "the report's degeneracy check owns this one"
        );
    }

    /// The eviction path: a declaration turns a difference into a pass, and
    /// says which declaration did it so the report can find the ones that never
    /// fired.
    #[test]
    fn a_declaration_accounts_for_a_difference() {
        let declarations = Declarations::new(vec![Declaration {
            path: "/known-diff".to_string(),
            kind: DiffKind::TextOrMarkup,
            reason: "the legacy drops a trailing space here".to_string(),
        }]);

        let outcome = compare(
            &html(200, "<p class=\"mt-3\">x</p>"),
            &shadow("<p class=\"mt-7\">x</p>"),
            &declarations,
            "/known-diff",
        );
        let Outcome::Declared {
            path,
            kind,
            reason,
            detail,
        } = outcome.clone()
        else {
            panic!("expected a declared difference, got {outcome:?}");
        };
        assert_eq!(path, "/known-diff");
        assert_eq!(kind, DiffKind::TextOrMarkup);
        assert_eq!(reason, "the legacy drops a trailing space here");
        assert!(!outcome.is_failure());

        // The difference itself survives being declared. Without this, a
        // soak's log could only say "the allowance fired N times", which is
        // the same thing it would say if the allowance had started excusing a
        // different bug.
        assert!(detail.contains("mt-3"), "{detail}");
        assert!(detail.contains("mt-7"), "{detail}");

        // And it is the *same* text an undeclared difference would have
        // carried, so a declaration changes the verdict and nothing else.
        let undeclared = compare(
            &html(200, "<p class=\"mt-3\">x</p>"),
            &shadow("<p class=\"mt-7\">x</p>"),
            &none(),
            "/known-diff",
        );
        assert_eq!(Outcome::Different { detail }, undeclared);
    }

    /// A declaration is scoped to its path. The same difference elsewhere is
    /// still a failure — otherwise one declaration would silence the gate for
    /// every page.
    #[test]
    fn a_declaration_does_not_apply_to_another_path() {
        let declarations = Declarations::new(vec![Declaration {
            path: "/known-diff".to_string(),
            kind: DiffKind::TextOrMarkup,
            reason: "…".to_string(),
        }]);
        let outcome = compare(
            &html(200, "<p class=\"mt-3\">x</p>"),
            &shadow("<p class=\"mt-7\">x</p>"),
            &declarations,
            "/another",
        );
        assert!(matches!(outcome, Outcome::Different { .. }), "{outcome:?}");
    }

    /// Nor to another kind. A declaration written for a missing image must not
    /// absorb a text difference on the same page.
    #[test]
    fn a_declaration_does_not_apply_to_another_kind() {
        let declarations = Declarations::new(vec![Declaration {
            path: "/p".to_string(),
            kind: DiffKind::MissingInShadow,
            reason: "…".to_string(),
        }]);
        let outcome = compare(
            &html(200, "<p class=\"mt-3\">x</p>"),
            &shadow("<p class=\"mt-7\">x</p>"),
            &declarations,
            "/p",
        );
        assert!(matches!(outcome, Outcome::Different { .. }), "{outcome:?}");
    }

    /// A `*` suffix is a prefix, so one declaration can cover a family of
    /// paths.
    #[test]
    fn a_star_suffix_matches_a_prefix() {
        let declarations = Declarations::new(vec![Declaration {
            path: "/@someone/*".to_string(),
            kind: DiffKind::TextOrMarkup,
            reason: "…".to_string(),
        }]);
        assert!(
            declarations
                .find("/@someone/a-post", DiffKind::TextOrMarkup)
                .is_some()
        );
        // The prefix must include the separator: `/@someonelse` is not under
        // `/@someone/`.
        assert!(
            declarations
                .find("/@someonelse", DiffKind::TextOrMarkup)
                .is_none()
        );
        assert!(
            declarations
                .find("/@someone", DiffKind::TextOrMarkup)
                .is_none()
        );
    }

    /// The three ways a difference can be shaped, each classified so a
    /// declaration can name the one it accounts for.
    #[test]
    fn a_difference_is_classified_by_shape() {
        let both = difference(&canonicalize("<p>a</p>"), &canonicalize("<p>b</p>")).unwrap();
        assert_eq!(both.kind, DiffKind::TextOrMarkup);
        assert_eq!(both.index, 0);

        let dropped =
            difference(&canonicalize("<p>a</p><p>b</p>"), &canonicalize("<p>a</p>")).unwrap();
        assert_eq!(dropped.kind, DiffKind::MissingInShadow);
        assert_eq!(dropped.index, 1);

        let added =
            difference(&canonicalize("<p>a</p>"), &canonicalize("<p>a</p><p>b</p>")).unwrap();
        assert_eq!(added.kind, DiffKind::MissingInPrimary);
        assert_eq!(added.index, 1);
    }

    /// Whitespace-only text nodes are not a difference — the templates are
    /// multi-line literals, so their indentation is in the DOM but not in the
    /// output.
    #[test]
    fn template_indentation_is_not_a_difference() {
        let primary = html(200, "\n    <div>x</div>\n  ");
        assert_eq!(
            compare(&primary, &shadow("<div>x</div>"), &none(), "/p"),
            Outcome::Identical
        );
    }

    /// An image is compared even though it carries no text, which is the
    /// property `canonical`'s marker nodes exist for — asserted here because a
    /// `compare` that short-circuited empty pages would lose it.
    #[test]
    fn a_wrong_image_source_is_a_difference() {
        let outcome = compare(
            &html(200, r#"<img src="https://a/1.png">"#),
            &shadow(r#"<img src="https://b/2.png">"#),
            &none(),
            "/p",
        );
        assert!(matches!(outcome, Outcome::Different { .. }), "{outcome:?}");
    }

    /// `Content-Type` parameters and case are noise; the media type is not.
    #[test]
    fn content_type_is_normalised_before_comparing() {
        let primary = Served::new(200, Some("TEXT/HTML; charset=UTF-8"), "<p>x</p>");
        let shadow = Answer::Served(Served::new(200, Some("text/html"), "<p>x</p>"));
        assert_eq!(
            compare(&primary, &shadow, &none(), "/p"),
            Outcome::Identical
        );
    }

    /// A typo in a declarations file must be an error, not a silently ignored
    /// line: a file that stopped applying would turn every declared difference
    /// back into a failure, blamed on the page rather than on the file.
    #[test]
    fn a_misspelled_declaration_field_is_an_error() {
        let json = r#"{"declarations":[{"path":"/p","kind":"text-or-markup","reasons":"typo"}]}"#;
        assert!(Declarations::from_json(json).is_err());

        let good = r#"{"declarations":[{"path":"/p","kind":"text-or-markup","reason":"why"}]}"#;
        let parsed = Declarations::from_json(good).expect("the correct spelling parses");
        assert_eq!(parsed.all().len(), 1);
        assert_eq!(parsed.all()[0].kind, DiffKind::TextOrMarkup);
    }

    /// An absent or empty declarations file is the normal starting state, and
    /// every difference is then a failure.
    #[test]
    fn no_declarations_means_every_difference_fails() {
        assert!(none().is_empty());
        assert!(Declarations::from_json("{}").unwrap().is_empty());
    }

    /// The kinds are spelled in the file the way they are spelled here, so a
    /// declaration and a report agree.
    #[test]
    fn diff_kinds_round_trip_through_json() {
        for kind in [
            DiffKind::Status,
            DiffKind::TextOrMarkup,
            DiffKind::MissingInShadow,
            DiffKind::MissingInPrimary,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<DiffKind>(&json).unwrap(), kind);
        }
        assert_eq!(
            serde_json::to_string(&DiffKind::MissingInShadow).unwrap(),
            "\"missing-in-shadow\""
        );
    }
}
