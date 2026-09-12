"""Imports the legacy Python parser offline, by stubbing its outside world.

The differential harness in `RUST_REWRITE_PLAN.md` §4 needs a reference
implementation. Copying `core.py` here would drift from it within a week, so the
harness imports the *real* module out of `legacy/` and neutralises only what
would otherwise reach for the network, the filesystem or a Redis instance:

* third-party packages that are not installed (`tld`, `bs4`, `loguru`, ...) get
  inert stand-ins registered in :data:`sys.modules`;
* `rl_string_helper.mixins.string_assignment` is a Cython extension with no
  compiled artifact on a fresh checkout, so a pure-Python transcription of
  `string_assignment.pyx` is registered in its place.

Nothing in `core.py`, `utils.py` or `rl_string_helper/string_helper.py` is
modified, and the modules under test are byte-identical to what runs in
production. The one deliberate approximation is `tld.get_fld`, documented on
:func:`_install_tld`.
"""

from __future__ import annotations

import sys
import types
from urllib.parse import urlparse

# --------------------------------------------------------------------------
# Module registry helpers
# --------------------------------------------------------------------------


def _stub(name: str, **attrs: object) -> types.ModuleType:
    """Registers a stand-in module under `name` and returns it."""
    module = types.ModuleType(name)
    for key, value in attrs.items():
        setattr(module, key, value)
    sys.modules[name] = module
    return module


def _stub_sub(parent_name: str, child_name: str, **attrs: object) -> types.ModuleType:
    """Registers `parent.child` and hangs it off the already-stubbed parent.

    Setting the attribute alone is not enough: `from curl_cffi.requests import
    AsyncSession` consults `sys.modules` for the dotted name, so a missing entry
    raises `KeyError: 'curl_cffi.requests'` however the parent looks.
    """
    full_name = f"{parent_name}.{child_name}"
    module = _stub(full_name, **attrs)
    setattr(sys.modules[parent_name], child_name, module)
    return module


# --------------------------------------------------------------------------
# loguru
# --------------------------------------------------------------------------


class _NullLogger:
    """Swallows every `logger.*` call, including the `logger.opt()` detour."""

    def __getattr__(self, _name: str):
        return self._noop

    def __call__(self, *_args: object, **_kwargs: object) -> "_NullLogger":
        return self

    _noop = __call__


# --------------------------------------------------------------------------
# tld
# --------------------------------------------------------------------------

#: Multi-label public suffixes, needed so `get_fld` does not mistake the label
#: before `.co.uk` for the registrable domain. This is a *subset* of the public
#: suffix list: it covers the domains the fixtures use and nothing more. The
#: Rust side uses the real `psl` crate, so `resolve.rs` carries a test that
#: asserts both agree on every domain named here — if that test fails, this
#: table is what needs updating, not the Rust code.
_MULTI_LABEL_SUFFIXES = frozenset(
    {
        "co.uk",
        "org.uk",
        "ac.uk",
        "co.jp",
        "co.kr",
        "co.id",
        "or.id",
        "com.au",
        "com.br",
        "com.cn",
        "co.in",
        "com.mx",
        "co.za",
    }
)


def _install_tld() -> None:
    class TldBadUrl(Exception):
        pass

    def get_fld(
        url: str,
        fail_silently: bool = False,
        fix_protocol: bool = False,
        search_public: bool = True,
        search_private: bool = True,
    ) -> str | None:
        """Returns the registrable domain, approximating the `tld` package.

        `core.py:589` wraps this in `try/except` and falls back to the bare
        hostname, so raising is a supported outcome — for a bare hostname such
        as `localhost` this raises, exactly as `tld` does when it finds no
        suffix it recognises.
        """
        if "//" not in url:
            if not fix_protocol:
                # The real package refuses to guess a protocol by default.
                if fail_silently:
                    return None
                raise TldBadUrl(f"no protocol in {url!r}")
            url = "//" + url
        hostname = urlparse(url).hostname
        if not hostname:
            if fail_silently:
                return None
            raise TldBadUrl(f"cannot parse {url!r}")
        labels = hostname.split(".")
        if len(labels) < 2:
            if fail_silently:
                return None
            raise TldBadUrl(f"no public suffix in {hostname!r}")
        suffix = ".".join(labels[-2:])
        if len(labels) >= 3 and suffix in _MULTI_LABEL_SUFFIXES:
            return ".".join(labels[-3:])
        return suffix

    def get_tld(url: str, **kwargs: object) -> str | None:
        hostname = urlparse(url if "//" in url else "//" + url).hostname or ""
        return get_fld(url, **kwargs) and hostname.partition(".")[2] or None

    _stub("tld", get_fld=get_fld, get_tld=get_tld, TldBadUrl=TldBadUrl)


# --------------------------------------------------------------------------
# Everything else that only ever sits in a type annotation or an elif branch
# --------------------------------------------------------------------------


def _install_misc_stubs() -> None:
    async def _async_noop(*_args: object, **_kwargs: object) -> None:
        return None

    def asyncify(func, *_args: object, **_kwargs: object):
        """`asyncer.asyncify` — core.py:758 awaits the result of this."""

        async def _run(*args: object, **kwargs: object):
            return func(*args, **kwargs)

        return _run

    def alru_cache(*_args: object, **_kwargs: object):
        """`async_lru.alru_cache` — a cache decorator, transparent offline."""

        def _decorate(func):
            return func

        return _decorate

    class _Dummy:
        def __init__(self, *_args: object, **_kwargs: object) -> None:
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_exc: object) -> None:
            return None

        async def get(self, *_args: object, **_kwargs: object):
            return None

        async def post(self, *_args: object, **_kwargs: object):
            return None

    class AbstractCacheBackend:
        """`database_lib.AbstractCacheBackend` — annotation-only offline."""

    _stub("loguru", logger=_NullLogger())
    _stub("bs4", BeautifulSoup=_Dummy)
    _stub("async_lru", alru_cache=alru_cache)
    _stub("asyncer", asyncify=asyncify)
    _stub("database_lib", AbstractCacheBackend=AbstractCacheBackend)
    _stub("aiohttp_retry", RetryClient=_Dummy, ExponentialRetry=_Dummy)
    # `utils.py:11` imports aiohttp at module level but only ever *uses* it
    # inside `resolve_medium_short_link`, `resolve_medium_url_old` and
    # `is_valid_medium_url_old` — all three network functions nothing offline
    # calls. So the stub exists to satisfy the import and nothing else. It has
    # to exist: without it the import fails on any interpreter that lacks the
    # package, which is every machine that has not installed the production
    # requirements. The local development box happens to have it, which is
    # exactly why this went unnoticed until the reference was run in a clean
    # virtualenv.
    _stub("aiohttp", ClientSession=_Dummy)
    curl_cffi = _stub("curl_cffi")
    _stub_sub(curl_cffi.__name__, "requests", AsyncSession=_Dummy)


# --------------------------------------------------------------------------
# rl_string_helper's Cython mixin
# --------------------------------------------------------------------------


class StringAssignmentMixinPython:
    """Pure-Python transcription of `mixins/string_assignment.pyx`.

    The mixin exists because Python strings are immutable and the legacy code
    needs `pop` and per-index assignment on a string. That is exactly the API
    this class reproduces — including `encode`, whose `surrogatepass` error
    handler is what lets `pre_utf_16_bang` encode a string holding lone
    surrogates when a bang char lands inside a surrogate pair.

    Note the deliberate asymmetry in `__setitem__`: a slice key is handed
    straight to `list.__setitem__`, which accepts an iterable and can therefore
    change the length, while a scalar key replaces one element. The legacy code
    relies on both.
    """

    def __init__(self, string: str) -> None:
        if isinstance(string, StringAssignmentMixinPython):
            string = str(string)
        self.string = string
        self.string_list = list(string)

    def _render(self) -> None:
        self.string = "".join(self.string_list)

    def __len__(self) -> int:
        return len(self.string_list)

    def __str__(self) -> str:
        self._render()
        return self.string

    __repr__ = __str__

    def pop(self, key: int) -> "StringAssignmentMixinPython":
        self.string_list.pop(key)
        return self

    def insert(self, key: int, value: str) -> "StringAssignmentMixinPython":
        self.string_list.insert(key, value)
        return self

    def encode(self, encoding: str) -> bytes:
        self._render()
        return self.string.encode(encoding, "surrogatepass")

    def __setitem__(self, key, value: str) -> None:
        self.string_list[key] = value

    def __getitem__(self, key):
        if isinstance(key, slice):
            return "".join(self.string_list[key])
        return self.string_list[key]


def _install_rl_string_helper_mixin() -> None:
    """Puts the pure-Python mixin where the Cython import expects it.

    `string_helper.py` does `from rl_string_helper.mixins.string_assignment
    import StringAssignmentMixin_py as StringAssignmentMixin`. Pre-registering
    that dotted name means the real (uncompiled) `.pyx` is never touched. The
    parent package is registered as a namespace package pointing at the real
    directory, so the rest of `rl_string_helper.mixins` still resolves.
    """
    import os

    parent = _stub("rl_string_helper.mixins")
    parent.__path__ = [  # type: ignore[attr-defined]
        os.path.join(_legacy_root(), "rl_string_helper", "rl_string_helper", "mixins")
    ]
    _stub(
        "rl_string_helper.mixins.string_assignment",
        StringAssignmentMixin=StringAssignmentMixinPython,
        StringAssignmentMixin_py=StringAssignmentMixinPython,
    )


# --------------------------------------------------------------------------
# Entry point
# --------------------------------------------------------------------------


def _legacy_root() -> str:
    """`<repo>/legacy`, derived from this file's location."""
    import os

    here = os.path.dirname(os.path.abspath(__file__))
    # xtask/difftest/py -> xtask/difftest -> xtask -> repo root
    return os.path.abspath(os.path.join(here, "..", "..", "..", "legacy"))


def install() -> None:
    """Stubs the outside world and puts both legacy packages on `sys.path`."""
    import os

    _install_tld()
    _install_misc_stubs()
    _install_rl_string_helper_mixin()

    root = _legacy_root()
    for package in ("rl_string_helper", "medium-parser"):
        path = os.path.join(root, package)
        if path not in sys.path:
            sys.path.insert(0, path)
