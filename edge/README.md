# `freedium-edge` — Fase 4's shadow edge

A Pingora service that sits in front of the primary, proxies every request to it
unchanged, and mirrors a sample of eligible requests onto a second, Rust instance
to compare the two answers. It writes one JSONL line per request; `difftest
shadow-report` reads them.

**It is not the production edge.** It has no deny-list, no static file serving,
no `pingora-cache`, no rate limiting, and no TLS. Those are Fase 7's jobs, and
putting them in now would mean Fase 4's evidence was collected by a component
doing six things the eventual edge will do differently. It proxies and it
compares. That is all.

## What it is for

`RUST_REWRITE_PLAN.md` §5 asks for real traffic to be mirrored onto the Rust
server with responses discarded, compared continuously, and gated on *seven
consecutive days with no semantic difference*. Fase 0–3 proved the Rust server
matches Python on **fixtures**. This is how that claim gets tested against the
posts people actually ask for, before Fase 5 moves any traffic.

## The three processes it assumes

```
client ──▶ freedium-edge :6755 ──▶ primary (Python) :7080
                    │
                    └── shadow (Rust, SHADOW_MODE=true) :7081
```

The client only ever sees the primary's bytes. The comparison happens after the
response is already on its way — see *Fail-open* below.

## Configuration

All read from the environment, never from a `.env` file, so the binary does not
behave differently depending on the directory it was started from. Compose
passes the values.

| Variable | Default | What it does |
|---|---|---|
| `EDGE_LISTEN` | `0.0.0.0:6755` | Where clients connect. |
| `EDGE_UPSTREAM` | `127.0.0.1:7080` | The primary — Python, in this phase. Deliberately **not** Caddy: this phase compares Rust against Python, not against Caddy's output. |
| `SHADOW_UPSTREAM` | `127.0.0.1:7081` | The Rust instance. A different port from `EDGE_UPSTREAM` on purpose — an edge pointed at itself compares a server with itself and always passes. |
| `SHADOW_ENABLED` | `false` | **The kill switch.** Off means a plain pass-through that writes no evidence. |
| `SHADOW_SAMPLE` | `1.0` | Fraction of *eligible* requests to mirror. Deterministic per path, so a request that differed can be requested again and will be sampled again. |
| `SHADOW_TIMEOUT_MS` | `5000` | How long the shadow request may take. Generous, because a timeout is recorded as `unreachable` and reads as a thin corpus rather than as a difference. |
| `SHADOW_MAX_BODY` | `2097152` | The most of a response the edge will buffer. Over it the request is *excluded*, not truncated. |
| `SHADOW_LOG` | `shadow.jsonl` | Appended to, never truncated, so a restarted edge continues a soak. |
| `SHADOW_DECLARATIONS` | *unset* | The file of accepted differences. Loaded by the edge, not only by the report — a declaration is what turns a difference into a pass, and that decision has to be made where the bodies are. |
| `EDGE_THREADS` | machine parallelism (min 2) | Pingora's default is one. The comparison parses two pages of HTML and shares these threads with live traffic. |

### The two defaults that matter

`SHADOW_ENABLED=false` is the rollback. §5's requirement is that putting Caddy
back in front is one line, and a kill switch that defaults to "on" cannot be
that.

`SHADOW_SAMPLE=1.0` looks aggressive and is the conservative choice. A rate that
defaults low makes a misconfigured soak look like a clean one: few comparisons,
no differences, a green gate. The failure mode of the opposite default is a gate
that cannot fail. The rate is a performance dial — Fase 4 ships it wide open so
the local proof compares everything, and §5 lowers it from a measurement of what
one extra `cache` read costs.

## Exclusions, and why each one is recorded

Every request gets a line, including the ones that were **not** compared, with a
reason token:

| Reason | Excludes |
|---|---|
| `shadow-disabled` | everything, when `SHADOW_ENABLED` is unset |
| `method` | anything but `GET` |
| `homepage` | `/` — Rust samples with `TABLESAMPLE SYSTEM`, Python with `ORDER BY RANDOM()`, and the two caches are independent by design, so two homepages legitimately differ |
| `miro`, `iframe` | `/@miro/*`, `/render_iframe/*` — both make their own outbound request through the proxy pool, so mirroring them would double WARP traffic for bytes that are not rendered output |
| `bypass-params` | `?no-redis`, `?no-db-cache` — both force a code path the caches exist to avoid, and both are admin-key gated |
| `sampled-out` | all but `SHADOW_SAMPLE` of the rest |
| `primary-encoded` | responses carrying a `Content-Encoding` — the edge holds the primary's *wire* bytes, and comparing gzipped output against decoded HTML would be an enormous difference that means nothing |
| `body-too-large` | responses over `SHADOW_MAX_BODY` |

This is the counterweight to the gate. A shadow that only logged what it
compared would be indistinguishable from one whose eligibility rules had quietly
grown to swallow the corpus, and seven days of nothing reads as success.

**Non-HTML is deliberately not excluded.** The comparator handles it: two
matching statuses and a non-HTML content type is `StatusOnly`.

## Fail-open, structurally

Everything expensive happens in a task spawned from Pingora's `logging` hook,
which runs after the response is finished. Nothing in the comparison can change,
delay, or fail the response it is shadowing — that is a property of where the
code sits, not a rule anyone has to remember. The one thing the edge adds to the
request path is `response_body_filter` accumulating bytes for eligible requests,
and that is what `SHADOW_SAMPLE` is a cap on.

The comparer runs in `tokio::task::spawn_blocking`: a full HTML parse per side is
CPU-bound, and running it on a Pingora worker would stall live traffic.

## Two things that will look wrong and are not

**No TLS provider.** TLS terminates outside the edge (§2.8 — Caddy does it), so
this process only ever speaks plain HTTP. Decision 7 in the plan said "rustls,
not BoringSSL"; Pingora 0.8 has **no rustls provider at all**, so the actual
choice is BoringSSL, OpenSSL, or *neither*, and neither is what a plain-HTTP edge
wants. This resolves SPIKE-3 more completely than rustls would have — no C++
toolchain, no OpenSSL headers, no version question. Verified, not assumed:
`cargo tree` in this workspace contains no `boring-sys`, no `openssl-sys`, no
`ring`, and no `aws-lc`. (It does contain `openssl-probe`, which is pure Rust,
links nothing, and exists only to find the system CA directory.)

**`#[async_trait]` on the impl.** Pingora's `ProxyHttp` is declared with
`#[cfg_attr(not(doc_async_trait), async_trait)]`, so its async methods are boxed
futures with bounds the compiler will not match against plain `async fn`s in an
impl. Omitting it produces `E0195: lifetime parameters or bounds on method ... do
not match the trait declaration`, which reads like a lifetime problem and is not
one.

## Its own workspace, and the cost

`edge/` is a separate cargo workspace with its own `Cargo.lock` (§2.6, §3.4), so
nothing it pulls in can reach the main workspace's dependency graph — the
`boring-sys`/`openssl-sys` question SPIKE-3 was about cannot even arise. It reads
`page-canonical` from the main workspace by path.

What that costs: one more `cargo build`/`cargo test`/`cargo clippy` invocation in
CI and one more lockfile to keep current. The root workspace's `exclude` lists
`edge` so a reader of the root manifest finds the intent where they are looking.

## Running it locally

The Python instance comes from compose (`--profile local`), the Rust instance
runs on the host (its `config.rs` reads `std::env`, not a file, so source `.env`
first), and the edge sits between them.

```sh
# 1. Postgres and Redis, plus Python on the host's 7080.
docker compose -f docker-compose/docker-compose.main.yml --profile local up -d

# 2. Seed the cache from the difftest fixtures, so the Rust instance has a corpus
#    and never has to reach Medium.
./target/release/difftest gen-shadow-seed --database-url "$DATABASE_URL"

# 3. The Rust instance. Note the port and the equal HOST_ADDRESS: both servers
#    must answer the same URLs or every article differs on the miro rewrite.
SHADOW_MODE=true \
HOST_ADDRESS=http://localhost:6752 \
DATABASE_URL=... REDIS_HOST=localhost \
  ./target/release/freedium-web

# 4. The edge, in front of both.
SHADOW_ENABLED=true \
EDGE_UPSTREAM=127.0.0.1:7080 \
SHADOW_UPSTREAM=127.0.0.1:7081 \
  ./target/release/freedium-edge

# 5. Drive it, then read the evidence.
curl -sS localhost:6755/0291df856c77 > /dev/null
./target/release/difftest shadow-report --log shadow.jsonl
```

`HOST_ADDRESS` must be **identical** on both instances. It is read at request
time and interpolated into every page, so a mismatch shows up as a difference on
every single article — a false alarm that would cost more time to diagnose than
the whole local run takes.

The run's log is a record of one run against one corpus, not a source file: the
host recipe's default (`edge/shadow.jsonl`) is in `.gitignore` for that reason,
and a soak's log belongs wherever its gate is read rather than in the tree.
`edge/shadow-declarations.local.json` is the opposite case and **is** committed —
compose mounts it, the edge refuses to start without it, and the local corpus is
the fixtures, so its declaration is reproducible from the repository. §5's soak
mounts its own file, which names real post ids and is evidence rather than source.

### The same run through compose, which is what was actually done

The recipe above puts the Rust instance on the host. The run that produced this
phase's evidence used the compose services instead (`freedium_web_rust` and
`freedium_edge`, both in the `local` profile), because that is the shape §5's
soak will take and a proof of a different shape proves less.

```sh
cd docker-compose
# 1. The stack. `freedium_web_mini` is the primary; 7080 is Python's on the host,
#    7081 is the Rust instance's (7080 inside), 6755 is the edge.
docker compose -f docker-compose.yml --profile local up -d \
    freedium_web_mini freedium_web_rust freedium_edge

# 2. Seed the corpus (the fixtures, as real `cache` rows), then drive it through
#    the edge — not through either server directly, or nothing is mirrored.
./target/release/difftest gen-shadow-seed --database-url "$DATABASE_URL"
while read -r p; do curl -sS -o /dev/null "http://localhost:6755$p"; done < paths.txt

# 3. Harvest the evidence and score it. `--declarations` is required here: the
#    edge decides with the same file, and a report that scored against a
#    different one would be reporting on a run that never happened. `../edge/`
#    is where `.gitignore` already excludes the log `shadow.jsonl` names.
docker compose -f docker-compose.yml cp freedium_edge:/data/shadow.jsonl ../edge/shadow.jsonl
./target/release/difftest shadow-report --log ../edge/shadow.jsonl \
    --declarations ../edge/shadow-declarations.local.json
```

The edge writes to a **named volume**, not a bind mount: it runs as uid 65532 in
a distroless image, and a host directory would be root-owned and unwritable. `cp`
out of the container is how you read it.

Step 2 needs one thing the repository deliberately does not have: a way to reach
`postgres_freedium` from the host. `docker-compose.db.yml` publishes no port for
it — it does not need one — so the seeder needs a one-off override (in a file
*outside* the repo, so the published port is not a permanent change to a stack
that does not want it):

```yaml
services:
  postgres_freedium:
    ports:
      - "15432:5432"   # not 5432: a port that collides fails at `up`, not at the seed
```

with `-f /tmp/shadow-override.yml` added to every `docker compose` line above and
`DATABASE_URL` pointed at `127.0.0.1:15432`. `gen-shadow-seed` is the only part of
the run that needs it; the two servers talk to Postgres over the compose network.

The stack also brings up `pgadmin4_freedium` — a base service in
`docker-compose.db.yml`, not gated on a profile — which binds 5433 on the host. On
the machine this was proven on, that port was already taken by another project, so
it stayed in `Created` while every step above worked: nothing in the proof goes
through pgAdmin.

### The render cache will lie to you

`freedium_web_rust` shares `redis_service` with Python and stores its **rendered
HTML** under `v2:post:<id>` with a five-hour TTL (`keys.rs`). Recreating the
container does not bypass that: a rebuilt shadow image goes on serving the
previous build's renders until the entries expire.

Found the hard way. A Tailwind class was patched in a template, the image was
rebuilt, the container recreated, the corpus replayed — and the report came back
**PASS**, because every answer had come out of Redis. A soak in that state
reports on a build that is no longer running, which is the worst possible failure
for evidence that is supposed to justify a cutover.

So, before any run whose point is "does the current binary agree":

```sh
docker exec redis_service sh -c \
  "redis-cli --scan --pattern 'v2:*' | xargs -r redis-cli del"
```

`v2:` and not `*`: the namespace (§2.4) is what makes this safe to do while
Python is running — Python's keys are the bare post ids and are not ours to
delete.

### Proving the harness can fail

A gate that has never failed is not evidence, so the check is part of the run
rather than a remark about it. Patch one Tailwind class in
`crates/medium-render/templates/post.html` (the Rust copy only), rebuild
`freedium_web_rust`, flush `v2:*`, replay, and score:

```text
FAIL — 22 request(s) with an undeclared difference, 0 consecutive clean day(s), 1 required
  /0375c53379ef
        canonical node #62: primary ... <div class="w-full px-4 text-xl leading-normal ..."
                              vs shadow ... <div class="w-full px-4 text-xl leading-tight ..."
```

Then revert the template, rebuild, flush, replay, and confirm the `PASS` comes
back with 22 identical and 1 declared. Both directions are required: a harness
that cannot fail and a harness that cannot pass look the same from one
observation.

Two notes on that check, both learned by running it:

- The template patch is **deliberately** in the Rust copy alone.
  `medium-render`'s `the_copies_have_not_drifted_from_the_originals` test pins the
  six templates to be byte-identical to `legacy/web/server/templates/`. Patching
  both copies would make the two sides *agree* — nothing for the shadow to catch
  — and patching one fails that test as well as this report. That is not a
  problem for the check (the point is that the shadow report is one of the layers
  and names the class), but it does mean the patch will not reach CI in this
  shape, so it is a one-off local procedure rather than a regression test.
- The flush in that procedure is not optional, and that is precisely how the
  stale-render trap above was found.

## What Fase 7 inherits

The workspace, the `ProxyHttp` skeleton, the separate-workspace decision, and the
no-TLS answer. Everything else — deny-list, static files, `pingora-cache`, rate
limiting, `header -Server`, certificates, and the removal of `caddy/` — is Fase
7's, and Caddy's config is deliberately untouched by this phase.
