//! Error pages — `utils/error.py` and `utils/exceptions.py`.
//!
//! Two entry points, matching the legacy's two:
//!
//! - [`html_error`] is `generate_error`: pick a message, notify, render
//!   `error.html` inside `base.html`, answer with a real status code.
//! - [`handler_error`] is `handle_exception`: log, then the same thing.
//!
//! # The correlation is passed in, not looked up
//!
//! The legacy keeps the current request's id and transponder code in
//! `ContextVar`s (`server/__init__.py:78-81`) so that any function on the call
//! stack can read them without threading them through. Those are a shared
//! mutable global keyed by task, which is a shape axum has no direct equivalent
//! for; the closest is a request extension.
//!
//! Rather than reproduce a global, the code is a plain parameter
//! ([`Correlation`]). It is set once per request by
//! [`crate::middleware::correlation`], which is also the only place that can
//! render an error page *outside* a handler — the timeout path — and there it
//! already has the value in hand.
//!
//! # The status code is the *exception's*, not the template's
//!
//! `generate_error(error_msg, title, status_code)` defaults to `500`, and each
//! caller in `handlers/post.py` passes its own: 404 for a URL that is not a
//! Medium article, 500 for an id that cannot be resolved. The HTML is identical
//! across all of them — only the status line differs — which is why the gate has
//! to compare the status separately from the body.
//!
//! # `quiet` suppresses the alert, never the page
//!
//! `utils/error.py:39-40` skips the Telegram message for `NotValidMediumURL`
//! (`handlers/post.py:87`) because a mistyped URL is routine traffic, not an
//! incident. The page is still rendered with the same 404.
//!
//! # A shadow instance never alerts, and that is wider than it needs to be
//!
//! Fase 4's `SHADOW_MODE` suppresses *every* alert, not only the ones this
//! instance causes. The narrow version — silence the declined fetch, keep the
//! rest — was the first cut, and it is wrong for the soak: the shadow sees a
//! copy of production's traffic, so an error production already alerted on
//! arrives here too and would be alerted a second time. Over §5's seven days,
//! every real incident becomes two Telegram messages and none of them tells an
//! operator anything the first did not.
//!
//! So the rule is the one a shadow should have had from the start: a process
//! that must not have consequences does not page anyone. [`should_alert`] is
//! that rule, as a pure function — the notifier cannot be the test instrument,
//! because [`crate::notify`] drops messages when `TELEGRAM_BOT_TOKEN` is unset
//! and so cannot observe a decision either way.
//!
//! # The not-comparable marker
//!
//! A shadow that declines to render still has to answer with *something*, and
//! whatever it is must not be compared against Python's — the whole point is
//! that there is no second opinion. [`html_error`] therefore stamps
//! [`SHADOW_HEADER`] on any response built from a [`PageError::declined`], and
//! the edge reads that header instead of guessing. Guessing from the status
//! would not work: Python answers a real 404 on this same route, and telling
//! "I declined" from "I agree it is missing" has to be exact.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use medium_render::page::{DEFAULT_ERROR_TITLE, PageConfig, render_error};

use crate::middleware::Correlation;
use crate::state::AppState;

/// `utils/error.py:32`.
pub const DEFAULT_STATUS: u16 = 500;

/// The header a shadow instance stamps on a response it declined to render.
///
/// Lowercase, as HTTP/2 requires and as `HeaderName::from_static` asserts.
///
/// # This is one half of a two-process constant, and a deliberate duplicate
///
/// The reader is `edge/freedium-edge/src/shadow.rs`, which takes the name from
/// `page_canonical::record::SHADOW_HEADER` — the edge already depends on that
/// crate for the comparator and the log format, so for the edge there is one
/// definition and no duplication at all.
///
/// The duplicate is here, on the *writer*, because this crate deliberately does
/// not depend on `page-canonical`: that crate parses HTML, and a crate that only
/// ever emits pages should not carry a parser. The same trade is recorded for
/// `config.rs::boolean` and for the edge's copy of it — two workspaces, one
/// nine-line function, a shared crate for it would exist only because both have
/// a `.env`.
///
/// What holds the pair together is [`the_marker_matches_the_shared_protocol`]
/// below plus the identical assertion in `page-canonical/src/record.rs`. A
/// rename on one side is a failing test on that side, and each test names the
/// other file. The failure mode if both were changed carelessly is loud rather
/// than silent: the edge would stop recognising declines and every uncached post
/// would become a `Different`, which is a report nobody could mistake for a
/// clean run.
///
/// [`the_marker_matches_the_shared_protocol`]: self::tests::the_marker_matches_the_shared_protocol
pub const SHADOW_HEADER: &str = "x-freedium-shadow";

/// The one reason token there is: the durable cache missed and this instance is
/// not allowed to fetch.
///
/// The value is a token rather than free text because it is counted and grouped
/// by `difftest shadow-report`, and because a reason assembled per request would
/// be one more thing that can differ between two runs of the same request. If a
/// second reason appears it becomes a second constant, and the report shows the
/// split rather than needing to guess at prose.
///
/// Duplicated the same way [`SHADOW_HEADER`] is, and pinned by the same test.
pub const SHADOW_NO_FETCH: &str = "no-fetch";

/// The status a shadow answers with when it declines.
///
/// Deliberately neither Python's 200 (there is no page) nor its 404 (the post
/// is not missing — this instance just will not go and get it), so that a curl
/// against a shadow does not mislead a human. Nothing in the comparison depends
/// on this value: the edge keys on [`SHADOW_HEADER`], because it has to tell
/// "I declined" from "I agree with Python that this is a 404" exactly, and no
/// status code can carry that.
pub const DECLINED_STATUS: u16 = 503;

/// The body a declined response carries, in place of the random error message.
///
/// Fixed, unlike a real error's: two declined responses must be identical to
/// each other, or "these are the same non-answer" stops being checkable.
pub const DECLINED_MESSAGE: &str = "This post is not in the cache, and this instance does not fetch. It is not a shadow comparison candidate.";

/// The title for the same. `generate_error`'s default is a sad face; this one
/// says which instance answered.
pub const DECLINED_TITLE: &str = "Shadow instance: no fetch";

/// A page-level error, as the handlers describe it.
///
/// The message is a *whole* sentence for the error body, not a status line —
/// see `handlers/post.py:73-89` for the four the post route uses.
#[derive(Debug, Clone)]
pub struct PageError {
    /// `generate_error`'s `error_msg`. `None` means "pick one from
    /// [`ERROR_MSG_LIST`](medium_render::page::ERROR_MSG_LIST)" — the legacy
    /// does that choice inside the function, which makes it unassertable, so
    /// here it is explicit and only [`PageError::unspecified`] takes it.
    pub message: Option<String>,

    /// `generate_error`'s `title`; `"Opppps.."` when absent.
    pub title: Option<String>,

    pub status: u16,

    /// `quiet=True` stops the Telegram alert but not the page.
    pub quiet: bool,

    /// Set when this "error" is really a shadow instance declining to render,
    /// carrying the reason token that goes into [`SHADOW_HEADER`].
    ///
    /// `None` — the ordinary case, and every case on a non-shadow instance —
    /// means the response is a real answer, comparable like any other.
    pub declined: Option<&'static str>,
}

impl PageError {
    /// The generic case: `generate_error()` with every default.
    pub fn unspecified() -> Self {
        Self {
            message: None,
            title: None,
            status: DEFAULT_STATUS,
            quiet: false,
            declined: None,
        }
    }

    /// A message with a status, which is what every call site in
    /// `handlers/post.py` actually wants.
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: Some(message.into()),
            title: None,
            status,
            quiet: false,
            declined: None,
        }
    }

    /// The shadow instance's answer when it will not render this request.
    ///
    /// Not an error the legacy has, and not one it can have: it only ever exists
    /// on an instance whose [`Config::shadow_mode`](crate::config::Config::shadow_mode)
    /// is on. The message is fixed rather than drawn from the random list, so
    /// that the body of a declined response is the same on every request — a
    /// human comparing two of them should see no difference, and a test can
    /// assert on it.
    ///
    /// `quiet` is left false on purpose: the alert decision belongs to
    /// [`should_alert`], and having two mechanisms able to suppress a message
    /// would mean a test could pass while the real path still paged someone.
    pub fn declined(reason: &'static str) -> Self {
        Self {
            message: Some(DECLINED_MESSAGE.to_string()),
            title: Some(DECLINED_TITLE.to_string()),
            status: DECLINED_STATUS,
            quiet: false,
            declined: Some(reason),
        }
    }

    #[must_use]
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    /// The message to render, resolving the random default.
    ///
    /// Extracted so a test — and the parity gate — can pin a message instead of
    /// accepting whichever of the fifteen came up.
    pub fn resolved_message(&self) -> &str {
        match &self.message {
            Some(message) => message,
            None => random_message(),
        }
    }

    pub fn resolved_title(&self) -> &str {
        self.title.as_deref().unwrap_or(DEFAULT_ERROR_TITLE)
    }
}

/// One of `ERROR_MSG_LIST`, chosen at random.
///
/// `medium-render` owns the list so the handler and the timeout layer cannot
/// drift apart; the choice of index lives here because it is the part that must
/// not be shared with the gate.
pub fn random_message() -> &'static str {
    let list = medium_render::page::ERROR_MSG_LIST;
    list[crate::transponder::random_index(list.len())]
}

/// Whether this error should reach Telegram.
///
/// Pure and total, so the rule can be tested directly: [`crate::notify`] drops
/// messages when `TELEGRAM_BOT_TOKEN` is unset, so a test that watched the
/// notifier would see silence either way and prove nothing.
///
/// Two things silence an alert, and they are unrelated:
///
/// - `error.quiet` — the legacy's per-call exception, e.g. a mistyped URL
///   (`utils/error.py:39-40`).
/// - `shadow_mode` — this process must have no consequences. See the module
///   docs for why this is wider than the declined case alone.
///
/// `error.declined` is deliberately not a third condition. It is only ever set
/// on a shadow instance, so `shadow_mode` already covers it, and a rule that
/// listed both would be a rule with a redundant clause — the kind that hides
/// which one is actually load-bearing when someone later deletes "the spare".
pub fn should_alert(error: &PageError, shadow_mode: bool) -> bool {
    !error.quiet && !shadow_mode
}

/// `generate_error` (`utils/error.py:31-50`).
///
/// Renders the page and answers with `status`. The Telegram alert is sent first,
/// as in the legacy, and never affects the response.
pub async fn html_error(state: &AppState, correlation: &Correlation, error: PageError) -> Response {
    let message = error.resolved_message().to_string();
    let title = error.resolved_title().to_string();

    if should_alert(&error, state.config.shadow_mode) {
        // `utils/error.py:40`: the URL being processed and the transponder code,
        // both wrapped in `<code>` for Telegram's HTML parse mode.
        state
            .notifier
            .send(
                &format!(
                    "📛 Error while processing url: <code>{}</code>, transponder_code: <code>{}</code>, error: <code>{message}</code>",
                    correlation.url, correlation.code,
                ),
                false,
                crate::notify::MessageStatus::Error,
            )
            .await;
    } else if error.quiet {
        tracing::debug!(%message, "quiet error: not alerting");
    } else {
        tracing::debug!(%message, "shadow instance: not alerting");
    }

    let mut response = render_response(state, correlation, &message, &title, error.status);

    // Stamped even when rendering failed and `render_response` fell back to
    // plain text: the header's job is to tell the edge "do not compare this",
    // and a fallback body is exactly as uncomparable as a rendered one.
    if let Some(reason) = error.declined {
        response.headers_mut().insert(
            header::HeaderName::from_static(SHADOW_HEADER),
            HeaderValue::from_static(reason),
        );
    }

    response
}

/// Render the error page for an already-resolved message and status.
///
/// Split out so the timeout layer — which has a `Correlation` but no
/// `PageError` — can share it. A rendering failure falls back to a plain-text
/// body rather than panicking: a template that cannot render must not turn a 404
/// into a dropped connection.
pub fn render_response(
    state: &AppState,
    correlation: &Correlation,
    message: &str,
    title: &str,
    status: u16,
) -> Response {
    let config =
        PageConfig::new(&state.config.host_address).with_ads_header(state.config.enable_ads_banner);

    match render_error(
        state.templates(),
        message,
        &correlation.code.to_string(),
        title,
        &config,
    ) {
        Ok(html) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (
                status,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "could not render the error page");
            (StatusCode::INTERNAL_SERVER_ERROR, "An error occurred").into_response()
        }
    }
}

/// `handle_exception` (`utils/exceptions.py:8-14`).
///
/// Note what the legacy does *not* do: the `sentry_sdk.capture_exception` call is
/// commented out (`utils/exceptions.py:11-12`), so a handled exception only ever
/// reached the log and the error page. Same here, which is why this takes the
/// error as `impl std::fmt::Debug` and does nothing with it but log.
pub async fn handler_error(
    state: &AppState,
    correlation: &Correlation,
    error: impl std::fmt::Debug,
    error_msg: Option<&str>,
    status: u16,
    quiet: bool,
) -> Response {
    tracing::error!(error = ?error, "handling an exception");

    let mut page = match error_msg {
        Some(message) => PageError::new(message, status),
        None => PageError::unspecified(),
    };
    page.quiet = quiet;

    html_error(state, correlation, page).await
}

/// A JSON error, for the endpoints that answer JSON.
///
/// Not in `utils/error.py`: this is the `JSONResponse` the two POST routes build
/// inline (`handlers/misc.py:22-37`).
pub fn json_error(message: String, status: u16) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::json!({ "message": message }).to_string();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("a JSON response with two known headers always builds")
}

/// The same, for the success case — `{"message": "OK"}` with a 200.
pub fn json_ok() -> Response {
    json_error("OK".to_string(), 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alert rule as a truth table over its two inputs.
    ///
    /// Tested here rather than through the notifier on purpose: `notify.rs`
    /// drops every message when `TELEGRAM_BOT_TOKEN` is unset, which it is in
    /// tests, so a test that watched for a message would see silence for both
    /// answers and pass whatever the rule said.
    #[test]
    fn only_a_real_error_on_a_real_instance_alerts() {
        let loud = PageError::new("boom", 500);
        let quiet = PageError::new("boom", 500).quiet();

        assert!(should_alert(&loud, false), "the ordinary case must alert");
        assert!(
            !should_alert(&quiet, false),
            "`quiet` silences it, as in the legacy"
        );
        assert!(
            !should_alert(&loud, true),
            "a shadow instance must not alert"
        );
        assert!(
            !should_alert(&quiet, true),
            "and the two silences compose rather than cancelling"
        );
    }

    /// A declined error is silenced by shadow mode, not by carrying `declined`.
    ///
    /// This is the clause that would be redundant if `should_alert` also listed
    /// `error.declined`, and the test states the consequence: the rule is
    /// total over *shadow mode*, so it cannot be made to alert by constructing a
    /// declined error on an instance that is not a shadow. That combination does
    /// not occur — the interlock only fires under shadow mode — and if it ever
    /// did, alerting would be the correct answer rather than a silent skip.
    #[test]
    fn a_declined_error_is_silenced_by_shadow_mode_alone() {
        let declined = PageError::declined(SHADOW_NO_FETCH);
        assert!(!declined.quiet, "the flag is not how this one is silenced");
        assert!(!should_alert(&declined, true));
        assert!(
            should_alert(&declined, false),
            "a combination that cannot arise"
        );
    }

    /// A declined error is not a real error: it has its own status and a fixed
    /// message, so two declined responses are identical to each other.
    #[test]
    fn a_declined_error_is_fixed_and_marked() {
        let declined = PageError::declined(SHADOW_NO_FETCH);
        assert_eq!(declined.status, DECLINED_STATUS);
        assert_eq!(declined.declined, Some(SHADOW_NO_FETCH));
        assert_eq!(declined.resolved_message(), DECLINED_MESSAGE);
        assert_ne!(
            declined.status, 404,
            "404 means Python agrees the post is missing"
        );
        assert_ne!(declined.status, 200, "there is no page here");
    }

    /// An ordinary error is not a decline. This is the half that keeps the edge
    /// from skipping every response: if `unspecified()` or `new()` ever set
    /// `declined`, every 404 in the traffic sample would be dropped from the
    /// comparison instead of compared.
    #[test]
    fn an_ordinary_error_is_not_declined() {
        assert_eq!(PageError::unspecified().declined, None);
        assert_eq!(PageError::new("x", 404).declined, None);
        assert_eq!(PageError::new("x", 404).quiet().declined, None);
    }

    fn correlation() -> Correlation {
        Correlation::new(
            "alpha-bravo-charlie".to_string(),
            12345,
            "https://freedium.cfd/0291df856c77".to_string(),
        )
    }

    /// The marker on the wire, which is the whole contract with the edge.
    ///
    /// `SHADOW_HEADER` is used through `from_static`, which panics on an invalid
    /// header name — so this also proves the constant is one.
    #[tokio::test]
    async fn a_declined_response_carries_the_marker_header() {
        let state = crate::state::tests::offline_state();

        let response =
            html_error(&state, &correlation(), PageError::declined(SHADOW_NO_FETCH)).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(SHADOW_HEADER)
                .map(|value| value.to_str().unwrap()),
            Some(SHADOW_NO_FETCH),
            "the reason token travels, so the report can group by it"
        );
    }

    /// And the other half: a real error page has no marker, so the edge compares
    /// it rather than skipping it.
    #[tokio::test]
    async fn an_ordinary_error_page_has_no_marker() {
        let state = crate::state::tests::offline_state();

        let response = html_error(&state, &correlation(), PageError::new("nope", 404)).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get(SHADOW_HEADER).is_none());
    }

    /// The marker's exact spelling, pinned on the writer's side.
    ///
    /// The reader is the Pingora edge, which takes both strings from
    /// `page_canonical::record` — the two live in different cargo workspaces
    /// (RUST_REWRITE_PLAN §3.4), so no compiler can compare them, and this crate
    /// deliberately does not depend on `page-canonical` to keep an HTML parser
    /// out of the server. See [`SHADOW_HEADER`].
    ///
    /// So the pair is held together by this test and its twin in
    /// `crates/page-canonical/src/record.rs::the_marker_matches_the_shared_protocol`.
    /// **If you change a string here, change it there.** The assertion is
    /// written as a literal rather than as `assert_eq!(SHADOW_HEADER, SHADOW_HEADER)`
    /// so that reading this test tells you what the protocol is without opening
    /// two other files.
    #[test]
    fn the_marker_matches_the_shared_protocol() {
        assert_eq!(SHADOW_HEADER, "x-freedium-shadow");
        assert_eq!(SHADOW_NO_FETCH, "no-fetch");

        // And the name must be one `from_static` accepts — it is used that way on
        // every declined response, where a panic would be a dropped connection
        // rather than an error page.
        assert_eq!(
            header::HeaderName::from_static(SHADOW_HEADER).as_str(),
            SHADOW_HEADER
        );
        assert_eq!(
            header::HeaderValue::from_static(SHADOW_NO_FETCH)
                .to_str()
                .expect("the token is ASCII"),
            SHADOW_NO_FETCH
        );
    }

    #[test]
    fn the_defaults_are_generate_errors_defaults() {
        let error = PageError::unspecified();
        assert_eq!(error.status, DEFAULT_STATUS);
        assert_eq!(error.status, 500);
        assert_eq!(error.resolved_title(), "Opppps..");
        assert!(!error.quiet);
    }

    /// An explicit message always wins over the random list, and a random one is
    /// always a member of the list.
    #[test]
    fn the_message_defaults_to_a_member_of_the_list() {
        assert_eq!(
            PageError::new("specific", 404).resolved_message(),
            "specific"
        );

        for _ in 0..200 {
            // Bound, because `resolved_message` borrows the error it resolved
            // against even in the `None` arm, where the string is `'static`.
            let error = PageError::unspecified();
            let message = error.resolved_message();
            assert!(
                medium_render::page::ERROR_MSG_LIST.contains(&message),
                "{message:?} is not in ERROR_MSG_LIST"
            );
        }
    }

    /// The random choice must actually vary, or the fifteen-line list is a
    /// one-line list with extra steps.
    #[test]
    fn the_random_message_varies() {
        let seen: std::collections::HashSet<&str> = (0..500).map(|_| random_message()).collect();
        assert!(
            seen.len() > 3,
            "only {} distinct messages in 500 draws",
            seen.len()
        );
    }

    #[test]
    fn json_error_carries_the_message_and_status() {
        let response = json_error("Wrong secret key: x".to_string(), 403);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn json_ok_is_a_200() {
        assert_eq!(json_ok().status(), StatusCode::OK);
    }
}
