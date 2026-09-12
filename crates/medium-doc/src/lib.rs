//! `medium-doc` — Medium GraphQL JSON → `Document` IR.
//!
//! Phase 1 of the Rust rewrite builds the parser and renderer here. Phase 0
//! only lands the one piece that is a *blocking risk* rather than a
//! translation job: [`difflib`], the port of CPython's
//! `difflib.SequenceMatcher` that decides whether a paragraph duplicates the
//! article title/subtitle (`medium-parser/medium_parser/utils.py:110`).

pub mod difflib;
