//! SPIKE-1 verdict: does the Rust client keep the fetcher working?
//!
//! RUST_REWRITE_PLAN §3.1 gates the rewrite on TLS-impersonation parity: the
//! candidate client (an impersonating HTTP client — `wreq`, the maintained
//! successor to the `rquest` the plan named) must match the `curl_cffi`
//! baseline's success rate to within 1% over the same requests through the same
//! WARP pool.
//!
//! This module only scores the two measurement files. It never makes a request,
//! so it runs anywhere — which matters, because the measurement itself can only
//! be taken in an environment that has the proxy pool.
//!
//! Both sides emit the same JSONL schema, one row per attempt:
//!
//! ```json
//! {"post_id":"515dd5a43948","proxy":"socks5://wgcf1:1080","ok":true,
//!  "status":200,"error":null,"elapsed_ms":412,"dry_run":false}
//! ```
//!
//! `ok: null` is legal input (an attempt that was not scored) but a file
//! containing any of them is refused rather than scored, because silently
//! dropping unscored attempts is how a measurement turns into a guess.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Attempt {
    pub post_id: String,
    #[serde(default)]
    pub proxy: Option<String>,
    /// `None` means the attempt was never scored.
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub dry_run: bool,
}

struct Side {
    label: String,
    attempts: Vec<Attempt>,
    ok: usize,
    fail: usize,
}

impl Side {
    fn load(label: &str, path: &str) -> Result<Self, String> {
        let file = File::open(path).map_err(|err| format!("cannot open {path}: {err}"))?;
        let mut attempts = Vec::new();
        for (lineno, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|err| format!("{path}:{}: {err}", lineno + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            let attempt: Attempt = serde_json::from_str(&line)
                .map_err(|err| format!("{path}:{}: {err}", lineno + 1))?;
            attempts.push(attempt);
        }

        if attempts.is_empty() {
            return Err(format!("{path} has no rows"));
        }
        if attempts.iter().any(|a| a.dry_run) {
            return Err(format!(
                "{path} contains dry-run rows (`\"dry_run\": true`). A dry run validates \
                 plumbing, not success rate — re-run without --dry-run to get a measurement."
            ));
        }
        let unscored = attempts.iter().filter(|a| a.ok.is_none()).count();
        if unscored > 0 {
            return Err(format!(
                "{path} has {unscored} unscored rows (`\"ok\": null`). Refusing to score a \
                 partial measurement."
            ));
        }

        let ok = attempts.iter().filter(|a| a.ok == Some(true)).count();
        let fail = attempts.len() - ok;
        Ok(Self {
            label: label.to_string(),
            attempts,
            ok,
            fail,
        })
    }

    fn total(&self) -> usize {
        self.attempts.len()
    }

    fn success_rate(&self) -> f64 {
        if self.attempts.is_empty() {
            0.0
        } else {
            self.ok as f64 / self.attempts.len() as f64
        }
    }

    /// Nearest-rank percentile of `elapsed_ms`, ignoring rows that lack it.
    fn percentile_ms(&self, percentile: f64) -> Option<u64> {
        let mut values: Vec<u64> = self.attempts.iter().filter_map(|a| a.elapsed_ms).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        let rank = ((percentile / 100.0) * values.len() as f64).ceil() as usize;
        let index = rank.saturating_sub(1).min(values.len() - 1);
        Some(values[index])
    }

    /// Failure reasons, grouped. `/` errors are truncated to their first clause
    /// so distinct messages of the same kind collapse together.
    fn failure_reasons(&self) -> BTreeMap<String, usize> {
        let mut reasons = BTreeMap::new();
        for attempt in &self.attempts {
            if attempt.ok == Some(true) {
                continue;
            }
            let key = match (&attempt.error, attempt.status) {
                (Some(error), _) => {
                    let head = error.split(':').next().unwrap_or(error).trim();
                    if head.is_empty() {
                        "unknown error".to_string()
                    } else {
                        head.to_string()
                    }
                }
                (None, Some(status)) => format!("HTTP {status}"),
                (None, None) => "unknown failure".to_string(),
            };
            *reasons.entry(key).or_insert(0) += 1;
        }
        reasons
    }

    fn per_proxy(&self) -> BTreeMap<String, (usize, usize)> {
        let mut by_proxy: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for attempt in &self.attempts {
            let key = attempt
                .proxy
                .clone()
                .unwrap_or_else(|| "(direct)".to_string());
            let entry = by_proxy.entry(key).or_insert((0, 0));
            if attempt.ok == Some(true) {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        by_proxy
    }

    /// Distinct post IDs seen. Low coverage means the ratio is measured over
    /// fewer articles than it looks like, and a handful of hard posts could be
    /// dominating the failures.
    fn distinct_posts(&self) -> usize {
        self.attempts
            .iter()
            .map(|a| a.post_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    /// Post IDs that failed, deduplicated and sorted. Directly actionable: these
    /// are the articles the candidate client cannot fetch.
    fn failing_posts(&self) -> Vec<&str> {
        self.attempts
            .iter()
            .filter(|a| a.ok == Some(false))
            .map(|a| a.post_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Scores baseline vs candidate. Returns `Ok(true)` when parity meets the gate.
pub fn report(
    baseline_path: &str,
    candidate_path: &str,
    threshold: f64,
    show_proxies: bool,
) -> Result<bool, String> {
    let baseline = Side::load("baseline", baseline_path)?;
    let candidate = Side::load("candidate", candidate_path)?;

    let baseline_rate = baseline.success_rate();
    let candidate_rate = candidate.success_rate();

    println!("SPIKE-1 — TLS impersonation parity (RUST_REWRITE_PLAN §3.1)");
    println!();
    println!(
        "{:<12} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9}",
        "side", "total", "ok", "fail", "rate", "p50 ms", "p95 ms"
    );
    for side in [&baseline, &candidate] {
        println!(
            "{:<12} {:>8} {:>8} {:>8} {:>8.2}% {:>9} {:>9}",
            side.label,
            side.total(),
            side.ok,
            side.fail,
            side.success_rate() * 100.0,
            side.percentile_ms(50.0)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            side.percentile_ms(95.0)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }

    println!();
    println!(
        "coverage: baseline {} attempts over {} distinct post IDs; candidate {} over {}",
        baseline.total(),
        baseline.distinct_posts(),
        candidate.total(),
        candidate.distinct_posts(),
    );

    for side in [&baseline, &candidate] {
        let reasons = side.failure_reasons();
        if reasons.is_empty() {
            continue;
        }
        println!("{} failure reasons:", side.label);
        for (reason, count) in reasons {
            println!("  {count:>6}  {reason}");
        }
    }

    let failing = candidate.failing_posts();
    if !failing.is_empty() {
        println!("candidate failing post IDs ({} distinct):", failing.len());
        for post_id in failing.iter().take(20) {
            println!("  {post_id}");
        }
        if failing.len() > 20 {
            println!("  ... and {} more", failing.len() - 20);
        }
    }

    if show_proxies {
        println!();
        println!("per-proxy success (candidate):");
        for (proxy, (ok, fail)) in candidate.per_proxy() {
            let total = ok + fail;
            let rate = if total == 0 {
                0.0
            } else {
                ok as f64 / total as f64 * 100.0
            };
            println!("  {rate:>6.2}%  {ok:>4}/{total:<4}  {proxy}");
        }
    }

    println!();
    if baseline_rate <= 0.0 {
        println!(
            "INCONCLUSIVE — the baseline success rate is 0.00%, so no ratio can be formed. \
             Either the pool is down or the baseline run was misconfigured."
        );
        return Ok(false);
    }

    let parity = candidate_rate / baseline_rate;
    let verdict = parity >= threshold;

    println!(
        "parity = candidate/baseline = {candidate_rate:.4} / {baseline_rate:.4} = {parity:.4}"
    );
    println!("gate   = parity >= {threshold:.4}");
    println!();
    if verdict {
        println!(
            "PASS — the candidate client meets the §3.1 gate. Proceed with the \
             impersonating `Transport` (option 1) as the `PostSource`."
        );
        // The ratio alone can pass while both sides are failing. `baseline_rate`
        // is printed above for that reason, but it is worth saying outright at
        // the moment someone reads the verdict and stops reading.
        if baseline_rate < 0.99 {
            println!(
                "       caveat: the baseline itself only reached {:.2}% — read the absolute \
                 rates above before treating this as a clean pass, and check that both \
                 sides failed on the same post IDs (the scorer does not compare the two \
                 files row by row).",
                baseline_rate * 100.0
            );
        }
    } else {
        println!(
            "FAIL — the candidate client does not meet the §3.1 gate. Per the plan, do \
             not abandon the rewrite: move to option 2 (libcurl-impersonate via FFI), and \
             if that also fails, option 3 (Python sidecar fetcher behind `PostSource`)."
        );
    }
    Ok(verdict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, rows: &[&str]) -> String {
        let path = std::env::temp_dir().join(format!("difftest-{name}.jsonl"));
        let mut file = File::create(&path).expect("create temp");
        for row in rows {
            writeln!(file, "{row}").expect("write temp");
        }
        path.to_string_lossy().into_owned()
    }

    fn row(post_id: &str, ok: bool, status: u16) -> String {
        format!(
            r#"{{"post_id":"{post_id}","proxy":"socks5://wgcf1:1080","ok":{ok},"status":{status},"error":null,"elapsed_ms":100,"dry_run":false}}"#
        )
    }

    #[test]
    fn identical_sides_pass_at_full_parity() {
        let rows: Vec<String> = (0..10).map(|_| row("abc", true, 200)).collect();
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let baseline = write_temp("base-pass", &refs);
        let candidate = write_temp("cand-pass", &refs);
        assert_eq!(report(&baseline, &candidate, 0.99, false), Ok(true));
    }

    #[test]
    fn one_percent_regression_sits_on_the_gate() {
        // 100 baseline attempts, all ok. Candidate: 99 ok, 1 failure -> parity
        // exactly 0.99, which must pass at `>= 0.99`.
        let base: Vec<String> = (0..100).map(|_| row("abc", true, 200)).collect();
        let base_refs: Vec<&str> = base.iter().map(String::as_str).collect();
        let baseline = write_temp("base-1pct", &base_refs);

        let mut cand = base.clone();
        cand[0] = row("abc", false, 403);
        let cand_refs: Vec<&str> = cand.iter().map(String::as_str).collect();
        let candidate = write_temp("cand-1pct", &cand_refs);

        assert_eq!(report(&baseline, &candidate, 0.99, false), Ok(true));
        // One more failure drops below the gate.
        let mut cand = base.clone();
        cand[0] = row("abc", false, 403);
        cand[1] = row("abc", false, 403);
        let cand_refs: Vec<&str> = cand.iter().map(String::as_str).collect();
        let candidate = write_temp("cand-2pct", &cand_refs);
        assert_eq!(report(&baseline, &candidate, 0.99, false), Ok(false));
    }

    #[test]
    fn dry_run_rows_are_refused_not_scored() {
        let rows = [
            r#"{"post_id":"a","proxy":null,"ok":null,"status":null,"error":null,"elapsed_ms":0,"dry_run":true}"#,
        ];
        let baseline = write_temp("base-dry", &rows);
        let candidate = write_temp("cand-dry", &rows);
        let err = report(&baseline, &candidate, 0.99, false).unwrap_err();
        assert!(err.contains("dry-run"), "unexpected error: {err}");
    }

    #[test]
    fn unscored_rows_are_refused_not_dropped() {
        let rows = [row("a", true, 200)];
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let baseline = write_temp("base-unscored", &refs);
        let partial = [
            r#"{"post_id":"a","proxy":null,"ok":null,"status":null,"error":null,"elapsed_ms":0,"dry_run":false}"#,
        ];
        let candidate = write_temp("cand-unscored", &partial);
        let err = report(&baseline, &candidate, 0.99, false).unwrap_err();
        assert!(err.contains("unscored"), "unexpected error: {err}");
    }

    #[test]
    fn zero_baseline_rate_is_inconclusive() {
        let rows = [row("a", false, 500)];
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let baseline = write_temp("base-zero", &refs);
        let candidate = write_temp("cand-zero", &refs);
        assert_eq!(report(&baseline, &candidate, 0.99, false), Ok(false));
    }
}
