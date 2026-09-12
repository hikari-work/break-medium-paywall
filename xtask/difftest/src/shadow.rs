//! The shadow report — `RUST_REWRITE_PLAN` §5's gate, read off Fase 4's log.
//!
//! `edge/freedium-edge` writes one JSONL line per request it proxies
//! ([`page_canonical::record`]); this reads that file back and decides whether
//! the run it describes is *evidence*. Three things it refuses to accept, in the
//! shape of the render gate's own refusals ([`render`](crate::render)):
//!
//! - an **undeclared difference** — §5's gate is "no semantic difference on real
//!   traffic", and a difference no declaration accounts for is the whole of what
//!   it forbids;
//! - a **dead declaration** — the discipline `render.rs` applies to
//!   `expected_divergence`, for the same reason. An allowance that has stopped
//!   firing is an allowance nobody is checking, and the next thing to land on
//!   that path and kind will be absorbed by it;
//! - a **thin corpus** — a log whose comparisons all came out one way, or which
//!   never canonicalised a page body at all, passes for *any* edge, including one
//!   that has quietly stopped shadowing. See [`degeneracies`].
//!
//! # The counter §5 actually asks for
//!
//! §5's gate is a duration — "seven consecutive days" — and the plan is specific
//! about which duration: *"the consecutive-days-without-an-undeclared-difference
//! counter"*. So that is what this counts, not a ratio and not a proxy for one.
//!
//! It buckets the log by UTC date and walks *backwards* from the last day,
//! counting while the day is clean, has at least one comparison in it, and is
//! the calendar day before the one already counted. A **missing day breaks the
//! run**: a day with no traffic is not a day without a difference, it is a day
//! without evidence, and a soak that was restarted halfway through has not run
//! for seven days.
//!
//! A *declared* difference does not break a day, and that is deliberate: it is
//! accounted for, and the point of a declarations file is that a run keeps going
//! while an allowance is still exercising what it was written for. What keeps
//! that from being a hole is the dead-declaration check on the other side — you
//! cannot extend the counter by declaring everything, because a declaration
//! nothing matches fails the run — together with the details printed against
//! each declaration, which show whether it is still excusing what it was written
//! for rather than something new.
//!
//! `--min-days` defaults to [`DEFAULT_MIN_DAYS`], which is the gate. The local
//! proof of §6 passes `--min-days 1`, and the report says out loud that it is
//! not running the gate rather than letting a one-day run read as one.
//!
//! # Reading the two latency columns
//!
//! They are **not a race**, and the report does not print a ratio between them.
//! The shadow's clock starts after the primary's response has already been
//! written to the client, and it measures a second instance against a cache the
//! primary filled a moment earlier. They are here because §5 asks for them and
//! because the *shape* is worth watching — a shadow whose p99 runs away is the
//! edge's own cost becoming visible — not because the smaller number is better.
//! [`percentile`] is nearest-rank for the same reason: an interpolated p99 would
//! be a latency no request had.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use page_canonical::{Declarations, RecordOutcome, ShadowRecord};

/// §5's gate, as the default. See the module docs.
pub const DEFAULT_MIN_DAYS: u32 = 7;

/// Milliseconds in a day. The log's `ts_ms` is Unix milliseconds.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// How many distinct difference details are printed per group, by default.
pub const DEFAULT_SHOW: usize = 5;

/// Every outcome, in the order the report prints them: the comparisons first,
/// then the ways a request can fail to become one.
///
/// Spelled out rather than derived, because the order is a presentation decision
/// and the *completeness* is checked by [`the_report_covers_every_outcome`]
/// against a count — so adding a variant to `RecordOutcome` fails a test here
/// rather than dropping a row from the table.
const ALL_OUTCOMES: [RecordOutcome; 8] = [
    RecordOutcome::Identical,
    RecordOutcome::StatusOnly,
    RecordOutcome::Declared,
    RecordOutcome::Different,
    RecordOutcome::Declined,
    RecordOutcome::Unreachable,
    RecordOutcome::Excluded,
    RecordOutcome::PrimaryError,
];

/// Reads a shadow log and reports on it. Returns `true` when the run passes.
///
/// `declarations_path` is **the file the edge was configured with**
/// (`SHADOW_DECLARATIONS`), not a fresh one: the log's `declared` outcomes were
/// decided by the edge, and the only question left for the report is whether
/// every declaration in that file ever fired. Given a different file, the
/// dead-declaration check measures the wrong thing.
pub fn report(
    log_path: &str,
    declarations_path: Option<&str>,
    min_days: u32,
    show: usize,
) -> Result<bool, String> {
    let records: Vec<ShadowRecord> = crate::read_jsonl(log_path)?;
    let declarations = match declarations_path {
        Some(path) => {
            let json =
                fs::read_to_string(path).map_err(|err| format!("cannot read {path}: {err}"))?;
            let parsed = Declarations::from_json(&json).map_err(|err| format!("{path}: {err}"))?;
            Some((path.to_string(), parsed))
        }
        None => None,
    };

    let mut tally = Tally::default();
    for record in &records {
        tally.add(record);
    }

    println!("shadow report: {log_path}");
    println!("{}", tally.headline(&records));

    tally.print_outcomes();
    tally.print_reasons();
    let undeclared = tally.print_differences(show);
    tally.print_declared(show);
    let dead = print_declarations(declarations.as_ref(), &tally.fired_paths());
    tally.print_latency();
    tally.print_error_rates(&records);
    let days = tally.print_gate(min_days);

    let blockers = degeneracies(&tally);
    if !blockers.is_empty() {
        println!();
        println!("DEGENERATE RUN — a PASS here would prove nothing:");
        for reason in &blockers {
            println!("  - {reason}");
        }
    }

    let passed = undeclared == 0 && dead.is_empty() && blockers.is_empty() && days >= min_days;
    println!();
    if passed {
        println!(
            "PASS — {} comparison(s), 0 undeclared differences, {days} clean day(s) \
             (of {min_days} required)",
            tally.comparisons
        );
        return Ok(true);
    }

    let mut failures = Vec::new();
    if undeclared > 0 {
        failures.push(format!(
            "{undeclared} request(s) with an undeclared difference"
        ));
    }
    if !dead.is_empty() {
        failures.push(format!("{} dead declaration(s)", dead.len()));
    }
    if days < min_days {
        failures.push(format!(
            "{days} consecutive clean day(s), {min_days} required"
        ));
    }
    if !blockers.is_empty() {
        failures.push(format!("{} corpus degeneracy reason(s)", blockers.len()));
    }
    println!("FAIL — {}", failures.join(", "));
    Ok(false)
}

/// Everything the report counts, in one pass over the log.
#[derive(Default)]
struct Tally {
    by_outcome: BTreeMap<RecordOutcome, usize>,
    /// Exclusions and declines are grouped by the reason token the edge wrote —
    /// which is the point of those reasons being tokens
    /// (`record::SHADOW_NO_FETCH`, and the edge's `eligibility` module).
    by_exclusion: BTreeMap<String, usize>,
    by_decline: BTreeMap<String, usize>,
    /// Unreachable reasons are prose, so they are grouped by their leading token
    /// (`connect`, `timeout`, … — see [`unreachable_class`]) with the first full
    /// text seen kept to print alongside.
    by_unreachable: BTreeMap<String, (usize, String)>,
    /// Undeclared differences, by request, each with every distinct `detail`
    /// seen. Keyed by the request label ([`request_label`]), so two requests for
    /// the same path with different queries are not merged.
    differences: BTreeMap<String, Vec<String>>,
    /// Declared differences, by *declaration path* — the same key
    /// [`print_declarations`] uses to find the ones that never fired.
    declared: BTreeMap<String, Fired>,
    /// Latency and size, over the requests that have both answers. See the
    /// module docs for why the two sides can only be read against each other.
    primary_ms: Vec<f64>,
    shadow_ms: Vec<f64>,
    primary_bytes: Vec<usize>,
    /// Requests keyed by UTC day, for §5's counter. See the module docs.
    by_day: BTreeMap<i64, DayStat>,
    /// Distinct paths that reached a comparison. The render gate's `outputs`
    /// set, one level up: distinct pages rather than distinct renderings.
    compared_paths: BTreeSet<String>,
    comparisons: usize,
    identical: usize,
    status_only: usize,
}

/// One declaration, and what it excused.
#[derive(Default)]
struct Fired {
    count: usize,
    /// The `reason` as the *edge* recorded it, for the report to echo back. From
    /// the record rather than from the declarations file, because what is being
    /// audited is what the edge believed, not what the file says today.
    reason: String,
    details: BTreeSet<String>,
}

/// One day's evidence. See the module docs for what makes a day count.
#[derive(Default)]
struct DayStat {
    /// Comparisons that reached a verdict on this day — the evidence.
    comparisons: usize,
    /// Of those, the ones nothing declared. What breaks the run.
    undeclared: usize,
}

impl Tally {
    fn add(&mut self, record: &ShadowRecord) {
        *self.by_outcome.entry(record.outcome).or_default() += 1;

        match record.outcome {
            RecordOutcome::Excluded => {
                *self.by_exclusion.entry(reason_token(record)).or_default() += 1;
            }
            RecordOutcome::Declined => {
                *self.by_decline.entry(reason_token(record)).or_default() += 1;
            }
            RecordOutcome::Unreachable => {
                let reason = record.reason.as_deref().unwrap_or("(no reason given)");
                let entry = self
                    .by_unreachable
                    .entry(unreachable_class(reason))
                    .or_insert_with(|| (0, reason.to_string()));
                entry.0 += 1;
            }
            RecordOutcome::Different => {
                let detail = record
                    .detail
                    .clone()
                    .unwrap_or_else(|| "(no detail recorded)".to_string());
                let seen = self.differences.entry(request_label(record)).or_default();
                if !seen.contains(&detail) {
                    seen.push(detail);
                }
            }
            RecordOutcome::Declared => {
                let matched = record.declared.as_ref();
                let entry = self
                    .declared
                    .entry(
                        matched
                            .map(|it| it.path.clone())
                            .unwrap_or_else(|| "(unnamed declaration)".to_string()),
                    )
                    .or_default();
                entry.count += 1;
                if entry.reason.is_empty() {
                    entry.reason = matched.map(|it| it.reason.clone()).unwrap_or_default();
                }
                if let Some(detail) = record.detail.clone() {
                    entry.details.insert(detail);
                }
            }
            // The outcomes that compared nothing. Counted in `by_outcome` and in
            // the error rates; there is nothing else to say about them.
            RecordOutcome::Identical | RecordOutcome::StatusOnly | RecordOutcome::PrimaryError => {}
        }

        // §5's counter. Every record is bucketed, including the ones that never
        // reached a comparison — see `clean_days`, which treats a day of those
        // as a gap rather than as a clean day.
        let stat = self.by_day.entry(day_of(record.ts_ms)).or_default();
        if record.outcome.is_comparison() {
            stat.comparisons += 1;
            self.comparisons += 1;
            self.compared_paths.insert(record.path.clone());
        }
        if record.outcome == RecordOutcome::Different {
            stat.undeclared += 1;
        }
        match record.outcome {
            RecordOutcome::Identical => self.identical += 1,
            RecordOutcome::StatusOnly => self.status_only += 1,
            _ => {}
        }

        // Only the requests with both answers, so the two columns describe the
        // same population and can be read against each other.
        if let Some(shadow) = record.shadow.as_ref() {
            self.primary_ms.push(record.primary.ms);
            self.primary_bytes.push(record.primary.bytes);
            self.shadow_ms.push(shadow.ms);
        }
    }

    fn count(&self, outcome: RecordOutcome) -> usize {
        self.by_outcome.get(&outcome).copied().unwrap_or(0)
    }

    fn records(&self) -> usize {
        self.by_outcome.values().sum()
    }

    /// §5's counter, as a number. Split from its printing so a test can assert
    /// it without capturing stdout.
    ///
    /// Walks the days backwards from the most recent. The run stops at the first
    /// day that is not the calendar day before the last one counted, at a day
    /// with an undeclared difference, and at a day that produced no comparison
    /// at all — the last of those being the case the module docs argue about.
    fn clean_days(&self) -> u32 {
        let mut clean = 0u32;
        let mut expected: Option<i64> = None;
        for (day, stat) in self.by_day.iter().rev() {
            if stat.undeclared > 0 || stat.comparisons == 0 {
                break;
            }
            if expected.is_some_and(|expected| *day != expected) {
                break;
            }
            expected = Some(day - 1);
            clean += 1;
        }
        clean
    }

    /// The declaration patterns the log recorded as firing. The complement, in
    /// the declarations file, is what [`print_declarations`] reports as dead.
    fn fired_paths(&self) -> BTreeSet<String> {
        self.declared.keys().cloned().collect()
    }

    fn headline(&self, records: &[ShadowRecord]) -> String {
        let (Some(first), Some(last)) = (records.first(), records.last()) else {
            return "the log is empty".to_string();
        };
        // The log is appended to in request order, so the first and last lines
        // are the ends of the run. Not asserted: a record out of order is a
        // curiosity, not a reason to refuse an otherwise good log.
        let (first_day, last_day) = (day_of(first.ts_ms), day_of(last.ts_ms));
        format!(
            "{} record(s), {} .. {} ({} day(s)), {} comparison(s)",
            records.len(),
            format_day(first_day),
            format_day(last_day),
            last_day - first_day + 1,
            self.comparisons,
        )
    }

    fn print_outcomes(&self) {
        let total = self.records();
        println!();
        println!("{:<14} {:>6}  share", "outcome", "count");
        for outcome in ALL_OUTCOMES {
            let count = self.count(outcome);
            let share = if total == 0 {
                0.0
            } else {
                100.0 * count as f64 / total as f64
            };
            println!("{:<14} {:>6}  {share:>5.1}%", outcome.token(), count);
        }
    }

    fn print_reasons(&self) {
        print_reason_table("excluded", &self.by_exclusion);
        print_reason_table("declined", &self.by_decline);

        if !self.by_unreachable.is_empty() {
            println!();
            println!("unreachable, by failure class:");
            let mut classes: Vec<(&String, &(usize, String))> =
                self.by_unreachable.iter().collect();
            classes.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
            for (class, (count, sample)) in classes {
                println!("  {class:<10} {count:>5}   e.g. {}", crate::preview(sample));
            }
        }
    }

    /// Prints the undeclared differences and returns how many requests had one.
    fn print_differences(&self, show: usize) -> usize {
        if self.differences.is_empty() {
            println!();
            println!("undeclared differences: none");
            return 0;
        }

        let count = self.differences.len();
        println!();
        println!("UNDECLARED DIFFERENCES ({count} request(s)) — this is what fails the gate:");
        for (label, details) in worst_first(&self.differences) {
            println!("  {label}");
            println!("      {} distinct detail(s):", details.len());
            for detail in details.iter().take(show) {
                println!("        {detail}");
            }
            if details.len() > show {
                println!("        …(+{} more)", details.len() - show);
            }
        }
        count
    }

    fn print_declared(&self, show: usize) {
        if self.declared.is_empty() {
            return;
        }

        println!();
        println!(
            "declared differences ({} declaration(s) fired):",
            self.declared.len()
        );
        for (path, fired) in &self.declared {
            println!("  {path}  {} request(s)", fired.count);
            if !fired.reason.is_empty() {
                println!("      reason: {}", fired.reason);
            }
            // The full text, not a count: a declaration matches a *class* of
            // differences, so the only way to see that it is still excusing what
            // it was written for — rather than something new that landed on the
            // same path and kind — is to read the difference itself.
            println!("      {} distinct detail(s):", fired.details.len());
            for detail in fired.details.iter().take(show) {
                println!("        {detail}");
            }
            if fired.details.len() > show {
                println!("        …(+{} more)", fired.details.len() - show);
            }
        }
    }

    fn print_latency(&self) {
        if self.primary_ms.is_empty() {
            println!();
            println!("latency: no request had both answers, so there is nothing to compare");
            return;
        }
        let mut primary = self.primary_ms.clone();
        let mut shadow = self.shadow_ms.clone();
        primary.sort_by(f64::total_cmp);
        shadow.sort_by(f64::total_cmp);

        println!();
        println!(
            "latency (ms) over the {} request(s) with both answers — see the module docs \
             for why this is not a race:",
            primary.len()
        );
        println!("{:<9} {:>9} {:>9} {:>9}", "", "p50", "p99", "max");
        for (name, values) in [("primary", &primary), ("shadow", &shadow)] {
            println!(
                "{name:<9} {:>9} {:>9} {:>9}",
                ms(percentile(values, 0.5)),
                ms(percentile(values, 0.99)),
                ms(values.last().copied()),
            );
        }
        println!(
            "  median response size, primary: {} byte(s)",
            median(&self.primary_bytes)
        );
    }

    fn print_error_rates(&self, records: &[ShadowRecord]) {
        let total = records.len();
        if total == 0 {
            return;
        }
        let non_success = records
            .iter()
            .filter(|record| !(200..300).contains(&record.primary.status))
            .count();
        let share = |count: usize| 100.0 * count as f64 / total as f64;

        println!();
        println!("rates, over {total} record(s):");
        println!(
            "  primary non-2xx        {:>5}  {:>5.1}%",
            non_success,
            share(non_success)
        );
        println!(
            "  primary error          {:>5}  {:>5.1}%   (the edge failed to proxy)",
            self.count(RecordOutcome::PrimaryError),
            share(self.count(RecordOutcome::PrimaryError))
        );
        println!(
            "  shadow unreachable     {:>5}  {:>5.1}%   (the shadow did not answer)",
            self.count(RecordOutcome::Unreachable),
            share(self.count(RecordOutcome::Unreachable))
        );
        println!(
            "  shadow declined        {:>5}  {:>5.1}%   (cache miss, and it may not fetch)",
            self.count(RecordOutcome::Declined),
            share(self.count(RecordOutcome::Declined))
        );
        println!(
            "  excluded by the edge   {:>5}  {:>5.1}%",
            self.count(RecordOutcome::Excluded),
            share(self.count(RecordOutcome::Excluded))
        );
    }

    /// §5's counter. Prints it and returns the run length, for the verdict.
    fn print_gate(&self, min_days: u32) -> u32 {
        let clean = self.clean_days();
        let dirty = self
            .by_day
            .values()
            .filter(|stat| stat.undeclared > 0)
            .count();

        println!();
        println!(
            "§5 gate: {clean} consecutive day(s) with no undeclared difference, needing \
             {min_days} ({} day(s) in the log, {dirty} with an undeclared difference)",
            self.by_day.len()
        );
        if min_days != DEFAULT_MIN_DAYS {
            println!(
                "  note: --min-days is {min_days}, not the §5 default of {DEFAULT_MIN_DAYS} — \
                 this run is not §5's gate, and the report says so rather than letting a \
                 short run read as a long one"
            );
        }
        if let (Some(first), Some(last)) = (
            self.by_day.keys().next().copied(),
            self.by_day.keys().next_back().copied(),
        ) {
            println!("  window: {} .. {}", format_day(first), format_day(last));
        }
        clean
    }
}

fn print_reason_table(title: &str, table: &BTreeMap<String, usize>) {
    if table.is_empty() {
        return;
    }
    println!();
    println!("{title}, by reason:");
    for (reason, count) in table {
        println!("  {reason:<18} {count:>5}");
    }
}

/// The declarations that never fired. Returns them, for the verdict.
///
/// `fired` holds the declaration *patterns* the log recorded. A declaration
/// whose path is not among them excused nothing during this run — the same
/// failure the render gate calls a stale `expected_divergence`, and treated the
/// same way: the allowance is removed or the corpus is fixed, because an
/// allowance nobody is exercising will eventually hide something.
fn print_declarations(
    declarations: Option<&(String, Declarations)>,
    fired: &BTreeSet<String>,
) -> Vec<String> {
    let Some((path, declarations)) = declarations else {
        println!();
        println!(
            "declarations: none supplied — pass `--declarations` with the file the edge was \
             configured with (SHADOW_DECLARATIONS) to have dead ones checked"
        );
        return Vec::new();
    };

    if declarations.is_empty() {
        println!();
        println!("declarations: {path} declares nothing");
        return Vec::new();
    }

    let dead: Vec<String> = declarations
        .all()
        .iter()
        .filter(|declaration| !fired.contains(&declaration.path))
        .map(|declaration| {
            format!(
                "{} ({:?}) — never fired during this run: {}",
                declaration.path, declaration.kind, declaration.reason
            )
        })
        .collect();

    println!();
    if dead.is_empty() {
        println!(
            "declarations: {} in {path}, all of them fired",
            declarations.all().len()
        );
    } else {
        println!(
            "DEAD DECLARATIONS ({} of {} in {path}):",
            dead.len(),
            declarations.all().len()
        );
        for reason in &dead {
            println!("  - {reason}");
        }
    }
    dead
}

/// Reasons the log cannot distinguish a working shadow from a broken one.
///
/// The same discipline as [`render`](crate::render)'s `degeneracies`, and the
/// same argument: a differential gate is only evidence if it *could* have
/// failed. Every reason here describes a log that would have come out the same
/// had the edge been misconfigured to shadow nothing — which is exactly the
/// failure mode a soak is least likely to notice, because a shadow that does
/// nothing writes a log with no differences in it.
///
/// Takes the tally rather than the records because every reason is a count, and
/// the counts are already there.
fn degeneracies(tally: &Tally) -> Vec<String> {
    let mut reasons = Vec::new();
    let records = tally.records();

    if records == 0 {
        reasons.push("the log is empty — no request reached the edge".to_string());
        return reasons;
    }
    if tally.comparisons == 0 {
        reasons.push(
            "not one request reached a comparison: every record is excluded, declined, \
             unreachable or a primary error, so nothing was actually shadowed"
                .to_string(),
        );
        return reasons;
    }
    if tally.status_only == tally.comparisons {
        reasons.push(
            "every comparison was status-only, so no page body was ever canonicalised and a \
             rendering difference could not have been seen"
                .to_string(),
        );
    }
    if tally.identical == 0 {
        reasons.push(format!(
            "none of the {} comparison(s) came out identical, so the run contains no evidence \
             that the two renderers agree at all",
            tally.comparisons
        ));
    }
    if tally.compared_paths.len() < 2 {
        reasons.push(format!(
            "only {} distinct path(s) were compared, so the comparison has nothing to \
             distinguish",
            tally.compared_paths.len()
        ));
    }
    reasons
}

/// The requests with an undeclared difference, worst first: most distinct
/// details, then by request label so the order is stable between runs.
fn worst_first(differences: &BTreeMap<String, Vec<String>>) -> Vec<(&String, &Vec<String>)> {
    let mut rows: Vec<(&String, &Vec<String>)> = differences.iter().collect();
    rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    rows
}

/// The request as one string: the path and its query, because two requests for
/// the same path with different queries are different requests.
fn request_label(record: &ShadowRecord) -> String {
    match record.query.as_deref() {
        Some(query) if !query.is_empty() => format!("{}?{query}", record.path),
        _ => record.path.clone(),
    }
}

/// The reason on a record, defaulting to a placeholder so a record written
/// without one is visible rather than silently grouped under the empty string.
fn reason_token(record: &ShadowRecord) -> String {
    record
        .reason
        .clone()
        .unwrap_or_else(|| "(no reason recorded)".to_string())
}

/// The class of an unreachable reason: the leading `token:` the edge writes.
///
/// Prose reasons would make every unreachable request its own category, which is
/// why the edge leads with a token. A reason with no colon is truncated instead,
/// so one malformed record cannot print a paragraph.
fn unreachable_class(reason: &str) -> String {
    match reason.split_once(':') {
        Some((token, _)) if !token.is_empty() && token.len() <= 24 => token.to_string(),
        _ => reason.chars().take(24).collect(),
    }
}

/// Nearest-rank percentile over a sorted slice. `None` when there is no data.
fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (quantile * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[(rank - 1).min(sorted.len() - 1)])
}

/// The middle value, as a string. Median rather than mean because a page's size
/// has a long right tail — one 5 MB article would move a mean and say nothing.
fn median(values: &[usize]) -> String {
    if values.is_empty() {
        return "0".to_string();
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2].to_string()
}

fn ms(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.1}"),
        None => "-".to_string(),
    }
}

/// A `ts_ms` as the UTC day it falls in, as days since 1970-01-01.
fn day_of(ts_ms: u64) -> i64 {
    (ts_ms as i64).div_euclid(MILLIS_PER_DAY)
}

fn format_day(day: i64) -> String {
    let (year, month, day) = civil_from_days(day);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 → `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days` — the same algorithm, with the same
/// reasoning, as `crates/medium-doc/src/metadata.rs`. Duplicated rather than
/// exported: that one is private to a crate that ships pages, its job is to
/// format a publication date, and widening a shipping crate's API so that a test
/// tool can bucket a log by day is the wrong direction for a twelve-line
/// published algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, so leap days land at the end of the year
    // and the month arithmetic below is a straight division.
    let shifted = days + 719_468;
    // Floor division: Rust truncates toward zero, and the era is negative before
    // the epoch.
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;

    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;

    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

// ---------------------------------------------------------------------------
// gen-shadow-seed
// ---------------------------------------------------------------------------

/// What `gen-shadow-seed` wrote, so the caller can print the corpus and the
/// local run knows which paths to request.
pub struct Seeded {
    /// `(fixture name, path to request)` for every fixture, in fixture order.
    pub paths: Vec<(String, String)>,
    /// Total bytes of serialised `post_data` written.
    pub bytes: usize,
}

/// The cache key a fixture is seeded under: 12 hex characters derived from the
/// fixture's name.
///
/// # Why not the fixture's own `post_id`
///
/// Every fixture in `xtask/difftest/fixtures` leaves `post_id` at its default
/// (`0291df856c77` — see `render.rs::default_post_id`), because the render gate
/// compares rendered output and does not care which id the page was rendered
/// for. Seeding them under their own ids would therefore write **one** row and
/// let the other twenty-two fixtures collide on it, which would look like a
/// corpus and behave like a single page.
///
/// Deriving the key from the name instead gives each fixture its own row, stable
/// across runs, in the shape `core.py:80`'s fallback accepts for a post id. The
/// fixtures themselves are not touched: the render gate has no opinion about
/// this, and editing a corpus to suit a second reader would be a change to the
/// first gate's inputs.
pub fn seed_post_id(name: &str) -> String {
    // FNV-1a, truncated to 48 bits. Nothing here is adversarial — the only
    // requirements are that two fixture names do not collide, and that the
    // result looks like the ids the router already accepts.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:012x}", hash & 0xffff_ffff_ffff)
}

/// The path a seeded fixture is served at — what the local run requests.
pub fn seed_path(name: &str) -> String {
    format!("/{}", seed_post_id(name))
}

/// Writes every fixture's `post_data` into the `cache` table, under
/// [`seed_post_id`].
///
/// This is what gives §6's local run a corpus without a network: the fixtures
/// are real GraphQL responses already on disk, so a seeded Postgres is enough to
/// put pages in front of the comparison and neither instance needs to fetch.
///
/// `dry_run` walks the corpus and reports it without touching the database,
/// which is what makes the seeding reviewable before it writes.
pub async fn seed(fixtures_dir: &str, database_url: &str, dry_run: bool) -> Result<Seeded, String> {
    let fixtures = crate::render::load_fixtures(fixtures_dir)?;

    // Connected once, before the loop, so a bad URL is one error rather than
    // twenty-three. `PostgresCache::connect` connects eagerly — deliberately, so
    // that a server finds out at boot rather than on its first request — and
    // this uses it for the same reason.
    let cache = if dry_run {
        None
    } else {
        Some(
            freedium_cache::postgres::PostgresCache::connect(database_url)
                .await
                .map_err(|err| format!("cannot connect to Postgres: {err}"))?,
        )
    };

    // `CREATE TABLE IF NOT EXISTS`, before the first write. In production the
    // server does this at boot and the seeder would be a no-op, but the order
    // this is meant to run in is *seed, then start the instances* (§6), and an
    // empty database would otherwise fail here with `relation "cache" does not
    // exist` — a seeding tool that cannot seed a fresh database is not one. This
    // is the same call, for the same reason, that `PostgresCache::init_db`
    // documents as bootstrapping a dev database; the plan is explicit that it
    // must never grow into a migration, and nothing here changes that.
    if let Some(cache) = cache.as_ref() {
        cache
            .init_db()
            .await
            .map_err(|err| format!("cannot create the cache table: {err}"))?;
    }

    let mut seeded = Seeded {
        paths: Vec::new(),
        bytes: 0,
    };

    for fixture in &fixtures {
        let key = seed_post_id(&fixture.name);
        // Compact JSON, which is what `PostgresCache::push` writes and therefore
        // what a row this harness writes already looks like. Nothing depends on
        // the spacing: Python's `json.loads` and this crate's `decode_json` read
        // either form. What does matter is the shape — the fixture's `post_data`
        // is the GraphQL envelope (`{"data": …}`), not the post alone, which is
        // what production stores under the bare post id.
        let value = serde_json::to_string(&fixture.post_data)
            .map_err(|err| format!("{}: cannot serialise post_data: {err}", fixture.name))?;

        if let Some(cache) = cache.as_ref() {
            cache
                .push(&key, &value)
                .await
                .map_err(|err| format!("{}: cannot write {key}: {err}", fixture.name))?;
        }

        seeded.bytes += value.len();
        seeded
            .paths
            .push((fixture.name.clone(), seed_path(&fixture.name)));
    }

    Ok(seeded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use page_canonical::{Declaration, DeclaredMatch, DiffKind, Outcome, Side};

    /// A midday millisecond on the given day, so a test can name a date.
    fn day(index: i64) -> u64 {
        (index * MILLIS_PER_DAY + 12 * 3_600_000) as u64
    }

    fn side(status: u16) -> Side {
        Side {
            status,
            ms: 10.0,
            bytes: 100,
        }
    }

    fn tally_of(records: &[ShadowRecord]) -> Tally {
        let mut tally = Tally::default();
        for record in records {
            tally.add(record);
        }
        tally
    }

    fn identical(ts_ms: u64, path: &str) -> ShadowRecord {
        ShadowRecord::record(
            ts_ms,
            path,
            None,
            side(200),
            Some(side(200)),
            &Outcome::Identical,
        )
    }

    fn different(ts_ms: u64, path: &str, detail: &str) -> ShadowRecord {
        ShadowRecord::record(
            ts_ms,
            path,
            None,
            side(200),
            Some(side(200)),
            &Outcome::Different {
                detail: detail.to_string(),
            },
        )
    }

    fn declared(ts_ms: u64, path: &str, pattern: &str, detail: &str) -> ShadowRecord {
        ShadowRecord::record(
            ts_ms,
            path,
            None,
            side(200),
            Some(side(200)),
            &Outcome::Declared {
                path: pattern.to_string(),
                kind: DiffKind::TextOrMarkup,
                reason: "the legacy drops a trailing space here".to_string(),
                detail: detail.to_string(),
            },
        )
    }

    fn status_only(ts_ms: u64, path: &str) -> ShadowRecord {
        ShadowRecord::record(
            ts_ms,
            path,
            None,
            side(404),
            Some(side(404)),
            &Outcome::StatusOnly,
        )
    }

    fn excluded(ts_ms: u64, path: &str, reason: &str) -> ShadowRecord {
        ShadowRecord::excluded(ts_ms, path, None, reason, side(200))
    }

    fn declarations(entries: &[(&str, &str)]) -> (String, Declarations) {
        let parsed = Declarations::new(
            entries
                .iter()
                .map(|(path, reason)| Declaration {
                    path: (*path).to_string(),
                    kind: DiffKind::TextOrMarkup,
                    reason: (*reason).to_string(),
                })
                .collect(),
        );
        ("decl.json".to_string(), parsed)
    }

    // -- the calendar ---------------------------------------------------------

    #[test]
    fn the_day_bucket_is_utc_and_matches_the_calendar() {
        // 2026-09-12 is epoch day 20 708.
        assert_eq!(format_day(20_708), "2026-09-12");
        assert_eq!(day_of(20_708 * MILLIS_PER_DAY as u64), 20_708);
        // Floor, not truncation: the millisecond before is the previous day.
        assert_eq!(day_of(20_708 * MILLIS_PER_DAY as u64 - 1), 20_707);
        // And a leap day, which is what the 0000-03-01 shift exists for.
        assert_eq!(format_day(19_814), "2024-04-01");
        assert_eq!(format_day(19_782), "2024-02-29");
    }

    #[test]
    fn percentile_is_nearest_rank_and_degrades_gracefully() {
        assert_eq!(percentile(&[], 0.5), None);
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&sorted, 0.5), Some(2.0));
        assert_eq!(percentile(&sorted, 0.99), Some(4.0));
        assert_eq!(percentile(&sorted, 0.0), Some(1.0));
    }

    #[test]
    fn the_median_is_the_middle_value() {
        assert_eq!(median(&[]), "0");
        assert_eq!(median(&[7]), "7");
        assert_eq!(median(&[9, 1, 5]), "5");
    }

    // -- the degeneracy check -------------------------------------------------

    /// A clean run over a corpus that could have failed is not degenerate.
    #[test]
    fn a_clean_run_over_a_real_corpus_is_not_degenerate() {
        let tally = tally_of(&[
            identical(day(0), "/a"),
            identical(day(0), "/b"),
            identical(day(0), "/c"),
        ]);
        assert_eq!(tally.comparisons, 3);
        assert!(
            degeneracies(&tally).is_empty(),
            "{:?}",
            degeneracies(&tally)
        );
    }

    /// The failure a soak is least likely to notice: an edge that shadows
    /// nothing writes a log with no differences in it.
    #[test]
    fn a_log_with_no_comparisons_is_degenerate() {
        let tally = tally_of(&[
            excluded(day(0), "/", "homepage"),
            excluded(day(0), "/static/x.css", "static"),
        ]);
        let reasons = degeneracies(&tally);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            reasons[0].contains("nothing was actually shadowed"),
            "{reasons:?}"
        );
    }

    #[test]
    fn an_empty_log_is_degenerate() {
        let reasons = degeneracies(&tally_of(&[]));
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("log is empty"), "{reasons:?}");
    }

    /// A corpus of error pages never canonicalises a body, so it would pass
    /// whatever the renderers did.
    #[test]
    fn a_status_only_run_is_degenerate() {
        let tally = tally_of(&[status_only(day(0), "/a"), status_only(day(0), "/b")]);
        assert_eq!(tally.comparisons, 2);
        let reasons = degeneracies(&tally);
        assert!(
            reasons.iter().any(|it| it.contains("status-only")),
            "{reasons:?}"
        );
    }

    /// One page compared with itself proves nothing about a corpus.
    #[test]
    fn a_single_distinct_path_is_degenerate() {
        let tally = tally_of(&[identical(day(0), "/a"), identical(day(0), "/a")]);
        let reasons = degeneracies(&tally);
        assert!(
            reasons.iter().any(|it| it.contains("distinct path")),
            "{reasons:?}"
        );
    }

    /// A declarations file that absorbed everything would leave the run with no
    /// evidence that the two renderers ever agree.
    #[test]
    fn an_all_declared_run_is_degenerate() {
        let tally = tally_of(&[
            declared(day(0), "/a", "/a", "node #1"),
            declared(day(0), "/b", "/b", "node #2"),
        ]);
        assert_eq!(
            tally.comparisons, 2,
            "a declared difference is a comparison"
        );
        let reasons = degeneracies(&tally);
        assert!(
            reasons
                .iter()
                .any(|it| it.contains("no evidence that the two renderers agree")),
            "{reasons:?}"
        );
    }

    // -- §5's counter ---------------------------------------------------------

    #[test]
    fn the_gate_counter_needs_consecutive_clean_days() {
        // Three clean days in a row.
        let tally = tally_of(&[
            identical(day(0), "/a"),
            identical(day(1), "/a"),
            identical(day(2), "/a"),
        ]);
        assert_eq!(tally.clean_days(), 3);

        // A difference on the last day breaks the run at once.
        let tally = tally_of(&[
            identical(day(0), "/a"),
            identical(day(1), "/a"),
            different(day(2), "/a", "node #3"),
        ]);
        assert_eq!(tally.clean_days(), 0);

        // A difference in the middle keeps the trailing run to one day.
        let tally = tally_of(&[
            identical(day(0), "/a"),
            different(day(1), "/a", "node #3"),
            identical(day(2), "/a"),
        ]);
        assert_eq!(tally.clean_days(), 1);

        // A gap is not a clean day: day 1 is missing, so the run is one day.
        let tally = tally_of(&[identical(day(0), "/a"), identical(day(2), "/a")]);
        assert_eq!(tally.clean_days(), 1);

        // A day whose traffic never reached a comparison is not evidence either,
        // so it breaks the run just as a difference does.
        let tally = tally_of(&[
            identical(day(0), "/a"),
            excluded(day(1), "/", "homepage"),
            identical(day(2), "/a"),
        ]);
        assert_eq!(tally.clean_days(), 1);
    }

    #[test]
    fn a_log_of_no_days_counts_no_clean_days() {
        assert_eq!(tally_of(&[]).clean_days(), 0);
    }

    /// A declared difference does not break a day — that is the point of a
    /// declarations file, and it is what the plan asks for: *"consecutive days
    /// without an undeclared difference"*. What keeps it from being a hole is
    /// the dead-declaration check, asserted further down.
    #[test]
    fn a_declared_difference_does_not_break_the_day() {
        let tally = tally_of(&[declared(day(0), "/a", "/a", "node #1")]);
        assert_eq!(tally.clean_days(), 1);
        // But it is still visible, and still a comparison, so a day of
        // declarations cannot be used to pad a run that compared nothing.
        assert_eq!(tally.count(RecordOutcome::Declared), 1);
        assert_eq!(tally.comparisons, 1);
    }

    /// The two halves of that rule, together: an undeclared difference breaks a
    /// run, a declared one does not.
    #[test]
    fn only_an_undeclared_difference_breaks_a_run() {
        let tally = tally_of(&[
            identical(day(0), "/a"),
            declared(day(1), "/a", "/a", "node #1"),
            different(day(2), "/a", "node #9"),
        ]);
        assert_eq!(tally.clean_days(), 0);

        // Same three days, with the last one clean: all three count, the
        // declared day included.
        let tally = tally_of(&[
            identical(day(0), "/a"),
            declared(day(1), "/a", "/a", "node #1"),
            identical(day(2), "/a"),
        ]);
        assert_eq!(tally.clean_days(), 3);
    }

    // -- declarations ---------------------------------------------------------

    /// The declared-difference bookkeeping must let the report name the
    /// declarations that fired, or the dead-declaration check has nothing to
    /// compare against.
    #[test]
    fn declarations_are_tracked_by_pattern_and_keep_their_details() {
        let tally = tally_of(&[
            declared(day(0), "/a", "/a", "node #1: x vs y"),
            declared(day(0), "/a", "/a", "node #1: x vs y"),
            declared(day(0), "/a", "/a", "node #2: p vs q"),
        ]);
        let fired = tally.declared.get("/a").expect("/a fired");
        assert_eq!(fired.count, 3);
        assert_eq!(fired.details.len(), 2, "distinct details, not a count");
        assert_eq!(fired.reason, "the legacy drops a trailing space here");
        assert!(tally.fired_paths().contains("/a"));
    }

    /// A declaration in the file that the log never mentions is dead, and the
    /// report has to say so — the `render.rs` stale-divergence rule.
    #[test]
    fn a_declaration_that_never_fired_is_reported() {
        let file = declarations(&[("/fired", "why"), ("/never", "why")]);
        let mut fired = BTreeSet::new();
        fired.insert("/fired".to_string());

        let dead = print_declarations(Some(&file), &fired);
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert!(dead[0].contains("/never"), "{}", dead[0]);
    }

    /// With no declarations file there is nothing to check, and the report says
    /// so rather than reporting a clean slate it cannot vouch for.
    #[test]
    fn no_declarations_file_means_no_dead_declaration_check() {
        assert!(print_declarations(None, &BTreeSet::new()).is_empty());
        let empty = declarations(&[]);
        assert!(print_declarations(Some(&empty), &BTreeSet::new()).is_empty());
    }

    /// `DeclaredMatch` is what the report groups on, so the record's own
    /// declaration path must survive into it.
    #[test]
    fn the_recorded_match_keeps_the_pattern() {
        let record = declared(day(0), "/@miro/abc", "/@miro/*", "node #1");
        assert_eq!(
            record.declared,
            Some(DeclaredMatch {
                path: "/@miro/*".to_string(),
                reason: "the legacy drops a trailing space here".to_string(),
            })
        );
        let tally = tally_of(&[record]);
        assert!(tally.fired_paths().contains("/@miro/*"));
    }

    /// The report's table is derived from `ALL_OUTCOMES`, so every outcome the
    /// log can hold must have a row in it. Spelled out as a count, so adding a
    /// variant to `RecordOutcome` fails here rather than silently dropping one
    /// from the table.
    #[test]
    fn the_report_covers_every_outcome() {
        assert_eq!(ALL_OUTCOMES.len(), 8);
        let tally = tally_of(&[]);
        for outcome in ALL_OUTCOMES {
            assert_eq!(tally.count(outcome), 0);
        }
    }

    // -- labels and grouping --------------------------------------------------

    /// The report groups an unreachable shadow by the leading token the edge
    /// writes, so a hundred connection failures are one row.
    #[test]
    fn unreachable_reasons_are_grouped_by_their_leading_token() {
        assert_eq!(unreachable_class("connect: connection refused"), "connect");
        assert_eq!(unreachable_class("timeout: 5000ms elapsed"), "timeout");
        // No colon: truncated rather than printed whole.
        assert_eq!(unreachable_class(&"x".repeat(80)), "x".repeat(24));
    }

    /// The request label carries the query, because two requests for one path
    /// with different queries are different requests.
    #[test]
    fn the_request_label_keeps_the_query() {
        let mut record = identical(0, "/a");
        assert_eq!(request_label(&record), "/a");
        record.query = Some("no-redis".to_string());
        assert_eq!(request_label(&record), "/a?no-redis");
        // An empty query is the same request as no query at all.
        record.query = Some(String::new());
        assert_eq!(request_label(&record), "/a");
    }

    /// The most-affected requests come first, and equal ones are ordered by
    /// label so two runs over the same log print the same thing.
    #[test]
    fn differences_are_ordered_worst_first_and_stably() {
        let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
        found.insert("/b".to_string(), vec!["one".to_string()]);
        found.insert("/a".to_string(), vec!["one".to_string(), "two".to_string()]);
        found.insert("/c".to_string(), vec!["one".to_string()]);
        let ordered: Vec<&str> = worst_first(&found)
            .into_iter()
            .map(|(label, _)| label.as_str())
            .collect();
        assert_eq!(ordered, ["/a", "/b", "/c"]);
    }

    // -- the seeded corpus ----------------------------------------------------

    /// Seeded ids are distinct per fixture and stable across runs — the whole
    /// reason the key is derived from the name rather than from `post_id`.
    #[test]
    fn seeded_ids_are_distinct_and_stable() {
        assert_eq!(seed_post_id("article"), seed_post_id("article"));
        assert_ne!(seed_post_id("article"), seed_post_id("code"));
        assert_eq!(seed_post_id("article").len(), 12);
        assert!(
            seed_post_id("article")
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "{}",
            seed_post_id("article")
        );
        assert_eq!(
            seed_path("article"),
            format!("/{}", seed_post_id("article"))
        );
    }

    /// The reason the derivation exists: the render gate's fixtures all share
    /// one `post_id`, so it is the only thing standing between this seed and a
    /// corpus of one row.
    ///
    /// Reads the real corpus, from the crate directory rather than the workspace
    /// root — `CARGO_MANIFEST_DIR` is right whether this runs from the workspace
    /// or from the package.
    #[test]
    fn the_fixture_corpus_does_not_collide_on_one_key() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");
        let fixtures = crate::render::load_fixtures(dir).expect("the fixture directory is read");
        assert!(
            fixtures.len() > 1,
            "the corpus has to be bigger than one post"
        );

        let ids: BTreeSet<String> = fixtures
            .iter()
            .map(|fixture| seed_post_id(&fixture.name))
            .collect();
        assert_eq!(ids.len(), fixtures.len(), "two fixtures share a seeded id");

        // And the fixtures really do share a post_id, which is what makes the
        // derivation necessary rather than merely tidy. If this ever stops
        // holding, the note on `seed_post_id` is out of date.
        let post_ids: BTreeSet<&str> = fixtures
            .iter()
            .map(|fixture| fixture.post_id.as_str())
            .collect();
        assert_eq!(
            post_ids.len(),
            1,
            "fixtures no longer share a post_id: {post_ids:?}"
        );

        // Every fixture carries the GraphQL envelope, not a bare post — that is
        // what production stores under the bare post id, and what a seeded row
        // therefore has to look like.
        for fixture in &fixtures {
            assert!(
                fixture.post_data.get("data").is_some(),
                "{} has no `data` envelope",
                fixture.name
            );
        }
    }
}
