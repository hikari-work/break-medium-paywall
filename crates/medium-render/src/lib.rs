//! `medium-render` — `Document` IR → output formats.
//!
//! Phase 1 ships HTML only ([`html`]). Fase 6 adds [`dto`], the public JSON
//! contract's half of the mapping, and [`markdown`], the fragment the public API
//! serves. Both read the IR; neither is covered by a differential gate, and
//! [`markdown`] has no legacy oracle at all — its tests are its specification.
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
//!
//! [`dto`] is the one module here that renders nothing *and* reads a second
//! source: the block tree comes from the IR, but every metadata string comes from
//! the raw payload, because the IR's [`medium_doc::ir::PostMeta`] is not where the
//! contract's values live. The module docs say why.
//!
//! [`markdown`] is the only module whose output no test can be checked against
//! anything but itself, so its tests carry more weight than their count suggests:
//! they are the specification of `/api/v1/posts/{id}/markdown`.

pub mod dto;
pub mod html;
pub mod markdown;
pub mod page;
pub mod post;
pub mod templates;
