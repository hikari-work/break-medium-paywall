//! `freedium-web` — the Fase 3 server.
//!
//! Ported from `legacy/web/server/`, which stays frozen and runnable as the
//! parity reference. Nothing user-visible changes in this phase: the point is
//! that Fase 4 can mirror production traffic onto this binary and diff the two.
//!
//! # The one thing with no legacy counterpart
//!
//! [`api`] — `/api/v1`, added in Fase 6. The legacy has no analogue at all (its
//! `/api*` prefix is a `403` from Caddy, `caddy/generate_caddy_file.py:21`), so
//! it is not ported from anything and has no difftest coverage; it is specified
//! by §2.7 and by its own tests. The column below is empty for it on purpose.
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
//! | [`api`] | — (Fase 6) |
//!
//! Most of it is a library rather than part of `main.rs` so that the pieces can
//! be tested without a socket — [`state::tests`] builds the whole [`AppState`]
//! with no database and no Redis, which is what lets the router's behaviour be
//! asserted rather than assumed.

pub mod api;
pub mod config;
pub mod error;
pub mod handlers;
pub mod middleware;
pub mod notify;
pub mod router;
pub mod state;
pub mod transponder;
