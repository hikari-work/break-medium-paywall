//! `freedium-web` — the Fase 3 server.
//!
//! Ported from `legacy/web/server/`, which stays frozen and runnable as the
//! parity reference. Nothing user-visible changes in this phase: the point is
//! that Fase 4 can mirror production traffic onto this binary and diff the two.
//! There is deliberately **no public API** here — §2.7 puts it in Fase 6.
//!
//! The modules, and the legacy file each one answers to:
//!
//! | here | `legacy/web/server/` |
//! |---|---|
//! | [`config`] | `config.py` |
//! | [`state`] | `__init__.py`'s module globals |
//! | [`router`] | `main.py` |
//! | [`middleware`] | `middlewares/` |
//! | [`handlers`] | `handlers/` |
//! | [`error`] | `utils/error.py`, `utils/exceptions.py` |
//! | [`notify`] | `utils/notify.py` |
//! | [`transponder`] | the id/code pair from `middlewares/logger.py` |
//!
//! Most of it is a library rather than part of `main.rs` so that the pieces can
//! be tested without a socket — [`state::tests`] builds the whole [`AppState`]
//! with no database and no Redis, which is what lets the router's behaviour be
//! asserted rather than assumed.

pub mod config;
pub mod error;
pub mod handlers;
pub mod middleware;
pub mod notify;
pub mod router;
pub mod state;
pub mod transponder;
