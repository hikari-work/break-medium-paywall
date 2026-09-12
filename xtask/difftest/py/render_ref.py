#!/usr/bin/env python3
"""Reference side of the render parity gate (RUST_REWRITE_PLAN §4).

Reads the case corpus emitted by `difftest gen-render-cases` and writes, for each
case, what the **real** legacy renderer produces for its content fragments plus
the title and subtitle it hands back.

"Real" is the point. `core.py`, `utils.py`, `markups.py` and `string_helper.py`
are imported straight out of `legacy/`, at the byte-identical revisions that run
in production, with only their outside world stubbed by `stubs.py` (see that
module for what is neutralised and why). Copying the renderer here would drift
from it within a week, and a parity gate against a copy proves nothing.

The call is the same one `core.py:759-768` makes, with the same argument order,
so the fixture payloads are read through exactly the code path a real request
would take:

    self._parse_and_render_content_html_post(
        post_data["data"]["post"]["content"],
        post_data["data"]["post"]["title"],
        post_data["data"]["post"]["previewContent"]["subtitle"],
        post_data["data"]["post"]["previewImage"]["id"],
        post_data["data"]["post"]["highlights"],
        post_data["data"]["post"]["tags"],
        post_data,
    )

Fragments are compared *unnormalised* here: this side writes the raw HTML, and
the Rust gate reduces both sides to the canonical form. Normalising in Python
would need a third HTML parser and would let the two sides' error recovery
diverge — `bs4` is stubbed out, so this process cannot parse HTML at all.

Standard library plus `jinja2`; no network access.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import stubs  # noqa: E402  (must be importable before the stubs are installed)

#: The template folder `core.py:60` reads `base.html` and `post.html` from. Fase 1
#: only renders content fragments, but the constructor loads them eagerly, so the
#: path has to be right.
_TEMPLATE_SUBDIR = ("web", "server", "templates")


def build_parser(host_address: str):
    """A `MediumParser` wired for offline fragment rendering.

    `cache` and `medium_api` are unused by `_parse_and_render_content_html_post`,
    and `timeout` is only read by the network paths, so all three are inert.
    """
    import os

    from medium_parser.core import MediumParser  # noqa: PLC0415

    template_folder = os.path.join(stubs._legacy_root(), *_TEMPLATE_SUBDIR)
    if not os.path.isdir(template_folder):
        sys.exit(f"FATAL: template folder not found: {template_folder}")

    return MediumParser(
        cache=None,
        medium_api=None,
        timeout=8,
        host_address=host_address,
        template_folder=template_folder,
    )


def render_case(parsers: dict, case: dict) -> dict:
    """Runs one fixture through the legacy renderer.

    Returns the fragments, the (possibly rewritten) title and subtitle, or an
    `error` when the legacy code raised. A fixture that raises is recorded rather
    than fatal — the point of the gate is to find out that it happens, and a
    crash mid-corpus would hide every case behind it.
    """
    host_address = case["host_address"]
    if host_address not in parsers:
        parsers[host_address] = build_parser(host_address)
    parser = parsers[host_address]

    post = case["post_data"]["data"]["post"]
    try:
        fragments, title, subtitle = parser._parse_and_render_content_html_post(  # noqa: SLF001
            post["content"],
            post["title"],
            post["previewContent"]["subtitle"],
            post["previewImage"]["id"],
            post["highlights"],
            post["tags"],
            case["post_data"],
        )
    except Exception as err:  # noqa: BLE001 — any raise is a result worth recording
        return {
            "index": case["index"],
            "name": case["name"],
            "error": f"{type(err).__name__}: {err}",
        }

    return {
        "index": case["index"],
        "name": case["name"],
        "fragments": fragments,
        "title": title,
        "subtitle": subtitle,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--cases", required=True, help="JSONL from `difftest gen-render-cases`"
    )
    parser.add_argument("--out", required=True, help="JSONL reference output")
    parser.add_argument(
        "--quiet", action="store_true", help="suppress the progress counter on stderr"
    )
    args = parser.parse_args()

    stubs.install()

    import medium_parser.core as core  # noqa: PLC0415

    print(
        f"reference: python {sys.version.split()[0]}, "
        f"legacy core at {core.__file__}",
        file=sys.stderr,
    )

    in_path = Path(args.cases)
    out_path = Path(args.out)
    parsers: dict = {}
    written = 0
    failures = 0

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

            row = render_case(parsers, case)
            if "error" in row:
                failures += 1
                print(f"  {row['name']}: {row['error']}", file=sys.stderr)

            fout.write(json.dumps(row, separators=(",", ":")) + "\n")
            written += 1

            if not args.quiet:
                print(f"  {written} cases...", file=sys.stderr)

    print(f"wrote {written} reference rows to {out_path}", file=sys.stderr)
    if failures:
        print(f"{failures} case(s) raised in the legacy renderer", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
