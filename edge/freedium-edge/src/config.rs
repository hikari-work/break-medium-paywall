//! The edge's configuration, from the environment.
//!
//! Same rule as `freedium-web`'s `config.rs`: read `std::env` only, never a
//! `.env` file, so the binary does not behave differently depending on the
//! directory it was started from. Compose passes the values.
//!
//! # The one default that matters, and why it is `false`
//!
//! [`Config::shadow_enabled`] defaults to `false`. An edge with no
//! `SHADOW_ENABLED` is a **plain pass-through** — it proxies and does nothing
//! else. That is the rollback: §5's requirement is that putting Python back in
//! front is one line, and a kill switch whose default is "on" cannot be that.
//!
//! With the switch on, [`Config::shadow_sample`] defaults to `1.0`: *every*
//! eligible request is mirrored. That looks like the aggressive choice and it is
//! the conservative one. A sample rate that defaults low makes a misconfigured
//! soak look like a clean one — few comparisons, no differences, a green gate —
//! so the failure mode of the opposite default is a gate that cannot fail. The
//! rate is a performance dial, and Fase 4 ships it wide open so that the local
//! proof compares the whole corpus; §5's soak is where it gets lowered, from a
//! measurement of what one extra `cache` read costs rather than from a guess.

use std::path::PathBuf;

/// `EDGE_LISTEN`.
///
/// 6755, not Caddy's 6752: in the `local` compose profile Caddy is still running
/// and still holds 6752, and the edge is beside it rather than in front of it
/// during the proof. Fase 7 is where it takes the port Caddy has.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:6755";

/// `EDGE_UPSTREAM` — the Python app the edge proxies for and compares against.
///
/// 7080 because that is where `freedium_web_mini` publishes itself in the
/// `local` profile. Deliberately **not** Caddy: the comparison this phase is
/// about is Rust-against-Python, and putting Caddy in the middle would compare
/// Python-plus-Caddy's-output against Python's, which is Fase 7's question.
pub const DEFAULT_UPSTREAM: &str = "127.0.0.1:7080";

/// `SHADOW_UPSTREAM` — the Rust instance, which must be running with
/// `SHADOW_MODE=true`.
///
/// 7081 rather than 7080, because the containerised Python publishes 7080 on the
/// host too. A shadow on the same port would be a bind failure, or worse, a
/// shadow that is really the primary.
pub const DEFAULT_SHADOW_UPSTREAM: &str = "127.0.0.1:7081";

/// `SHADOW_LOG` — where the JSONL records go.
pub const DEFAULT_SHADOW_LOG: &str = "shadow.jsonl";

/// `SHADOW_SAMPLE` — the fraction of eligible requests to mirror, `0.0`–`1.0`.
///
/// `1.0`, and see the module docs for why the aggressive-looking default is the
/// conservative one.
pub const DEFAULT_SHADOW_SAMPLE: f64 = 1.0;

/// `SHADOW_TIMEOUT_MS` — how long the shadow request may take.
///
/// Generous relative to a page render, because this request is served by an
/// instance nobody is waiting on and a timeout that fires is recorded as
/// `unreachable` — which the report's degeneracy check reads as a thin corpus,
/// not as a difference. Cutting it close would manufacture that.
pub const DEFAULT_SHADOW_TIMEOUT_MS: u64 = 5_000;

/// `SHADOW_MAX_BODY` — the most of a response the edge will buffer to compare.
///
/// 2 MiB. The largest page the render gate produces is a few hundred kilobytes,
/// so this is headroom rather than a limit anything real meets. It exists because
/// the alternative is unbounded: the edge accumulates bodies in memory on the
/// request path, and a response that is not a page (a huge asset, a runaway
/// upstream) would be buffered in full by a component that is supposed to be a
/// pass-through. Over the cap the request is recorded as excluded, not truncated
/// — a truncated body would compare as a difference and read as a renderer bug.
pub const DEFAULT_SHADOW_MAX_BODY: usize = 2 * 1024 * 1024;

/// Why the edge could not be configured. Both cases are boot failures.
#[derive(Debug)]
pub enum ConfigError {
    Invalid {
        name: &'static str,
        value: String,
        expected: &'static str,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid {
                name,
                value,
                expected,
            } => write!(
                f,
                "{name} is set to {value:?}, which is not a valid {expected}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct Config {
    /// `EDGE_LISTEN` — where the edge accepts client connections.
    pub listen: String,

    /// `EDGE_UPSTREAM` — the primary: Python.
    pub upstream: String,

    /// `SHADOW_UPSTREAM` — the Rust instance the sample is mirrored onto.
    pub shadow_upstream: String,

    /// `SHADOW_ENABLED` — the kill switch. See the module docs.
    pub shadow_enabled: bool,

    /// `SHADOW_SAMPLE` — the fraction of eligible requests to mirror, `0.0`–`1.0`.
    pub shadow_sample: f64,

    /// `SHADOW_TIMEOUT_MS`.
    pub shadow_timeout_ms: u64,

    /// `SHADOW_MAX_BODY`, in bytes.
    pub shadow_max_body: usize,

    /// `SHADOW_LOG` — the JSONL file, opened append so a restarted edge continues
    /// the soak rather than truncating it.
    pub shadow_log: PathBuf,

    /// `SHADOW_DECLARATIONS` — the file of accepted differences.
    ///
    /// Loaded **by the edge**, not only by the report: a declaration is what
    /// turns a difference into a pass, and the decision has to be made where the
    /// bodies are. The report takes the same file so it can report a declaration
    /// that never fired — see `docs` in `Declarations`.
    pub shadow_declarations: Option<PathBuf>,

    /// `EDGE_THREADS` — Pingora service threads.
    ///
    /// Defaults to the machine's parallelism rather than to Pingora's `1`,
    /// because the comparison is CPU-bound work (parsing two pages of HTML) and
    /// this process is also serving live traffic. One thread would mean every
    /// mirrored page stalls a real client for the duration of two parses.
    pub threads: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let fallback_threads = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .max(2);

        Ok(Self {
            listen: text("EDGE_LISTEN", DEFAULT_LISTEN),
            upstream: text("EDGE_UPSTREAM", DEFAULT_UPSTREAM),
            shadow_upstream: text("SHADOW_UPSTREAM", DEFAULT_SHADOW_UPSTREAM),
            shadow_enabled: boolean("SHADOW_ENABLED", false)?,
            shadow_sample: fraction("SHADOW_SAMPLE", DEFAULT_SHADOW_SAMPLE)?,
            shadow_timeout_ms: number("SHADOW_TIMEOUT_MS", DEFAULT_SHADOW_TIMEOUT_MS)?,
            shadow_max_body: number("SHADOW_MAX_BODY", DEFAULT_SHADOW_MAX_BODY)?,
            shadow_log: PathBuf::from(text("SHADOW_LOG", DEFAULT_SHADOW_LOG)),
            shadow_declarations: optional("SHADOW_DECLARATIONS").map(PathBuf::from),
            threads: number("EDGE_THREADS", fallback_threads)?,
        })
    }
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn text(name: &str, default: &str) -> String {
    optional(name).unwrap_or_else(|| default.to_string())
}

fn number<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
{
    let Some(raw) = optional(name) else {
        return Ok(default);
    };
    raw.trim().parse::<T>().map_err(|_| ConfigError::Invalid {
        name,
        value: raw,
        expected: "number",
    })
}

/// A probability, and the one place `NaN` could get in.
///
/// Rejected rather than clamped: a `SHADOW_SAMPLE=0.5x` typo that silently
/// became `1.0` would mirror everything on a process whose operator thought it
/// was mirroring half.
fn fraction(name: &'static str, default: f64) -> Result<f64, ConfigError> {
    let Some(raw) = optional(name) else {
        return Ok(default);
    };
    let value = raw
        .trim()
        .parse::<f64>()
        .map_err(|_| ConfigError::Invalid {
            name,
            value: raw.clone(),
            expected: "number between 0.0 and 1.0",
        })?;
    if !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::Invalid {
            name,
            value: raw,
            expected: "number between 0.0 and 1.0",
        });
    }
    Ok(value)
}

/// `starlette.config`'s boolean cast: `1/true/yes/on/t/y` and their negatives,
/// case-insensitive.
///
/// **A deliberate duplicate** of `crates/freedium-web/src/config.rs::boolean`.
/// The two live in different cargo workspaces (RUST_REWRITE_PLAN §3.4), so
/// neither can call the other, and the alternative — a shared crate for one
/// nine-line function — would be a crate whose only reason to exist is that two
/// workspaces both have a `.env`. The tests below assert the same spelling list
/// as that function's, so if one grows a spelling the other's test is where a
/// reader will notice the asymmetry.
fn boolean(name: &'static str, default: bool) -> Result<bool, ConfigError> {
    let Some(raw) = optional(name) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "t" | "y" => Ok(true),
        "0" | "false" | "no" | "off" | "f" | "n" => Ok(false),
        _ => Err(ConfigError::Invalid {
            name,
            value: raw,
            expected: "boolean (1/0, true/false, yes/no, on/off)",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three defaults the phase's safety rests on. `shadow_enabled` off is
    /// the kill switch; `shadow_sample` at 1.0 is what keeps a misconfigured soak
    /// from looking clean; the two upstreams differing is what keeps a shadow
    /// from being compared against itself.
    #[test]
    fn the_safe_defaults_are_the_defaults() {
        assert_eq!(DEFAULT_LISTEN, "0.0.0.0:6755");
        assert_eq!(DEFAULT_UPSTREAM, "127.0.0.1:7080");
        assert_eq!(DEFAULT_SHADOW_UPSTREAM, "127.0.0.1:7081");
        assert_eq!(DEFAULT_SHADOW_SAMPLE, 1.0);
        assert_ne!(
            DEFAULT_UPSTREAM, DEFAULT_SHADOW_UPSTREAM,
            "mirroring a server onto itself compares it against itself and always passes"
        );
    }

    #[test]
    fn a_fraction_outside_the_unit_interval_is_rejected() {
        // `fraction` reads the environment, so the pure part is exercised
        // through the same classification it does.
        for (raw, expected) in [
            ("0", Some(0.0)),
            ("0.5", Some(0.5)),
            ("1", Some(1.0)),
            (" 0.25 ", Some(0.25)),
            ("nan", None),
            ("1.5", None),
            ("-0.1", None),
            ("half", None),
        ] {
            let parsed = raw
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| (0.0..=1.0).contains(v));
            assert_eq!(parsed, expected, "{raw:?}");
        }
    }

    /// The same spellings `freedium-web`'s `boolean` accepts, asserted here so
    /// that the duplicate stays a duplicate.
    #[test]
    fn booleans_accept_the_starlette_spellings() {
        for truthy in ["1", "true", "TRUE", "Yes", "on", "T", "y", " true "] {
            assert_eq!(classify(truthy), Some(true), "{truthy:?}");
        }
        for falsy in ["0", "false", "NO", "off", "F", "n", " false "] {
            assert_eq!(classify(falsy), Some(false), "{falsy:?}");
        }
        assert_eq!(classify("maybe"), None, "junk must not silently be false");
    }

    fn classify(raw: &str) -> Option<bool> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "t" | "y" => Some(true),
            "0" | "false" | "no" | "off" | "f" | "n" => Some(false),
            _ => None,
        }
    }
}
