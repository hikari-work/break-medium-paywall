//! The shadow half: ask the Rust instance, compare, write one line.
//!
//! # Everything here happens after the client has been served
//!
//! [`Shadow::spawn`] is called from `logging`, which Pingora runs once the
//! response is on its way back. Nothing in this module can change what the client
//! receives, and every failure in it — a refused connection, a timeout, a
//! comparison task that panics — is logged and dropped. That is the design, not a
//! fallback: an edge that fails a live request because its *shadow* was
//! unreachable would be worse than having no shadow at all.
//!
//! # What "the comparison" costs, and where it runs
//!
//! [`page_canonical::compare`] canonicalises two pages of HTML, which is a full
//! parse each — CPU-bound, tens of milliseconds, no I/O. Run inline in the
//! spawned task it would still be occupying a Pingora worker thread, and the
//! workers are the same threads serving clients. So it goes to
//! [`tokio::task::spawn_blocking`], where the runtime can treat it as what it is.
//!
//! # Declines are told apart from real answers by the header, never by status
//!
//! A shadow instance on a cache miss answers `503` **and** carries
//! [`SHADOW_HEADER`]. Python, on the same request, may answer a real `404`. A
//! status alone cannot separate "I chose not to render this" from "I agree with
//! you that it is gone", which is why the marker exists — see
//! `crates/freedium-web/src/error.rs`. Reading it first, before the body, also
//! means a decline costs no body read.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use page_canonical::{
    Answer, Declarations, SHADOW_HEADER, SHADOW_NO_FETCH, Served, ShadowRecord, Side, compare,
};

use crate::config::Config;

/// The `Content-Type` header, raw.
///
/// Deliberately *not* interpreted here. [`Served::new`] normalises it — strips
/// the parameters, lowercases, trims — and one opinion about what `text/html`
/// means is the reason `page-canonical` is a crate. A second normaliser on this
/// side is how the edge and the render gate would come to disagree about whether
/// a page is a page.
fn content_type_of(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()
        .map(str::to_string)
}

/// Why the shadow did not answer, as a short token plus the detail.
///
/// The token leads so `shadow-report` can group by it, and the full error
/// follows because the alternative is an operator reading "unreachable" and
/// having to guess whether that was a refused socket, a timeout, or a shadow
/// that is up and answering nonsense.
fn unreachable_reason(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_redirect() {
        "redirect"
    } else {
        "request"
    };
    format!("{kind}: {error}")
}

/// A primary response the edge has finished proxying, ready to be compared.
///
/// Assembled in `logging` and moved into the spawned task. Owned strings
/// throughout: the request context is Pingora's and does not outlive the hook.
#[derive(Debug)]
pub struct Pending {
    pub path: String,
    pub query: Option<String>,

    /// The client's request headers, minus the hop-by-hop ones. Replayed onto the
    /// shadow request so that anything the app varies on — a cookie, an
    /// `Accept-Language`, a `X-Forwarded-For` — is the same on both sides. A
    /// header that reached only the primary would make the two answers differ for
    /// a reason that has nothing to do with rendering.
    pub headers: Vec<(String, String)>,

    /// What the primary answered, body and all.
    pub primary: Served,

    /// Status, time and size of the primary, as recorded.
    pub primary_side: Side,
}

/// The shadow instance, and the log it writes to.
pub struct Shadow {
    client: reqwest::Client,
    /// `host:port` of the Rust instance, with `SHADOW_MODE=true`.
    upstream: String,
    declarations: Arc<Declarations>,
    /// Append-only and shared between every comparison task, hence the mutex. A
    /// lock held for one `write` is not a contention point; the alternative, one
    /// `File` per task, would be an `open` per request.
    log: Mutex<std::fs::File>,
    log_path: String,
}

impl Shadow {
    /// Build the client and open the log.
    ///
    /// An `Err` here is a boot failure at the call site. An edge configured to
    /// shadow but unable to record would silently produce no evidence, and §5's
    /// seven-day counter reads no evidence as seven clean days — so this is one
    /// of the few things worth refusing to start over.
    pub fn new(config: &Config, declarations: Declarations) -> std::io::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.shadow_timeout_ms))
            // The shadow is compared against the primary byte for byte, so the
            // client must not follow anywhere the server did not intend to send
            // it. The app's own redirects are the comparison's business.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(std::io::Error::other)?;

        let log_path = config.shadow_log.display().to_string();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config.shadow_log)?;

        Ok(Self {
            client,
            upstream: config.shadow_upstream.clone(),
            declarations: Arc::new(declarations),
            log: Mutex::new(file),
            log_path,
        })
    }

    /// Compare `pending` in the background and return immediately.
    ///
    /// Takes `self: &Arc<Self>` because the task outlives the hook call; the
    /// clone is the only thing keeping the `Shadow` alive if the process is
    /// shutting down, which is fine — a comparison in flight when the edge stops
    /// is one line of evidence lost, not a hang.
    pub fn spawn(self: &Arc<Self>, pending: Pending) {
        let shadow = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = shadow.compare_and_log(pending).await {
                tracing::error!(%error, "the shadow comparison could not be recorded");
            }
        });
    }

    /// The whole of the post-response work, as one fallible unit.
    async fn compare_and_log(&self, pending: Pending) -> std::io::Result<()> {
        let started = Instant::now();
        let answer = self.ask(&pending).await;
        let shadow_ms = elapsed_ms(started);

        // The shadow's own measurement, present exactly when it answered with a
        // body. `Declined` and `Unreachable` produced no answer to time, and
        // recording a duration for one would be a number the report would print
        // as if it meant something. Computed before `answer` is moved.
        let shadow_side = match &answer {
            Answer::Served(served) => Some(Side {
                status: served.status,
                ms: shadow_ms,
                bytes: served.body.len(),
            }),
            Answer::Declined { .. } | Answer::Unreachable { .. } => None,
        };

        // Parse-and-diff, off the worker threads — see the module docs.
        //
        // The closure takes the whole of what is left of `pending`, so the
        // record is built from inside it and nothing has to outlive the task.
        // Assembling the record out here instead would mean cloning the path
        // (a `String`) purely to satisfy the `'static` bound, and would put the
        // comparison's inputs in two places.
        let declarations = Arc::clone(&self.declarations);
        let record = tokio::task::spawn_blocking(move || {
            let outcome = compare(&pending.primary, &answer, &declarations, &pending.path);
            ShadowRecord::record(
                now_ms(),
                &pending.path,
                pending.query.as_deref(),
                pending.primary_side,
                shadow_side,
                &outcome,
            )
        })
        .await
        // A panicking comparison — a bug in the canonicaliser, most likely —
        // must not take the edge down with it. The task is gone and the request
        // it belonged to was served long ago; the record is simply missing, and
        // the report's degeneracy check is what notices a run of those.
        .map_err(std::io::Error::other)?;

        // A difference is the one thing an operator watching the soak needs to
        // see as it happens, rather than by tailing a file — a seven-day gate
        // that fails should fail loudly on the request that broke it.
        if record.outcome.is_difference() {
            tracing::warn!(
                path = %record.path,
                outcome = ?record.outcome,
                detail = record.detail.as_deref().unwrap_or(""),
                reason = record.reason.as_deref().unwrap_or(""),
                "shadow difference"
            );
        } else {
            tracing::debug!(
                path = %record.path,
                outcome = ?record.outcome,
                "shadow compared"
            );
        }

        self.write(&record)
    }

    /// Ask the shadow instance the same question the client asked.
    ///
    /// The URL is built from the path as it arrived, not from a configured base
    /// — the two servers are behind the same route set, and the primary's own
    /// location handling is part of what is being compared.
    async fn ask(&self, pending: &Pending) -> Answer {
        let mut url = format!("http://{}{}", self.upstream, pending.path);
        if let Some(query) = &pending.query {
            url.push('?');
            url.push_str(query);
        }

        let mut request = self.client.get(&url);
        for (name, value) in &pending.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        // Whatever the client asked for, the comparison needs bytes it can
        // canonicalise. The primary's encoding was checked before this ran — see
        // `eligibility::encoding_allows_comparison` — so both sides are plain.
        request = request.header(reqwest::header::ACCEPT_ENCODING, "identity");

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return Answer::Unreachable {
                    reason: unreachable_reason(&error),
                };
            }
        };

        let status = response.status().as_u16();
        let content_type = content_type_of(response.headers());

        // Before the body, and that ordering is the point: a decline is a fact
        // about the header, and reading a body the edge is going to discard would
        // be work done on a hot path for nothing.
        if let Some(marker) = response.headers().get(SHADOW_HEADER) {
            let reason = marker
                .to_str()
                .unwrap_or(SHADOW_NO_FETCH)
                .trim()
                .to_string();
            return Answer::Declined { reason };
        }

        match response.bytes().await {
            Ok(body) => Answer::Served(Served::new(
                status,
                content_type,
                String::from_utf8_lossy(&body).into_owned(),
            )),
            // The status and headers arrived and the body did not. Recorded as
            // unreachable rather than served: a `Served` with a truncated body
            // would compare as a difference and read as a renderer bug, which is
            // exactly the mis-conclusion this exists to avoid.
            Err(error) => Answer::Unreachable {
                reason: unreachable_reason(&error),
            },
        }
    }

    /// One JSONL line, flushed.
    ///
    /// Flushed per line on purpose. A soak's evidence is worthless if the last
    /// uploads are still in a buffer when the process is replaced — and the
    /// comparison has already cost a second HTTP request and an HTML parse, so a
    /// write syscall is not the expensive part.
    fn write(&self, record: &ShadowRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');

        let mut file = self
            .log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(line.as_bytes())?;
        file.flush()
    }

    /// Record a request the comparison did not produce — an exclusion or a
    /// primary failure.
    ///
    /// A failed write is logged rather than propagated: this is called from
    /// `logging`, where there is no longer a client waiting and no way to fail a
    /// request that has already been answered. Losing a line is worse than losing
    /// the response, which is the trade the whole module is built on.
    pub fn record(&self, record: &ShadowRecord) {
        if let Err(error) = self.write(record) {
            tracing::error!(
                %error,
                path = %record.path,
                log = %self.log_path,
                "the shadow log could not be written"
            );
        }
    }

    /// Where the records are going, for the boot log.
    pub fn log_path(&self) -> &str {
        &self.log_path
    }
}

/// Unix milliseconds.
///
/// A `u64` rather than a formatted timestamp: the formatting belongs where the
/// report is read, and two processes agreeing on a date format is one more thing
/// that can differ.
///
/// `pub(crate)` because `proxy.rs` needs it for the records it builds itself —
/// exclusions and primary failures, which never reach a comparison task.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Whether the declarations file is readable, so the edge can fail at boot
/// rather than at the first declined request.
pub fn load_declarations(path: Option<&Path>) -> std::io::Result<Declarations> {
    let Some(path) = path else {
        return Ok(Declarations::default());
    };
    let text = std::fs::read_to_string(path)?;
    Declarations::from_json(&text).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use page_canonical::{Declarations, Outcome};
    use reqwest::header::{HeaderMap, HeaderValue as ReqwestValue};

    /// The header is passed through raw, and `Served::new` is what interprets it.
    ///
    /// This is deliberately a weak-looking test. The strong version — "the
    /// normalised type is `text/html`" — belongs in `page-canonical`'s
    /// `content_type_is_normalised_before_comparing`, and asserting it here too
    /// would be a second copy of the rule that the shared crate exists to hold
    /// once. What this pins is the boundary: the edge hands over the upstream's
    /// bytes unmodified, parameters and all, and does not form its own opinion.
    #[test]
    fn the_content_type_is_passed_through_unnormalised() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            ReqwestValue::from_static("text/html; charset=utf-8"),
        );
        assert_eq!(
            content_type_of(&headers).as_deref(),
            Some("text/html; charset=utf-8")
        );

        assert_eq!(content_type_of(&HeaderMap::new()), None);
    }

    /// An unreachable reason leads with a grouping token, because the report
    /// counts by it.
    ///
    /// Exercises the real classification: a loopback port with nothing listening
    /// is a refused connection on every platform this runs on, and that is what a
    /// shadow that has not been started produces.
    #[tokio::test]
    async fn an_unreachable_reason_leads_with_a_token() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("nothing is listening on port 1");
        let reason = unreachable_reason(&error);
        assert!(
            reason.starts_with("connect:"),
            "expected a connect token, got {reason:?}"
        );
        assert!(
            reason.len() > "connect:".len(),
            "the detail must follow the token, got {reason:?}"
        );
    }

    /// The declarations file is loaded by the edge, not only by the report, so a
    /// missing file is a boot failure rather than a difference that appears
    /// undeclared. This is the failure the operator most needs early.
    #[test]
    fn a_missing_declarations_file_is_an_error_not_an_empty_set() {
        assert!(load_declarations(Some(Path::new("/nonexistent/declarations.json"))).is_err());
        assert!(load_declarations(None).unwrap().is_empty());
    }

    /// A malformed declarations file is an error too: silently reading it as
    /// empty would make every declared difference a failure, which looks like a
    /// renderer regression rather than a config typo.
    #[test]
    fn a_malformed_declarations_file_is_an_error() {
        let path = std::env::temp_dir().join("freedium-edge-bad-declarations.json");
        std::fs::write(&path, "{not json").expect("the temp file is writable");
        let result = load_declarations(Some(&path));
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    /// And a well-formed one loads, with the entries intact — otherwise the two
    /// errors above could both pass with a loader that always failed.
    #[test]
    fn a_well_formed_declarations_file_loads() {
        let path = std::env::temp_dir().join("freedium-edge-good-declarations.json");
        std::fs::write(
            &path,
            r#"{"declarations":[{"path":"/0291df856c77","kind":"text-or-markup","reason":"ad slot"}]}"#,
        )
        .expect("the temp file is writable");
        let declarations = load_declarations(Some(&path)).expect("a well-formed file loads");
        let _ = std::fs::remove_file(&path);

        assert!(!declarations.is_empty());
        assert!(
            declarations
                .find("/0291df856c77", page_canonical::DiffKind::TextOrMarkup)
                .is_some()
        );
    }

    /// `now_ms` is a Unix timestamp, not a monotonic clock or a zero. A record
    /// with a broken timestamp would make the report's consecutive-days counter
    /// meaningless, which is §5's actual gate.
    #[test]
    fn now_ms_is_a_plausible_unix_timestamp() {
        // 2020-01-01, in milliseconds. Anything before this is a clock that is
        // wrong in a way the report would not notice.
        const JAN_2020: u64 = 1_577_836_800_000;
        assert!(now_ms() > JAN_2020, "{}", now_ms());
    }

    /// The record a decline produces: no shadow side, the reason preserved. This
    /// is the shape `shadow-report` reads to prove the interlock fired.
    #[test]
    fn a_declined_answer_records_the_reason_and_no_shadow_side() {
        let answer = Answer::Declined {
            reason: SHADOW_NO_FETCH.to_string(),
        };
        let shadow_side: Option<Side> = match &answer {
            Answer::Served(served) => Some(Side {
                status: served.status,
                ms: 1.0,
                bytes: served.body.len(),
            }),
            Answer::Declined { .. } | Answer::Unreachable { .. } => None,
        };
        assert!(shadow_side.is_none());

        let record = ShadowRecord::record(
            0,
            "/p",
            None,
            Side {
                status: 503,
                ms: 4.0,
                bytes: 100,
            },
            shadow_side,
            &compare(
                &Served::new(503, Some("text/html"), "<p>declined</p>"),
                &answer,
                &Declarations::default(),
                "/p",
            ),
        );

        assert_eq!(record.outcome, page_canonical::RecordOutcome::Declined);
        assert_eq!(record.reason.as_deref(), Some(SHADOW_NO_FETCH));
    }

    /// A served answer produces a shadow side, and the difference between the
    /// two statuses is what the record shows. The control for the test above.
    #[test]
    fn a_served_answer_records_a_shadow_side() {
        let answer = Answer::Served(Served::new(200, Some("text/html"), "<p>hi</p>"));
        let shadow_side = match &answer {
            Answer::Served(served) => Some(Side {
                status: served.status,
                ms: 1.0,
                bytes: served.body.len(),
            }),
            Answer::Declined { .. } | Answer::Unreachable { .. } => None,
        };

        let record = ShadowRecord::record(
            0,
            "/p",
            None,
            Side {
                status: 500,
                ms: 4.0,
                bytes: 100,
            },
            shadow_side,
            &compare(
                &Served::new(500, Some("text/html"), "<p>oops</p>"),
                &answer,
                &Declarations::default(),
                "/p",
            ),
        );

        assert_eq!(
            record.shadow.expect("a served answer is recorded").status,
            200
        );
        assert_eq!(record.outcome, page_canonical::RecordOutcome::Different);
        let outcome = compare(
            &Served::new(500, Some("text/html"), "<p>oops</p>"),
            &answer,
            &Declarations::default(),
            "/p",
        );
        assert!(matches!(outcome, Outcome::Different { .. }));
    }
}
