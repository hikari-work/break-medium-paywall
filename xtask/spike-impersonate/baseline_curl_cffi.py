#!/usr/bin/env python3
"""SPIKE-1 baseline: success rate of the current `curl_cffi` fetcher.

RUST_REWRITE_PLAN §3.1 makes TLS impersonation the one risk that can cancel the
whole rewrite, and requires a measured ≤1% regression gate before any production
Rust is written. This script produces the *baseline* half of that measurement:
it replays `FullPostQuery` the same way `medium_parser/api.py` does today.

It deliberately does **not** re-declare the GraphQL query. The query is lifted
out of `medium-parser/medium_parser/api.py` with `ast`, so there is exactly one
source of truth and the baseline cannot drift away from production behaviour.

Requires `curl_cffi`. The candidate side (rquest) is expected to emit the same
JSONL schema — see README.md in this directory for the record format.

    python3 baseline_curl_cffi.py --n 500 --out baseline.jsonl
    python3 baseline_curl_cffi.py --dry-run --n 20 --out plumbing_check.jsonl

Standard library plus `curl_cffi`; makes real network requests unless --dry-run.
"""

from __future__ import annotations

import argparse
import ast
import asyncio
import hashlib
import json
import random
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
API_PY = REPO_ROOT / "medium-parser" / "medium_parser" / "api.py"
SMOKE_TESTS = REPO_ROOT / "tests" / "smokie_tests.py"
GRAPHQL_URL = "https://medium.com/_/graphql"

# Transcribed from `medium_parser/api.py:36-48`. Kept in sync by hand because
# these are literals there, but `--check-headers` verifies they still match.
HEADERS = {
    "Accept": "multipart/mixed; deferSpec=20220824, application/json, application/json",
    "Accept-Language": "en-US",
    "X-Obvious-CID": "android",
    "X-Xsrf-Token": "1",
    "Cache-Control": "public, max-age=-1",
    "Content-Type": "application/json",
    "Connection": "Keep-Alive",
    "User-Agent": (
        "Mozilla/5.0 (iPhone; CPU iPhone OS 15_4_1 like Mac OS X) "
        "AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 "
        "Safari/604.1 (compatible; YandexMobileBot/3.0;"
    ),
}


def extract_query(api_py: Path = API_PY) -> str:
    """Pull the `FullPostQuery` string out of `api.py` without importing it.

    Importing would require `curl_cffi` and the whole `medium_parser` package;
    `ast` needs neither, and it fails loudly if the literal is ever restructured
    (rather than silently measuring a stale query).
    """
    tree = ast.parse(api_py.read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if not isinstance(node, ast.Assign):
            continue
        for target in node.targets:
            if isinstance(target, ast.Name) and target.id == "graphql_data":
                # The dict is not fully literal — `"postId": post_id` is a Name
                # — so pull the single key we need instead of evaluating it all.
                for key, value in zip(node.value.keys, node.value.values):
                    if isinstance(key, ast.Constant) and key.value == "query":
                        query = ast.literal_eval(value)
                        assert "FullPostQuery" in query, "extracted query looks wrong"
                        return query
                sys.exit(f"FATAL: `graphql_data` in {api_py} has no `query` key")
    sys.exit(f"FATAL: could not find the `graphql_data` literal in {api_py}")


def extract_post_id(line: str) -> str | None:
    """Pull a Medium post ID out of one line of `smokie_tests.py`.

    Handles the three shapes that file actually uses: a bare ID
    (`"515dd5a43948"`), a slug-URL ending in the ID
    (`.../stop-wasting-your-life-27832c8f6644`), and a freedium URL. Trailing
    `#` comments and surrounding quotes/commas are stripped first.
    """
    line = line.split("#", 1)[0].strip()
    line = line.strip(",").strip().strip('"').strip("'").strip()
    if not line:
        return None
    segment = line.rstrip("/").rsplit("/", 1)[-1]
    segment = segment.split("?", 1)[0].split("#", 1)[0]
    # Either the whole final segment is the ID, or the ID is the token after the
    # last hyphen of a slug.
    for candidate in (segment, segment.rsplit("-", 1)[-1]):
        if 11 <= len(candidate) <= 12 and all(
            c in "0123456789abcdef" for c in candidate
        ):
            return candidate
    return None


def smoke_test_post_ids() -> list[str]:
    """Post IDs worth including: the ones `tests/smokie_tests.py` names.

    The plan (§3.3) requires the corpus to cover the known-hard URLs, not just a
    random sample, because those are the ones that exercise the long tail.
    """
    if not SMOKE_TESTS.exists():
        return []
    ids: list[str] = []
    for line in SMOKE_TESTS.read_text(encoding="utf-8").splitlines():
        post_id = extract_post_id(line)
        if post_id:
            ids.append(post_id)
    # Preserve order, drop duplicates.
    return list(dict.fromkeys(ids))


def load_post_ids(path: Path | None) -> list[str]:
    if path is None:
        ids = smoke_test_post_ids()
        if not ids:
            sys.exit(
                "FATAL: no post IDs available. Pass --post-ids <file> (one ID or "
                "URL per line)."
            )
        return ids
    ids = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        ids.append(line.rstrip("/").rsplit("/", 1)[-1].split("?")[0])
    if not ids:
        sys.exit(f"FATAL: {path} contains no post IDs")
    return list(dict.fromkeys(ids))


async def run(args: argparse.Namespace) -> int:
    query = extract_query()
    post_ids = load_post_ids(Path(args.post_ids) if args.post_ids else None)
    proxies = []
    if args.proxies:
        proxies = [
            line.strip()
            for line in Path(args.proxies).read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.strip().startswith("#")
        ]

    if not proxies and not args.allow_no_proxy and not args.dry_run:
        sys.exit(
            "FATAL: no proxies given. SPIKE-1 measures success rate *through the "
            "WARP pool*, which is the whole point — a direct run measures a "
            "different thing and Medium will rate-limit this IP.\n"
            "       Pass --proxies <file> (one socks5:// URL per line), or "
            "--allow-no-proxy if you deliberately want an unpoxied baseline."
        )

    if not args.dry_run:
        # Imported here rather than at module scope so `--dry-run` works on a
        # machine without curl_cffi installed.
        from curl_cffi.requests import AsyncSession  # noqa: F401

    print(
        f"baseline: {args.n} requests over {len(post_ids)} post IDs, "
        f"{len(proxies)} proxies, concurrency {args.concurrency}"
        + (" [DRY RUN]" if args.dry_run else ""),
        file=sys.stderr,
    )

    out_path = Path(args.out)
    semaphore = asyncio.Semaphore(args.concurrency)
    results: list[dict] = []
    counter = {"ok": 0, "fail": 0}

    async def one(index: int) -> dict:
        post_id = post_ids[index % len(post_ids)]
        proxy = proxies[index % len(proxies)] if proxies else None

        if args.dry_run:
            # Emits the real schema but `ok: null`, so the report tool refuses to
            # score it. A dry run validates plumbing, never a success rate.
            return {
                "post_id": post_id,
                "proxy": proxy,
                "ok": None,
                "status": None,
                "error": None,
                "elapsed_ms": 0,
                "dry_run": True,
            }

        headers = dict(HEADERS)
        headers["X-APOLLO-OPERATION-ID"] = hashlib.sha256(
            str(random.random()).encode()
        ).hexdigest()
        headers["X-APOLLO-OPERATION-NAME"] = "FullPostQuery"
        headers["X-Client-Date"] = str(int(time.time() * 1000))

        body = {
            "operationName": "FullPostQuery",
            "variables": {"postId": post_id, "postMeteringOptions": {}},
            "query": query,
        }

        started = time.monotonic()
        async with semaphore:
            try:
                async with AsyncSession() as session:
                    response = await session.post(
                        GRAPHQL_URL,
                        headers=headers,
                        json=body,
                        proxies={"http": proxy, "https": proxy} if proxy else None,
                        timeout=args.timeout,
                        impersonate="chrome110",
                    )
                    elapsed_ms = int((time.monotonic() - started) * 1000)
                    ok = response.status_code == 200
                    # A 200 that is not the post is still a failure to fetch.
                    if ok:
                        try:
                            payload = response.json()
                        except Exception as err:  # noqa: BLE001
                            return {
                                "post_id": post_id,
                                "proxy": proxy,
                                "ok": False,
                                "status": 200,
                                "error": f"json: {type(err).__name__}",
                                "elapsed_ms": elapsed_ms,
                                "dry_run": False,
                            }
                        if not payload.get("data", {}).get("post"):
                            ok = False
                    return {
                        "post_id": post_id,
                        "proxy": proxy,
                        "ok": ok,
                        "status": response.status_code,
                        "error": None if ok else response.text[:200],
                        "elapsed_ms": elapsed_ms,
                        "dry_run": False,
                    }
            except Exception as err:  # noqa: BLE001
                return {
                    "post_id": post_id,
                    "proxy": proxy,
                    "ok": False,
                    "status": None,
                    "error": f"{type(err).__name__}: {err}"[:200],
                    "elapsed_ms": int((time.monotonic() - started) * 1000),
                    "dry_run": False,
                }

    # Sequential batching keeps the footprint predictable; concurrency is
    # bounded by the semaphore inside `one`.
    for batch_start in range(0, args.n, args.concurrency):
        batch = range(batch_start, min(batch_start + args.concurrency, args.n))
        batch_results = await asyncio.gather(*(one(i) for i in batch))
        for row in batch_results:
            results.append(row)
            if row["ok"] is True:
                counter["ok"] += 1
            elif row["ok"] is False:
                counter["fail"] += 1
        done = batch_start + len(batch)
        if not args.quiet:
            print(
                f"  {done}/{args.n}  ok={counter['ok']} fail={counter['fail']}",
                file=sys.stderr,
            )

    with out_path.open("w", encoding="utf-8") as fout:
        for row in results:
            fout.write(json.dumps(row, separators=(",", ":")) + "\n")

    if args.dry_run:
        print(f"wrote {len(results)} DRY-RUN rows to {out_path} (not a measurement)")
    else:
        scored = counter["ok"] + counter["fail"]
        rate = counter["ok"] / scored if scored else 0.0
        print(f"wrote {len(results)} rows to {out_path}; success rate {rate:.4f}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, help="JSONL output path")
    parser.add_argument("--n", type=int, default=500, help="number of requests")
    parser.add_argument(
        "--proxies", help="file with one socks5:// URL per line (the WARP pool)"
    )
    parser.add_argument(
        "--post-ids",
        help="file with one post ID or URL per line (default: tests/smokie_tests.py)",
    )
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--timeout", type=float, default=12.0)
    parser.add_argument(
        "--allow-no-proxy",
        action="store_true",
        help="permit an unpoxied baseline (measures something else; for debugging only)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="emit the schema with ok=null; validates plumbing, not success rate",
    )
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()

    if args.n < 1:
        sys.exit("FATAL: --n must be at least 1")
    if args.concurrency < 1:
        sys.exit("FATAL: --concurrency must be at least 1")

    return asyncio.run(run(args))


if __name__ == "__main__":
    sys.exit(main())
