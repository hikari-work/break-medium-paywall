#!/usr/bin/env python3
"""Reference side of the difflib parity gate (RUST_REWRITE_PLAN §3.2 / SPIKE-2).

Reads the case corpus emitted by `difftest gen-cases` and writes, for each pair,
the CPython `difflib.SequenceMatcher(None, a, b).ratio()` result and the boolean
decision that `medium_parser/core.py` actually makes from it.

The computation is deliberately a transcription of the call site, not a
paraphrase of it — `medium-parser/medium_parser/utils.py:110`:

    def getting_percontage_of_match(string, matched_string) -> float:
        if string is None or matched_string is None:
            return 0.0
        return difflib.SequenceMatcher(None, string, matched_string).ratio() * 100

...compared with `> 80` at `core.py:261` and `core.py:276`.

Floats are emitted as raw IEEE-754 bit patterns so the Rust side can compare
exactly. Decimal text would round away the last-bit differences that flip
decisions on the boundary.

Standard library only; no network access.
"""

from __future__ import annotations

import argparse
import difflib
import inspect
import json
import struct
import sys
from pathlib import Path


def bits(value: float) -> int:
    """IEEE-754 double as an unsigned 64-bit integer."""
    return struct.unpack("<Q", struct.pack("<d", value))[0]


def check_implementation() -> None:
    """Refuse to run against anything but CPython's pure-Python SequenceMatcher.

    `difflib` would transparently prefer a C accelerator if one existed, and its
    tie-breaking could differ from the reference implementation this port was
    written against. Better to fail loudly than to certify parity against the
    wrong baseline.
    """
    source = ""
    try:
        source = inspect.getsource(difflib.SequenceMatcher)
    except (OSError, TypeError):
        pass

    if "__chain_b" not in source:
        sys.exit(
            "FATAL: difflib.SequenceMatcher is not the pure-Python implementation "
            f"this harness was written against (source lookup gave {len(source)} chars). "
            "The parity baseline must be re-established before trusting any result."
        )

    print(
        f"reference: python {sys.version.split()[0]}, "
        f"difflib at {inspect.getfile(difflib)}",
        file=sys.stderr,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", required=True, help="JSONL from `difftest gen-cases`")
    parser.add_argument("--out", required=True, help="JSONL reference output")
    parser.add_argument(
        "--quiet", action="store_true", help="suppress the progress counter on stderr"
    )
    args = parser.parse_args()

    check_implementation()

    in_path = Path(args.cases)
    out_path = Path(args.out)

    written = 0
    with in_path.open("r", encoding="utf-8") as fin, out_path.open(
        "w", encoding="utf-8"
    ) as fout:
        for lineno, line in enumerate(fin, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                case = json.loads(line)
            except json.JSONDecodeError as err:
                sys.exit(f"FATAL: {in_path}:{lineno}: {err}")

            # Mirrors the call site exactly. `SequenceMatcher(None, a, b)` —
            # argument order matters because autojunk is applied to `b`.
            a = case["a"]
            b = case["b"]
            ratio = difflib.SequenceMatcher(None, a, b).ratio()
            percentage = ratio * 100
            decision = percentage > 80

            fout.write(
                json.dumps(
                    {
                        "index": case["index"],
                        "ratio_bits": bits(ratio),
                        "pct_bits": bits(percentage),
                        "decision": decision,
                    },
                    separators=(",", ":"),
                )
                + "\n"
            )
            written += 1

            if not args.quiet and written % 2000 == 0:
                print(f"  {written} cases...", file=sys.stderr)

    print(f"wrote {written} reference rows to {out_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
