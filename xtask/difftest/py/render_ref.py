#!/usr/bin/env python3
"""Reference side of the render parity gate (RUST_REWRITE_PLAN §4).

Reads the case corpus emitted by `difftest gen-render-cases` and writes, for each
case, what the **real** legacy renderer produces: the content fragments and the
title/subtitle it hands back, plus — from Fase 3 — the whole `post.html` page
with the page title, description and url `_render_as_html` builds around it.

"Real" is the point. `core.py`, `utils.py`, `markups.py` and `string_helper.py`
are imported straight out of `legacy/`, at the byte-identical revisions that run
in production, with only their outside world stubbed by `stubs.py` (see that
module for what is neutralised and why). Copying the renderer here would drift
from it within a week, and a parity gate against a copy proves nothing.

# Two entry points, and why both

`_parse_and_render_content_html_post` is called first, with exactly the argument
order `core.py:759-768` uses, so the fixture payloads are read through the code
path a real request takes and a fragment-level mismatch still says *which block*
diverged.

Then `_render_as_html` renders the body around it, and `handlers/post.py:91-98`
wraps that in `base.html`. That is the Fase 3 gate: the page is the unit that
reaches a reader, and it is the only place `generate_metadata` — the
description, the `Free:` flag, the UTC dates, the double-escaped description —
is exercised at all. The three overlap by design: the page embeds the fragments,
so a fragment bug shows up in all of them, and the finer comparison is what
localises it.

**`_render_as_html` is not the whole page**, which is worth stating because its
`HtmlResult` looks like one. Its `data` is the rendered *body template*; the
served document is `base.html` with that string as `body_template`. Comparing
the body alone would leave the shell — the `{{ title }}`/`{{ description }}`
interpolation into `<title>` and `<meta>`, the `{{ host_address }}` base for
`/@miro/`, the `{% if creator %}` and `{% if enable_ads_header %}` branches —
untested, and the shell is what production caches and serves.

The one production step that is *not* reproduced is `handlers/post.py:99-100`'s
`parse`/`serialize` (the html5lib round-trip). Fase 3 drops it deliberately: the
Rust side emits well-formed HTML directly, and both sides of the gate are
reduced by the same canonical form, which is what makes the round-trip
unobservable. Running it here would only add a second HTML parser whose error
recovery could quietly rewrite what is being compared.

`_render_as_html` is `async`, so it runs under `asyncio.run`. Nothing in it
awaits anything that can block — `generate_metadata` is a coroutine that awaits
nothing, and `asyncify` (stubbed in `stubs.py`) wraps the synchronous renderer —
so a single-threaded loop with no network is enough.

Fragments are compared *unnormalised* here: this side writes the raw HTML, and
the Rust gate reduces both sides to the canonical form. Normalising in Python
would need a third HTML parser and would let the two sides' error recovery
diverge — `bs4` is stubbed out, so this process cannot parse HTML at all.

Standard library plus `jinja2`; no network access.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
from pathlib import Path

# --------------------------------------------------------------------------
# The timezone, pinned before anything else can read it
# --------------------------------------------------------------------------
#
# `convert_datetime_to_human_readable` (`time.py:13`) is
# `datetime.fromtimestamp(unix_time / 1000)`, which is **local** time. The dates
# it produces reach the page (`{{ firstPublishedAt }}`, `{{ updatedAt }}`), so
# which day — and near midnight, which month — the comparison sees depends on
# the `TZ` of whoever runs the gate.
#
# Production is UTC: `python:3.12.3` sets no `TZ`, and the Rust side formats in
# UTC deliberately (see `medium_doc::metadata`). So the reference is pinned to
# UTC here, in the script, rather than in a shell prefix or a Makefile — a gate
# whose answer changes with the operator's laptop is not a gate, and this is the
# one place that can be enforced for every caller.
os.environ["TZ"] = "UTC"
if hasattr(time, "tzset"):
    # Absent on Windows, where the local zone cannot be changed this way. The
    # check below then decides.
    time.tzset()

sys.path.insert(0, str(Path(__file__).resolve().parent))

import stubs  # noqa: E402  (must be importable before the stubs are installed)

#: The template folder `core.py:60` reads `base.html` and `post.html` from.
_TEMPLATE_SUBDIR = ("web", "server", "templates")

#: Two timestamps just inside either end of 2023-08-14 UTC. Their calendar date
#: is the same in UTC and different in any zone more than half an hour off it —
#: west of UTC the first rolls back to the 13th, east of UTC the second rolls
#: forward to the 15th. One probe would only catch one direction.
#:
#: In **milliseconds**, because `convert_datetime_to_human_readable` divides by
#: 1000 before handing the value to `fromtimestamp` (`time.py:13`). Passing
#: seconds here does not fail quietly: it lands in January 1970 and the check
#: reports a timezone problem that is really a units problem. It did, the first
#: time this was run.
_TZ_PROBES_MS = ((1_691_973_000_000, "August 13, 2023"), (1_692_055_800_000, "August 15, 2023"))
#: What both probes must render as when the process really is in UTC.
_TZ_PROBE_UTC = "August 14, 2023"


def assert_utc() -> None:
    """Fails loudly if the process is not in UTC.

    `time.tzset()` is a POSIX call and `TZ` is ignored by some embedded Python
    builds, so setting it is a request rather than a guarantee. Checking the
    result is what makes the pinning real: the alternative is a gate that
    silently compares different dates on different machines.
    """
    from medium_parser.time import convert_datetime_to_human_readable  # noqa: PLC0415

    for unix_ms, wrong_answer in _TZ_PROBES_MS:
        actual = convert_datetime_to_human_readable(unix_ms)
        if actual != _TZ_PROBE_UTC:
            sys.exit(
                f"FATAL: this process is not in UTC: {unix_ms} formats as "
                f"{actual!r}, expected {_TZ_PROBE_UTC!r} (a zone that far from "
                f"UTC would say {wrong_answer!r}). The reference formats the "
                f"publish dates with `datetime.fromtimestamp`, which is local "
                f"time, and the Rust side formats in UTC — running the gate in "
                f"another zone makes every page date a false mismatch."
            )



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


def render_document(parser, case: dict, page) -> str:
    """`base.html` around the body, exactly as `handlers/post.py:91-98` does it.

    The context is that function's `base_context`, key for key. Two keys need a
    word:

    - `host_address` comes from the *fixture*, not from `config.HOST_ADDRESS`.
      The gate feeds the Rust side `case["host_address"]`, so reading the
      deployed value here would compare two different inputs and make every
      page differ on the one string that is supposed to be free to vary.
    - `enable_ads_header` is the fixture's flag, defaulting to `False`, which is
      `config.ENABLE_ADS_BANNER`'s default (`config.py:29`). It is read from the
      fixture rather than from `config` because importing `web.server.config`
      would evaluate `config("ADMIN_SECRET_KEY")`, which has no default and
      raises without the environment variable set — a gate that only runs on a
      developer machine with production's env loaded is not a gate.

    `template_env` in `services/jinja.py:6-8` is `Environment(loader=
    FileSystemLoader(...))` with Jinja2's defaults, and so is `core.py:59-61`'s
    `jinja_template`; asking the parser for the template keeps the whole
    reference on one loader and one definition of the environment.
    """
    base_context = {
        "host_address": case["host_address"],
        "enable_ads_header": case.get("enable_ads_header", False),
        "body_template": page.data,
        "title": page.title,
        "description": page.description,
    }
    return parser.jinja_template.get_template("base.html").render(base_context)


def render_document_probe(parser, case: dict, page) -> str:
    """`base.html` on its own, for the byte-for-byte comparison.

    [`render_document`] wraps the body, so a difference inside the body hides one
    in the shell around it, and the two sides are reduced by a canonical form
    that deliberately drops whitespace-only text nodes — which is precisely
    where two template engines' defaults part company. Jinja2 strips a template
    source's trailing newline; minijinja keeps it unless told otherwise. No
    browser can see the difference and no canonical comparison can either, which
    is why the plan asks for this check to be bytes.

    Rendering the shell with a fixed body is what isolates it, and the body is
    the **page title**. That choice is not arbitrary: the title is the one piece
    of context this gate already requires both sides to produce byte for byte
    (`page_title` is compared exactly), so the probe cannot fail because its
    *input* differed — which is what an earlier version did, feeding it the
    joined fragments and thereby inheriting `markups-nested`'s splice-versus-nest
    difference, which the canonical form exists to permit. The honest reading of
    the failure was that the probe was comparing two things at once.

    It still exercises the `{{ body_template }}` wiring: a context key that
    reached one template engine and not the other renders "" on one side. And it
    still catches a live autoescape, because the title of the `page-quotes`
    fixture carries an `&`.
    """
    base_context = {
        "host_address": case["host_address"],
        "enable_ads_header": case.get("enable_ads_header", False),
        "body_template": page.title,
        "title": page.title,
        "description": page.description,
    }
    return parser.jinja_template.get_template("base.html").render(base_context)


def render_case(parsers: dict, case: dict) -> dict:
    """Runs one fixture through the legacy renderer.

    Returns the fragments, the (possibly rewritten) title and subtitle, the
    rendered page with its title/description/url and the bare-shell probe, or an
    `error` when the legacy code raised. A fixture that raises is recorded rather
    than fatal — the point of the gate is to find out that it happens, and a
    crash mid-corpus would hide every case behind it.

    The stages are attempted *independently* so that a failure in one still
    reports the others' results. `_render_as_html` calls
    `_parse_and_render_content_html_post` itself, so a fixture that breaks the
    content renderer breaks both — but a fixture that only breaks
    `generate_metadata` (a missing key, a timestamp that will not format) is a
    page-only failure, and collapsing the two would hide which it was.
    """
    host_address = case["host_address"]
    if host_address not in parsers:
        parsers[host_address] = build_parser(host_address)
    parser = parsers[host_address]

    post = case["post_data"]["data"]["post"]
    row: dict = {"index": case["index"], "name": case["name"]}
    errors: list[str] = []
    fragments: list[str] = []

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
        errors.append(f"content: {type(err).__name__}: {err}")
    else:
        row["fragments"] = fragments
        row["title"] = title
        row["subtitle"] = subtitle

    post_id = case.get("post_id", "0291df856c77")
    try:
        page = asyncio.run(parser._render_as_html(case["post_data"], post_id))  # noqa: SLF001
    except Exception as err:  # noqa: BLE001
        errors.append(f"page: {type(err).__name__}: {err}")
    else:
        # `HtmlResult(title, description, url, data)`
        # (`models/html_result.py`) — note the first field is the *page* title,
        # not the article title the content renderer returns, so the two `title`s
        # in this row are different things. `data` is the body template, which
        # `render_document` wraps; see this module's docstring.
        row["page_title"] = page.title
        row["description"] = page.description
        row["url"] = page.url
        try:
            row["page"] = render_document(parser, case, page)
            row["base_bare"] = render_document_probe(parser, case, page)
        except Exception as err:  # noqa: BLE001
            errors.append(f"base: {type(err).__name__}: {err}")

    if errors:
        row["error"] = "; ".join(errors)
    return row


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
    # After `stubs.install()`, which is what puts the legacy package on the path
    # that `assert_utc` imports `medium_parser.time` from.
    assert_utc()

    import medium_parser.core as core  # noqa: PLC0415

    print(
        f"reference: python {sys.version.split()[0]}, "
        f"legacy core at {core.__file__}, TZ={os.environ.get('TZ')}",
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
        print(
            f"{failures} case(s) raised in the legacy renderer — see the `error` "
            f"field of each; the gate compares what did render",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
