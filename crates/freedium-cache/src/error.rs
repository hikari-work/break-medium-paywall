//! The error type both cache backends share.
//!
//! One enum rather than one per backend, because the two are used together on
//! the same path: a post render reads Redis, misses, reads Postgres, misses,
//! fetches, then writes both. A caller that has to name two error types at
//! every step of that sequence is a caller that will `unwrap` instead.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("postgres: {0}")]
    Postgres(#[from] sqlx::Error),

    #[error("redis: {0}")]
    Redis(#[from] fred::error::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// A value that failed a JSON parse *and* failed to decode as hex. This is
    /// the corrupt-row case, and it is deliberately distinct from `Json`: the
    /// first parse failing is routine (see [`crate::decode`]), the fallback
    /// failing means the row is genuinely unreadable.
    #[error("value is neither JSON nor hex-encoded JSON: {0}")]
    Hex(#[from] hex::FromHexError),

    #[error("hex-decoded value is not UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("msgpack encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("msgpack decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),

    /// A malformed connection URL, or a client the builder refused to
    /// construct. Distinct from [`CacheError::Redis`] because it is a
    /// configuration fault rather than a runtime one — retrying will not help,
    /// and the operator needs to see that.
    #[error("redis configuration: {0}")]
    Config(String),
}
