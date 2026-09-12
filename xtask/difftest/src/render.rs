//! The render parity gate — RUST_REWRITE_PLAN §4, for Fase 1's output. Fase 3
//! extended it to the whole page, which is what the last three sections of
//! [`collect_differences`] are.
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
//! equal after [`page_canonical::canonicalize`] — see that module for why a
//! tree comparison would fail on every article and what the reduced form keeps.
//! The title and subtitle are compared exactly, because both sides can rewrite
//! them while de-duplicating.
//!
//! # What the page comparison adds
//!
//! The fragments are what *localises* a difference; the page is what a reader
//! gets. `post.html` embeds the fragments, so a fragment bug fails both — but
//! three things reach only the page:
//!
//! - `generate_metadata`: the description, the `Free:` flag, the UTC dates, the
//!   double-escaped description. Nothing else calls it.
//! - the page title, which `core.py:786-788` builds by hand and `post.html` does
//!   not contain;
//! - `base.html` — the shell, the `<title>`, the `<meta>`, `{{ host_address }}`,
//!   and the two `{% if %}` branches. The reference renders it the way
//!   `handlers/post.py:91-98` does, so the comparison covers the document that
//!   production actually caches and serves rather than the body alone.
//!
//! The shell gets a **second**, byte-for-byte comparison
//! ([`RustRender::base_bare`]) because the canonical form drops whitespace-only
//! text nodes, and whitespace is where two template engines' defaults differ.
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

use page_canonical::{CanonicalNode, canonicalize};

/// One fixture file, as written by hand.
#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub note: String,
    /// Set when this fixture is *meant* to differ; the value is the reason.
    #[serde(default)]
    pub expected_divergence: Option<String>,
    pub host_address: String,
    /// The post id the page was rendered for.
    ///
    /// `generate_metadata` takes one and returns it in its dict, and
    /// `_render_as_html` passes it down; the page itself interpolates
    /// `mediumUrl` rather than the id, so nothing in `post.html` shows it. It is
    /// part of the corpus anyway because it is part of the call.
    #[serde(default = "default_post_id")]
    pub post_id: String,
    /// `config.ENABLE_ADS_BANNER` (`config.py:29`, default `False`), which
    /// `base.html:172` reads as `enable_ads_header`.
    ///
    /// A fixture knob rather than the deployed value: `web.server.config` has no
    /// default for `ADMIN_SECRET_KEY` and raises on import without it, and a gate
    /// that only runs where production's environment is loaded is not a gate. It
    /// is per-fixture so that both branches of the `{% if %}` are in the corpus.
    #[serde(default)]
    pub enable_ads_header: bool,
    pub post_data: serde_json::Value,
}

/// A 12-hex id of the shape `core.py:80`'s fallback accepts.
fn default_post_id() -> String {
    "0291df856c77".to_string()
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
    pub post_id: String,
    pub enable_ads_header: bool,
    pub post_data: serde_json::Value,
}

/// One line of the Python reference output.
///
/// The four fragment fields are absent exactly when the content renderer raised,
/// and the five page fields exactly when the page renderer did — the two are
/// attempted independently, so a row can have one group and not the other. When
/// both raised, `error` carries both reasons, newline-separated.
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
    /// The served document: `base.html` around the `post.html` body
    /// (`handlers/post.py:91-98`).
    #[serde(default)]
    pub page: Option<String>,
    /// `base.html` rendered on its own, with the page title as the body.
    /// Compared **byte for byte** — see [`RustRender::base_bare`].
    #[serde(default)]
    pub base_bare: Option<String>,
    /// `HtmlResult.title` — `"{title} | by {creator.name}[ | in {collection.name}]"`.
    /// **Not** the article title in `title` above.
    #[serde(default)]
    pub page_title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
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
            post_id: fixture.post_id.clone(),
            enable_ads_header: fixture.enable_ads_header,
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

    // --- Fase 3: the page gate's own coverage -------------------------------
    //
    // The fragment counters above say nothing about whether the *page* gate can
    // fail. A corpus of cases that all render the same page title, or none of
    // which truncate a description, would pass a broken `generate_metadata`
    // unnoticed — which is the same trap `degeneracies` exists to close for the
    // fragments, so the page gets its own set of counters rather than inheriting
    // confidence it has not earned.
    /// Cases where the legacy rendered a page at all.
    pages: usize,
    /// Distinct canonical pages seen.
    page_outputs: BTreeSet<Vec<String>>,
    /// Cases whose page title carries the ` | in {collection}` clause, i.e.
    /// where the collection branch of `core.py:786-788` is live.
    with_collection: usize,
    /// Cases with no collection, i.e. where that branch is *not* taken.
    without_collection: usize,
    /// Cases where `isLocked` is true, so `free_access` is `"No"`.
    locked: usize,
    /// Cases where `isLocked` is false, so `free_access` is `"Yes"`. Both are
    /// counted because the pair is inverted from the field's name and a
    /// hard-coded answer would pass with only one of them present.
    unlocked: usize,
    /// Cases whose description was truncated to the `...` placeholder, i.e. where
    /// `textwrap.shorten` actually did something.
    with_truncated_description: usize,
    /// Cases whose description contains an escaped entity, i.e. where the second
    /// escaping pass over the already-escaped subtitle is observable.
    with_escaped_description: usize,
    /// Cases where all three page fields were present to compare.
    with_page_fields: usize,
    /// Cases with `ENABLE_ADS_BANNER` on, i.e. where `base.html:172`'s
    /// `{% if enable_ads_header %}` branch is taken.
    with_ads_banner: usize,
}

/// One case's outcome, for the summary table.
fn label_for(reference: &RenderRefRow, verdict: &str) -> String {
    let blocks = match reference.fragments.as_ref() {
        Some(fragments) => fragments.len().to_string(),
        None => "—".to_string(),
    };
    format!("{blocks:>6}  {verdict}")
}

/// Everything one case renders to on the Rust side.
///
/// The fields mirror the reference row's fields one for one, so
/// [`collect_differences`] is a straight field-by-field comparison and a new
/// field cannot be compared against the wrong thing.
#[derive(Debug)]
struct RustRender {
    /// The content fragments, un-normalised — the reference writes raw HTML too.
    fragments: Vec<String>,
    /// The article title the content renderer returns, possibly rewritten.
    title: String,
    subtitle: String,
    /// The whole `post.html` + `base.html` page (`HtmlResult.html`).
    page: String,
    /// `base.html` alone, with the page title as the body.
    ///
    /// The reason this exists next to [`Self::page`], which already contains
    /// `base.html`: the page is compared *canonically*, and
    /// [`page_canonical::canonicalize`] drops whitespace-only text nodes — which
    /// is exactly where two template engines' defaults differ. Jinja2 strips a
    /// template source's trailing newline; minijinja keeps it. No browser can see
    /// that, and no canonical comparison can either, so the shell gets one
    /// comparison that is bytes.
    base_bare: String,
    /// `HtmlResult.title` — the **page** title, not the article's.
    page_title: String,
    description: String,
    url: String,
}

/// The differences between one case's two renderings, as human-readable lines.
/// Empty when the case matches.
///
/// Seven comparisons: three from Fase 1 (the canonical fragments, the article
/// title, the subtitle) and four page-level ones from Fase 3. Each is reported
/// separately so a failure names the stage rather than "the output is wrong" —
/// and the page is compared *as well as* the fragments it contains, because the
/// page is what a reader gets and the fragments are what localises a bug.
fn collect_differences(
    ref_nodes: &[CanonicalNode],
    rust_nodes: &[CanonicalNode],
    reference: &RenderRefRow,
    rust: &RustRender,
) -> Vec<String> {
    let mut differences = Vec::new();
    if ref_nodes != rust_nodes {
        differences.push(first_difference("fragments", ref_nodes, rust_nodes));
    }
    if Some(rust.title.as_str()) != reference.title.as_deref() {
        differences.push(format!(
            "title: py={:?} rust={:?}",
            reference.title.as_deref().unwrap_or("<missing>"),
            rust.title
        ));
    }
    if Some(rust.subtitle.as_str()) != reference.subtitle.as_deref() {
        differences.push(format!(
            "subtitle: py={:?} rust={:?}",
            reference.subtitle.as_deref().unwrap_or("<missing>"),
            rust.subtitle
        ));
    }

    // --- Fase 3: the page ---
    match reference.page.as_deref() {
        None => differences.push(
            "page: the legacy did not render a page, so there is nothing to compare".to_string(),
        ),
        Some(page) => {
            let ref_page_nodes = canonicalize(page);
            let rust_page_nodes = canonicalize(&rust.page);
            if ref_page_nodes != rust_page_nodes {
                differences.push(first_difference("page", &ref_page_nodes, &rust_page_nodes));
            }
        }
    }
    match reference.base_bare.as_deref() {
        None => differences.push(
            "base_bare: the legacy did not render the shell, so there is nothing to compare"
                .to_string(),
        ),
        Some(shell) => {
            if shell != rust.base_bare {
                differences.push(byte_difference("base_bare", shell, &rust.base_bare));
            }
        }
    }
    if Some(rust.page_title.as_str()) != reference.page_title.as_deref() {
        differences.push(format!(
            "page_title: py={:?} rust={:?}",
            reference.page_title.as_deref().unwrap_or("<missing>"),
            rust.page_title
        ));
    }
    if Some(rust.description.as_str()) != reference.description.as_deref() {
        differences.push(format!(
            "description: py={:?} rust={:?}",
            reference.description.as_deref().unwrap_or("<missing>"),
            rust.description
        ));
    }
    if Some(rust.url.as_str()) != reference.url.as_deref() {
        differences.push(format!(
            "url: py={:?} rust={:?}",
            reference.url.as_deref().unwrap_or("<missing>"),
            rust.url
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
        let rust = match render_rust(case) {
            Ok(rendered) => rendered,
            Err(err) => {
                failures.push(format!("{}: {err}", case.name));
                rows.push((case.name.clone(), label_for(reference, "FAIL")));
                continue;
            }
        };

        let rust_nodes = canonicalize(&rust.fragments.concat());
        if rust_nodes.iter().any(|node| node.text.is_empty()) {
            coverage.with_marker += 1;
        }
        coverage.outputs.insert(describe(&rust_nodes));

        // The page-level coverage, accumulated from the reference so that a
        // missing page on the legacy side is a *failure* rather than a silent
        // gap in the counts.
        if let Some(page) = reference.page.as_deref() {
            coverage.pages += 1;
            coverage.page_outputs.insert(describe(&canonicalize(page)));
        }
        if let Some(page_title) = reference.page_title.as_deref() {
            if page_title.contains(" | in ") {
                coverage.with_collection += 1;
            } else {
                coverage.without_collection += 1;
            }
        }
        if reference.description.is_some()
            && reference.page_title.is_some()
            && reference.url.is_some()
        {
            coverage.with_page_fields += 1;
        }
        if case.enable_ads_header {
            coverage.with_ads_banner += 1;
        }
        if let Some(description) = reference.description.as_deref() {
            if description.ends_with("...") {
                coverage.with_truncated_description += 1;
            }
            if description.contains('&') {
                coverage.with_escaped_description += 1;
            }
        }
        if payload_is_locked(&case.post_data) {
            coverage.locked += 1;
        } else {
            coverage.unlocked += 1;
        }

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
        let differences = collect_differences(&ref_nodes, &rust_nodes, reference, &rust);

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
                    println!("    rust:   {}", snippets(&rust.fragments));
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
    println!(
        "page gate: {} page(s), {} distinct, {} in a collection, {} truncated description(s), \
         {} with escapes, {} locked, {} with the ad banner",
        coverage.pages,
        coverage.page_outputs.len(),
        coverage.with_collection,
        coverage.with_truncated_description,
        coverage.with_escaped_description,
        coverage.locked,
        coverage.with_ads_banner
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
///
/// The page is rendered from the same `Document` and `PostMetadata` the
/// fragment path uses, so the two comparisons cannot disagree about their
/// inputs. `render_post` returns `RenderedPost`, whose field order matches
/// `HtmlResult`'s (`models/html_result.py:4-9`) — title, description, url, html
/// — which is why the assignment below reads the way it does.
fn render_rust(case: &RenderCase) -> Result<RustRender, String> {
    let payload = medium_doc::parse::PostPayload::from_value(case.post_data.clone())
        .map_err(|err| format!("payload did not deserialise: {err}"))?;
    let document = medium_doc::parse::parse(&payload, &case.host_address);
    let fragments = medium_render::html::render_blocks(&document);

    let metadata = medium_doc::metadata::from_payload(&payload, &case.post_id);
    let config = medium_render::page::PageConfig::new(&case.host_address)
        .with_ads_header(case.enable_ads_header);
    let rendered = medium_render::page::render_post(
        &medium_render::templates::environment(),
        &document,
        &metadata,
        &config,
    )
    .map_err(|err| format!("could not render the page: {err}"))?;

    // The same context `render_document_probe` (`py/render_ref.py`) builds, from
    // the same inputs. The body is the page title — the one piece of context this
    // gate already requires both sides to agree on byte for byte, so the probe
    // cannot fail because its *input* differed. Feeding it the fragments instead
    // made it inherit `markups-nested`'s splice-versus-nest difference, which the
    // canonical form exists to permit, and the failure said less about the shell
    // than about comparing two things at once.
    let base_bare = medium_render::page::render_base(
        &medium_render::templates::environment(),
        &rendered.title,
        &rendered.title,
        &rendered.description,
        &config,
    )
    .map_err(|err| format!("could not render base.html: {err}"))?;

    Ok(RustRender {
        fragments,
        title: document.meta.title.clone(),
        subtitle: document.meta.subtitle.clone(),
        page: rendered.html,
        base_bare,
        page_title: rendered.title,
        description: rendered.description,
        url: rendered.url,
    })
}

/// The first place two strings disagree, at whatever resolution that is.
///
/// A byte comparison over a whole document reports the *first* differing offset,
/// which for two template engines' output is usually near the end — the trailing
/// newline Jinja2 strips. Naming the offset and both lengths keeps a one-byte
/// difference from reading like a hundred-byte one, and the excerpts show what
/// is actually there rather than the whole 23 KB again.
fn byte_difference(what: &str, expected: &str, actual: &str) -> String {
    let Some(offset) = expected
        .bytes()
        .zip(actual.bytes())
        .position(|(left, right)| left != right)
    else {
        return format!(
            "{what}: the two are equal over the shorter length but lengths differ \
             ({} vs {})",
            expected.len(),
            actual.len()
        );
    };
    let start = floor_char_boundary(expected, offset);
    format!(
        "{what}: bytes differ at offset {offset} (of {} vs {}):\n      python: {}\n      rust:   {}",
        expected.len(),
        actual.len(),
        crate::preview(&expected[start..]),
        crate::preview(&actual[floor_char_boundary(actual, offset)..]),
    )
}

/// The largest char boundary at or below `index`, for slicing at a byte offset
/// that may land inside a multi-byte character.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The first place two canonical sequences disagree, described for a human.
///
/// `what` names the compared thing — `fragments` or `page` — because both go
/// through this and a bare "canonical node #3" would not say which.
fn first_difference(what: &str, expected: &[CanonicalNode], actual: &[CanonicalNode]) -> String {
    for index in 0..expected.len().max(actual.len()) {
        let left = expected.get(index).map(CanonicalNode::describe);
        let right = actual.get(index).map(CanonicalNode::describe);
        if left != right {
            return format!(
                "{what}: canonical node #{index} (of {} vs {}):\n      python: {}\n      rust:   {}",
                expected.len(),
                actual.len(),
                left.unwrap_or_else(|| "<absent>".to_string()),
                right.unwrap_or_else(|| "<absent>".to_string()),
            );
        }
    }
    format!(
        "{what}: sequences are equal but lengths differ ({} vs {})",
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

/// `post.isLocked`, which decides `free_access` (`core.py:712`).
///
/// A missing field reads as `false`, matching the `"Yes"` a payload without it
/// would produce — but the corpus is expected to carry it explicitly, and the
/// degeneracy check above is what insists on both values appearing.
fn payload_is_locked(post_data: &serde_json::Value) -> bool {
    post_data
        .get("data")
        .and_then(|data| data.get("post"))
        .and_then(|post| post.get("isLocked"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
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
/// loses coverage of one of these mechanisms will fail the gate, which is
/// intentional: the corpus is part of the test.
///
/// The page checks in the second group are the Fase 3 additions. They exist
/// because the fragment counters say nothing about `generate_metadata`: a
/// corpus of 24 cases whose descriptions are all short, all unescaped and all
/// in a collection would compare twelve fields per case and never once exercise
/// `shorten`, the second escaping pass, or the collection branch.
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

    // --- Fase 3: the page gate's own coverage -------------------------------
    if coverage.pages < coverage.cases {
        reasons.push(format!(
            "only {} of {} cases rendered a page, so the page comparison is not covering \
             the corpus",
            coverage.pages, coverage.cases
        ));
    }
    if coverage.page_outputs.len() < 2 && coverage.pages > 0 {
        reasons.push(
            "every case renders to the same canonical page, so the page comparison has \
             nothing to distinguish"
                .to_string(),
        );
    }
    if coverage.with_page_fields < coverage.cases {
        reasons.push(format!(
            "only {} of {} cases carry a page title, description and url, so the page \
             fields are not compared everywhere",
            coverage.with_page_fields, coverage.cases
        ));
    }
    if coverage.with_collection == 0 {
        reasons.push(
            "no case is in a collection, so the ` | in {collection.name}` clause of the \
             page title (core.py:786-788) is never taken"
                .to_string(),
        );
    }
    if coverage.without_collection == 0 {
        reasons.push(
            "every case is in a collection, so the clause that is *not* added when there \
             is none is never exercised"
                .to_string(),
        );
    }
    if coverage.locked == 0 || coverage.unlocked == 0 {
        reasons.push(
            "`isLocked` takes only one value across the corpus, so an inverted `free_access` \
             (`\"No\" if isLocked else \"Yes\"`, core.py:712) would pass"
                .to_string(),
        );
    }
    if coverage.with_truncated_description == 0 {
        reasons.push(
            "no description was truncated to the placeholder, so `textwrap.shorten` \
             (core.py:703-705) is never exercised through `generate_metadata`"
                .to_string(),
        );
    }
    if coverage.with_escaped_description == 0 {
        reasons.push(
            "no description contains an escaped entity, so the second escaping pass over \
             the already-escaped subtitle is unobservable"
                .to_string(),
        );
    }
    if coverage.with_ads_banner == 0 || coverage.with_ads_banner == coverage.cases {
        reasons.push(
            "`enable_ads_header` takes only one value across the corpus, so one branch of \
             `base.html:172`'s `{% if %}` is never rendered"
                .to_string(),
        );
    }

    reasons
}

#[cfg(test)]
mod tests {
    use super::{
        Coverage, byte_difference, degeneracies, payload_has_astral, payload_has_overlap,
        payload_is_locked,
    };

    /// A coverage report that trips none of the checks — every counter set to
    /// what a healthy corpus produces. Each test below removes exactly one
    /// thing from this, so the failure it produces is attributable.
    fn sound() -> Coverage {
        let cases = 24;
        let mut coverage = Coverage {
            cases,
            with_marker: 5,
            with_astral: 2,
            with_overlap: 5,
            with_window_overflow: 5,
            // The page counters: every case rendered a page with all three
            // fields, one case has no collection, one is locked, one truncated
            // its description, one carries an escaped entity and one shows the
            // ad banner.
            pages: cases,
            with_collection: cases - 1,
            without_collection: 1,
            locked: 1,
            unlocked: cases - 1,
            with_truncated_description: 1,
            with_escaped_description: 1,
            with_page_fields: cases,
            with_ads_banner: 1,
            ..Coverage::default()
        };
        coverage.outputs.insert(vec!["a".to_string()]);
        coverage.outputs.insert(vec!["b".to_string()]);
        coverage.page_outputs.insert(vec!["p".to_string()]);
        coverage.page_outputs.insert(vec!["q".to_string()]);
        coverage
    }

    /// One row of the table below: what to call the change, how to make it, and a
    /// phrase the complaint about it has to contain.
    type Sabotage = (&'static str, fn(&mut Coverage), &'static str);

    /// Every page check, one at a time, and the phrase that must identify it.
    ///
    /// Table-driven because the point is that *each* mechanism is separately
    /// load-bearing: a single check that collapsed them would still pass while
    /// leaving, say, `shorten` unexercised.
    #[test]
    fn losing_one_page_mechanism_names_it() {
        let cases: &[Sabotage] = &[
            ("no page rendered", |c| c.pages = 0, "rendered a page"),
            (
                "every page identical",
                |c| {
                    c.page_outputs.clear();
                    c.page_outputs.insert(vec!["p".to_string()]);
                },
                "same canonical page",
            ),
            (
                "page fields missing",
                |c| c.with_page_fields = 0,
                "page title, description and url",
            ),
            (
                "no collection",
                |c| {
                    c.with_collection = 0;
                    c.without_collection = c.cases;
                },
                "never taken",
            ),
            (
                "every case in a collection",
                |c| {
                    c.with_collection = c.cases;
                    c.without_collection = 0;
                },
                "never exercised",
            ),
            (
                "all locked",
                |c| {
                    c.locked = c.cases;
                    c.unlocked = 0;
                },
                "inverted `free_access`",
            ),
            (
                "all unlocked",
                |c| {
                    c.locked = 0;
                    c.unlocked = c.cases;
                },
                "inverted `free_access`",
            ),
            (
                "no truncation",
                |c| c.with_truncated_description = 0,
                "textwrap.shorten",
            ),
            (
                "no escapes",
                |c| c.with_escaped_description = 0,
                "second escaping pass",
            ),
            ("no ad banner", |c| c.with_ads_banner = 0, "never rendered"),
            (
                "ad banner everywhere",
                |c| c.with_ads_banner = c.cases,
                "never rendered",
            ),
        ];

        for (what, break_it, expected) in cases {
            let mut coverage = sound();
            break_it(&mut coverage);
            let reasons = degeneracies(&coverage);
            assert!(
                reasons.iter().any(|reason| reason.contains(expected)),
                "{what}: expected a complaint containing {expected:?}, got {reasons:?}"
            );
        }
    }

    /// The two branches of the corpus checks are *both* required, which is the
    /// subtlety: a counter that is only checked for zero cannot catch a corpus
    /// that never takes the other branch.
    #[test]
    fn both_sides_of_a_two_branch_check_are_required() {
        let mut all_locked = sound();
        all_locked.locked = all_locked.cases;
        all_locked.unlocked = 0;
        assert_eq!(degeneracies(&all_locked).len(), 1);

        let mut all_unlocked = sound();
        all_unlocked.locked = 0;
        all_unlocked.unlocked = all_unlocked.cases;
        assert_eq!(degeneracies(&all_unlocked).len(), 1);
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

    /// A payload with no `isLocked` counts as unlocked, which is the side that
    /// keeps the coverage check honest: an absent field must not be silently
    /// counted as the *locked* case, or a corpus with no locked fixture would
    /// report one.
    #[test]
    fn lock_detection_defaults_to_unlocked() {
        let payload = |value: serde_json::Value| serde_json::json!({ "data": { "post": { "isLocked": value } } });
        assert!(payload_is_locked(&payload(serde_json::json!(true))));
        assert!(!payload_is_locked(&payload(serde_json::json!(false))));
        assert!(!payload_is_locked(&payload(serde_json::Value::Null)));
        assert!(!payload_is_locked(
            &serde_json::json!({ "data": { "post": {} } })
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

    /// The diagnostic for the one comparison that is bytes rather than canonical
    /// nodes. It has to name the offset, because the difference it exists to
    /// catch is a single trailing newline in a 23 KB document.
    #[test]
    fn a_byte_difference_names_its_offset() {
        let reported = byte_difference("base_bare", "abcdef", "abcXef");
        assert!(reported.contains("offset 3"), "{reported}");
        assert!(reported.contains("(of 6 vs 6)"), "{reported}");

        // A prefix is not a difference at an offset: there is none.
        let short = byte_difference("base_bare", "abcdef", "abc");
        assert!(short.contains("lengths differ (6 vs 3)"), "{short}");
        assert!(!short.contains("offset"), "{short}");
    }

    /// Slicing at the first differing *byte* can land inside a character, which
    /// a `&str` slice would panic on. The excerpt is the whole point of the
    /// message, so it has to survive that.
    #[test]
    fn a_difference_inside_a_character_does_not_panic_the_excerpt() {
        // `é` and `è` share their first UTF-8 byte, so the first differing byte
        // is at offset 4 — the second byte of a two-byte character, which is not
        // a char boundary in either string.
        let reported = byte_difference("base_bare", "café", "cafè");
        assert!(reported.contains("offset 4"), "{reported}");
        // The excerpt starts at the enclosing character, not at the byte: the
        // `f` is behind it and the `é`/`è` are whole.
        assert!(
            reported.contains("\"é\"") && reported.contains("\"è\""),
            "{reported}"
        );
    }
}
