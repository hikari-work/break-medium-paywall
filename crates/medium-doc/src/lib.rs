//! `medium-doc` — Medium GraphQL JSON → `Document` IR.
//!
//! Phase 1 of the Rust rewrite builds the parser and renderer here. Phase 0
//! landed the one piece that is a *blocking risk* rather than a translation
//! job: [`difflib`], the port of CPython's `difflib.SequenceMatcher` that
//! decides whether a paragraph duplicates the article title/subtitle
//! (`legacy/medium-parser/medium_parser/utils.py:110`).
//!
//! Modules:
//!
//! - [`ir`] — the `Document`/`Block`/`Inline` types every output renders from.
//! - [`utf16`] — Medium's UTF-16 markup offsets → byte offsets, replacing
//!   `rl_string_helper` entirely.
//! - [`text`] — the quote folding applied before the offset map is built.
//! - [`escape`] — HTML escaping and the code-forces-minimal rule.
//! - [`inline`] — markup ranges → an `Inline` tree.
//! - [`inline_html`] — an `Inline` tree → HTML, shared by the renderer and by
//!   the parser's highlight check.
//! - [`parse`] — a GraphQL post payload → a `Document`.
//! - [`resolve`] — Medium URLs → post ids, and the domain lists that decide
//!   which URLs are Medium's at all.

pub mod difflib;
pub mod escape;
pub mod inline;
pub mod inline_html;
pub mod ir;
pub mod parse;
pub mod resolve;
pub mod text;
pub mod utf16;
