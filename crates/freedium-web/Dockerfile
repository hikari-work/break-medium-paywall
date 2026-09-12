# syntax=docker/dockerfile:1

# The Rust server's image — Fase 3 of RUST_REWRITE_PLAN, §5.
#
# **The build context is the repository root.** The Cargo workspace, its
# lockfile and the `caddy/static` copy this image serves all live above
# `crates/`, so build it from the root:
#
#     docker build -f crates/freedium-web/Dockerfile -t freedium-web .
#
# The file lives here rather than beside the root `Dockerfile` because that one
# is the *Python* server, which is still what production runs until Fase 5. This
# file does not touch it, and until Fase 4 adds the compose service nothing
# consumes this image — so it is built and smoke-run by hand, not in CI.
#
# # The compose healthcheck has to change when this image is used
#
# `docker-compose/docker-compose.main.yml:29` runs
# `CMD-SHELL curl -f http://localhost:7080/ ...`. That cannot work here: the
# runtime stage is distroless, so there is no shell for `CMD-SHELL` to hand the
# string to and no `curl` to run even if there were. Switch it to
#
#     test: ["CMD", "/app/freedium-web", "healthcheck"]
#
# which is what `src/main.rs`'s `healthcheck` subcommand exists for, and which
# sends the same request the current line does (same `User-Agent`, same 80 s
# `--max-time`). Getting this wrong is not a cosmetic failure: a `CMD-SHELL`
# healthcheck in a shell-less image always fails, and `freedium_autoheal`
# (`AUTOHEAL_CONTAINER_LABEL=all`) would then restart a perfectly healthy
# container in a loop.

# --- the toolchain, and cargo-chef ------------------------------------------
#
# `rust:bookworm`, i.e. the latest stable on Debian bookworm, matching
# `rust-toolchain.toml`'s `channel = "stable"` — the file is copied in with the
# rest of the context, so a version pinned here and a channel pinned there would
# be two answers to one question. That file means rustup re-resolves "stable" on
# the first `cargo` invocation in each stage and downloads the components if the
# image's copy is older than the day's stable (it was: the build log shows a sync
# to 1.98.1). Correct, and the reason a build from a cold cache is not as fast as
# the layer count suggests.
#
# glibc rather than musl. `RUST_REWRITE_PLAN` §3.4 reaches the same conclusion
# for the edge image ("consider glibc, BoringSSL on Alpine is a nuisance"), and
# the same reasoning applies earlier: `ring` and the sqlx/reqwest tree are known
# quantities on glibc, and a static musl build here would be its own project.
#
# `cargo-chef` is what makes an edit to a `.rs` file re-run only the last stage:
# `prepare` reduces the workspace to its manifests, `cook` compiles every
# *dependency* against dummy sources, and the dependency layer is then keyed by
# `Cargo.lock` instead of by the source.
FROM rust:bookworm AS chef
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.78
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS cook
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# --- the binary --------------------------------------------------------------
FROM chef AS build
COPY . .
# The cooked dependencies, before the sources, so that this layer survives a
# source edit. `.dockerignore` excludes `target/`, so the `COPY . .` above does
# not clobber what the cook stage produced.
COPY --from=cook /src/target target
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --release --locked -p freedium-web

# --- the runtime -------------------------------------------------------------
#
# `cc-debian12` — the variant that carries glibc, which a normally-linked Rust
# binary needs. It is `nonroot` (uid 65532), and the server binds 7080, so no
# capability is needed for that either.
#
# There is no `COPY` of templates: `medium-render` compiles them into the binary
# via `minijinja-embed` (`build.rs`), which is what makes a single-file runtime
# stage possible — and why a template syntax error fails the build rather than
# the first request.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=build /src/target/release/freedium-web /app/freedium-web

# In production Caddy answers these paths itself with explicit `handle_path`
# blocks and never forwards them here, but the router's `ServeDir` fallback
# needs something to serve, and `Config::static_dir` documents a container
# setting `STATIC_DIR` to "the copy the image actually holds". This is that copy.
COPY caddy/static /app/static
ENV STATIC_DIR=/app/static

EXPOSE 7080

# `ENTRYPOINT` plus `CMD` rather than one string, so that
# `docker run image healthcheck` replaces the subcommand and keeps the binary —
# which is exactly the shape the compose healthcheck needs.
ENTRYPOINT ["/app/freedium-web"]
CMD ["server"]
