//! The canonical form a rendered page is compared in, and the verdict two
//! answers are reduced to.
//!
//! # Why this is a crate and not a module of the harness
//!
//! It was `xtask/difftest/src/canonical.rs` — the render gate's normaliser —
//! and it moved here when Fase 4 needed the *same* comparison in a second
//! place: the Pingora edge that mirrors live traffic onto the Rust server
//! (`edge/freedium-edge`). Two implementations of "are these two pages the
//! same" would drift, and the gate's semantics are the contract everything
//! else is measured against. §3.4 puts the edge in a **separate cargo
//! workspace**, which is why this cannot simply be a `pub mod` of the harness:
//! a workspace cannot re-export a module across its own boundary, and the edge
//! must not depend on a `[bin]`.
//!
//! Nothing here ships a page. [`canonical::canonicalize`] parses HTML, and the
//! production crates only ever *emit* it — `html5ever` stays out of
//! `medium-render` on purpose.
//!
//! # The three layers
//!
//! | Module | Question it answers |
//! |---|---|
//! | [`canonical`] | What is this page, in a form two renderers can agree on? |
//! | [`compare`] | Given what each server answered, is that a difference? |
//! | [`record`] | What does one shadowed request leave in the log? |
//!
//! They were one concern in the render gate, where every fixture is a success
//! and the only question is whether the markup matches. Shadow traffic adds the
//! cases a fixture cannot have: a request the shadow declined to answer at all,
//! an error page, a status one side got wrong. Those are decisions about
//! *comparing*, not about canon, so they live in [`compare`] — but they use
//! [`canonical`] to make the markup half of the call, which is the point of
//! keeping them in one crate.
//!
//! [`record`] is here for the same reason one layer up. It is written by the
//! edge and read by `difftest shadow-report`, and those two live in different
//! cargo workspaces (§3.4) — so a second definition of the log format could not
//! be caught by the compiler anywhere else.
//!
//! [`SHADOW_HEADER`] and [`SHADOW_NO_FETCH`] ride along for exactly that reason,
//! one step further out: they are the two strings on the wire between the server
//! and the edge, and nothing else in the build can see both processes.

pub mod canonical;
pub mod compare;
pub mod record;

pub use canonical::{Annotation, CanonicalNode, canonicalize};
pub use compare::{
    Answer, Declaration, Declarations, DiffKind, Difference, Outcome, Served, compare,
};
pub use record::{
    DeclaredMatch, RecordOutcome, SHADOW_HEADER, SHADOW_NO_FETCH, ShadowRecord, Side,
};
