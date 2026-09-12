# SPIKE-1 — TLS impersonation

RUST_REWRITE_PLAN §3.1. This is the one risk that can cancel the rewrite, so the
plan requires it be settled before any production Rust is written.

## The question

`legacy/medium-parser/medium_parser/api.py:75` fetches with `curl_cffi` and
`impersonate="chrome110"`. Without a browser-like TLS/HTTP2 fingerprint,
`medium.com/_/graphql` blocks the request. Rust has no first-party equivalent.

**Gate: the candidate client's success rate must be ≥99% of the `curl_cffi`
baseline, over the same requests, through the same WARP pool.**

## Status: measured on a direct egress — PASS; the pooled gate is still open

A Rust client clears the fingerprint. `xtask/spike-impersonate` (`wreq` 0.16.1 +
`wreq-util` 0.2.0, `Emulation::Chrome110`) and `curl_cffi` 0.16.3 were run
head-to-head, direct, on 2026-09-12, 50 requests each, over the same 15-post list:

| run | ok/total | rate | p50 | p95 |
|---|---|---|---|---|
| baseline (`curl_cffi`, before) | 46/50 | 92.00% | 695 ms | 900 ms |
| candidate (`wreq`) | 46/50 | 92.00% | 595 ms | 918 ms |
| baseline (`curl_cffi`, after) | 46/50 | 92.00% | — | — |

`parity = 1.0000`, gate `>= 0.99` → **PASS**.

Read the two caveats before quoting that number, because both are the kind that
make a parity figure mean less than it looks:

- **Every one of the 150 requests returned HTTP 200.** There was no 403 and no
  Cloudflare challenge on either side, so the 92% is not a blocked-vs-blocked
  floor. The 4 failures are the *same* post, `b4ee755ee6c5`, in all three runs —
  `{"data":{"post":null,...}}`, a deleted or unpublished post. It is a property
  of that post, not of either client.
- **This was a direct run, not the formal §3.1 gate.** The gate is specified
  "through the same WARP pool", and the `wgcf1..N` compose pool is not running
  here. What was measured is the host's own egress, which happens to be WARP
  already (`cdn-cgi/trace` → `warp=plus`, `loc=ID`) — so it is a WARP egress, but
  not *the pool's* exits. A pooled run tests the same TLS question through
  different addresses; it can still fail on proxy handling.

**Consequence for Phase 0:** the `PostSource` decision is *supported* but not
closed. §3.1's decision tree stops at option 1 — an impersonating client — and
option 2 (libcurl-impersonate via FFI) is not needed. To close the deliverable,
re-run both sides at `--n 500` with `--proxies` against a live pool.

## The gate never measured `reqwest`, and one day it passed anyway

Worth recording, because it was written down as an absolute and then contradicted.

The claim was "`reqwest` will not get past Medium's bot check" — it appears in
`crates/medium-client/src/http.rs` and in the Fase 3 module docs. **The gate above
does not test it.** Both arms of the A/B are impersonating clients (`curl_cffi`
and `wreq`); plain `reqwest` was never an arm, so its rejection was inherited
assumption, not a measurement.

On 2026-09-12, running the production server end to end (wiring check, see
`crates/freedium-web/src/state.rs`) from the same WARP egress the runs above used:

| client | headers | result |
|---|---|---|
| `reqwest` (production code) | full production set | **200**, real article, 6/6 cold posts |
| `curl` | same full production set | 403 Cloudflare block page |
| `curl` | minimal (UA + Content-Type) | 403 "Just a moment…" challenge |
| `wreq` (production code) | full production set | 200 |

So the fingerprint is load-bearing — `curl` is blocked with identical headers from
the identical address — and `reqwest`'s rustls fingerprint happens to pass from a
WARP IP today.

**This does not move the fetch to `reqwest`,** and the reasons are the ones that
made the gate the thing that decided it in the first place:

- Cloudflare's bot score is dominated by IP reputation. A WARP IP is not a
  datacenter IP, and §2.2's warned-about case — a direct fetch from a datacenter
  address — has not been measured with `reqwest`.
- The rules are not static. "rustls was not blocked on one afternoon from one
  egress" is not a gate; parity 1.0000 against production's own client over a
  defined corpus is.
- The failure mode is silent and total: every cache miss 502s.

The correct reading of the table is *the assumption was stated too strongly*, not
*the impersonation is unnecessary*.

## What the earlier "BLOCKED" note got wrong

The prerequisites table that stood here listed three blockers. Two were not real:

| Claimed prerequisite | Reality |
|---|---|
| `curl_cffi` not installed | True, and easy to fix — a wheel for this interpreter exists. Both sides now run from a venv. |
| The pool does not resolve | Refers to the **Docker** pool. The host already egresses through WARP natively, so a direct run *is* a WARP run. |
| `curl_cffi` gets a 403 from medium.com | The probe behind this tested **plain `curl`**. A plain `curl` 403 is the premise `curl_cffi` exists to defeat, not evidence about `curl_cffi`. |

The third one is the one worth remembering: it is a measurement of the wrong
program, and it sat in this file as a blocker for a phase.

A real bug was also found and fixed while re-checking the baseline:
`baseline_curl_cffi.py` re-declared the request headers in a literal dict, and had
drifted from `api.py:36-48` — it put both Apollo headers last instead of first.
Since the whole point is comparing two clients' request bytes, that would have
been measured as a difference between the clients. It now lifts the header names
*and their order* out of `api.py` with `ast`, exactly as it already did for the
query, and the order now matches (`--echo-headers` on both sides differs only by
`Connection`, which is hop-by-hop and dropped under HTTP/2).

## What is here

- `baseline_curl_cffi.py` — the baseline runner. Lifts the query and the header
  order from `api.py`; runs in a venv with `curl_cffi` installed.
- `src/main.rs` — the candidate runner. The impersonating client itself now lives
  in `crates/medium-client/src/wreq_transport.rs`: SPIKE-1's verdict was to adopt
  it, so it moved to where production could reach it, and this harness became an
  external consumer of it.
- `difftest spike-impersonate-report` — the verdict tool. Pure offline scoring,
  compiled and unit-tested. Takes both logs and applies the gate.
- 15 post IDs extracted from `tests/smokie_tests.py`, including the ones flagged
  "still have problems" — these are the corpus §3.3 requires, not a random sample.

## Running the spike

Both sides must be given **the same post-ID file**. Generate it from the
baseline's own parser rather than by hand, so the two runs cannot disagree about
which post sits at which sequence number:

```bash
python3 -c "
import importlib.util, pathlib
spec = importlib.util.spec_from_file_location('b', 'baseline_curl_cffi.py')
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
pathlib.Path('post-ids.txt').write_text('\n'.join(m.smoke_test_post_ids()) + '\n')
"
```

> **Do not put slug URLs in that file.** Both runners take the last path segment
> verbatim, so `https://medium.com/@x/a-post-27832c8f6644` becomes the id
> `a-post-27832c8f6644`, both sides fail on it, and the report reads as perfect
> parity. The candidate refuses such a file before sending anything; the baseline
> does not. `smoke_test_post_ids()` strips slugs, which is why the list is
> generated above rather than assembled by hand.

```bash
pip install curl_cffi

# 1. Baseline (the current production path), direct.
python3 baseline_curl_cffi.py \
    --n 50 --concurrency 8 --allow-no-proxy --impersonate chrome110 \
    --post-ids post-ids.txt --out baseline.jsonl

# 2. Candidate. Same --n, same --concurrency, same post-ID file.
#    Omit --proxies for a direct run; pass --proxies <file> for the pooled one,
#    and then also drop --allow-no-proxy from step 1.
cargo run --release -- --n 50 --concurrency 8 \
    --post-ids post-ids.txt --out candidate.jsonl

# 3. Verdict. Offline; runs anywhere.
cargo run --release -p difftest -- spike-impersonate-report \
    --baseline baseline.jsonl --candidate candidate.jsonl --show-proxies
```

Exit code 0 means the gate passed. `--threshold` defaults to `0.99`.

Run the two sides **A/B/A** in one session — baseline, candidate, baseline again
— so that temporal drift in Medium's own behaviour shows up as a difference
between the two baseline runs rather than as a difference between the clients.
`--impersonate` and `--emulation` must name the same profile on both sides, or the
comparison measures the profiles instead of the implementations.

Validate plumbing before committing to a 500-request run:

```bash
python3 baseline_curl_cffi.py --dry-run --n 20 --out plumbing.jsonl
cargo run -- --dry-run --n 20 --post-ids post-ids.txt --out plumbing-rs.jsonl
```

Dry-run rows carry `"ok": null` and `"dry_run": true`; the report tool **refuses
to score them** rather than dropping them, so a plumbing check can never be
mistaken for a measurement.

Each run also writes `<out>.meta.json` beside the records — profile, argv, the
post-ID list, and start/end times. Metadata never goes *inside* the JSONL: the
report tool rejects a whole file on its first unparseable line.

`--echo-headers` prints the header order each side intends to send, with no
network and no `--out`, for diffing the two.

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
a successful fetch. That rule is Python truthiness, not Rust's `is_some`:
`{"data":{"post":null}}` **and** `{"data":{"post":{}}}` are both failures. The
candidate reproduces it exactly and has unit tests pinning both edges, because a
client that scored them differently would move its own success rate without
anything on the wire differing.

The candidate adds three keys, which the report ignores (its `Attempt` is not
`deny_unknown_fields`):

| Extra field | Meaning |
|---|---|
| `seq` | Request index. The report does not check that the two files agree row for row, so this is the only way to confirm both sides requested the same posts in the same order. |
| `emulation` | The profile used, so a file cannot be mistaken for a run of another profile. |
| `validate_ok` | `medium_client::response::validate`'s verdict, recorded *beside* `ok` rather than instead of it. The two disagree at two edges (`{"error": null, ...}` and `{"data":{"post":{}}}`), and the run prints a note if they ever diverge. |

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

The candidate takes all of this from `medium_client::request` instead of
transcribing it: the endpoint, the query, and the header names, order and values.
Nothing about the request is re-declared on the Rust side, because a copied header
block is exactly the drift that shows up as a gate failure for the wrong reason.
`Connection` is the single exception and it is deliberate — see
`medium_client::request::headers`.

**The spike never sets `MEDIUM_AUTH_COOKIES`, and neither should a re-run.**
§2.7 warning 2: that is a subscriber account's session, and the
`MeteringInfoData` unlock quota it buys is bound to that account. All runs above
were anonymous.

## The candidate client

Written, in `src/`. Two things about it are worth knowing before reading the code:

**It is a `Transport`, and the spike still does not drive it through
`HttpPostSource`.** Implementing `medium_client::http::Transport` is what made
adoption a wiring change, and it has since happened: the client is now
`medium_client::wreq_transport::WreqTransport`, and the seam test lives in this
harness's own `#[cfg(test)]` module — deliberately out here, where it is a
cross-crate check rather than a tautology. But the spike itself still runs a plain
loop, because `HttpPostSource` retries: `RetryPolicy::DEFAULT` is two attempts, so
one record could mean two requests on the wire — inflating the candidate's rate
against a baseline that sends one, doubling the load on Medium, folding backoff
sleep into `elapsed_ms`, and dropping the status on `FetchError::NoPost`. Retry
belongs in production; it does not belong in a measurement.

**The profile name is not the fingerprint.** `wreq-util`'s `Chrome110` and
curl_cffi's `chrome110` are different constructions, and `wreq-util`'s is not even
Chrome 110's: its `v110` module takes `v100::build_emulation` for the TLS and
HTTP/2 settings, so only the *headers* are 110's (`emulate/profile/chrome.rs`).
curl_cffi's `chrome110` is a patch to curl built from its own capture. The two
matching by name guarantees nothing, which is exactly the risk §3.1 flagged — and
is why the gate, not a reading of the source, was the thing that decided it.

`wreq` replaces the `rquest` that §3.1 named: `rquest` 5.2.0 is yanked and its
crates.io metadata is broken. §3.4's warning still applies — `wreq` reaches
BoringSSL through `btls`/`btls-sys`, which are non-optional and have no rustls
alternative, so the C++ toolchain requirement is permanent for anything that
depends on this client.

(The crate names are `btls`/`btls-sys`, not the `boring2`/`tokio-boring2` this
paragraph and the root manifest used to name; the crates were renamed upstream,
and neither lockfile contains a `boring*` package at all.)

**Adopting this client moved that requirement into the root workspace.** The
exclusion below is no longer what keeps BoringSSL out — `crates/medium-client`
depends on it, so the root builds it either way. The harness stays excluded
because it is a measurement harness and not production code.

## Decision tree (§3.1, verbatim, with the outcome)

Stop at the first option that clears the gate:

1. **An impersonating Rust client** — the plan says `rquest`; that crate is
   yanked, and **`wreq` 0.16.1 is what was measured**. Gate: ≥99% parity over 500
   requests through the WARP pool. → **This option is the one that works.** The
   direct run above cleared the fingerprint; the pooled 500-request run is what
   remains to formally close it.
2. **libcurl-impersonate via FFI** — link the `.so` that `curl_cffi` already
   binds. Heavier build, identical fingerprint. → Not needed.
3. **Python sidecar fetcher** — keep ~80 LOC of Python exposing
   `POST /fetch/{post_id}` behind the `PostSource` trait. → Not needed.

If 1 and 2 fail, **still proceed with 3**. The plan is explicit that a TLS
fingerprint problem must not cancel the rewrite: the parser and renderer — the
source of every rendering bug — still move to Rust, and the residual risk stays
isolated in 80 lines.
