//! SPIKE-1 candidate: the same `FullPostQuery` replay as `baseline_curl_cffi.py`,
//! but through a Rust client that impersonates a browser's TLS/HTTP2 fingerprint.
//!
//! `RUST_REWRITE_PLAN` §3.1 requires a measured ≤1% regression against the
//! `curl_cffi` baseline before any production Rust client is adopted. This binary
//! produces the candidate half of that measurement, in the same JSONL record
//! schema (`README.md`), which `difftest spike-impersonate-report` then scores.
//!
//! # Why this is not `HttpPostSource`
//!
//! The transport plugs into [`medium_client::http::Transport`] — the production
//! seam — but the loop here is a plain one, for three reasons that all corrupt
//! the measurement:
//!
//! - `RetryPolicy::DEFAULT` is two attempts. One baseline record is one curl
//!   request, so a retrying candidate could issue two wire requests per row while
//!   reporting one outcome, inflating its own success rate and doubling the load
//!   it puts on Medium.
//! - `elapsed_ms` would then include the backoff sleep, so the p50/p95 columns
//!   would stop being latency.
//! - `FetchError::NoPost` carries no status, and the record schema wants the raw
//!   status for every row.
//!
//! # What is shared with production
//!
//! Everything that decides the request bytes comes from
//! [`medium_client::request`]: the endpoint, the query (itself `include_str!`-ed
//! and pinned against `api.py` by a test in that crate), the header names, order
//! and values, and the two per-request randomised headers. None of it is
//! re-declared here, because a copied header block is exactly the drift that
//! would show up as a gate failure for the wrong reason.
//!
//! # Direct, not pooled
//!
//! Per the approved scope this runs **without a proxy** (`--allow-no-proxy`'s
//! equivalent on the baseline side). That is a *fingerprint* test, not the formal
//! §3.1 gate: the gate is specified "through the same WARP pool", and the pool
//! container is not running here. `--proxies` exists so the pooled run is a flag
//! rather than a code change when it is.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use medium_client::request;

// The transport used to live here. It moved into `medium-client` when SPIKE-1's
// verdict was adopted — production needed it, and a second copy would have been
// a second thing to keep in step with the measurement. This harness is now a
// genuine *external* consumer of it, which is the role its tests were written
// for: `the_transport_plugs_into_the_production_source_seam` below only means
// something from out here.
#[cfg(feature = "wreq-transport")]
use {
    medium_client::http::{Method, Transport, TransportRequest},
    medium_client::wreq_transport::{Profile, WreqTransport},
    std::sync::Arc,
    std::time::Instant,
};

/// One attempt, in the schema `xtask/difftest/src/impersonate.rs` reads.
///
/// The last three fields are additions. The scorer's `Attempt` is not
/// `#[serde(deny_unknown_fields)]`, so they are ignored rather than rejected —
/// and `seq` is what makes the two sides alignable, since the scorer itself
/// checks neither row count nor post-ID mapping.
#[derive(Debug, Clone, Serialize)]
struct Record {
    post_id: String,
    proxy: Option<String>,
    ok: Option<bool>,
    status: Option<u16>,
    error: Option<String>,
    elapsed_ms: u64,
    dry_run: bool,
    seq: usize,
    emulation: String,
    /// `medium_client::response::validate`'s verdict, recorded beside `ok` rather
    /// than instead of it. The two disagree at two edges (`{"error": null, ...}`
    /// is `GraphQl` for `validate` but passes Python's truthiness test, and
    /// `{"data":{"post":{}}}` is `Ok` for `validate` but falsy in Python), and
    /// `ok` here deliberately follows the *baseline's* rule so the two sides are
    /// scored the same way.
    validate_ok: Option<bool>,
}

fn main() -> std::process::ExitCode {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("FATAL: {message}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("FATAL: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

struct Args {
    out: Option<PathBuf>,
    n: usize,
    post_ids: Option<PathBuf>,
    proxies: Option<PathBuf>,
    concurrency: usize,
    timeout: Duration,
    profile_name: String,
    dry_run: bool,
    quiet: bool,
    echo_headers: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Args {
            out: None,
            n: 500,
            post_ids: None,
            proxies: None,
            concurrency: 8,
            timeout: Duration::from_secs(12),
            profile_name: "chrome110".to_string(),
            dry_run: false,
            quiet: false,
            echo_headers: false,
        };

        let mut argv = std::env::args().skip(1);
        while let Some(flag) = argv.next() {
            let mut take = |name: &str| -> Result<String, String> {
                argv.next().ok_or_else(|| format!("{name} needs a value"))
            };
            match flag.as_str() {
                "--out" => args.out = Some(PathBuf::from(take("--out")?)),
                "--n" => {
                    args.n = take("--n")?
                        .parse()
                        .map_err(|_| "--n must be an integer".to_string())?
                }
                "--post-ids" => args.post_ids = Some(PathBuf::from(take("--post-ids")?)),
                "--proxies" => args.proxies = Some(PathBuf::from(take("--proxies")?)),
                "--concurrency" => {
                    args.concurrency = take("--concurrency")?
                        .parse()
                        .map_err(|_| "--concurrency must be an integer".to_string())?
                }
                "--timeout" => {
                    let secs: f64 = take("--timeout")?
                        .parse()
                        .map_err(|_| "--timeout must be a number".to_string())?;
                    args.timeout = Duration::from_secs_f64(secs)
                }
                "--emulation" => args.profile_name = take("--emulation")?,
                "--dry-run" => args.dry_run = true,
                "--quiet" => args.quiet = true,
                "--echo-headers" => args.echo_headers = true,
                other => return Err(format!("unknown argument {other}")),
            }
        }

        if args.n < 1 {
            return Err("--n must be at least 1".to_string());
        }
        if args.concurrency < 1 {
            return Err("--concurrency must be at least 1".to_string());
        }
        if !args.echo_headers && args.out.is_none() {
            return Err("--out is required unless --echo-headers is given".to_string());
        }
        Ok(args)
    }
}

// ---------------------------------------------------------------------------
// Post IDs
// ---------------------------------------------------------------------------

/// Mirrors `load_post_ids` in `baseline_curl_cffi.py`.
///
/// The two runners must agree on *which* post each sequence number maps to, so
/// this copies that function's parsing (last path segment, minus any query
/// string) and its order-preserving dedup. The strongest guarantee is still to
/// hand both sides the same file — see the runbook in `README.md`.
///
/// # Why the shape check is here and not in the baseline
///
/// Note what that parsing does **not** do: strip a slug. A file containing
/// `https://medium.com/@someone/a-post-27832c8f6644` yields the id
/// `a-post-27832c8f6644` on both sides, both requests fail, and the two files
/// then agree perfectly — a parity of 1.0 measured over requests neither client
/// ever made. That is the whole failure mode the scorer's missing baseline floor
/// leaves open, so it is caught here, before any request.
///
/// Only the candidate refuses. Making the baseline refuse too would be a change
/// to the baseline's behaviour on a file it currently accepts, and this is a
/// pre-flight guard rather than a difference in what gets sent: every file this
/// accepts, both sides parse identically.
fn load_post_ids(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;

    let mut ids: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let segment = line.rsplit('/').next().unwrap_or(line);
        let id = segment.split('?').next().unwrap_or(segment);
        if !id.is_empty() && !ids.iter().any(|seen| seen == id) {
            ids.push(id.to_string());
        }
    }

    if ids.is_empty() {
        return Err(format!("{} contains no post IDs", path.display()));
    }

    // The same 11-or-12-lowercase-hex rule `extract_post_id` applies to
    // `tests/smokie_tests.py`, applied here as a floor rather than a parser.
    let malformed: Vec<&str> = ids
        .iter()
        .filter(|id| {
            !(11..=12).contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(String::as_str)
        .collect();
    if !malformed.is_empty() {
        return Err(format!(
            "{} does not hold post IDs: {} — a slug URL reaches the endpoint \
             as a slug, fails identically on both sides, and reports as perfect \
             parity. Regenerate the file (see README.md); the baseline's \
             `--post-ids` does not strip slugs either.",
            path.display(),
            malformed.join(", "),
        ));
    }

    Ok(ids)
}

fn load_proxies(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------
//
// Only `fetch_one` calls these, and it is behind the feature — hence the
// `allow(dead_code)` below. They stay compiled either way so the tests under
// them run in a `--no-default-features` build, which is the build a machine
// without cmake has.

/// Python truthiness, for `data.post`.
///
/// `baseline_curl_cffi.py` writes `if not payload.get("data", {}).get("post")`,
/// so an empty object is a *failure* there. Rust's `Option::is_some` would call
/// it a success, and the two sides would then disagree about the same response.
#[cfg_attr(
    not(feature = "wreq-transport"),
    allow(dead_code, reason = "called only from the feature-gated fetch_one")
)]
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_none_or(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(entries) => !entries.is_empty(),
    }
}

/// The baseline's success rule and its error string, reproduced exactly.
#[cfg_attr(
    not(feature = "wreq-transport"),
    allow(dead_code, reason = "called only from the feature-gated fetch_one")
)]
fn score(status: u16, body: &[u8]) -> (bool, Option<String>) {
    if status != 200 {
        return (false, Some(truncate(&String::from_utf8_lossy(body))));
    }
    match serde_json::from_slice::<Value>(body) {
        // The baseline writes the exception *type* here (`json: JSONDecodeError`),
        // where this has `serde_json`'s failure category instead. The strings
        // differ; the `json:` prefix — which is the part `Side::failure_reasons`
        // groups on, and the part anyone reads — is the same.
        Err(err) => (false, Some(format!("json: {:?}", err.classify()))),
        Ok(payload) => {
            let has_post = payload
                .get("data")
                .and_then(|data| data.get("post"))
                .is_some_and(truthy);
            if has_post {
                (true, None)
            } else {
                (false, Some(truncate(&String::from_utf8_lossy(body))))
            }
        }
    }
}

#[cfg_attr(
    not(feature = "wreq-transport"),
    allow(dead_code, reason = "called only from the feature-gated score")
)]
fn truncate(text: &str) -> String {
    text.chars().take(200).collect()
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn run(args: &Args) -> Result<(), String> {
    if args.echo_headers {
        // The same JSON shape as the baseline's `--echo-headers`, so the two can
        // be diffed directly. `Connection` is deliberately absent: `api.py:48`
        // sets it, but it is hop-by-hop and curl drops it under HTTP/2
        // (`medium_client::request` documents this), so that one difference is
        // expected rather than drift.
        let headers = request::headers(
            &request::operation_id(),
            request::client_date_ms(),
            None, // No Cookie: the spike is anonymous (§2.7 warning 2).
        );
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "header_order": names }))
                .expect("a list of strings serialises")
        );
        return Ok(());
    }

    #[cfg(feature = "wreq-transport")]
    let profile = Profile::parse(&args.profile_name)
        .ok_or_else(|| format!("unknown --emulation {}", args.profile_name))?;

    #[cfg(not(feature = "wreq-transport"))]
    if !args.dry_run {
        return Err(
            "built without the `wreq-transport` feature, so only --dry-run and \
             --echo-headers are available"
                .to_string(),
        );
    }

    let post_ids_path = args.post_ids.clone().ok_or_else(|| {
        "--post-ids is required (see README.md for how to freeze the list)".to_string()
    })?;
    let post_ids = load_post_ids(&post_ids_path)?;
    let proxies = match &args.proxies {
        Some(path) => load_proxies(path)?,
        None => Vec::new(),
    };

    if proxies.is_empty() && !args.dry_run {
        eprintln!(
            "note: no --proxies given, so this is a DIRECT run. That is a fingerprint \
             test, not the formal §3.1 gate, which is specified through the WARP pool."
        );
    }

    let out_path = args.out.clone().expect("checked in Args::parse");
    let started_at_ms = epoch_ms();

    #[cfg(feature = "wreq-transport")]
    let transport: Option<Arc<WreqTransport>> = if args.dry_run {
        None
    } else {
        Some(Arc::new(
            WreqTransport::new(profile, args.timeout).map_err(|err| err.to_string())?,
        ))
    };

    eprintln!(
        "candidate: {} requests over {} post IDs, {} proxies, concurrency {}, \
         profile {}{}",
        args.n,
        post_ids.len(),
        proxies.len(),
        args.concurrency,
        args.profile_name,
        if args.dry_run { " [DRY RUN]" } else { "" },
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start the runtime: {err}"))?;

    let (mut records, ok, fail) = runtime.block_on(async {
        // Indexed by `seq` so the output is in request order regardless of the
        // order the batch happens to complete in.
        let mut records: Vec<Option<Record>> = vec![None; args.n];
        let mut ok = 0usize;
        let mut fail = 0usize;

        // Batches of `concurrency`, each drained before the next begins —
        // `baseline_curl_cffi.py` does the same, and the load shape is part of
        // what is being compared.
        for batch_start in (0..args.n).step_by(args.concurrency) {
            let batch_end = (batch_start + args.concurrency).min(args.n);

            let mut set = tokio::task::JoinSet::new();
            for seq in batch_start..batch_end {
                let post_id = post_ids[seq % post_ids.len()].clone();
                let proxy = if proxies.is_empty() {
                    None
                } else {
                    Some(proxies[seq % proxies.len()].clone())
                };
                let profile_name = args.profile_name.clone();
                let timeout = args.timeout;
                let dry_run = args.dry_run;

                #[cfg(feature = "wreq-transport")]
                let transport = transport.clone();

                set.spawn(async move {
                    if dry_run {
                        return dry_run_record(seq, post_id, proxy, profile_name);
                    }
                    // Without the feature the loop above already refused to get
                    // here, so this arm exists only to keep the two cfgs from
                    // both having to name the same variables.
                    #[cfg(not(feature = "wreq-transport"))]
                    {
                        let _ = (seq, post_id, proxy, profile_name, timeout);
                        unreachable!("run() returns early without the feature")
                    }
                    #[cfg(feature = "wreq-transport")]
                    fetch_one(seq, post_id, proxy, profile_name, timeout, transport).await
                });
            }

            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(record) => {
                        if record.ok == Some(true) {
                            ok += 1;
                        } else if record.ok == Some(false) {
                            fail += 1;
                        }
                        let seq = record.seq;
                        records[seq] = Some(record);
                    }
                    Err(err) => return Err(format!("a task panicked: {err}")),
                }
            }

            if !args.quiet {
                eprintln!("  {batch_end}/{}  ok={ok} fail={fail}", args.n);
            }
        }

        Ok::<_, String>((records, ok, fail))
    })?;

    let records: Vec<Record> = records
        .drain(..)
        .map(|record| record.expect("every seq was filled"))
        .collect();

    write_jsonl(&out_path, &records)?;
    write_sidecar(
        args,
        &out_path,
        &post_ids,
        &proxies,
        started_at_ms,
        epoch_ms(),
    )?;

    if args.dry_run {
        println!(
            "wrote {} DRY-RUN rows to {} (not a measurement)",
            records.len(),
            out_path.display()
        );
    } else {
        let scored = ok + fail;
        let rate = if scored == 0 {
            0.0
        } else {
            ok as f64 / scored as f64
        };
        let disagreements = records
            .iter()
            .filter(|record| record.validate_ok.is_some() && record.validate_ok != record.ok)
            .count();
        println!(
            "wrote {} rows to {}; success rate {rate:.4}",
            records.len(),
            out_path.display()
        );
        if disagreements > 0 {
            // A hidden asymmetry turned into data rather than folded into `ok`.
            println!(
                "note: `ok` and `validate` disagreed on {disagreements} row(s) — \
                 see the validate_ok key"
            );
        }
    }
    Ok(())
}

fn dry_run_record(seq: usize, post_id: String, proxy: Option<String>, profile: String) -> Record {
    Record {
        post_id,
        proxy,
        ok: None,
        status: None,
        error: None,
        elapsed_ms: 0,
        dry_run: true,
        seq,
        emulation: profile,
        validate_ok: None,
    }
}

/// One request, one record. No retry — see the module doc.
#[cfg(feature = "wreq-transport")]
async fn fetch_one(
    seq: usize,
    post_id: String,
    proxy: Option<String>,
    profile: String,
    timeout: Duration,
    transport: Option<Arc<WreqTransport>>,
) -> Record {
    let transport = transport.expect("a transport is built for every non-dry run");
    let started = Instant::now();

    let request = TransportRequest {
        url: request::ENDPOINT.to_string(),
        method: Method::Post,
        headers: request::headers(
            &request::operation_id(),
            request::client_date_ms(),
            None, // Anonymous: no MEDIUM_AUTH_COOKIES (§2.7 warning 2).
        ),
        body: serde_json::to_vec(&request::body(&post_id)).expect("the body serialises"),
        proxy,
        timeout,
    };

    let proxy = request.proxy.clone();

    match transport.send(request).await {
        Ok(response) => {
            let (ok, error) = score(response.status, &response.body);
            let validate_ok = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .map(|payload| medium_client::response::validate(&payload).is_ok());
            Record {
                post_id,
                proxy,
                ok: Some(ok),
                status: Some(response.status),
                error,
                elapsed_ms: started.elapsed().as_millis() as u64,
                dry_run: false,
                seq,
                emulation: profile,
                validate_ok,
            }
        }
        Err(err) => Record {
            post_id,
            proxy,
            ok: Some(false),
            status: None,
            error: Some(err.to_string()),
            elapsed_ms: started.elapsed().as_millis() as u64,
            dry_run: false,
            seq,
            emulation: profile,
            validate_ok: None,
        },
    }
}

fn write_jsonl(path: &Path, records: &[Record]) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    for record in records {
        let line = serde_json::to_string(record).map_err(|err| format!("cannot encode: {err}"))?;
        writeln!(writer, "{line}").map_err(|err| format!("cannot write: {err}"))?;
    }
    writer
        .flush()
        .map_err(|err| format!("cannot flush {}: {err}", path.display()))
}

/// Everything a reader needs to reproduce the run, kept **out** of the JSONL.
///
/// A metadata line inside the records file would make `Side::load` reject the
/// whole file on its first line, so this goes to `*.meta.json` beside it. Both
/// names are ignored by the root `.gitignore` — this describes one run, and is
/// regenerated by re-running it, so committing it would only create a copy that
/// goes stale.
///
/// # `emulation` is a name, not a version
///
/// The field records `--emulation` as it was typed, and that is all it can
/// record: the profile's meaning depends on the `wreq`/`wreq-util` revisions
/// behind it, which this file never saw. Those two pins are exact and live in
/// the root workspace manifest's `[workspace.dependencies]` — the transport, and
/// therefore the pins, moved into `crates/medium-client`. A sidecar read months
/// from now is only interpretable against the commit that produced it.
fn write_sidecar(
    args: &Args,
    out_path: &Path,
    post_ids: &[String],
    proxies: &[String],
    started_at_ms: u64,
    ended_at_ms: u64,
) -> Result<(), String> {
    let sidecar = out_path.with_extension("meta.json");
    let mut argv: Vec<String> = std::env::args().collect();
    argv.remove(0);

    let payload = serde_json::json!({
        "emulation": args.profile_name,
        "n": args.n,
        "concurrency": args.concurrency,
        "timeout_secs": args.timeout.as_secs_f64(),
        "dry_run": args.dry_run,
        "proxies": proxies,
        // The full ordered list, not a hash: it is short, and it is the thing the
        // two sides have to agree on for the comparison to mean anything.
        "post_ids": post_ids,
        "post_ids_count": post_ids.len(),
        "argv": argv,
        "started_at_ms": started_at_ms,
        "ended_at_ms": ended_at_ms,
        // This crate's version, not `wreq`'s — there is no compile-time
        // dependency-version query without a build script, and adding one to
        // record what the committed `Cargo.lock` already pins exactly
        // (`wreq = "=0.16.1"`, `wreq-util = "=0.2.0"`) would be noise.
        "spike_version": env!("CARGO_PKG_VERSION"),
    });

    let text = serde_json::to_string_pretty(&payload).map_err(|err| err.to_string())?;
    std::fs::write(&sidecar, text)
        .map_err(|err| format!("cannot write {}: {err}", sidecar.display()))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Why the transport lives in `medium-client` and is consumed from out
    /// here.**
    ///
    /// This is the harness's own copy of the seam test the transport used to
    /// carry. It fails to compile the moment the `Transport` trait or the
    /// `HttpPostSource` constructor drifts away from what the server wires — and
    /// that is a claim only an *external* consumer can make. Inside
    /// `medium-client` the same test would degenerate into "does `impl Transport`
    /// still satisfy `HttpPostSource::new`", which the compiler already enforces
    /// for every in-crate caller.
    ///
    /// It builds a client but sends nothing, so it needs no network and no
    /// proxy. It *does* need the real BoringSSL build, because `new` builds the
    /// direct client eagerly rather than on the first request.
    #[cfg(feature = "wreq-transport")]
    #[test]
    fn the_transport_plugs_into_the_production_source_seam() {
        use medium_client::http::HttpPostSource;
        use medium_client::proxy::{HealthProbe, ProxyEndpoint, ProxyPool};

        /// `ProxyPool::new` takes a probe even for an empty slot list, and an
        /// empty list (`ProxyChoice::Direct` on every `next()`) is the direct
        /// run. Returning `false` is not a lie about anything reachable: it is
        /// the only value that cannot accidentally mark an exit healthy.
        struct NeverProbed;

        #[async_trait::async_trait]
        impl HealthProbe for NeverProbed {
            async fn probe(&self, _endpoint: &ProxyEndpoint) -> bool {
                false
            }
        }

        let transport = WreqTransport::new(Profile::Chrome110, Duration::from_secs(12))
            .expect("the direct client builds");
        let pool = Arc::new(ProxyPool::new(Vec::new(), Arc::new(NeverProbed)));

        let source = HttpPostSource::new(transport, pool);
        // Bound to a name so the unused-variable lint cannot drop it, and read
        // back through the type the server will hold it as.
        let _: HttpPostSource<WreqTransport> = source;
    }

    /// The two edges where Python truthiness and Rust's `is_some` disagree.
    /// Both are real Medium responses: `{"data":{"post":null}}` is a
    /// deleted/blocked post, and `{"data":{"post":{}}}` is a shape the legacy
    /// decoder tolerates. Getting either wrong moves the candidate's success
    /// rate relative to the baseline's without anything on the wire differing.
    #[test]
    fn truthiness_follows_python_not_rust() {
        let post_of = |body: &str| {
            serde_json::from_str::<Value>(body)
                .unwrap()
                .get("data")
                .unwrap()
                .get("post")
                .unwrap()
                .clone()
        };

        assert!(!truthy(&post_of(r#"{"data":{"post":null}}"#)));
        assert!(!truthy(&post_of(r#"{"data":{"post":{}}}"#)));
        assert!(!truthy(&post_of(r#"{"data":{"post":[]}}"#)));
        assert!(!truthy(&post_of(r#"{"data":{"post":""}}"#)));
        assert!(!truthy(&post_of(r#"{"data":{"post":false}}"#)));
        assert!(!truthy(&post_of(r#"{"data":{"post":0}}"#)));
        assert!(truthy(&post_of(r#"{"data":{"post":{"id":"x"}}}"#)));
    }

    #[test]
    fn a_200_without_the_post_is_a_failure() {
        let (ok, error) = score(200, br#"{"data":{"post":null}}"#);
        assert!(!ok, "a 200 that is not the post is still a failed fetch");
        assert!(
            error.is_some(),
            "the body is kept for the failure_reasons table"
        );

        let (ok, error) = score(200, br#"{"data":{"post":{"id":"x"}}}"#);
        assert!(ok);
        assert!(error.is_none());
    }

    /// A non-200 keeps its status and its body, which is what makes a Cloudflare
    /// challenge (`403` + `cf-mitigated`) readable in the JSONL rather than
    /// summarized away.
    #[test]
    fn a_non_200_reports_the_body_as_the_error() {
        let (ok, error) = score(403, b"<html>Just a moment...</html>");
        assert!(!ok);
        assert_eq!(error.unwrap(), "<html>Just a moment...</html>");
    }

    #[test]
    fn an_unparseable_200_groups_under_json() {
        let (ok, error) = score(200, b"<html>Just a moment...</html>");
        assert!(!ok);
        let error = error.unwrap();
        assert_eq!(
            error.split(':').next().unwrap().trim(),
            "json",
            "the scorer groups failures on the first clause, and the baseline \
             writes `json: JSONDecodeError` — the grouping must match"
        );
    }

    /// `Side::failure_reasons` keeps the error string whole in the record, so a
    /// Cloudflare body must not be allowed to bloat the file 500 times over.
    #[test]
    fn long_bodies_are_truncated_to_the_baselines_limit() {
        let (_, error) = score(500, &vec![b'x'; 5_000]);
        assert_eq!(error.unwrap().chars().count(), 200);
    }

    /// `load_post_ids` mirrors the baseline's parser, and the two must agree on
    /// the *order* as well as the contents — `seq % len` maps sequence numbers
    /// onto this list, so a different order silently compares different posts.
    ///
    /// The URL here has a bare id as its last segment, which is the only URL
    /// shape that survives the guard below (see
    /// [`a_slug_url_file_is_refused_before_any_request`] for the other one).
    #[test]
    fn post_ids_parse_from_urls_and_bare_ids_in_order() {
        let dir = std::env::temp_dir().join(format!("spike-ids-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ids.txt");
        std::fs::write(
            &path,
            "# a comment\n\
             \n\
             https://medium.com/@someone/27832c8f6644?source=whatever\n\
             515dd5a43948\n\
             https://medium.com/p/27832c8f6644\n",
        )
        .unwrap();

        let ids = load_post_ids(&path).unwrap();
        assert_eq!(
            ids,
            vec!["27832c8f6644", "515dd5a43948"],
            "URLs reduce to their last path segment, the query string is \
             dropped, and a duplicate is not re-requested"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file of URLs whose last segment is a *slug* must be refused rather than
    /// run. Both sides would send the slug, both would fail, and the report
    /// would read as perfect parity — see the doc comment on `load_post_ids`.
    #[test]
    fn a_slug_url_file_is_refused_before_any_request() {
        let dir = std::env::temp_dir().join(format!("spike-ids-slug-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ids.txt");
        std::fs::write(
            &path,
            "https://medium.com/@someone/stop-wasting-your-life-27832c8f6644\n",
        )
        .unwrap();

        let err = load_post_ids(&path).expect_err("a slug is not a post ID");
        assert!(
            err.contains("stop-wasting-your-life-27832c8f6644"),
            "the error must name the offending line: {err}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_post_id_file_is_an_error_not_an_empty_run() {
        let dir = std::env::temp_dir().join(format!("spike-ids-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ids.txt");
        std::fs::write(&path, "# nothing but comments\n").unwrap();

        assert!(load_post_ids(&path).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
