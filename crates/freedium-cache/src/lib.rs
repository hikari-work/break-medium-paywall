//! `freedium-cache` — the Postgres and Redis backends, for Fase 2 of the Rust
//! rewrite (`RUST_REWRITE_PLAN.md` §5).
//!
//! These two are one crate because they are used as a pair on one path: render
//! reads Redis, misses, reads Postgres, misses, fetches, then writes both. They
//! stay separate *modules* because they fail differently — Redis is a cache in
//! front of a cache and is routinely bypassed
//! ([`redis::RedisStore::is_available`]), while Postgres is the durable corpus.
//!
//! Modules:
//!
//! - [`decode`] — a stored `cache.value` back into JSON, including the `\x`
//!   hex-encoded rows that need the legacy workaround.
//! - [`postgres`] — the `cache` table. Same schema, same keys, same bytes as
//!   the Python backend, so the two can run side by side.
//! - [`redis`] — MessagePack values under the `v2:` namespace.
//! - [`keys`] — that namespace, and why it is not the legacy one.
//! - [`error`] — one error type for both.
//!
//! There is deliberately no `RenderedPost` here. [`redis::RedisStore`] is
//! generic over the payload, and the concrete type it stores is defined by
//! whoever produces it (`medium-render`). This crate knows what a *cache* is
//! and nothing about what a *post* is.
//!
//! Like every crate in this workspace there are no re-exports: consumers name
//! the full path (`freedium_cache::postgres::PostgresCache`).

pub mod decode;
pub mod error;
pub mod keys;
pub mod postgres;
pub mod redis;
