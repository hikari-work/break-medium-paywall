//! Embeds `templates/` into the binary.
//!
//! `minijinja-embed` splits this in two: this file runs the glob at build time
//! and writes a generated `add_template` call per file into `OUT_DIR`, and
//! `load_templates!` in `src/templates.rs` includes it. A template with a syntax
//! error therefore fails the *build*, not the first request — which is the point,
//! and why the templates are embedded rather than read from disk at startup.

fn main() {
    // `.html` only: the directory holds nothing else today, and restricting it
    // means an editor's `.swp` file cannot end up in the bundle.
    minijinja_embed::embed_templates!("templates", &[".html"]);
}
