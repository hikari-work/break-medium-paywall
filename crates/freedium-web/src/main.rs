//! The binary: boot, shutdown, and the `healthcheck` subcommand.
//!
//! `__main__.py` → `services/cli.py` in the legacy, with one addition — the
//! `healthcheck` subcommand from §5 decision 4.
//!
//! # Why the port check is the bind, and not a probe
//!
//! `services/cli.py:23` calls `is_port_in_use(opts.port)` before starting, and
//! errors with `"Port {port} is in use or permission denied"`. That check is a
//! *probe*: it opens a socket, closes it, and reports what it found. Between the
//! probe and uvicorn's own bind, another process can take the port, and the
//! legacy would then fail with uvicorn's error rather than its own.
//!
//! Here the bind *is* the check, which closes that window and produces the same
//! outcome: a clear message and a non-zero exit. The message is not the legacy's
//! wording, because the information is the OS's (`Address already in use`) and
//! paraphrasing it would lose detail an operator wants.
//!
//! # The healthcheck has to work in a distroless image
//!
//! That is the whole reason it is a subcommand rather than the compose
//! healthcheck's `curl` (`docker-compose/docker-compose.main.yml:29`). The
//! runtime stage holds one static binary and nothing else — no shell, no curl,
//! no `CMD-SHELL` to run them in — so the check has to be the binary asking
//! itself. Until Fase 4 adds the compose service, nothing consumes this.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use freedium_web::config::{Config, DEFAULT_PORT};
use freedium_web::router;
use freedium_web::state::AppState;

/// `services/cli.py:7-13`.
const USAGE: &str = "\
Freedium server

Usage:
  freedium-web server [--port <PORT>]   Serve HTTP on 0.0.0.0
  freedium-web healthcheck              Check a local server and exit

Options:
  --port <PORT>   Port to bind (default 7080, or PORT from the environment)
  -h, --help      Print this message
";

/// The `User-Agent` the compose healthcheck sends
/// (`docker-compose/docker-compose.main.yml:29`).
///
/// Copied rather than chosen: the check is meant to be the same request the
/// current `CMD-SHELL` makes, so that switching the compose file over to this
/// subcommand is not also a change in what is being tested. It is a browser
/// string because anything in front of the app may treat a bot-like agent
/// differently, and a healthcheck that gets itself blocked is worse than none.
const HEALTHCHECK_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.3 Safari/605.1.15";

/// `curl --max-time 80` (`docker-compose/docker-compose.main.yml:29`).
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(80);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("server") => match parse_port(&args[1..]) {
            Ok(port) => server(port),
            Err(message) => {
                eprintln!("{message}\n\n{USAGE}");
                ExitCode::from(2)
            }
        },
        Some("healthcheck") => healthcheck(),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
        None => {
            eprintln!("a command is required\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `--port` (`services/cli.py:11`). `None` when it was not given, so the
/// environment can supply it instead.
fn parse_port(args: &[String]) -> Result<Option<u16>, String> {
    let mut port = None;
    let mut rest = args.iter();

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--port" => {
                let value = rest.next().ok_or("--port needs a value")?;
                port = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| format!("--port is not a port number: {value}"))?,
                );
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }

    Ok(port)
}

/// `server_cmd` (`services/cli.py:20-32`) — boot and serve until signalled.
#[tokio::main]
async fn server(cli_port: Option<u16>) -> ExitCode {
    // Before anything else, so a misconfiguration is a message rather than a
    // panic. `ADMIN_SECRET_KEY` is the one required variable.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("freedium-web: {error}");
            return ExitCode::FAILURE;
        }
    };

    init_tracing(&config);

    // The CLI wins over the environment, then the environment over the default —
    // `--port` has `const=7080, default=7080` and no fallback to a variable in
    // the legacy, so this is a superset of it.
    let port = cli_port.unwrap_or(config.port);

    tracing::info!("Application startup");

    let state = match AppState::new(config).await {
        Ok(state) => state,
        Err(error) => {
            tracing::error!(error = %error, "could not start");
            return ExitCode::FAILURE;
        }
    };

    // The listener first, so a port that is taken is reported before the banner
    // that says the server is up.
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(port, error = %error, "could not bind");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("Listening on 0.0.0.0:{port}");

    // Held separately from the state the router owns so the connection pool can
    // be closed on the way out — `server/main.py:37-38`'s `redis_storage.close()`
    // in the lifespan. `RedisStore` is a cheap clone over a pooled client.
    let redis = state.redis.clone();

    let served = axum::serve(
        listener,
        // `with_connect_info`, because the request log prints the peer address
        // (`middlewares/logger.py:31`). Without it that line logs `unknown`.
        router::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await;

    if let Err(error) = served {
        tracing::error!(error = %error, "the server stopped with an error");
        return ExitCode::FAILURE;
    }

    tracing::debug!("Close Redis connection");
    redis.close().await;
    tracing::info!("Application shutdown");

    ExitCode::SUCCESS
}

/// The tracing subscriber, at `LOG_LEVEL_NAME`'s level.
///
/// `RUST_LOG` wins when it is set, because that is what every Rust developer and
/// every `kubectl logs`-adjacent workflow expects, and because it is the only way
/// to raise the level of a single module without redeploying. Otherwise
/// `LOG_LEVEL_NAME` (`config.py:14`, default `INFO`) is the directive.
///
/// # The format is not loguru's, and that is a real difference
///
/// `utils/logger.py:29-37` builds `"[{process.id}] | {time} | {level} | {name}:{function}:{line} | [{extra[id]}] - {message}"`.
/// The `[{extra[id]}]` is the per-request correlation id, which loguru carries in
/// a `contextualize` context. `tracing` has the same idea but spells it as span
/// fields, and the equivalent here would be a span entered by the middleware —
/// not done in this phase, so the id appears as an explicit field on the lines
/// that log it rather than on all of them.
///
/// What that costs: a log line no longer carries the request id automatically, so
/// grepping a request's lines takes the id from the line that has it. Worth
/// stating plainly rather than leaving for someone to discover while debugging.
fn init_tracing(config: &Config) {
    let default = config.log_level_name.to_lowercase();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&default));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Resolves when the process is asked to stop.
///
/// `SIGTERM` is what `docker stop` sends, and `SIGINT` is Ctrl-C — both, because
/// the legacy's compose file sets `stop_grace_period: 2m`
/// (`docker-compose/docker-compose.main.yml:34`) on the assumption that the
/// process uses that time to finish in-flight requests, and a server that only
/// handled one of the two would be killed uncleanly by the other.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("the SIGINT handler can be installed");
    };

    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                // Cannot happen on Linux, where this binary runs. Reported rather
                // than panicking, so a port to another platform degrades to
                // Ctrl-C only instead of refusing to start.
                tracing::warn!(error = %error, "no SIGTERM handler; only Ctrl-C will stop the server");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        () = ctrl_c => tracing::debug!("interrupted"),
        () = terminate => tracing::debug!("terminated"),
    }
}

/// `healthcheck` — asks a local server for `/` and exits on what it answers.
///
/// `/` and not a dedicated endpoint, because `/` is what the compose healthcheck
/// fetches today and it is the only route that exercises the whole stack: config,
/// Postgres, the templates and the homepage render. A `/health` that returned
/// `200 OK` without touching any of them would report a healthy process that
/// cannot serve a page.
///
/// # It does not read the configuration
///
/// `Config::from_env` fails without `ADMIN_SECRET_KEY`, and a healthcheck should
/// not fail for a reason that is not the server's health. `PORT` is read directly
/// with the same default, so this works in a container that has the key and in
/// one where it was removed while debugging.
#[tokio::main]
async fn healthcheck() -> ExitCode {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}/");

    let client = match reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        .user_agent(HEALTHCHECK_USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            eprintln!("healthcheck: could not build a client: {error}");
            return ExitCode::FAILURE;
        }
    };

    match client.get(&url).send().await {
        // `curl -f` fails on any 4xx or 5xx, and so does this — a server
        // answering its own error page is not healthy.
        Ok(response) if response.status().is_success() => {
            println!("ok: {url} {}", response.status().as_u16());
            ExitCode::SUCCESS
        }
        Ok(response) => {
            eprintln!("healthcheck: {url} answered {}", response.status().as_u16());
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("healthcheck: {url} failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn the_port_defaults_to_none_so_the_environment_can_supply_it() {
        assert_eq!(parse_port(&[]), Ok(None));
        assert_eq!(parse_port(&args(&["--port", "8080"])), Ok(Some(8080)));
    }

    /// A bad port is a usage error, not a silent fall back to the default — the
    /// legacy's `type=int` gets this from argparse.
    #[test]
    fn a_bad_port_is_an_error() {
        assert!(parse_port(&args(&["--port"])).is_err());
        assert!(parse_port(&args(&["--port", "not-a-port"])).is_err());
        assert!(
            parse_port(&args(&["--port", "70000"])).is_err(),
            "out of range"
        );
        assert!(parse_port(&args(&["--port", "-1"])).is_err());
        assert!(parse_port(&args(&["--wat"])).is_err());
    }

    /// The UA is copied from the compose healthcheck verbatim; a drifted copy
    /// would silently test a different request than production sends.
    #[test]
    fn the_healthcheck_user_agent_is_the_compose_one() {
        assert!(
            HEALTHCHECK_USER_AGENT.starts_with("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)")
        );
        assert!(HEALTHCHECK_USER_AGENT.contains("Version/17.3 Safari/605.1.15"));
        assert_eq!(HEALTHCHECK_TIMEOUT, Duration::from_secs(80));
    }
}
