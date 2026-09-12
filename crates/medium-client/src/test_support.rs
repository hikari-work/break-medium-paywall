//! Test scaffolding shared by the crate's unit tests.
//!
//! `http`, `resolver` and `media` all need the same two things: a way to stand
//! up a real HTTP conversation without a web framework, and a way to script a
//! [`Transport`]'s replies without any conversation at all. Both were written
//! once for `http`'s tests; they live here so the other two use the same code
//! rather than a second copy of it.
//!
//! Compiled only under `cfg(test)`, so none of this is in a release binary.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::error::TransportError;
use crate::http::{Transport, TransportRequest, TransportResponse};
use crate::proxy::{HealthProbe, ProxyEndpoint, ProxyPool};
use crate::retry::Sleeper;

/// What the stub does when a connection arrives.
pub enum Behaviour {
    Reply(String),
    /// Accept and never answer, so the client hits its own timeout.
    Hang,
}

/// A minimal HTTP/1.1 server.
///
/// Hand-rolled on `std::net` rather than pulling in `axum` or `hyper` for
/// tests: the point is to exercise the real transport, and a test-only web
/// framework would be the largest dependency in the crate.
pub struct Stub {
    pub url: String,
    /// Every request, verbatim, in the order it arrived. Requests that were
    /// never read — [`Behaviour::Hang`] — are not recorded.
    pub requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Stub {
    /// `responses` are served in order; the last one repeats.
    pub fn start(responses: Vec<Behaviour>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("can bind a loopback port");
        let address = listener.local_addr().expect("has a local address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        std::thread::spawn(move || {
            for (served, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let behaviour = responses.get(served).unwrap_or_else(|| {
                    responses
                        .last()
                        .expect("at least one response is configured")
                });

                match behaviour {
                    Behaviour::Hang => {
                        // Hold the connection open until the test ends.
                        std::thread::sleep(Duration::from_secs(30));
                    }
                    Behaviour::Reply(response) => {
                        recorded
                            .lock()
                            .expect("no test panics while holding this")
                            .push(read_request(&mut stream));
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                    }
                }
            }
        });

        Self {
            url: format!("http://{address}"),
            requests,
        }
    }

    /// A stub that always answers 200 with `body`.
    pub fn serving(body: &str) -> Self {
        Self::start(vec![Behaviour::Reply(http_ok(body))])
    }

    /// The requests received so far, decoded as UTF-8.
    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("no test panics while holding this")
            .iter()
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
            .collect()
    }
}

pub fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

pub fn http_status(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A response with exactly the headers given, so a test can send a `Location`
/// or a `Content-Type` and nothing else.
pub fn http_with_headers(
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    response
}

/// Reads one request off the wire and returns it.
///
/// It must be read in full before replying: a client still writing into a
/// socket that has already been closed sees a reset instead of the response,
/// which surfaces as a flaky transport error rather than a test failure. So:
/// headers to the blank line, then exactly as much body as `Content-Length`
/// promised.
fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 8192];

    let body_start = loop {
        if let Some(position) = find(&request, b"\r\n\r\n") {
            break position + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return request,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    };

    let content_length = content_length(&request);
    while request.len() < body_start + content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return request,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }

    request
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(request: &[u8]) -> usize {
    let text = String::from_utf8_lossy(request);
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// A probe that treats every endpoint as healthy; the pool is not what these
/// tests are about.
pub struct AlwaysHealthy;

#[async_trait]
impl HealthProbe for AlwaysHealthy {
    async fn probe(&self, _endpoint: &ProxyEndpoint) -> bool {
        true
    }
}

pub fn direct_pool() -> Arc<ProxyPool> {
    Arc::new(ProxyPool::new(vec![], Arc::new(AlwaysHealthy)))
}

/// A pool with one exit, for the tests that need to see a proxy on the wire.
pub fn one_exit_pool() -> Arc<ProxyPool> {
    Arc::new(ProxyPool::new(
        vec![ProxyEndpoint::new("socks5://wgcf1:1080")],
        Arc::new(AlwaysHealthy),
    ))
}

/// A [`Transport`] that replays canned replies and records what it was asked
/// for — no socket, no runtime, no timing.
///
/// [`Stub`] is the right tool when the *request* is what is under test. This one
/// is for when the retry or the pool is: a scripted failure is the only way to
/// make attempt 0 fail without also making the test slow.
///
/// [`Clone`] because the callers take their transport by value: a test hands one
/// clone to the source and keeps another to ask what was sent. The clone shares
/// the state, as `Arc` implies.
#[derive(Clone)]
pub struct ScriptedTransport {
    inner: Arc<ScriptedInner>,
}

struct ScriptedInner {
    replies: Mutex<Vec<Result<TransportResponse, TransportError>>>,
    /// Sent once the script runs out, so a test only has to write down the
    /// interesting prefix.
    fallback: Result<TransportResponse, TransportError>,
    requests: Mutex<Vec<TransportRequest>>,
}

impl ScriptedTransport {
    pub fn new(
        replies: Vec<Result<TransportResponse, TransportError>>,
        fallback: TransportResponse,
    ) -> Self {
        Self {
            inner: Arc::new(ScriptedInner {
                replies: Mutex::new(replies),
                fallback: Ok(fallback),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Every reply fails, so the outcome is entirely the retry loop's.
    pub fn always_failing(error: TransportError) -> Self {
        Self {
            inner: Arc::new(ScriptedInner {
                replies: Mutex::new(Vec::new()),
                fallback: Err(error),
                requests: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The requests it was asked to send, in order.
    pub fn requests(&self) -> Vec<TransportRequest> {
        self.inner
            .requests
            .lock()
            .expect("no test panics while holding this")
            .clone()
    }

    pub fn request_count(&self) -> usize {
        self.inner
            .requests
            .lock()
            .expect("no test panics while holding this")
            .len()
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, TransportError> {
        self.inner
            .requests
            .lock()
            .expect("no test panics while holding this")
            .push(request);

        let mut replies = self
            .inner
            .replies
            .lock()
            .expect("no test panics while holding this");
        if replies.is_empty() {
            return self.inner.fallback.clone();
        }
        replies.remove(0)
    }
}

/// A response carrying `status`, `headers` and `body`, for a script.
pub fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> TransportResponse {
    TransportResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body: body.to_vec(),
    }
}

/// A [`Sleeper`] that counts how often it was asked to wait.
///
/// Sleeping for real would make a two-attempt retry test take a second, and the
/// number of sleeps is the assertion anyway.
#[derive(Default)]
pub struct CountingSleeper {
    calls: AtomicUsize,
}

impl CountingSleeper {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Sleeper for CountingSleeper {
    async fn sleep(&self, _duration: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}
