//! Differential testing harness for the Freedium Rust rewrite.
//!
//! Implements the SPIKE-2 gate from RUST_REWRITE_PLAN §3.2: the Rust
//! `difflib.SequenceMatcher` port must make the **same boolean decision** as
//! CPython at the `> 80` threshold used by `core.py:261` and `core.py:276`.
//!
//! Usage:
//!
//! ```text
//! difftest gen-cases  --out cases.jsonl [--scale 1.0]
//! python3 py/difflib_ref.py --cases cases.jsonl --out ref.jsonl
//! difftest run-difflib --cases cases.jsonl --reference ref.jsonl [--show 10]
//! ```
//!
//! The gate is stricter than the plan requires: it demands bit-identical
//! `ratio()` values as well as identical decisions. Bit-identical ratios imply
//! identical decisions, so passing both is strictly safer — and since the port
//! performs the same float operations in the same order, bit-identity is
//! achievable rather than aspirational. A bit difference that happens not to
//! flip a decision today would still flip one at a different input, and
//! `core.py`'s inputs are arbitrary article text.
//!
//! §4's full harness (rendering whole GraphQL responses on both sides) is a
//! later phase; it needs a corpus dumped from the production `cache` table,
//! which is not available in this working tree.

mod cases;
mod impersonate;
mod prng;
mod render;
mod shadow;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::ExitCode;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use cases::Case;

/// Where `gen-render-cases` looks when `--fixtures` is not given, relative to the
/// workspace root — which is where `cargo run` puts the working directory.
const DEFAULT_FIXTURES_DIR: &str = "xtask/difftest/fixtures";

/// One line of the Python reference output.
///
/// Floats cross the boundary as raw IEEE-754 bit patterns so the comparison is
/// exact; decimal formatting would hide exactly the last-bit differences this
/// harness exists to find.
#[derive(Debug, Serialize, Deserialize)]
struct Reference {
    index: usize,
    ratio_bits: u64,
    pct_bits: u64,
    decision: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        usage();
        return ExitCode::FAILURE;
    };

    let opts = match Options::parse(&args[1..]) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("error: {err}");
            usage();
            return ExitCode::FAILURE;
        }
    };

    match command.as_str() {
        "gen-cases" => cmd_gen_cases(&opts),
        "run-difflib" => cmd_run_difflib(&opts),
        "gen-render-cases" => cmd_gen_render_cases(&opts),
        "run-render" => cmd_run_render(&opts),
        "spike-impersonate-report" => cmd_spike_impersonate_report(&opts),
        "gen-shadow-seed" => cmd_gen_shadow_seed(&opts),
        "shadow-report" => cmd_shadow_report(&opts),
        "help" | "-h" | "--help" => {
            usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("error: unknown command `{other}`");
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "\
difftest — differential testing harness (RUST_REWRITE_PLAN §4)

Commands:
  gen-cases    --out <path> [--scale <f>]
      Write the deterministic difflib case corpus as JSONL.
      --scale multiplies the randomised groups (default 1.0).

  run-difflib  --cases <path> --reference <path> [--show <n>]
      Compare the Rust port against the Python reference.
      Exits non-zero on any difference in ratio bits or in the boolean
      decision at the `> 80` threshold.

  gen-render-cases  --out <path> [--fixtures <dir>]
      Write the hand-written render fixtures as JSONL, ordered by file name.
      --fixtures defaults to `xtask/difftest/fixtures`.

  run-render  --cases <path> --reference <path> [--show <n>]
      Render every fixture with medium-doc + medium-render and compare
      against the legacy renderer, after both sides are reduced to the
      canonical form. Exits non-zero on an undeclared difference, on a
      declared difference that no longer happens, or on a corpus too thin
      to prove anything (see the degeneracy report).

  spike-impersonate-report  --baseline <path> --candidate <path>
                            [--threshold <f>] [--show-proxies]
      Score SPIKE-1: compare two attempt logs (curl_cffi baseline vs the
      Rust candidate) and apply the §3.1 parity gate.
      --threshold defaults to 0.99. --show-proxies adds a per-proxy
      breakdown. Never makes a network request.

  gen-shadow-seed  --database-url <url> [--fixtures <dir>] [--dry-run]
      Write every fixture's post_data into the `cache` table, so a local
      shadow run has a corpus with no network. Each fixture is stored
      under a 12-hex key derived from its *name*, not from its post_id:
      all of the fixtures share one post_id, so seeding by that would
      write a single row. Prints the table either way.
      --fixtures defaults to `xtask/difftest/fixtures`.
      --dry-run walks the corpus and prints it without writing.

  shadow-report  --log <path> [--declarations <file>] [--min-days <n>]
                 [--show <n>]
      Score a Fase 4 shadow log (SHADOW_LOG, one JSONL record per request).
      Exits non-zero on an undeclared difference, on a declaration that
      never fired, on fewer than --min-days consecutive clean days, and on
      a log too thin to have caught a broken edge (the degeneracy report).
      --declarations should be the file the edge ran with
      (SHADOW_DECLARATIONS); without it, dead declarations are not checked.
      --min-days defaults to 7, which is §5's gate.

  help
"
    );
}

fn cmd_spike_impersonate_report(opts: &Options) -> ExitCode {
    let (Some(baseline), Some(candidate)) = (opts.get("baseline"), opts.get("candidate")) else {
        eprintln!(
            "error: spike-impersonate-report requires --baseline <path> and --candidate <path>"
        );
        return ExitCode::FAILURE;
    };
    let threshold: f64 = match opts.get("threshold").map(str::parse) {
        Some(Ok(threshold)) if (0.0..=1.0).contains(&threshold) => threshold,
        Some(Ok(other)) => {
            eprintln!("error: --threshold must be within 0.0..=1.0, got {other}");
            return ExitCode::FAILURE;
        }
        Some(Err(err)) => {
            eprintln!("error: --threshold: {err}");
            return ExitCode::FAILURE;
        }
        None => 0.99,
    };
    let show_proxies = opts.has("show-proxies");

    match impersonate::report(baseline, candidate, threshold, show_proxies) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Seeds the `cache` table from the fixture corpus, for §6's local run.
///
/// The corpus *is* the render gate's fixtures — real GraphQL responses already on
/// disk — rather than a second corpus invented here. See `shadow::seed_post_id`
/// for why the key is derived from the fixture's name.
fn cmd_gen_shadow_seed(opts: &Options) -> ExitCode {
    let Some(database_url) = opts.get("database-url") else {
        eprintln!("error: gen-shadow-seed requires --database-url <postgres url>");
        return ExitCode::FAILURE;
    };
    let dir = opts.get("fixtures").unwrap_or(DEFAULT_FIXTURES_DIR);
    let dry_run = opts.has("dry-run");

    // Built here rather than with `#[tokio::main]` on `main`, so that every
    // other subcommand stays a plain synchronous program. This is the only one
    // that touches a database, and it should not make the rest carry a reactor.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: cannot start a tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    let seeded = match runtime.block_on(shadow::seed(dir, database_url, dry_run)) {
        Ok(seeded) => seeded,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // The mapping, printed unconditionally: it is what lets a request in a shadow
    // report be traced back to the fixture that produced it, and a diff that
    // named a fixture would otherwise be the only thing anyone had.
    println!(
        "{}{} fixture(s) from {dir}",
        if dry_run { "would seed " } else { "seeded " },
        seeded.paths.len()
    );
    println!();
    println!("{:<24} {:<14} request", "fixture", "cache key");
    for (name, path) in &seeded.paths {
        // The key is the path without its leading slash. Printed on the same row
        // as the fixture so the two are read together.
        println!("{:<24} {:<14} {path}", name, &path[1..]);
    }
    println!();
    println!("{} byte(s) of post_data in total", seeded.bytes);
    if dry_run {
        println!(
            "nothing was written: --dry-run. Drop it to seed, pointing --database-url at the \
             database both instances share."
        );
    }
    ExitCode::SUCCESS
}

/// Reads Fase 4's shadow log and applies §5's gate. See `shadow::report`.
fn cmd_shadow_report(opts: &Options) -> ExitCode {
    let Some(log) = opts.get("log") else {
        eprintln!("error: shadow-report requires --log <path>");
        return ExitCode::FAILURE;
    };
    let min_days: u32 = match opts.get("min-days").map(str::parse) {
        Some(Ok(days)) if days > 0 => days,
        Some(Ok(_)) => {
            eprintln!("error: --min-days must be at least 1");
            return ExitCode::FAILURE;
        }
        Some(Err(err)) => {
            eprintln!("error: --min-days: {err}");
            return ExitCode::FAILURE;
        }
        None => shadow::DEFAULT_MIN_DAYS,
    };
    let show: usize = match opts.get("show").map(str::parse) {
        Some(Ok(show)) => show,
        Some(Err(err)) => {
            eprintln!("error: --show: {err}");
            return ExitCode::FAILURE;
        }
        None => shadow::DEFAULT_SHOW,
    };

    match shadow::report(log, opts.get("declarations"), min_days, show) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_gen_cases(opts: &Options) -> ExitCode {
    let Some(out) = opts.get("out") else {
        eprintln!("error: gen-cases requires --out <path>");
        return ExitCode::FAILURE;
    };
    let scale = match opts.get("scale").map(str::parse::<f64>) {
        Some(Ok(scale)) if scale > 0.0 => scale,
        Some(Ok(_)) => {
            eprintln!("error: --scale must be positive");
            return ExitCode::FAILURE;
        }
        Some(Err(err)) => {
            eprintln!("error: --scale: {err}");
            return ExitCode::FAILURE;
        }
        None => 1.0,
    };

    let corpus = cases::generate(scale);

    let file = match File::create(out) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("error: cannot create {out}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut writer = BufWriter::new(file);
    for case in &corpus {
        if let Err(err) = serde_json::to_writer(&mut writer, case) {
            eprintln!("error: serialising case {}: {err}", case.index);
            return ExitCode::FAILURE;
        }
        if let Err(err) = writer.write_all(b"\n") {
            eprintln!("error: writing {out}: {err}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(err) = writer.flush() {
        eprintln!("error: flushing {out}: {err}");
        return ExitCode::FAILURE;
    }

    let mut per_group: BTreeMap<&str, usize> = BTreeMap::new();
    let mut max_chars = 0usize;
    for case in &corpus {
        *per_group.entry(case.group.as_str()).or_default() += 1;
        max_chars = max_chars.max(case.a.chars().count().max(case.b.chars().count()));
    }
    println!("wrote {} cases to {out}", corpus.len());
    for (group, count) in per_group {
        println!("  {group:>18}  {count}");
    }
    println!("longest sequence: {max_chars} code points");
    ExitCode::SUCCESS
}

fn cmd_gen_render_cases(opts: &Options) -> ExitCode {
    let Some(out) = opts.get("out") else {
        eprintln!("error: gen-render-cases requires --out <path>");
        return ExitCode::FAILURE;
    };
    let dir = opts.get("fixtures").unwrap_or(DEFAULT_FIXTURES_DIR);

    let fixtures = match render::load_fixtures(dir) {
        Ok(fixtures) => fixtures,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = render::write_cases(&fixtures, out) {
        eprintln!("error: {err}");
        return ExitCode::FAILURE;
    }

    let declared = fixtures
        .iter()
        .filter(|fixture| fixture.expected_divergence.is_some())
        .count();
    println!("wrote {} cases to {out}", fixtures.len());
    for fixture in &fixtures {
        let marker = if fixture.expected_divergence.is_some() {
            "  (difference declared)"
        } else {
            ""
        };
        println!("  {:<24}{marker}", fixture.name);
    }
    println!("{} fixture(s) declare an expected divergence", declared);
    ExitCode::SUCCESS
}

fn cmd_run_render(opts: &Options) -> ExitCode {
    let (Some(cases_path), Some(reference_path)) = (opts.get("cases"), opts.get("reference"))
    else {
        eprintln!("error: run-render requires --cases <path> and --reference <path>");
        return ExitCode::FAILURE;
    };
    let show: usize = match opts.get("show").map(str::parse) {
        Some(Ok(show)) => show,
        Some(Err(err)) => {
            eprintln!("error: --show: {err}");
            return ExitCode::FAILURE;
        }
        None => 5,
    };

    match render::run(cases_path, reference_path, show) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_run_difflib(opts: &Options) -> ExitCode {
    let (Some(cases_path), Some(reference_path)) = (opts.get("cases"), opts.get("reference"))
    else {
        eprintln!("error: run-difflib requires --cases <path> and --reference <path>");
        return ExitCode::FAILURE;
    };
    let show: usize = match opts.get("show").map(str::parse) {
        Some(Ok(show)) => show,
        Some(Err(err)) => {
            eprintln!("error: --show: {err}");
            return ExitCode::FAILURE;
        }
        None => 10,
    };

    let corpus: Vec<Case> = match read_jsonl(cases_path) {
        Ok(corpus) => corpus,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let reference: Vec<Reference> = match read_jsonl(reference_path) {
        Ok(reference) => reference,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    if corpus.len() != reference.len() {
        eprintln!(
            "error: {} cases but {} reference rows — the reference was generated from a \
             different corpus",
            corpus.len(),
            reference.len()
        );
        return ExitCode::FAILURE;
    }

    println!(
        "difflib parity: {} cases, {} reference rows",
        corpus.len(),
        reference.len()
    );
    println!(
        "gate §3.2: boolean decision at `> 80` must match 100%; this harness also \
         requires bit-identical ratios\n"
    );

    #[derive(Default, Clone, Copy)]
    struct Stats {
        total: usize,
        ratio_bit_diff: usize,
        pct_bit_diff: usize,
        decision_diff: usize,
        decisions_true: usize,
        exact_boundary: usize,
        near_boundary: usize,
    }

    let mut per_group: BTreeMap<&str, Stats> = BTreeMap::new();
    let mut overall = Stats::default();
    let mut shown = 0usize;

    for (case, reference) in corpus.iter().zip(&reference) {
        if case.index != reference.index {
            eprintln!(
                "error: case/reference index mismatch at position: {} vs {}",
                case.index, reference.index
            );
            return ExitCode::FAILURE;
        }

        let ratio = medium_doc::difflib::ratio_of_str(&case.a, &case.b);
        let pct = medium_doc::difflib::percentage_of_match(Some(&case.a), Some(&case.b));
        let decision = medium_doc::difflib::is_match_over_80(Some(&case.a), Some(&case.b));

        let ratio_bits = ratio.to_bits();
        let pct_bits = pct.to_bits();
        let ratio_diff = ratio_bits != reference.ratio_bits;
        let pct_diff = pct_bits != reference.pct_bits;
        let decision_diff = decision != reference.decision;

        let stats = per_group.entry(case.group.as_str()).or_default();
        stats.total += 1;
        overall.total += 1;
        if ratio_diff {
            stats.ratio_bit_diff += 1;
            overall.ratio_bit_diff += 1;
        }
        if pct_diff {
            stats.pct_bit_diff += 1;
            overall.pct_bit_diff += 1;
        }
        if decision_diff {
            stats.decision_diff += 1;
            overall.decision_diff += 1;
        }
        if decision {
            stats.decisions_true += 1;
            overall.decisions_true += 1;
        }
        if pct == 80.0 {
            stats.exact_boundary += 1;
            overall.exact_boundary += 1;
        }
        if (pct - 80.0).abs() < 1.0 {
            stats.near_boundary += 1;
            overall.near_boundary += 1;
        }

        if (ratio_diff || pct_diff || decision_diff) && shown < show {
            shown += 1;
            println!(
                "--- MISMATCH #{} [{}] {}",
                case.index, case.group, case.note
            );
            println!(
                "    a = {} ({} chars)",
                preview(&case.a),
                case.a.chars().count()
            );
            println!(
                "    b = {} ({} chars)",
                preview(&case.b),
                case.b.chars().count()
            );
            if ratio_diff {
                println!(
                    "    ratio   py={:e} rust={:e}  (py={} rust={})",
                    f64::from_bits(reference.ratio_bits),
                    ratio,
                    reference.ratio_bits,
                    ratio_bits
                );
            }
            if pct_diff {
                println!(
                    "    pct     py={:e} rust={:e}  (py={} rust={})",
                    f64::from_bits(reference.pct_bits),
                    pct,
                    reference.pct_bits,
                    pct_bits
                );
            }
            if decision_diff {
                println!(
                    "    decision py={} rust={}   <-- gate failure",
                    reference.decision, decision
                );
            }
        }
    }

    println!();
    println!(
        "{:<18} {:>7} {:>9} {:>9} {:>9} {:>8} {:>9}",
        "group", "cases", "ratio≠", "pct≠", "decide≠", "true", "≈80"
    );
    for (group, stats) in &per_group {
        println!(
            "{:<18} {:>7} {:>9} {:>9} {:>9} {:>8} {:>9}",
            group,
            stats.total,
            stats.ratio_bit_diff,
            stats.pct_bit_diff,
            stats.decision_diff,
            stats.decisions_true,
            stats.near_boundary,
        );
    }
    println!(
        "{:<18} {:>7} {:>9} {:>9} {:>9} {:>8} {:>9}",
        "TOTAL",
        overall.total,
        overall.ratio_bit_diff,
        overall.pct_bit_diff,
        overall.decision_diff,
        overall.decisions_true,
        overall.near_boundary,
    );
    println!();
    println!(
        "boundary coverage: {} cases with pct exactly 80.0, {} within ±1.0 of 80 \
         (these are the cases where a last-bit difference flips the decision)",
        overall.exact_boundary, overall.near_boundary
    );

    let blockers = degeneracies(
        overall.total,
        overall.exact_boundary,
        overall.decisions_true,
    );
    if !blockers.is_empty() {
        println!();
        println!("DEGENERATE CORPUS — a PASS here would prove nothing:");
        for reason in &blockers {
            println!("  - {reason}");
        }
    }

    let passed = overall.decision_diff == 0
        && overall.ratio_bit_diff == 0
        && overall.pct_bit_diff == 0
        && blockers.is_empty();
    println!();
    if passed {
        println!(
            "PASS — {} cases, 0 decision differences, 0 ratio differences",
            overall.total
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "FAIL — {} decision differences, {} ratio differences, {} pct differences \
             out of {} cases",
            overall.decision_diff, overall.ratio_bit_diff, overall.pct_bit_diff, overall.total
        );
        if !blockers.is_empty() {
            println!(
                "       and the corpus does not exercise the gate ({} reason(s) above), \
                 so a clean comparison is not sufficient evidence",
                blockers.len()
            );
        }
        ExitCode::FAILURE
    }
}

/// Reasons the corpus cannot distinguish a correct port from a broken one.
///
/// A 100% match is only evidence if the corpus is capable of producing a
/// mismatch. If every case decides the same way, or none sits on the threshold,
/// the gate passes for any implementation that is consistently wrong in the same
/// direction — so treat that as a failure of the harness, not a success of the
/// port. Returns an empty vector when the corpus is sound.
fn degeneracies(total: usize, exact_boundary: usize, decisions_true: usize) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if total == 0 {
        reasons.push("the corpus is empty");
    }
    if decisions_true == 0 {
        reasons.push("every case decides `false`, so the threshold is never crossed");
    }
    if decisions_true == total && total > 0 {
        reasons.push("every case decides `true`, so the threshold is never missed");
    }
    if exact_boundary == 0 {
        reasons.push(
            "no case lands exactly on 80.0, so the strict `>` boundary is untested \
             (`>=` would pass this gate)",
        );
    }
    reasons
}

/// Renders a string for diagnostics: escapes control characters, truncates to
/// 80 chars, and marks where it was cut.
fn preview(s: &str) -> String {
    let mut out = String::from("\"");
    let chars: Vec<char> = s.chars().collect();
    for c in chars.iter().take(80) {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c.is_control()) => out.push_str(&format!("\\u{{{:x}}}", *c as u32)),
            c => out.push(*c),
        }
    }
    out.push('"');
    if chars.len() > 80 {
        out.push_str(&format!(" …(+{} chars)", chars.len() - 80));
    }
    out
}

fn read_jsonl<T: DeserializeOwned>(path: &str) -> Result<Vec<T>, String> {
    let file = File::open(path).map_err(|err| format!("cannot open {path}: {err}"))?;
    let mut rows = Vec::new();
    for (lineno, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|err| format!("{path}:{}: {err}", lineno + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let row =
            serde_json::from_str(&line).map_err(|err| format!("{path}:{}: {err}", lineno + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

/// Minimal `--key value` / `--flag` parser. Avoids a `clap` dependency for what
/// is a handful of options.
struct Options {
    values: BTreeMap<String, String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.next() {
            let Some(key) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument `{arg}`"));
            };
            // A value is whatever follows that is not itself another `--key`.
            // Anything else means this was a bare flag, stored as an empty
            // string and detected with `has`.
            let value = match iter.peek() {
                Some(next) if !next.starts_with("--") => iter.next().cloned().unwrap_or_default(),
                _ => String::new(),
            };
            values.insert(key.to_string(), value);
        }
        Ok(Self { values })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// True when a bare `--flag` was passed.
    fn has(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::degeneracies;

    #[test]
    fn sound_corpus_has_no_complaints() {
        // Real numbers from the full 5810-case run.
        assert!(degeneracies(5810, 51, 1777).is_empty());
    }

    #[test]
    fn all_true_corpus_is_rejected() {
        // Every case crossing the threshold means `>= 80` would pass too.
        assert!(!degeneracies(100, 5, 100).is_empty());
    }

    #[test]
    fn all_false_corpus_is_rejected() {
        assert!(!degeneracies(100, 5, 0).is_empty());
    }

    #[test]
    fn missing_exact_boundary_is_rejected() {
        // The strict `>` is only tested when something lands exactly on 80.0.
        let reasons = degeneracies(100, 0, 50);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("exactly on 80.0"));
    }

    #[test]
    fn empty_corpus_is_rejected() {
        assert!(!degeneracies(0, 0, 0).is_empty());
    }
}
