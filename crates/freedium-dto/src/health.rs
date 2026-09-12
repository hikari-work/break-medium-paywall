//! The liveness contract.
//!
//! # What a health check may spend
//!
//! Two dependency probes, each under a 2-second timeout, and three local reads.
//! Nothing else: no Medium, no WARP exit, no proxy pool, no link resolver. A
//! health check that spends the fetch budget is the opposite of a health check,
//! and `freedium-web` has a test asserting the post source is never touched.
//!
//! # Why the checks are in the body rather than only in the status
//!
//! A load balancer reads the status code; a human reads the body. Reporting
//! *which* dependency is down is the difference between "the instance is
//! unready" and a five-minute investigation, and the two checks are independent
//! enough that the distinction is real.

use serde::{Deserialize, Serialize};

/// Whether the instance can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Both dependencies answered.
    Ok,
    /// Postgres answered and Redis did not. Still a `200`: a Redis outage is
    /// survivable — the page route degrades to Postgres-only — and a `503` here
    /// would take the instance out of rotation for a condition it serves through.
    Degraded,
}

/// The `GET /api/v1/health` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HealthDto {
    pub schema_version: u8,
    pub status: HealthStatus,
    /// `CARGO_PKG_VERSION`, so a deployment can be identified from a dashboard.
    pub version: String,
    /// **Worth looking at.** A shadow instance accidentally serving production
    /// traffic is the misconfiguration that takes the site down, and one field
    /// makes it visible without reading a process environment.
    pub shadow_mode: bool,
    /// Whether `MEDIUM_AUTH_COOKIES` is configured.
    ///
    /// The API is anonymous-pure by construction and never takes the cookie path,
    /// but the quota that path spends is bound to a real account, so whether a
    /// deployment has it configured is worth being able to audit from outside.
    pub auth_cookies_configured: bool,
    pub checks: Vec<CheckDto>,
}

impl HealthDto {
    #[must_use]
    pub fn new(
        status: HealthStatus,
        version: impl Into<String>,
        shadow_mode: bool,
        auth_cookies_configured: bool,
        checks: Vec<CheckDto>,
    ) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            status,
            version: version.into(),
            shadow_mode,
            auth_cookies_configured,
            checks,
        }
    }
}

/// One dependency probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CheckDto {
    /// `"postgres"` or `"redis"`.
    pub name: String,
    pub ok: bool,
    /// Why it failed. `None` when `ok`, so a passing check carries no text that
    /// a reader might mistake for a warning.
    pub detail: Option<String>,
}

impl CheckDto {
    #[must_use]
    pub fn ok(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: true,
            detail: None,
        }
    }

    #[must_use]
    pub fn failed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok: false,
            detail: Some(detail.into()),
        }
    }
}
