#!/bin/env python3

from os import listdir
from os.path import isfile, join
from typing import Dict, List

from jinja2 import Template

# Constants
APP_ROOT = "/static"
STATIC_DIR = "./static"
ACCESS_DENIED_PATHS: List[str] = [
    "websocket",
    "meta.json",
    "cdn-cgi/challenge-platform/scripts/jsd/main.js",
    "cdn-cgi/rum",
    "graphql/websocket",
    "onboarding/*",
    "wp-*",
    ".env",
    # `"api*"` used to be here, and its removal is Fase 6. It answered
    # `403 Access denied` to `/api*` — every path, not only `/api/` — which is
    # the whole of the legacy's "this deployment has no public API". The Rust
    # server serves one at `/api/v1` (§2.7), so the denylist entry has to go with
    # it. Regenerating `caddy/Caddyfile` after this edit removes exactly one
    # `handle_path /api*` block and adds nothing.
    #
    # **This is prepared and not deployed.** Until a reload happens, a deployed
    # edge still answers `403` for `/api/*`; the API's own proof runs against the
    # Rust server directly. Deploy order is Caddy first, then telling clients the
    # API exists — the reverse leaves every caller with a `403` from an edge that
    # is answering correctly for the config it has.
    "apple-touch-icon-precomposed.png",
    "rss.xml",
    ".git/*",
    "apple-touch-icon-120x120.png",
    "apple-touch-icon-120x120-precomposed.png",
    "apple-touch-icon-152x152.png",
    "apple-touch-icon-152x152-precomposed.png",
    ".well-known/*",
    "cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1",
    "cdn-cgi/challenge-platform/h/g/orchestrate/chl_page/v1",
]

CADDY_FILE_TEMPLATES: Dict[str, str] = {
    "CaddyfileTemplate": "Caddyfile",
}


def get_static_files(directory: str) -> List[str]:
    return [f for f in listdir(directory) if isfile(join(directory, f))]


def generate_static_file_rules(files: List[str]) -> List[str]:
    template = Template(
        """
    handle_path /{{ file }} {
        root * {{ app_root }}/{{ file }}
        file_server
    }
    """
    )
    return [template.render(file=file, app_root=APP_ROOT) for file in files]


def generate_access_denied_rules(paths: List[str]) -> List[str]:
    template = Template(
        """
    handle_path /{{ file }} {
        respond "Access denied" 403
    }
    """
    )
    return [template.render(file=path) for path in paths]


def generate_caddy_rules() -> str:
    static_files = get_static_files(STATIC_DIR)
    static_rules = generate_static_file_rules(static_files)
    denied_rules = generate_access_denied_rules(ACCESS_DENIED_PATHS)
    return "\n".join(static_rules + denied_rules)


def render_caddy_file(template_path: str, output_path: str, rules: str) -> None:
    try:
        with open(template_path, "r") as file:
            template = Template(file.read())

        rendered_content = normalize(template.render(template=rules))

        with open(output_path, "w") as file:
            file.write(rendered_content)
    except IOError as e:
        print(f"Error processing {template_path}: {e}")


def normalize(content: str) -> str:
    """Trailing whitespace off every line, exactly one newline at the end.

    Not cosmetic, and not a preference: without it this script does **not**
    reproduce the file it just wrote. The rule templates above end with the
    indentation of their closing `\"\"\"`, so every rule contributes a line of
    four spaces after itself — 36 of them in the current `Caddyfile` — and the
    last rule ends the file with no newline at all. Both are invisible in Caddy
    and both made every regeneration a 38-line diff of whitespace wrapped around
    whatever actually changed.

    The committed `Caddyfile` was normalised by hand at some point, which is why
    the two disagreed. Doing it here instead makes `python3
    generate_caddy_file.py` idempotent: regenerate and the diff is the real
    change and nothing else. That is the property Fase 6's one-block diff needs,
    and it is the property any future edit needs too.
    """
    return "\n".join(line.rstrip() for line in content.splitlines()) + "\n"


def main() -> None:
    rules = generate_caddy_rules()

    for template_file, output_file in CADDY_FILE_TEMPLATES.items():
        render_caddy_file(template_file, output_file, rules)


if __name__ == "__main__":
    main()
