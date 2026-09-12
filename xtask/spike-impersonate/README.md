# SPIKE-1 — TLS impersonation

RUST_REWRITE_PLAN §3.1. This is the one risk that can cancel the rewrite, so the
plan requires it be settled before any production Rust is written.

## The question

`medium-parser/medium_parser/api.py:75` fetches with `curl_cffi` and
`impersonate="chrome110"`. Without a browser-like TLS/HTTP2 fingerprint,
`medium.com/_/graphql` blocks the request. Rust has no first-party equivalent.

**Gate: the candidate client's success rate must be ≥99% of the `curl_cffi`
baseline, over the same requests, through the same WARP pool.**

## Status: BLOCKED — not yet measured

The measurement cannot be taken in the current working tree. All three
prerequisites are missing:

| Prerequisite | State here |
|---|---|
| `curl_cffi` installed | absent (`ModuleNotFoundError`) |
| WARP pool `wgcf1..N` | `wgcf1` and `haproxy-pb` do not resolve |
| `curl_cffi` reached `medium.com` without a 403 | `curl https://medium.com` → **403** |

The pool is not incidental. §3.1 measures parity *through the WARP egress*, so a
run without it measures a different thing, and running 500 requests at Medium
from an unpoxied IP would both fail differently and abuse an address that is not
a designated egress. `baseline_curl_cffi.py` refuses to run without
`--proxies` for exactly this reason.

**Consequence for Phase 0:** the `PostSource` decision stays open. It is the one
Phase 0 deliverable that cannot be closed here.

## What *is* done

- `baseline_curl_cffi.py` — the baseline runner (runs in an environment with the
  pool; `--dry-run` validates plumbing anywhere).
- `difftest spike-impersonate-report` — the verdict tool. Pure offline scoring,
  compiled and unit-tested. Takes both logs and applies the gate.
- 15 post IDs extracted from `tests/smokie_tests.py`, including the ones flagged
  "still have problems" — these are the corpus §3.3 requires, not a random sample.

## Running the spike

On a host with the WARP pool reachable:

```bash
pip install curl_cffi

# 1. Baseline (the current production path).
python3 baseline_curl_cffi.py \
    --n 500 --proxies proxies.txt --out baseline.jsonl

# 2. Candidate (rquest). Must emit the same schema — see below.
#    Run it with the identical --n, --proxies and post-ID list.

# 3. Verdict. Offline; runs anywhere.
cargo run --release -p difftest -- spike-impersonate-report \
    --baseline baseline.jsonl --candidate candidate.jsonl --show-proxies
```

Exit code 0 means the gate passed. `--threshold` defaults to `0.99`.

Validate plumbing before committing to a 500-request run:

```bash
python3 baseline_curl_cffi.py --dry-run --n 20 --out plumbing.jsonl
```

Dry-run rows carry `"ok": null` and `"dry_run": true`; the report tool **refuses
to score them** rather than dropping them, so a plumbing check can never be
mistaken for a measurement.

## Record schema

One JSON object per attempt, one attempt per line. Both sides must emit this.

```json
{"post_id":"515dd5a43948","proxy":"socks5://wgcf1:1080","ok":true,
 "status":200,"error":null,"elapsed_ms":412,"dry_run":false}
```

| Field | Meaning |
|---|---|
| `post_id` | The post requested |
| `proxy` | Proxy used, or `null` for a direct request |
| `ok` | `true`/`false` = scored; `null` = never scored (rejected by the report) |
| `status` | HTTP status, or `null` if the request never completed |
| `error` | Failure detail, or `null` |
| `elapsed_ms` | Wall time, for the p50/p95 columns |
| `dry_run` | `true` marks a plumbing row; rejected by the report |

An `ok: true` row must mean *the post actually came back*: the baseline counts a
200 without `data.post` as a failure, because a 200 carrying an error body is not
a successful fetch.

## Request shape

The baseline lifts the `FullPostQuery` string out of `api.py` with `ast` rather
than re-declaring it, so there is exactly one source of truth and the baseline
cannot drift away from production. A candidate client must send the same body.

Headers are transcribed from `api.py:36-48`:

```
X-APOLLO-OPERATION-ID:   <random sha256 per request>
X-APOLLO-OPERATION-NAME: FullPostQuery
Accept:                  multipart/mixed; deferSpec=20220824, application/json, application/json
Accept-Language:         en-US
X-Obvious-CID:           android
X-Xsrf-Token:            1
X-Client-Date:           <unix ms>
User-Agent:              Mozilla/5.0 (iPhone; CPU iPhone OS 15_4_1 like Mac OS X)
                         AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0
                         Mobile/15E148 Safari/604.1 (compatible; YandexMobileBot/3.0;
Cache-Control:           public, max-age=-1
Content-Type:            application/json
Connection:              Keep-Alive
Cookie:                  <only when MEDIUM_AUTH_COOKIES is set — see §2.7 warning 2>
```

Body: `{"operationName":"FullPostQuery","variables":{"postId":<id>,"postMeteringOptions":{}},"query":<query>}`
to `POST https://medium.com/_/graphql`.

## The candidate client is not written yet — deliberately

`rquest` is intentionally **not** a dependency here, and no Rust candidate client
is committed. Two reasons:

1. §3.4 warns that `rquest` and `pingora` both reach BoringSSL through
   `boring-sys`; a mismatched version pair will not resolve in one workspace, and
   the BoringSSL build needs a C++ toolchain and is slow. The directory is
   `exclude`d from the root workspace for the same reason the plan gives the edge
   its own workspace.
2. Pinning rquest's impersonation API blind would mean committing code that has
   never compiled. `rquest` has moved that API across major versions
   (`Impersonate::Chrome110` on a client builder in some releases, `rquest-util`
   in others), so the exact incantation must be taken from the version actually
   pinned.

The shape of the client is a small adapter behind the `PostSource` trait (§2.6):
build the request above, send it through a client configured with a
Chrome-like impersonation profile and the SOCKS5 proxy, return the parsed JSON
or an error. Budget ~60 lines plus the record emitter.

## Decision tree (§3.1, verbatim)

Stop at the first option that clears the gate:

1. **`rquest`** — BoringSSL, chrome/safari impersonation profiles. Gate: ≥99%
   parity over 500 requests through the WARP pool.
2. **libcurl-impersonate via FFI** — link the `.so` that `curl_cffi` already
   binds. Heavier build, identical fingerprint.
3. **Python sidecar fetcher** — keep ~80 LOC of Python exposing
   `POST /fetch/{post_id}` behind the `PostSource` trait.

If 1 and 2 fail, **still proceed with 3**. The plan is explicit that a TLS
fingerprint problem must not cancel the rewrite: the parser and renderer — the
source of every rendering bug — still move to Rust, and the residual risk stays
isolated in 80 lines.
