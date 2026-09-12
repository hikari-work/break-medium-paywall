# syntax=docker/dockerfile:1

# The shadow edge's image — Fase 4 of RUST_REWRITE_PLAN.
#
# **The build context is the repository root**, not `edge/`. `freedium-edge`
# depends on `crates/page-canonical` by path (§3.4 — the edge and the render gate
# must not be able to disagree about what a difference is), and that crate lives
# in the main workspace, so a context of `edge/` would not contain it:
#
#     docker build -f edge/Dockerfile -t freedium-edge .
#
# # Why this is not cargo-chef's usual shape
#
# The web image (`crates/freedium-web/Dockerfile`) cooks the *root* workspace's
# recipe. This one cooks the **edge workspace's** recipe, from `edge/`, because
# that is where the `[workspace]` table is — and the recipe records
# `page-canonical` as `../../crates/page-canonical`, which resolves correctly
# only because the whole context is at the same depth it is in the repository.
# Moving `edge/` deeper would break the cook stage rather than the build, so it
# is worth knowing before reorganising anything.
#
# # No healthcheck, and that is a decision
#
# A pass-through proxy has no honest self-test. The check an operator would reach
# for first — "can it proxy a request?" — returns non-200 whenever the *primary*
# is down, so autoheal would restart the edge to fix a problem that is not the
# edge's. The check that is technically correct is a TCP connect to its own
# listen port, which is nearly information-free: a bind failure means the process
# exited, and `restart: always` already handles that.
#
# So the compose service declares no `healthcheck`. That is safe even with
# `freedium_autoheal` running `AUTOHEAL_CONTAINER_LABEL=all`
# (`docker-compose.utility.yml:7`): autoheal acts on containers in the
# `unhealthy` state, and a container with no healthcheck is never unhealthy.

# --- the toolchain, and cargo-chef ------------------------------------------
#
# `rust:bookworm` and `cargo-chef` at the same version as the web image, so that
# a change to one of these two Dockerfiles is a change a reader can compare
# line for line. See that file for why glibc and not musl.
FROM rust:bookworm AS chef

# `cmake`, for exactly one crate in this tree: `pingora-core` depends on
# `flate2`, which depends on `libz-ng-sys`, whose build script runs cmake to
# compile zlib-ng. No feature of `flate2` can be turned off from here to avoid
# it — the choice is pingora's — and nothing else in this build uses cmake, so
# the install lives in this Dockerfile rather than being copied into the web
# image, which shares this base and needs none of it.
#
# It is a **build-time** tool only. `libz-ng-sys` is configured with
# `BUILD_SHARED_LIBS=OFF`, so what reaches the runtime stage is a static
# archive and distroless `cc-debian12` is unaffected. `cc` and `make` are
# already in the base image, which is why only this one package is added.
#
# It goes in `chef` rather than in `cook`, so that `planner` and `build` get it
# too — the `build` stage reuses `cook`'s `target/`, but a source edit that
# invalidates part of that tree recompiles there, and it must not discover the
# missing tool then.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends cmake

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo install cargo-chef --locked --version 0.1.78
# The *edge* workspace, not the root one. Everything below runs from here.
WORKDIR /src/edge

FROM chef AS planner
COPY . /src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS cook
COPY --from=planner /src/edge/recipe.json recipe.json

# The three lines below exist because of a limitation worth naming: **`cargo chef
# prepare` records the manifests of the workspace it ran in, and nothing
# outside it.** Its recipe is written relative to that workspace root, and here
# the workspace root is `edge/` — so the recipe holds exactly two manifests
# (`edge/Cargo.toml` and `edge/freedium-edge/Cargo.toml`) and no entry for
# `page-canonical`, which lives in the *main* workspace. What it does keep is the
# dependency itself: the skeleton manifest still says
#
#     [dependencies.page-canonical]
#     path = "../../crates/page-canonical"
#
# so `cook` tries to load a manifest at a path nothing has created, and dies with
# `failed to read /src/crates/page-canonical/Cargo.toml`. The failure is in the
# cook stage, which is early enough that it reads like a Dockerfile problem
# rather than the recipe limitation it is.
#
# So reconstruct the two pieces that path needs:
#
# - **the main root manifest**, because `page-canonical` inherits from it —
#   `version.workspace = true`, `html5ever = { workspace = true }` — and Cargo
#   resolves that by walking *up* from the crate's own directory. Without it the
#   error changes to `error inheriting `edition` from workspace root manifest's
#   `workspace.package.edition``, which at least points at the right file. Its
#   `members = ["crates/*", "xtask/*"]` matches only `page-canonical` here, and a
#   glob that matches nothing is not an error.
# - **the crate's real manifest**, so that what gets cooked is its real
#   dependency set. `page-canonical` is where `html5ever` enters this build, and
#   pre-building that tree is most of what the cook stage is for.
# - **an empty `lib.rs`**, because Cargo will not load a manifest whose target
#   file does not exist. `cargo-chef` does exactly this for registry crates.
#
# The manifest is copied rather than the crate, and that is the point: this layer
# is then keyed by the *dependency set*, so editing `canonical.rs` — which is the
# file most likely to change, being the comparator both this edge and the render
# gate are judged by — does not recompile Pingora. The real sources arrive in the
# `build` stage, which reuses this stage's `target/`.
COPY --from=planner /src/Cargo.toml /src/Cargo.toml
COPY --from=planner /src/crates/page-canonical/Cargo.toml /src/crates/page-canonical/Cargo.toml
RUN mkdir -p /src/crates/page-canonical/src && : > /src/crates/page-canonical/src/lib.rs

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json

# --- the binary --------------------------------------------------------------
FROM chef AS build
COPY . /src
# The cooked dependencies, after the sources and not before: `.dockerignore`
# excludes `target/`, so the `COPY` above cannot clobber what the cook stage
# produced, and this layer is then the one that survives a source edit.
COPY --from=cook /src/edge/target target

# The one line the rest of this file exists to make work.
#
# Cargo decides whether a source file is fresh by **mtime**, and `COPY` preserves
# the mtimes from the build context — but the stub `lib.rs` the cook stage wrote
# carries the mtime of the cook *layer*, which is later than every file in a
# checkout made before it. So the real `crates/page-canonical/src/lib.rs`, three
# hours old, looks *older* than the artifact compiled from the empty stub, cargo
# calls it fresh, and the link fails on a crate that exported nothing:
#
#     error[E0432]: unresolved imports `page_canonical::Served`,
#     `page_canonical::ShadowRecord`, `page_canonical::Side`
#
# Which is a nasty shape of failure: the cook stage proves nothing about the
# sources it stubbed, and the error names the new API rather than the staleness,
# so it reads as "the crate does not export this" instead of "this crate was
# never compiled".
#
# Worse, it is cache-dependent rather than deterministic. In CI the checkout
# lands immediately before the build, so every source is newer than a cook layer
# that was itself cached from an earlier run — the same Dockerfile passes there
# and fails here. Touching the sources makes the ordering explicit instead of
# leaving it to whichever layer BuildKit happened to reuse.
#
# It costs seconds — `page-canonical` and `freedium-edge`, two small crates — and
# cannot reach the expensive part of the cook stage: every other dependency in
# this build comes from the registry, whose artifacts are keyed by content hash
# and are not what `touch` marks dirty.
RUN find /src -name '*.rs' -exec touch {} +

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo build --release --locked -p freedium-edge

# The shadow log's directory, made here because the runtime stage is distroless
# and has no `mkdir`. Empty on purpose — see the runtime stage for why it is not
# simply borrowed from somewhere that already has files in it.
RUN mkdir -p /data

# --- the runtime -------------------------------------------------------------
#
# `cc-debian12:nonroot`, the same base as the web image. No TLS provider is
# linked (see the crate README), so this stage needs no certificate store and no
# C++ runtime beyond what `cc` already carries.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=build /src/edge/target/release/freedium-edge /app/freedium-edge

# Where the shadow log goes. A **directory in the image, owned by the nonroot
# user**, rather than `SHADOW_LOG` pointing at `/` or `/tmp`:
#
# - `/` is not writable by uid 65532, and the image would start and then fail to
#   record anything — the one failure this phase cannot tolerate, because a soak
#   with no log reads as seven clean days.
# - `/tmp` is writable, and a soak's evidence would vanish with the container.
#
# Docker initialises a fresh named volume from the image's content at that path,
# ownership included, so `freedium_shadow_log:/data` in compose is writable on
# first run without any entrypoint chown. `SHADOW_LOG`'s own default is a
# relative path, which is right for a host run and would be a write into `/` here
# — so this sets it, and the compose service mounts the volume that makes it
# durable.
#
# Copied out of the build stage rather than out of the build context, and that is
# the point: the obvious one-liner here is `COPY data /data`, borrowing the
# repository's `data/` directory because it happens to exist. It would work, and
# it would put the site's favicon and logo into the volume this edge keeps its
# evidence in — a log directory that arrives with four assets in it and no
# explanation. The edge serves no static files in this phase (`include_dir!` is
# Fase 7), so the only thing that directory should ever hold is the log.
COPY --from=build --chown=65532:65532 /data /data
ENV SHADOW_LOG=/data/shadow.jsonl

EXPOSE 6755

# An `ENTRYPOINT` and no `CMD`, because there is no subcommand to select — this
# image runs one thing. Without either the container refuses to start
# (`service:freedium_edge: no command specified`), which is how this line came to
# exist: a distroless stage has neither a shell nor an inherited default.
#
# The web image's `ENTRYPOINT` + `CMD` pair exists so a compose healthcheck can
# replace the subcommand and keep the binary. There is no equivalent here: a
# pass-through proxy has no honest self-test, so this service declares no
# healthcheck at all (see the top of this file).
#
# The binary takes no arguments. Pingora's own CLI would fill `Opt` in from `-c
# conf.yaml -d`; `main.rs` passes `Opt::default()` and reads every knob from the
# environment (`config.rs`), so anything appended to this line is ignored rather
# than parsed — which is the honest shape for a container configured entirely by
# env vars.
ENTRYPOINT ["/app/freedium-edge"]
