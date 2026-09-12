//! The render parity gate — RUST_REWRITE_PLAN §4, for Fase 1's output.
//!
//! Each hand-written fixture in `fixtures/` is a GraphQL envelope. It goes
//! through two independent renderers:
//!
//! ```text
//! fixtures/*.json ──┬─> legacy/medium-parser (real code, stubbed imports) ─> HTML
//!                   └─> medium-doc::parse -> medium-render             ─> HTML
//! ```
//!
//! and the two HTML outputs must be **semantically** equal, which here means
//! equal after [`crate::canonical::canonicalize`] — see that module for why a
//! tree comparison would fail on every article and what the reduced form keeps.
//! The title and subtitle are compared exactly, because both sides can rewrite
//! them while de-duplicating.
//!
//! Two things this gate refuses to accept:
//!
//! - A **degenerate corpus**: one that no plausible bug could fail. See
//!   [`degeneracies`]; a clean run over such a corpus is not evidence, so the
//!   gate fails on it rather than reporting a hollow PASS.
//! - A **stale divergence declaration**. A fixture may set
//!   `expected_divergence` to record a difference the rewrite intends (the
//!   legacy's mid-surrogate corruption is the one that exists). The declaration
//!   is only honoured if the case really does differ: a declaration that has
//!   stopped diverging fails the gate, so an allowance cannot quietly outlive
//!   the bug it excuses.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};

use serde::{Deserialize, Serialize};

use crate::canonical::{CanonicalNode, canonicalize};

/// One fixture file, as written by hand.
#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub note: String,
    /// Set when this fixture is *meant* to differ; the value is the reason.
    #[serde(default)]
    pub expected_divergence: Option<String>,
    pub host_address: String,
    pub post_data: serde_json::Value,
}

/// One line of the corpus handed to the Python reference.
#[derive(Debug, Serialize, Deserialize)]
pub struct RenderCase {
    pub index: usize,
    pub name: String,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_divergence: Option<String>,
    pub host_address: String,
    pub post_data: serde_json::Value,
}

/// One line of the Python reference output.
///
/// `fragments`, `title` and `subtitle` are absent exactly when `error` is
/// present: the legacy renderer raised, and the case has no reference to
/// compare against.
#[derive(Debug, Deserialize)]
pub struct RenderRefRow {
    #[allow(dead_code)] // Kept so the index can be cross-checked against the case.
    pub index: usize,
    #[allow(dead_code)]
    pub name: String,
    #[serde(default)]
    pub fragments: Option<Vec<String>>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Reads every `*.json` in `dir`, ordered by file name so the corpus is
/// byte-identical between runs.
pub fn load_fixtures(dir: &str) -> Result<Vec<Fixture>, String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("cannot read {dir}: {err}"))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|err| format!("cannot read {dir}: {err}"))?
            .path();
        if path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut fixtures = Vec::with_capacity(paths.len());
    for path in paths {
        let text = fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let fixture: Fixture =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        fixtures.push(fixture);
    }
    Ok(fixtures)
}

/// Writes the corpus as JSONL.
pub fn write_cases(fixtures: &[Fixture], out: &str) -> Result<(), String> {
    let file = File::create(out).map_err(|err| format!("cannot create {out}: {err}"))?;
    let mut writer = BufWriter::new(file);
    for (index, fixture) in fixtures.iter().enumerate() {
        let case = RenderCase {
            index,
            name: fixture.name.clone(),
            note: fixture.note.clone(),
            expected_divergence: fixture.expected_divergence.clone(),
            host_address: fixture.host_address.clone(),
            post_data: fixture.post_data.clone(),
        };
        serde_json::to_writer(&mut writer, &case)
            .map_err(|err| format!("serialising case {index}: {err}"))?;
        writer
            .write_all(b"\n")
            .map_err(|err| format!("writing {out}: {err}"))?;
    }
    writer
        .flush()
        .map_err(|err| format!("flushing {out}: {err}"))
}

/// What the corpus covers, accumulated while comparing.
#[derive(Debug, Default)]
struct Coverage {
    cases: usize,
    /// Cases whose rendering contained a text-less marker (an `<img>` or
    /// `<iframe>`), i.e. where attributes are the only thing compared.
    with_marker: usize,
    /// Cases whose payload holds a non-BMP character, i.e. where a byte offset
    /// and a UTF-16 offset disagree.
    with_astral: usize,
    /// Cases with two markups over overlapping ranges, i.e. where the legacy
    /// splices siblings and the IR nests.
    with_overlap: usize,
    /// Cases with more than four paragraphs, i.e. where the de-duplication
    /// window has closed.
    with_window_overflow: usize,
    /// Distinct canonical renderings seen, to catch a corpus that renders
    /// everything the same way.
    outputs: BTreeSet<Vec<String>>,
}

/// One case's outcome, for the summary table.
fn label_for(reference: &RenderRefRow, verdict: &str) -> String {
    let blocks = match reference.fragments.as_ref() {
        Some(fragments) => fragments.len().to_string(),
        None => "—".to_string(),
    };
    format!("{blocks:>6}  {verdict}")
}

/// The differences between one case's two renderings, as human-readable lines.
/// Empty when the case matches.
fn collect_differences(
    ref_nodes: &[CanonicalNode],
    rust_nodes: &[CanonicalNode],
    reference: &RenderRefRow,
    rust_title: &str,
    rust_subtitle: &str,
) -> Vec<String> {
    let mut differences = Vec::new();
    if ref_nodes != rust_nodes {
        differences.push(first_difference(ref_nodes, rust_nodes));
    }
    if Some(rust_title) != reference.title.as_deref() {
        differences.push(format!(
            "title: py={:?} rust={rust_title:?}",
            reference.title.as_deref().unwrap_or("<missing>")
        ));
    }
    if Some(rust_subtitle) != reference.subtitle.as_deref() {
        differences.push(format!(
            "subtitle: py={:?} rust={rust_subtitle:?}",
            reference.subtitle.as_deref().unwrap_or("<missing>")
        ));
    }
    differences
}

/// Runs the whole gate. Returns `true` when it passes.
pub fn run(cases_path: &str, reference_path: &str, show: usize) -> Result<bool, String> {
    let cases: Vec<RenderCase> = crate::read_jsonl(cases_path)?;
    let reference: Vec<RenderRefRow> = crate::read_jsonl(reference_path)?;

    if cases.len() != reference.len() {
        return Err(format!(
            "{} cases but {} reference rows — the reference was generated from a different corpus",
            cases.len(),
            reference.len()
        ));
    }

    println!(
        "render parity: {} cases, {} reference rows",
        cases.len(),
        reference.len()
    );
    println!(
        "gate §4: canonical output must be identical, except where a fixture declares \
         `expected_divergence`\n"
    );

    let mut coverage = Coverage::default();
    let mut matched = 0usize;
    let mut declared_diffs = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut shown = 0usize;

    for (case, reference) in cases.iter().zip(&reference) {
        coverage.cases += 1;
        if payload_has_astral(&case.post_data) {
            coverage.with_astral += 1;
        }
        if payload_has_overlap(&case.post_data) {
            coverage.with_overlap += 1;
        }
        if payload_paragraphs(&case.post_data).len() > 4 {
            coverage.with_window_overflow += 1;
        }

        // Render the Rust side first: a failure here is a failure of the port
        // regardless of what the reference did.
        let (rust_fragments, rust_title, rust_subtitle) = match render_rust(case) {
            Ok(rendered) => rendered,
            Err(err) => {
                failures.push(format!("{}: {err}", case.name));
                rows.push((case.name.clone(), label_for(reference, "FAIL")));
                continue;
            }
        };

        let rust_nodes = canonicalize(&rust_fragments.concat());
        if rust_nodes.iter().any(|node| node.text.is_empty()) {
            coverage.with_marker += 1;
        }
        coverage.outputs.insert(describe(&rust_nodes));

        let Some(ref_fragments) = reference.fragments.as_ref() else {
            let detail = reference
                .error
                .as_deref()
                .unwrap_or("no fragments and no error");
            failures.push(format!(
                "{}: the legacy renderer failed, so there is nothing to compare: {detail}",
                case.name
            ));
            rows.push((case.name.clone(), label_for(reference, "FAIL")));
            continue;
        };

        let ref_nodes = canonicalize(&ref_fragments.concat());
        let differences = collect_differences(
            &ref_nodes,
            &rust_nodes,
            reference,
            &rust_title,
            &rust_subtitle,
        );

        match (differences.is_empty(), case.expected_divergence.as_deref()) {
            (true, None) => {
                matched += 1;
                rows.push((case.name.clone(), label_for(reference, "match")));
            }
            (false, Some(_)) => {
                declared_diffs += 1;
                rows.push((case.name.clone(), label_for(reference, "differ (declared)")));
            }
            (true, Some(reason)) => {
                failures.push(format!(
                    "{}: declares `expected_divergence` but renders identically — the \
                     allowance is stale and must be removed:\n      {reason}",
                    case.name
                ));
                rows.push((case.name.clone(), label_for(reference, "FAIL")));
            }
            (false, None) => {
                let detail = differences.join("\n    ");
                failures.push(format!("{}: outputs differ\n    {detail}", case.name));
                rows.push((case.name.clone(), label_for(reference, "FAIL")));
                if shown < show {
                    shown += 1;
                    println!("--- MISMATCH [{}]", case.name);
                    println!("    {}", case.note);
                    println!("    {detail}");
                    println!("    python: {}", snippets(ref_fragments));
                    println!("    rust:   {}", snippets(&rust_fragments));
                    println!();
                }
            }
        }
    }

    println!("{:<28} {:>6}  verdict", "fixture", "blocks");
    for (name, label) in &rows {
        println!("{name:<28} {label}");
    }

    println!();
    println!(
        "{} matched, {} differ as declared, {} failed",
        matched,
        declared_diffs,
        failures.len()
    );

    let blockers = degeneracies(&coverage);
    if !blockers.is_empty() {
        println!();
        println!("DEGENERATE CORPUS — a PASS here would prove nothing:");
        for reason in &blockers {
            println!("  - {reason}");
        }
    }

    if !failures.is_empty() {
        println!();
        println!("FAILURES:");
        for failure in &failures {
            println!("  - {failure}");
        }
    }

    let passed = failures.is_empty() && blockers.is_empty();
    println!();
    if passed {
        println!(
            "PASS — {} cases, 0 undeclared differences, 0 stale declarations",
            coverage.cases
        );
        Ok(true)
    } else {
        println!(
            "FAIL — {} case(s) failed, {} corpus degeneracy reason(s)",
            failures.len(),
            blockers.len()
        );
        Ok(false)
    }
}

/// Parses and renders one case with the Rust pipeline.
fn render_rust(case: &RenderCase) -> Result<(Vec<String>, String, String), String> {
    let payload = medium_doc::parse::PostPayload::from_value(case.post_data.clone())
        .map_err(|err| format!("payload did not deserialise: {err}"))?;
    let document = medium_doc::parse::parse(&payload, &case.host_address);
    let fragments = medium_render::html::render_blocks(&document);
    Ok((
        fragments,
        document.meta.title.clone(),
        document.meta.subtitle.clone(),
    ))
}

/// The first place two canonical sequences disagree, described for a human.
fn first_difference(expected: &[CanonicalNode], actual: &[CanonicalNode]) -> String {
    for index in 0..expected.len().max(actual.len()) {
        let left = expected.get(index).map(CanonicalNode::describe);
        let right = actual.get(index).map(CanonicalNode::describe);
        if left != right {
            return format!(
                "canonical node #{index} (of {} vs {}):\n      python: {}\n      rust:   {}",
                expected.len(),
                actual.len(),
                left.unwrap_or_else(|| "<absent>".to_string()),
                right.unwrap_or_else(|| "<absent>".to_string()),
            );
        }
    }
    format!(
        "sequences are equal but lengths differ ({} vs {})",
        expected.len(),
        actual.len()
    )
}

/// A canonical sequence as comparable strings, so a corpus of identical
/// renderings can be detected.
fn describe(nodes: &[CanonicalNode]) -> Vec<String> {
    nodes.iter().map(CanonicalNode::describe).collect()
}

/// Up to three fragments, for a mismatch report.
fn snippets(fragments: &[String]) -> String {
    let shown: Vec<String> = fragments
        .iter()
        .take(3)
        .map(|f| crate::preview(f))
        .collect();
    if fragments.len() > shown.len() {
        format!(
            "{} …(+{} more)",
            shown.join(" "),
            fragments.len() - shown.len()
        )
    } else {
        shown.join(" ")
    }
}

fn payload_paragraphs(post_data: &serde_json::Value) -> &[serde_json::Value] {
    post_data
        .get("data")
        .and_then(|data| data.get("post"))
        .and_then(|post| post.get("content"))
        .and_then(|content| content.get("bodyModel"))
        .and_then(|body| body.get("paragraphs"))
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// True when the payload holds a character outside the BMP, which is the only
/// thing that makes a UTF-16 offset differ from a code-point offset *and* shift
/// a byte offset. Without one, a byte-indexed map would pass this gate.
fn payload_has_astral(post_data: &serde_json::Value) -> bool {
    let text = post_data.to_string();
    text.chars().any(|ch| ch.len_utf16() == 2)
}

/// True when some paragraph has two markups over ranges that overlap.
fn payload_has_overlap(post_data: &serde_json::Value) -> bool {
    payload_paragraphs(post_data).iter().any(|paragraph| {
        let ranges: Vec<(u64, u64)> = paragraph
            .get("markups")
            .and_then(serde_json::Value::as_array)
            .map(|markups| {
                markups
                    .iter()
                    .filter_map(|markup| {
                        Some((markup.get("start")?.as_u64()?, markup.get("end")?.as_u64()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ranges.iter().enumerate().any(|(index, a)| {
            ranges
                .iter()
                .skip(index + 1)
                .any(|b| a.0 < b.1 && b.0 < a.1)
        })
    })
}

/// Reasons the corpus cannot distinguish a correct port from a broken one.
///
/// The point of a differential gate is that it *could* fail. A corpus where
/// every fixture renders the same way, or where the interesting machinery is
/// never reached, passes for any implementation — so treat these as failures of
/// the harness rather than as a success of the port. Adding a fixture that
/// loses coverage of one of these five mechanisms will fail the gate, which is
/// intentional: the corpus is part of the test.
fn degeneracies(coverage: &Coverage) -> Vec<String> {
    let mut reasons = Vec::new();
    if coverage.cases == 0 {
        reasons.push("the corpus is empty".to_string());
    }
    if coverage.outputs.len() < 2 && coverage.cases > 0 {
        reasons.push(
            "every case renders to the same canonical form, so the comparison has nothing \
             to distinguish"
                .to_string(),
        );
    }
    if coverage.with_marker == 0 {
        reasons.push(
            "no case renders a text-less element, so a wrong <img> or <iframe> attribute \
             would pass unnoticed"
                .to_string(),
        );
    }
    if coverage.with_astral == 0 {
        reasons.push(
            "no case contains a non-BMP character, so a byte-offset map would pass this gate"
                .to_string(),
        );
    }
    if coverage.with_overlap == 0 {
        reasons.push(
            "no case has overlapping markup ranges, so the sibling-vs-nested normalisation \
             is never exercised"
                .to_string(),
        );
    }
    if coverage.with_window_overflow == 0 {
        reasons.push(
            "no case has more than four paragraphs, so the de-duplication window boundary \
             (core.py:259) is untested"
                .to_string(),
        );
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::{Coverage, degeneracies, payload_has_astral, payload_has_overlap};

    fn sound() -> Coverage {
        let mut coverage = Coverage {
            cases: 18,
            with_marker: 5,
            with_astral: 2,
            with_overlap: 5,
            with_window_overflow: 5,
            ..Coverage::default()
        };
        coverage.outputs.insert(vec!["a".to_string()]);
        coverage.outputs.insert(vec!["b".to_string()]);
        coverage
    }

    #[test]
    fn a_sound_corpus_has_no_complaints() {
        assert!(degeneracies(&sound()).is_empty());
    }

    #[test]
    fn an_empty_corpus_is_rejected() {
        assert!(!degeneracies(&Coverage::default()).is_empty());
    }

    /// Each mechanism gets its own check, so losing one names the fixture that
    /// has to come back rather than failing generically.
    #[test]
    fn losing_one_mechanism_names_it() {
        let mut no_marker = sound();
        no_marker.with_marker = 0;
        let reasons = degeneracies(&no_marker);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("text-less element"));

        let mut no_astral = sound();
        no_astral.with_astral = 0;
        assert!(degeneracies(&no_astral)[0].contains("non-BMP"));

        let mut no_overlap = sound();
        no_overlap.with_overlap = 0;
        assert!(degeneracies(&no_overlap)[0].contains("overlapping"));

        let mut no_window = sound();
        no_window.with_window_overflow = 0;
        assert!(degeneracies(&no_window)[0].contains("four paragraphs"));
    }

    #[test]
    fn a_corpus_that_renders_one_way_is_rejected() {
        let mut identical = sound();
        identical.outputs.clear();
        identical.outputs.insert(vec!["a".to_string()]);
        let reasons = degeneracies(&identical);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("same canonical form"));
    }

    #[test]
    fn astral_detection_sees_an_emoji_and_not_a_bmp_accent() {
        assert!(payload_has_astral(&serde_json::json!({ "text": "hi 🎉" })));
        assert!(!payload_has_astral(&serde_json::json!({ "text": "café" })));
        assert!(!payload_has_astral(
            &serde_json::json!({ "text": "日本語" })
        ));
    }

    #[test]
    fn overlap_detection_covers_containment_equality_and_touching() {
        let with_ranges = |ranges: serde_json::Value| {
            serde_json::json!({
                "data": { "post": { "content": { "bodyModel": { "paragraphs": [
                    { "type": "P", "markups": ranges }
                ] } } } }
            })
        };
        // Contained, and identical.
        assert!(payload_has_overlap(&with_ranges(serde_json::json!([
            { "start": 0, "end": 10 }, { "start": 2, "end": 5 }
        ]))));
        assert!(payload_has_overlap(&with_ranges(serde_json::json!([
            { "start": 2, "end": 5 }, { "start": 2, "end": 5 }
        ]))));
        // Touching but disjoint is not overlap — the legacy renders that case
        // as plain siblings, which the IR reaches by itself.
        assert!(!payload_has_overlap(&with_ranges(serde_json::json!([
            { "start": 0, "end": 5 }, { "start": 5, "end": 10 }
        ]))));
        assert!(!payload_has_overlap(&with_ranges(serde_json::json!([
            { "start": 0, "end": 5 }
        ]))));
    }
}
