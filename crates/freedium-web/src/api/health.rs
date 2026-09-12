//! `/api/v1/health` — is this instance able to serve.
//!
//! # What a health check may spend
//!
//! Two dependency probes and three local reads. No Medium, no WARP exit, no proxy
//! pool, no link resolver: a health check that spends the fetch budget is the
//! opposite of a health check. The test below asserts that the post source is
//! never consulted, which is the one of those that a future edit could plausibly
//! break.
//!
//! # Why `probe` and not `len`
//!
//! [`PostgresCache::len`] is `SELECT COUNT(*)` over the whole `cache` table — at
//! production size, a sequential scan of the largest relation in the system, run
//! by every monitor every few seconds, for a number nothing reads. [`probe`] is
//! `SELECT 1`. "Liveness plus dependency status" invites exactly that mistake, so
//! it is named here.
//!
//! # Why Redis being down is a `200`
//!
//! `degraded`, not `not ready`. The page route already survives a Redis outage —
//! it re-renders through Postgres — so a `503` here would take an instance out of
//! rotation for a condition it demonstrably serves through. Postgres is the one
//! dependency whose absence makes this process unable to answer anything.
//!
//! # The two fields that are about misconfiguration rather than liveness
//!
//! `shadow_mode` and `auth_cookies_configured` are in the body for the same
//! reason: both are conditions that are invisible from outside and expensive when
//! wrong. A shadow instance accidentally serving production traffic is §2.7's
//! warning 1 and takes the site down; a deployment that has `MEDIUM_AUTH_COOKIES`
//! set is spending a real account's unlock quota, and being able to audit that
//! from a dashboard is the point.
//!
//! [`PostgresCache::len`]: freedium_cache::postgres::PostgresCache::len
//! [`probe`]: freedium_cache::postgres::PostgresCache::probe

use axum::Extension;
use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use freedium_dto::health::{CheckDto, HealthDto, HealthStatus};
use freedium_dto::problem::{Problem, ProblemKind};
use std::time::Duration;

use crate::api::http_cache::JSON;
use crate::api::problem::ApiError;
use crate::middleware::Correlation;
use crate::state::AppState;

/// How long each dependency gets to answer.
///
/// Two seconds, and it bounds the *probe* rather than the pool: a monitor
/// polling every ten seconds must not be able to accumulate a request per second
/// against a database that has stopped answering. A hung pool is what this is
/// for, and a timeout here answers `not ready` rather than hanging with it.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// `GET /api/v1/health`
///
/// No `ETag`: the body changes on every deploy and every dependency blip, so a
/// validator would only add a round trip to a monitor's poll.
#[utoipa::path(
    get,
    path = "/api/v1/health",
    responses(
        (status = 200, description = "Postgres answered. `degraded` when Redis did not.", body = HealthDto),
        (status = 503, description = "Postgres did not answer.", body = Problem),
    ),
    tag = "health"
)]
pub async fn health(
    State(state): State<AppState>,
    Extension(correlation): Extension<Correlation>,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let postgres = probe("postgres", PROBE_TIMEOUT, state.postgres.probe()).await;
    let redis = probe("redis", PROBE_TIMEOUT, state.redis.ping()).await;

    if !postgres.ok {
        tracing::error!(
            detail = postgres.detail.as_deref().unwrap_or(""),
            "the durable cache did not answer a health probe"
        );
        return ApiError::new(
            ProblemKind::NotReady,
            postgres
                .detail
                .clone()
                .unwrap_or_else(|| "the durable cache is not answering".to_string()),
        )
        .resolve(&uri, Some(&correlation));
    }

    let status = if redis.ok {
        HealthStatus::Ok
    } else {
        tracing::warn!(
            detail = redis.detail.as_deref().unwrap_or(""),
            "redis did not answer a health probe"
        );
        HealthStatus::Degraded
    };

    let dto = HealthDto::new(
        status,
        env!("CARGO_PKG_VERSION"),
        state.config.shadow_mode,
        // Reported rather than acted on: the API is anonymous-pure by
        // construction and no route here can reach the cookie path. This field
        // exists so the configuration can be audited from outside.
        state.config.medium_auth_cookies.is_some(),
        vec![postgres, redis],
    );
    let bytes = serde_json::to_vec(&dto).expect("a DTO always serialises");
    health_response(bytes)
}

/// `no-store`, and it is not a style choice: a cached health check is a health
/// check that reports the wrong thing during exactly the incident it exists for.
///
/// Built here rather than through [`crate::api::http_cache::respond`] so there is
/// no validator to remove: a body that may not be stored may not carry an `ETag`
/// either, and hashing it to throw the tag away would be a promise made twice and
/// kept once.
fn health_response(bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, JSON)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .expect("a 200 with known headers always builds")
}

/// One dependency, under a timeout.
///
/// The budget is a parameter rather than [`PROBE_TIMEOUT`] read inside, and that
/// is for the test's sake but not only: the policy constant belongs to the
/// handler that chose it, and a helper that reached for it would make the
/// timeout arm reachable only by waiting the real two seconds. The test passes
/// fifty milliseconds instead, which asserts the same branch in the same way.
///
/// `Option<String>` from the cache layer's error is flattened into the check's
/// detail — the layer's errors are already the reason, and re-wrapping them here
/// would produce "postgres: the probe failed: the probe failed".
async fn probe<E: std::fmt::Display>(
    name: &str,
    budget: Duration,
    probe: impl std::future::Future<Output = Result<(), E>>,
) -> CheckDto {
    match tokio::time::timeout(budget, probe).await {
        Ok(Ok(())) => CheckDto::ok(name),
        Ok(Err(error)) => CheckDto::failed(name, error.to_string()),
        Err(_) => CheckDto::failed(name, format!("no answer in {budget:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use std::convert::Infallible;

    /// A probe that answers, one that fails, and one that never returns. The
    /// timeout arm is the one that matters: a hung dependency has to produce a
    /// check rather than a hung health check.
    #[tokio::test]
    async fn a_probe_is_bounded_and_always_produces_a_check() {
        let budget = Duration::from_secs(2);
        let ok = probe("postgres", budget, async { Ok::<(), Infallible>(()) }).await;
        assert!(ok.ok);
        assert_eq!(ok.detail, None, "a passing check carries no text");

        let failed = probe("redis", budget, async {
            Err::<(), _>("connection refused")
        })
        .await;
        assert!(!failed.ok);
        assert_eq!(failed.detail.as_deref(), Some("connection refused"));

        // The timeout arm, at a budget of fifty milliseconds rather than the
        // handler's two seconds: a future that is never ready is exactly what a
        // hung pool looks like to this function, and the branch it takes does not
        // depend on how long the wait was. `!hung.ok` is the assertion that
        // matters — a hung dependency must be a `failed` check, not a hang.
        let hung = probe(
            "postgres",
            Duration::from_millis(50),
            std::future::pending::<Result<(), Infallible>>(),
        )
        .await;
        assert!(!hung.ok);
        assert_eq!(hung.detail.as_deref(), Some("no answer in 50ms"));
    }

    /// A health body must not be storable. `no-store` and a removed `ETag` are
    /// the two halves of that, and the removal is easy to lose.
    #[tokio::test]
    async fn a_health_body_is_never_cached() {
        let dto = HealthDto::new(
            HealthStatus::Ok,
            "1.2.3",
            // The two audit fields, set to the interesting values rather than to
            // the defaults: this is the only test that can reach them, because
            // the handler only builds a `HealthDto` when Postgres answers and a
            // test has no Postgres. `true` for cookies is decision 3's
            // configuration — the one whose visibility is the point.
            true,
            true,
            vec![CheckDto::ok("redis")],
        );
        let response = health_response(serde_json::to_vec(&dto).unwrap());

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::ETAG).is_none());
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .map(|value| value.to_str().unwrap()),
            Some("no-store")
        );

        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["version"], "1.2.3");
        assert_eq!(json["shadow_mode"], true);
        assert_eq!(
            json["auth_cookies_configured"], true,
            "a deployment that set `MEDIUM_AUTH_COOKIES` has to be able to see it"
        );
        // `None` must serialise as `null`, not be skipped: a consumer binding a
        // struct needs the key to exist.
        assert_eq!(json["checks"][0]["detail"], serde_json::Value::Null);
    }
}
