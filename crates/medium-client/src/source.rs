//! The seam the rewrite is built around.
//!
//! §2.6 puts `medium-client` behind this trait and makes it the only crate that
//! touches the outbound network. That is not tidiness — it is what keeps §3.1's
//! risk contained. The whole impersonation question is "which implementation of
//! this one method do we use", and the three candidates (a `rquest` client, a
//! libcurl-impersonate binding, a Python sidecar over HTTP) differ in nothing
//! a caller can see.
//!
//! It also means Fase 3's server can be written and tested against a source
//! that returns fixed JSON, with no network and no WARP pool.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::FetchError;

/// Fetches one post's raw GraphQL payload.
///
/// The return type is the decoded `Value`, not the `Document` IR: parsing is
/// `medium-doc`'s job, and this crate has no opinion about the document. What
/// it promises is that the value it returns has been through
/// [`crate::response::validate`], so a successful call means `data.post` is
/// present.
#[async_trait]
pub trait PostSource: Send + Sync {
    async fn fetch_post(&self, post_id: &str) -> Result<Value, FetchError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A source with no network behind it — the point of the seam.
    struct FixedSource(Value);

    #[async_trait]
    impl PostSource for FixedSource {
        async fn fetch_post(&self, _post_id: &str) -> Result<Value, FetchError> {
            Ok(self.0.clone())
        }
    }

    /// The trait must be usable as a trait object, because callers hold the
    /// chosen implementation behind `Arc<dyn PostSource>` — that indirection is
    /// what lets SPIKE-1's outcome be swapped in without touching a caller.
    #[tokio::test]
    async fn the_trait_is_object_safe() {
        let source: std::sync::Arc<dyn PostSource> =
            std::sync::Arc::new(FixedSource(json!({"data": {"post": {"id": "abc"}}})));

        let payload = source.fetch_post("abc").await.unwrap();
        assert_eq!(payload["data"]["post"]["id"], "abc");
    }
}
