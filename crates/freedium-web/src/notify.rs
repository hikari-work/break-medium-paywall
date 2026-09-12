//! Telegram alerts — `utils/notify.py`.
//!
//! One function's worth of behaviour: post a message to the admin's chat. It is
//! called from the error page, from the timeout middleware and from
//! `/report-problem`.
//!
//! # It goes through `medium-client`'s transport
//!
//! `medium-client`'s own docs say it "is the only crate in the workspace that
//! reaches the outbound network", and that is worth keeping true — so this sends
//! the Telegram request through [`Transport`] rather than pulling a second HTTP
//! client into the server. It also means a test can assert what would be sent
//! without a socket, which is the only way any of this is testable.
//!
//! # Three legacy details kept
//!
//! - **`GOOD` messages are dropped.** `notify.py:20-22` returns early for
//!   `status == "GOOD"`, so every "successfully rendered" alert is a no-op that
//!   only logs. §7 item 8 removes the per-request `GOOD` call site entirely;
//!   the branch stays here so the next caller cannot accidentally start
//!   flooding Telegram.
//! - **A missing token or id is not an error.** `notify.py:16-18` logs a warning
//!   and returns; so does this.
//! - **Over-long messages are truncated, not rejected** — `text[:4000]`.
//!
//! One thing is *not* kept: `send_message` here is `async` and awaited, where
//! the legacy used blocking `urllib3` and could not be awaited at all. It is
//! called on paths that are already failing, so it carries its own short
//! timeout; see [`DEFAULT_TIMEOUT`].

use form_urlencoded::Serializer;
use medium_client::http::{Method, ReqwestTransport, Transport, TransportRequest};

use crate::config::Config;

/// `notify.py:24` truncates to 4000 characters.
pub const MAX_MESSAGE_CHARS: usize = 4000;

/// Telegram accepts long polls of this size; `notify.py` used no timeout at all
/// (blocking `urllib3`), which is not something to reproduce. Short, because
/// this runs on the error path and must not hold a response open.
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `notify.py:9-11`.
///
/// A real enum rather than the legacy's string: `notify.py:14` takes
/// `status: MessageStatus = "ERROR"` and `notify.py:20` compares it to
/// `MessageStatus.GOOD.value`, so the annotation and the value disagree and a
/// caller passing `"GOOD"` gets the early return by string luck. §7 item 6 asks
/// for the enum; this is it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageStatus {
    #[default]
    Error,
    /// Suppressed — see the module docs.
    Good,
}

/// Sends messages. Holds the transport, because building one per message would
/// build a TLS client per alert.
///
/// Generic over [`Transport`] for the same reason everything else in this
/// workspace is: a test can then assert *what would be sent* — or that nothing
/// would be — without a socket.
pub struct Notifier<T: Transport> {
    transport: T,
    /// `None` when `TELEGRAM_BOT_TOKEN` is unset or `TELEGRAM_ADMIN_ID` is `0`.
    credentials: Option<Credentials>,
}

/// The notifier the server actually runs.
pub type Telegram = Notifier<ReqwestTransport>;

struct Credentials {
    token: String,
    admin_id: i64,
}

impl<T: Transport> Notifier<T> {
    /// A notifier that will not send, for callers that have no token.
    pub fn disabled(transport: T) -> Self {
        Self {
            transport,
            credentials: None,
        }
    }

    /// Reads the two Telegram values out of the config.
    pub fn new(transport: T, config: &Config) -> Self {
        let credentials = match (&config.telegram_bot_token, config.telegram_admin_id) {
            (Some(token), admin_id) if admin_id != 0 => Some(Credentials {
                token: token.clone(),
                admin_id,
            }),
            _ => None,
        };

        Self {
            transport,
            credentials,
        }
    }

    /// `notify.py:14-41`.
    ///
    /// Never fails and never returns anything: every outcome is a log line, as
    /// in the legacy. A caller on the error path has no way to act on a Telegram
    /// failure and should not be handed one.
    pub async fn send(&self, text: &str, silent: bool, status: MessageStatus) {
        let Some(credentials) = &self.credentials else {
            tracing::warn!(
                "Can't send log messages, because of lack of some informations. Ignore...."
            );
            return;
        };

        if status == MessageStatus::Good {
            tracing::warn!("Ignoring sending GOOD message");
            return;
        }

        let text = truncate(text);
        let body = body(&credentials.admin_id, text, silent);
        let request = TransportRequest {
            url: format!(
                "https://api.telegram.org/bot{}/sendMessage",
                credentials.token
            ),
            method: Method::Post,
            headers: vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: body.into_bytes(),
            // `None`, always: Telegram is not Medium, and routing an alert
            // through the WARP pool would spend an exit and could lose the alert.
            proxy: None,
            timeout: DEFAULT_TIMEOUT,
        };

        match self.transport.send(request).await {
            Ok(response) if response.status == 200 => {
                tracing::info!("Message sent successfully");
            }
            Ok(response) => {
                tracing::warn!("Failed to send message. Status: {}", response.status);
            }
            Err(err) => {
                tracing::warn!("Failed to send message. Error: {err}");
            }
        }
    }
}

/// `text[:4000]`, on character boundaries.
///
/// Python slices `str` by code point, so slicing `&text[..4000]` here would
/// panic on a message whose 4000th byte lands inside a multi-byte character —
/// and these messages carry emoji, so that is a live possibility rather than a
/// theoretical one.
fn truncate(text: &str) -> &str {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return text;
    }

    tracing::warn!("Message is too long ({}), truncating", text.chars().count());
    match text.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((byte_index, _)) => &text[..byte_index],
        None => text,
    }
}

/// The `fields=` dict `notify.py:29-34` posts, form-encoded.
fn body(admin_id: &i64, text: &str, silent: bool) -> String {
    Serializer::new(String::new())
        .append_pair("chat_id", &admin_id.to_string())
        .append_pair("text", text)
        .append_pair("parse_mode", "HTML")
        // `urllib3`'s `fields=` renders the Python `True`/`False` here; Telegram
        // takes both spellings, and lowercase is what its own docs show.
        .append_pair(
            "disable_notification",
            if silent { "true" } else { "false" },
        )
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that records what it was asked to send and always answers
    /// 200, so a test can ask whether the network was touched at all.
    #[derive(Clone, Default)]
    struct Recorder {
        sent: std::sync::Arc<std::sync::Mutex<Vec<TransportRequest>>>,
    }

    impl Recorder {
        fn sent(&self) -> Vec<TransportRequest> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Transport for Recorder {
        async fn send(
            &self,
            request: TransportRequest,
        ) -> Result<medium_client::http::TransportResponse, medium_client::error::TransportError>
        {
            self.sent.lock().unwrap().push(request);
            Ok(medium_client::http::TransportResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    fn notifier(transport: Recorder, configured: bool) -> Notifier<Recorder> {
        let config = crate::config::Config {
            host_address: String::new(),
            medium_auth_cookies: None,
            medium_impersonate: medium_client::wreq_transport::Profile::Chrome110,
            admin_secret_key: "s".into(),
            telegram_admin_id: if configured { 42 } else { 0 },
            telegram_bot_token: configured.then(|| "tok".to_string()),
            log_level_name: "INFO".into(),
            more_logs: false,
            disable_external_docs: true,
            timeout: std::time::Duration::from_secs(38),
            request_timeout: std::time::Duration::from_secs(12),
            worker_timeout: std::time::Duration::from_secs(85),
            cache_life_time: std::time::Duration::from_secs(60),
            home_page_max_posts: 45,
            enable_ads_banner: false,
            shadow_mode: false,
            redis_host: "r".into(),
            redis_port: 6379,
            redis_timeout: 1.75,
            database_url: "postgres://x".into(),
            proxy_list: Vec::new(),
            port: 7080,
            static_dir: "caddy/static".into(),
            // Deliberately not `state::tests::test_config()`, however tempting
            // the duplication looks: this fixture varies `host_address`,
            // `cache_life_time` and the Redis and Postgres targets on purpose,
            // and inheriting those from the state fixture would make this test
            // depend on values it does not read. The Fase 6 API fields are here
            // only because the struct has no `Default` — this test reads neither
            // of them.
            api_rate_limit_per_minute: 10,
            api_rate_limit_burst: 5,
            api_miss_limit_per_minute: 3,
            api_miss_limit_burst: 1,
            api_fetch_budget_per_minute: 30,
            api_fetch_budget_burst: 5,
            api_token_limit_per_minute: 60,
            api_token_limit_burst: 20,
            api_token: None,
            api_cache_seconds: 300,
            api_trust_proxy: false,
            cors_allow_origins: Vec::new(),
            medium_graphql_endpoint: None,
        };
        Notifier::new(transport, &config)
    }

    /// An unconfigured notifier must not send — `notify.py:16-18` warns and
    /// returns, and a live Telegram call from a server with no token would be a
    /// very expensive mistake.
    #[tokio::test]
    async fn an_unconfigured_notifier_sends_nothing() {
        let recorder = Recorder::default();
        notifier(recorder.clone(), false)
            .send("anything", false, MessageStatus::Error)
            .await;

        assert!(recorder.sent().is_empty());
    }

    /// `GOOD` returns before touching the network — this is the branch that
    /// keeps every successful render from alerting.
    #[tokio::test]
    async fn a_good_message_is_dropped_before_the_network() {
        let recorder = Recorder::default();
        notifier(recorder.clone(), true)
            .send("✅ Successfully rendered post", true, MessageStatus::Good)
            .await;

        assert!(recorder.sent().is_empty());
    }

    /// And the error path *does* send, to the documented URL, with the token in
    /// the path and no proxy.
    #[tokio::test]
    async fn an_error_message_is_sent_to_telegram_without_a_proxy() {
        let recorder = Recorder::default();
        notifier(recorder.clone(), true)
            .send("📛 boom", false, MessageStatus::Error)
            .await;

        let sent = recorder.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].url,
            format!("https://api.telegram.org/bot{}/sendMessage", "tok")
        );
        assert_eq!(sent[0].method, Method::Post);
        assert!(
            sent[0].proxy.is_none(),
            "an alert must not spend a WARP exit"
        );
        assert_eq!(sent[0].timeout, DEFAULT_TIMEOUT);

        let decoded: String = form_urlencoded::parse(&sent[0].body)
            .find(|(key, _)| key == "text")
            .expect("text is present")
            .1
            .into_owned();
        assert_eq!(decoded, "📛 boom");
    }

    /// An over-long body must be cut, not sent whole — Telegram rejects it, and
    /// the caller is on the error path already.
    #[tokio::test]
    async fn an_over_long_message_is_truncated_before_sending() {
        let recorder = Recorder::default();
        notifier(recorder.clone(), true)
            .send(
                &"✨".repeat(MAX_MESSAGE_CHARS + 500),
                false,
                MessageStatus::Error,
            )
            .await;

        let decoded: String = form_urlencoded::parse(&recorder.sent()[0].body)
            .find(|(key, _)| key == "text")
            .expect("text is present")
            .1
            .into_owned();
        assert_eq!(decoded.chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn the_body_is_form_encoded_with_the_four_legacy_fields() {
        let encoded = body(&42, "hello world", false);
        assert!(encoded.contains("chat_id=42"), "{encoded}");
        assert!(encoded.contains("text=hello+world"), "{encoded}");
        assert!(encoded.contains("parse_mode=HTML"), "{encoded}");
        assert!(encoded.contains("disable_notification=false"), "{encoded}");
    }

    #[test]
    fn silent_sets_disable_notification() {
        assert!(body(&1, "x", true).contains("disable_notification=true"));
    }

    /// The text must survive percent-encoding intact — it is HTML with emoji and
    /// newlines, all of which have to round-trip.
    #[test]
    fn the_text_is_escaped_not_stripped() {
        let text = "📛 <code>a&b</code>\nsecond line";
        let encoded = body(&1, text, false);

        let decoded: String = form_urlencoded::parse(encoded.as_bytes())
            .find(|(key, _)| key == "text")
            .expect("text is present")
            .1
            .into_owned();
        assert_eq!(decoded, text);
    }

    /// A short message is returned as-is, and a long one is cut to exactly the
    /// limit rather than at a byte that happens to be 4000.
    #[test]
    fn truncate_counts_characters_not_bytes() {
        let short = "a".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(truncate(&short).chars().count(), MAX_MESSAGE_CHARS);

        // Every character is three bytes, so a byte-based cut would land
        // mid-character and panic.
        let wide = "✨".repeat(MAX_MESSAGE_CHARS + 10);
        let cut = truncate(&wide);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS);
        assert!(cut.chars().all(|c| c == '✨'));
    }
}
