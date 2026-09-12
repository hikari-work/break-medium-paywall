//! `medium-render` — `Document` IR → output formats.
//!
//! Phase 1 ships HTML only ([`html`]). Markdown (§2.1's other promised free
//! win) and the public JSON DTO are Fase 6.
//!
//! The block-level half of the HTML output: the Tailwind class strings, the
//! attribute order, and the wrapper elements. The *inline* half — text, tags and
//! escaping — lives in `medium_doc::inline_html`, because the parser needs the
//! same rendering to reproduce the legacy highlight check (`core.py:309`).
//!
//! The point of rendering from the IR rather than splicing strings is that HTML
//! escaping happens exactly once, at emit time. `Inline::Text` carries raw text;
//! nothing upstream of the renderer is aware of entities.
//!
//! [`post`] is the odd one out: it renders nothing. It holds [`post::RenderedPost`],
//! the packaged result that gets cached — placed here so that the crate which
//! stores it does not have to know what HTML is, and this one does not have to
//! know what Redis is.
//!
//! [`templates`] and [`page`] are Fase 3: the same six Jinja templates the legacy
//! server serves, embedded, plus the contexts it hands them. They live here
//! rather than in the `freedium-web` binary because `xtask/difftest` has to
//! render a page without a database, an axum router or a `[bin]` to depend on.

pub mod html;
pub mod page;
pub mod post;
pub mod templates;
