//! `freedium-edge` — the Fase 4 Pingora edge.
//!
//! A pass-through in front of the primary that mirrors a sample of eligible
//! requests onto a second, Rust instance and compares the two answers. See
//! `README.md` for what it is *not* — it is not the production edge, and it
//! deliberately has none of the jobs Caddy still does.
//!
//! # Not `#[tokio::main]`
//!
//! Pingora owns the runtime. `Server::run_forever` boots its own multi-threaded
//! runtime and drives the services on it; wrapping `main` in `#[tokio::main]`
//! would create a second runtime, spawn the services onto the wrong one, and
//! silently give the shadow's `tokio::spawn` no reactor to run on. The build
//! would succeed and the comparisons would never happen, which is a failure mode
//! worth naming.
//!
//! # Boot fails loudly
//!
//! Every configuration problem — an unparseable number, a missing declarations
//! file, a shadow log that cannot be opened — exits before the listener is bound.
//! An edge that starts without being able to record its evidence would produce a
//! soak that reads as clean, and §5's gate cannot distinguish that from a soak
//! that passed.

mod config;
mod eligibility;
mod proxy;
mod shadow;

use std::net::ToSocketAddrs;
use std::process::ExitCode;
use std::sync::Arc;

use pingora::prelude::*;

use crate::config::Config;
use crate::proxy::FreediumEdge;
use crate::shadow::{Shadow, load_declarations};

/// `EX_CONFIG`, as `sysexits.h` defines it: the shell convention for "the
/// configuration is wrong, do not retry". Deliberately distinct from a runtime
/// failure so a supervisor can tell the two apart.
const EX_CONFIG: u8 = 78;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("freedium-edge: {message}");
            ExitCode::from(EX_CONFIG)
        }
    }
}

fn run() -> Result<(), String> {
    // Logging first, so a configuration problem is reported through the same
    // channel as everything else rather than only on stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().map_err(|error| error.to_string())?;

    // `add_tcp` panics on an address it cannot resolve, and a panic is a
    // confusing way to report a typo — it bypasses the exit code a supervisor
    // reads. Resolved once here so a bad `EDGE_LISTEN` fails the same way every
    // other configuration problem does.
    if let Err(error) = config.listen.to_socket_addrs() {
        return Err(format!(
            "EDGE_LISTEN is set to {:?}: {error}",
            config.listen
        ));
    }

    let config = Arc::new(config);

    let declarations = load_declarations(config.shadow_declarations.as_deref())
        .map_err(|error| format!("SHADOW_DECLARATIONS: {error}"))?;

    let shadow = Shadow::new(&config, declarations).map_err(|error| error.to_string())?;

    // The one summary line an operator needs: where the traffic goes, where the
    // mirror goes, and whether the mirror is on at all.
    if config.shadow_enabled {
        tracing::info!(
            listen = %config.listen,
            primary = %config.upstream,
            shadow = %config.shadow_upstream,
            sample = config.shadow_sample,
            declarations = config.shadow_declarations.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".to_string()),
            log = shadow.log_path(),
            "freedium-edge: mirroring a sample of eligible traffic",
        );
    } else {
        tracing::warn!(
            listen = %config.listen,
            primary = %config.upstream,
            "freedium-edge: SHADOW_ENABLED is off; this is a plain pass-through and will write no evidence"
        );
    }

    let mut server = Server::new(Some(Opt::default())).map_err(|error| error.to_string())?;

    let listen = config.listen.clone();
    let mut service = http_proxy_service(
        &server.configuration,
        FreediumEdge::new(Arc::clone(&config), shadow),
    );
    // Per *service*, not `server.configuration.threads`. The global setting lives
    // behind an `Arc<ServerConf>` and cannot be assigned through it; Pingora's
    // service-level override is the supported hook, and this service is the only
    // one that exists. `None` there means "follow the global setting".
    service.threads = Some(config.threads);
    service.add_tcp(&listen);

    server.add_service(service);
    server.run_forever();
}
