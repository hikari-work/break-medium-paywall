//! Medium URL handling.
//!
//! Ports `legacy/medium-parser/medium_parser/utils.py`. Two things about that
//! file are worth knowing before reading this one:
//!
//! - `resolve_medium_url` returns `False` for "could not resolve" and a `str`
//!   for "resolved", in the same return channel, and recurses into itself
//!   without a depth limit. Here it returns `Option<PostId>` (§7 item 5 of the
//!   rewrite plan) and stops after [`MAX_REDIRECT_DEPTH`] hops, which the legacy
//!   code would have followed forever on a URL that redirects to itself.
//! - The one branch that needs the network is `link.medium.com` →
//!   `rsci.app.link`. That is behind the [`LinkResolver`] seam so Fase 1 stays
//!   pure and testable; Fase 2 supplies the HTTP implementation.
//!
//! ## Divergences from `urllib.parse`
//!
//! Python's `urlparse` never fails — a schemeless string comes back with an
//! empty netloc and the whole string as the path — while `url::Url` is strict.
//! Where the two disagree the `url` crate wins, because the alternative is a
//! hand-rolled parser with its own bugs:
//!
//! - `parsed_url.netloc` in Python includes userinfo and the port, and preserves
//!   the host's case. The `url` crate's host is lowercased and excludes both, so
//!   `https://LINK.Medium.com/x` resolves here and does not resolve in Python.
//!   That is the better behaviour and it is confined to this module.
//! - `unquerify_url` returns the input unchanged when the `url` crate cannot
//!   parse it, where `urlunparse` would have round-tripped it.

use std::fmt;

use tracing::{debug, warn};
use url::Url;

/// Protocol assumed for a bare `www.` host (`utils.py:20`).
pub const DEFAULT_URL_PROTOCOL: &str = "https://";

/// Domains that host a Medium publication under a custom name
/// (`utils.py:24-37`).
pub const KNOWN_MEDIUM_CUSTOM_DOMAINS: &[&str] = &[
    "javascript.plainenglish.io",
    "blog.llamaindex.ai",
    "code.likeagirl.io",
    "medium.datadriveninvestor.com",
    "blog.det.life",
    "python.plainenglish.io",
    "blog.stackademic.com",
    "ai.gopubby.com",
    "blog.devops.dev",
    "levelup.gitconnected.com",
    "betterhumans.coach.me",
    "ai.plainenglish.io",
];

/// Medium's own domains and the publications that use the `medium.com` path
/// (`utils.py:38-69`).
pub const KNOWN_MEDIUM_DOMAINS: &[&str] = &[
    "medium.com",
    "uxplanet.org",
    "osintteam.blog",
    "ahmedelfakharany.com",
    "drlee.io",
    "artificialcorner.com",
    "generativeai.pub",
    "productcoalition.com",
    "towardsdev.com",
    "infosecwriteups.com",
    "towardsdatascience.com",
    "thetaoist.online",
    "devopsquare.com",
    "laceydearie.com",
    "bettermarketing.pub",
    "itnext.io",
    "eand.co",
    "betterprogramming.pub",
    "curiouse.co",
    "betterhumans.pub",
    "uxdesign.cc",
    "thebolditalic.com",
    "arcdigital.media",
    "codeburst.io",
    "psiloveyou.xyz",
    "writingcooperative.com",
    "entrepreneurshandbook.co",
    "prototypr.io",
    "theascent.pub",
    "storiusmag.com",
];

/// Domains that are never a Medium article, however they arrive
/// (`utils.py:70-89`).
pub const NOT_MEDIUM_DOMAINS: &[&str] = &[
    "github.com",
    "yandex.ru",
    "yandex.kz",
    "youtube.com",
    "nytimes.com",
    "wsj.com",
    "reddit.com",
    "elpais.com",
    "forbes.com",
    "bloomberg.com",
    "lesechos.fr",
    "otz.de",
    "businessinsider.com",
    "buff.ly",
    "delish.com",
    "economist.com",
    "wired.com",
    "rollingstone.com",
];

/// Redirect sites that are recognised before the domain lists are consulted
/// (`utils.py:432`).
const REDIRECT_ONLY_DOMAINS: &[&str] = &[
    "12ft.io",
    "google.com",
    "facebook.com",
    "googleusercontent.com",
];

/// How many redirects [`resolve_medium_url`] will follow before giving up.
///
/// The legacy function had no limit; a URL that redirected to itself would spin
/// until the process ran out of stack. Eight is far more than the two or three
/// hops a real tracking link takes.
pub const MAX_REDIRECT_DEPTH: usize = 8;

/// A resolved Medium post id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PostId(String);

impl PostId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A URL that is definitively not a Medium article (`utils.py:436`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotValidMediumUrl;

impl fmt::Display for NotValidMediumUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("100% not a valid Medium URL")
    }
}

impl std::error::Error for NotValidMediumUrl {}

/// The network seam for the one branch of [`resolve_medium_url`] that needs it.
pub trait LinkResolver {
    /// Follows `https://rsci.app.link/{short_url_id}` and returns the URL it
    /// redirects to (`utils.py:240-255`).
    fn resolve_short_link(&self, short_url_id: &str) -> Option<String>;
}

/// The resolver Fase 1 ships.
///
/// It refuses to make the call, so a `link.medium.com` link comes back
/// unresolved instead of reaching out. Fase 2 replaces this with the HTTP
/// client the rest of the rewrite will already have.
#[derive(Debug, Clone, Copy, Default)]
pub struct OfflineLinkResolver;

impl LinkResolver for OfflineLinkResolver {
    fn resolve_short_link(&self, short_url_id: &str) -> Option<String> {
        warn!(
            short_url_id,
            "no LinkResolver is configured; the short link stays unresolved"
        );
        None
    }
}

/// Drops the query string and a trailing slash (`utils.py:139-156`).
pub fn unquerify_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_string();
    };
    if parsed.query().is_some() {
        parsed.set_query(None);
    }
    // `removesuffix("/")`, not `trim_end_matches`: Python takes off at most one.
    parsed
        .as_str()
        .strip_suffix('/')
        .unwrap_or(parsed.as_str())
        .to_string()
}

/// Strips a leading `www.` (`utils.py:159-164`).
pub fn un_wwwify(url: &str) -> &str {
    url.strip_prefix("www.").unwrap_or(url)
}

/// Drops a `/page/2` pagination suffix and a trailing slash
/// (`utils.py:195-200`).
pub fn unplaginate_url(url: &str) -> String {
    let unpaginated = url.strip_suffix("/page/2").unwrap_or(url);
    unpaginated
        .strip_suffix('/')
        .unwrap_or(unpaginated)
        .to_string()
}

/// `tld.get_fld` — the registrable domain of a URL (`utils.py:408-416`).
///
/// Returns `None` when the URL has no scheme, when it has no host, or when the
/// host is not under a public suffix, which is what `tld.get_fld` raising
/// amounts to at every call site.
pub fn registrable_domain(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    psl::domain_str(&host).map(str::to_string)
}

/// The host of a URL, with no port and no userinfo (`utils.py:594-595`).
pub fn host_of(url: &str) -> Option<String> {
    Url::parse(url).ok()?.host_str().map(str::to_string)
}

/// `correct_url` (`utils.py:167-192`) — **a port of a no-op**.
///
/// The function computes `unquerified_url` and `unplaginated_url`, logs whether
/// each differed, and then returns `url` itself. The Safari workaround it was
/// written for has been commented out, and so has the protocol fix-up. Every
/// caller therefore gets its argument back unchanged.
///
/// It is ported as an identity so the gate sees the same bytes, with the debug
/// lines kept because they are the only observable effect. Whether the two
/// computations should actually be returned is open question §8.3 of the
/// rewrite plan and needs the repo owner's answer before Fase 4.
pub fn correct_url(url: &str) -> String {
    let unquerified = unquerify_url(url);
    debug!(
        changed = unquerified != url,
        "is the URL carrying query data"
    );
    let unplaginated = unplaginate_url(&unquerified);
    debug!(
        changed = unplaginated != unquerified,
        "is the URL paginated"
    );
    url.to_string()
}

/// True when every character is an ASCII letter or digit and the length is 8 to
/// 12 (`utils.py:208-227`).
pub fn basic_hex_check(hex_string: &str) -> bool {
    if !hex_string.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return false;
    }
    (8..=12).contains(&hex_string.chars().count())
}

/// The last hex-looking id in a string (`utils.py:230-237`).
///
/// Prefers an id preceded by a `-`; falls back to a bare one. `match[-1]` takes
/// the **last** match, not the first, which matters for a URL like
/// `/a-12345678-b-87654321`.
pub fn extract_hex_string(input: &str) -> Option<&str> {
    // The patterns are literals and cannot fail to compile; a `Regex` is built
    // per call because there is no `lru_cache` here and the call sites are not
    // hot.
    let prefixed = regex::Regex::new(r"-(\b[a-fA-F0-9]{8,12}\b)").expect("literal pattern");
    let captures: Vec<_> = prefixed.captures_iter(input).collect();
    if let Some(last) = captures.last() {
        return last.get(1).map(|matched| matched.as_str());
    }

    let bare = regex::Regex::new(r"(\b[a-fA-F0-9]{8,12}\b)").expect("literal pattern");
    bare.captures_iter(input)
        .last()
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str())
}

/// True when the string contains something that looks like a post id
/// (`utils.py:203-205`).
pub fn is_has_valid_medium_post_id(hex_string: &str) -> bool {
    extract_hex_string(hex_string).is_some()
}

/// Follows a Medium URL, including the tracking redirects Medium wraps around
/// it, to a post id (`utils.py:258-356`).
///
/// Returns `None` where the legacy function returned `False`.
pub fn resolve_medium_url(url: &str, resolver: &dyn LinkResolver) -> Option<PostId> {
    resolve_medium_url_at(url, resolver, 0)
}

fn resolve_medium_url_at(url: &str, resolver: &dyn LinkResolver, depth: usize) -> Option<PostId> {
    if depth >= MAX_REDIRECT_DEPTH {
        warn!(url, depth, "redirect chain is too long; giving up");
        return None;
    }
    debug!(url, depth, "resolving");

    // A URL the `url` crate rejects cannot be walked, and the legacy code's
    // `urlparse` reading of it (empty netloc, whole string as the path) only
    // ever reached the final `else` branch below. Falling back to the raw string
    // as the path reproduces that.
    let Ok(parsed) = Url::parse(url) else {
        return post_id_from_path(url);
    };
    let netloc = un_wwwify(parsed.host_str().unwrap_or_default());
    let path = parsed.path();

    // `utils.py:264-267` — a Medium "mobile" link carries the id in its path.
    if let Some(rest) = path.strip_prefix("/p/") {
        debug!("URL is a Medium 'mobile' link");
        let post_id = rest.rsplit("/p/").next().unwrap_or(rest);
        return to_post_id(post_id);
    }

    // The four tracking-redirect shapes. Each pulls one query parameter and
    // recurses; anything else is unresolvable, which is the legacy `return
    // False`.
    let redirect = if netloc == "l.facebook.com" && path.starts_with("/l.php") {
        debug!("URL looks like a Facebook tracking redirect");
        single_query_param(&parsed, "u")
    } else if netloc == "webcache.googleusercontent.com" && path.starts_with("/search") {
        debug!("URL looks like a Google webcache link");
        single_query_param(&parsed, "q").map(|post_url| {
            post_url
                .strip_prefix("cache:")
                .map_or(post_url.clone(), str::to_string)
        })
    } else if netloc == "google.com" && path.starts_with("/url") {
        debug!("URL looks like a Google tracking redirect");
        single_query_param(&parsed, "url").or_else(|| single_query_param(&parsed, "q"))
    } else if netloc == "12ft.io" {
        debug!("URL looks like a 12ft.io link");
        single_query_param(&parsed, "q")
    } else if path.starts_with("/m/global-identity-2") {
        debug!("URL looks like a Medium email redirect");
        single_query_param(&parsed, "redirectUrl")
    } else if netloc == "link.medium.com" {
        debug!("URL looks like a Medium short link");
        let short_url_id = path.strip_prefix('/').unwrap_or(path);
        resolver.resolve_short_link(short_url_id)
    } else {
        debug!("URL shape is unknown; falling back to the id in the path");
        let post_url = path.rsplit('/').next().unwrap_or_default();
        let post_id = post_url.rsplit('-').next().unwrap_or(post_url);
        return to_post_id(post_id);
    };

    match redirect {
        Some(post_url) => resolve_medium_url_at(&post_url, resolver, depth + 1),
        None => {
            debug!("the redirect URL could not be read; giving up");
            None
        }
    }
}

/// The `else` branch of `utils.py:345-350`: everything after the last `-` in the
/// last path segment.
fn post_id_from_path(path: &str) -> Option<PostId> {
    let without_query = path.split(['?', '#']).next().unwrap_or(path);
    let post_url = without_query.rsplit('/').next().unwrap_or_default();
    to_post_id(post_url.rsplit('-').next().unwrap_or(post_url))
}

fn to_post_id(candidate: &str) -> Option<PostId> {
    if is_has_valid_medium_post_id(candidate) {
        Some(PostId::new(candidate))
    } else {
        warn!(candidate, "not a valid post id");
        None
    }
}

/// `parse_qs` keeps only non-blank values, so a blank one is dropped here too.
fn single_query_param(parsed: &Url, key: &str) -> Option<String> {
    let pairs: Vec<(String, String)> =
        form_urlencoded::parse(parsed.query().unwrap_or_default().as_bytes())
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
    let mut matching = pairs.iter().filter(|(name, _)| name == key);
    let first = matching.next()?;
    // `len(parsed_query[key]) == 1` — more than one and the legacy code gives up.
    if matching.next().is_some() {
        return None;
    }
    Some(first.1.clone())
}

/// Is this URL a Medium article? (`utils.py:419-448`)
///
/// `Err(NotValidMediumUrl)` is the legacy `raise`, which callers are expected to
/// treat differently from a plain `false`.
pub fn is_valid_medium_url(
    url: &str,
    resolver: &dyn LinkResolver,
) -> Result<bool, NotValidMediumUrl> {
    let parsed = Url::parse(url).ok();
    let domain = registrable_domain(url);
    let domain_netloc = parsed
        .as_ref()
        .and_then(|parsed| parsed.host_str())
        .map(un_wwwify);

    if let Some(domain) = domain.as_deref() {
        if REDIRECT_ONLY_DOMAINS.contains(&domain) {
            return Ok(true);
        }
    }

    let is_known_bad = |candidate: Option<&str>| {
        candidate.is_some_and(|candidate| NOT_MEDIUM_DOMAINS.contains(&candidate))
    };
    if is_known_bad(domain.as_deref()) || is_known_bad(domain_netloc) {
        return Err(NotValidMediumUrl);
    }

    let is_known_good = domain
        .as_deref()
        .is_some_and(|domain| KNOWN_MEDIUM_DOMAINS.contains(&domain))
        || domain_netloc.is_some_and(|netloc| KNOWN_MEDIUM_CUSTOM_DOMAINS.contains(&netloc));
    if is_known_good {
        return Ok(true);
    }

    warn!(url, "URL was not recognised as a known Medium domain");
    // The legacy code's own note: the domain list is incomplete, so a resolve is
    // the tie-breaker.
    Ok(resolve_medium_url(url, resolver).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver that answers without the network.
    struct StubResolver(Option<&'static str>);

    impl LinkResolver for StubResolver {
        fn resolve_short_link(&self, _short_url_id: &str) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    fn offline() -> OfflineLinkResolver {
        OfflineLinkResolver
    }

    #[test]
    fn unquerify_drops_the_query_and_a_trailing_slash() {
        assert_eq!(
            unquerify_url("https://example.com/post?utm_source=x"),
            "https://example.com/post"
        );
        assert_eq!(
            unquerify_url("https://example.com/post/"),
            "https://example.com/post"
        );
        assert_eq!(
            unquerify_url("https://example.com/post"),
            "https://example.com/post"
        );
    }

    /// The legacy version round-trips every URL through `urlunparse`, so a URL
    /// it cannot parse comes back untouched — as it does here.
    #[test]
    fn unquerify_leaves_an_unparseable_url_alone() {
        assert_eq!(unquerify_url("not a url"), "not a url");
    }

    #[test]
    fn un_wwwify_strips_only_a_leading_www() {
        assert_eq!(un_wwwify("www.medium.com"), "medium.com");
        assert_eq!(un_wwwify("medium.com"), "medium.com");
        // `removeprefix` strips one occurrence, not the whole prefix run.
        assert_eq!(un_wwwify("www.www.medium.com"), "www.medium.com");
        assert_eq!(un_wwwify("wwww.medium.com"), "wwww.medium.com");
    }

    #[test]
    fn unplaginate_drops_the_page_suffix() {
        assert_eq!(
            unplaginate_url("https://x.test/a/page/2"),
            "https://x.test/a"
        );
        assert_eq!(unplaginate_url("https://x.test/a/"), "https://x.test/a");
        assert_eq!(
            unplaginate_url("https://x.test/a/page/3"),
            "https://x.test/a/page/3"
        );
    }

    /// The dates and the pagination pass run but are discarded — see the doc
    /// comment. This test exists to lock that in, so a well-meaning "fix" here
    /// shows up as a failure rather than as a silent behaviour change.
    #[test]
    fn correct_url_returns_its_argument_unchanged() {
        for url in [
            "https://example.com/post?utm_source=x",
            "https://example.com/post/page/2",
            "",
        ] {
            assert_eq!(correct_url(url), url, "{url:?}");
        }
    }

    #[test]
    fn registrable_domain_needs_a_public_suffix() {
        assert_eq!(
            registrable_domain("https://www.example.com/post").as_deref(),
            Some("example.com")
        );
        assert_eq!(registrable_domain("https://localhost/x"), None);
        assert_eq!(registrable_domain("example.com/x"), None, "no scheme");
        assert_eq!(registrable_domain("not a url"), None);
    }

    /// `stubs.py` approximates `tld` with a hand-written suffix table so the
    /// Python reference can run offline. If the real list disagreed, the
    /// reference and this side would compute different embed sites, so the
    /// domains that table names are pinned here.
    #[test]
    fn the_public_suffix_list_agrees_with_the_python_stub() {
        for (url, expected) in [
            ("https://www.example.co.uk/post", "example.co.uk"),
            ("https://a.example.org.uk/p", "example.org.uk"),
            ("https://x.example.ac.uk/p", "example.ac.uk"),
            ("https://x.example.co.jp/p", "example.co.jp"),
            ("https://x.example.co.kr/p", "example.co.kr"),
            ("https://x.example.co.id/p", "example.co.id"),
            ("https://x.example.or.id/p", "example.or.id"),
            ("https://x.example.com.au/p", "example.com.au"),
            ("https://x.example.com.br/p", "example.com.br"),
            ("https://x.example.com.cn/p", "example.com.cn"),
            ("https://x.example.co.in/p", "example.co.in"),
            ("https://x.example.com.mx/p", "example.com.mx"),
            ("https://x.example.co.za/p", "example.co.za"),
        ] {
            assert_eq!(registrable_domain(url).as_deref(), Some(expected), "{url}");
        }
    }

    #[test]
    fn basic_hex_check_bounds() {
        assert!(basic_hex_check("12345678"));
        assert!(basic_hex_check("abcdef123456"));
        assert!(
            basic_hex_check("abcdefg1"),
            "letters are not restricted to a-f"
        );
        assert!(!basic_hex_check("1234567"), "too short");
        assert!(!basic_hex_check("1234567890123"), "too long");
        assert!(!basic_hex_check("abcdef-1"), "a dash is not alphanumeric");
        assert!(!basic_hex_check(""), "an empty id is too short");
    }

    /// The suffix after the last `-` is preferred, and among several candidates
    /// the *last* one wins (`match[-1]`).
    #[test]
    fn extract_hex_string_prefers_a_dash_prefixed_id() {
        assert_eq!(
            extract_hex_string("post-title-0291df856c77"),
            Some("0291df856c77")
        );
        assert_eq!(extract_hex_string("0291df856c77"), Some("0291df856c77"));
        assert_eq!(extract_hex_string("no id here"), None);
    }

    #[test]
    fn extract_hex_string_takes_the_last_candidate() {
        assert_eq!(
            extract_hex_string("a-12345678-b-87654321"),
            Some("87654321"),
            "the legacy code indexes match[-1]"
        );
    }

    /// A dash is required only for the first attempt; the fallback finds the id
    /// without one.
    #[test]
    fn extract_hex_string_falls_back_to_a_bare_id() {
        assert_eq!(extract_hex_string("hello world 12345678"), Some("12345678"));
    }

    #[test]
    fn resolving_a_medium_mobile_link() {
        assert_eq!(
            resolve_medium_url("https://medium.com/p/0291df856c77", &offline()),
            Some(PostId::new("0291df856c77"))
        );
    }

    #[test]
    fn resolving_a_plain_post_url_takes_the_path_suffix() {
        assert_eq!(
            resolve_medium_url(
                "https://medium.com/@someone/some-title-0291df856c77",
                &offline()
            ),
            Some(PostId::new("0291df856c77"))
        );
    }

    #[test]
    fn an_invalid_post_id_does_not_resolve() {
        assert_eq!(
            resolve_medium_url("https://medium.com/@someone/title", &offline()),
            None
        );
    }

    #[test]
    fn a_google_tracking_redirect_is_followed() {
        let url = "https://google.com/url?url=https%3A%2F%2Fmedium.com%2F%40x%2Ft-0291df856c77";
        assert_eq!(
            resolve_medium_url(url, &offline()),
            Some(PostId::new("0291df856c77"))
        );
    }

    #[test]
    fn a_google_webcache_link_drops_the_cache_prefix() {
        let url = "https://webcache.googleusercontent.com/search?q=cache:https://medium.com/@x/t-0291df856c77";
        assert_eq!(
            resolve_medium_url(url, &offline()),
            Some(PostId::new("0291df856c77"))
        );
    }

    /// `len(parsed_query["u"]) == 1` — a repeated parameter is unresolvable.
    #[test]
    fn a_repeated_query_parameter_does_not_resolve() {
        let url = "https://l.facebook.com/l.php?u=https%3A%2F%2Fmedium.com%2Fp%2F0291df856c77&u=https%3A%2F%2Fmedium.com%2Fp%2F0291df856c77";
        assert_eq!(resolve_medium_url(url, &offline()), None);
    }

    /// A blank value is dropped by `parse_qs`, so the parameter counts as absent.
    #[test]
    fn a_blank_query_parameter_does_not_resolve() {
        assert_eq!(
            resolve_medium_url("https://l.facebook.com/l.php?u=", &offline()),
            None
        );
    }

    #[test]
    fn a_short_link_goes_through_the_resolver() {
        let resolver = StubResolver(Some("https://medium.com/@x/t-0291df856c77"));
        assert_eq!(
            resolve_medium_url("https://link.medium.com/abc123", &resolver),
            Some(PostId::new("0291df856c77"))
        );
        assert_eq!(
            resolve_medium_url("https://link.medium.com/abc123", &offline()),
            None,
            "the offline resolver makes no call"
        );
    }

    /// The legacy function recursed without a limit and would exhaust the stack
    /// on a redirect that pointed at itself. This one stops.
    #[test]
    fn a_redirect_loop_terminates() {
        let url = "https://google.com/url?url=https%3A%2F%2Fgoogle.com%2Furl%3Furl%3Dhttps%253A%252F%252Fgoogle.com%252Furl";
        assert_eq!(resolve_medium_url(url, &offline()), None);
    }

    #[test]
    fn known_domains_are_accepted() {
        assert_eq!(
            is_valid_medium_url("https://medium.com/@x/t-0291df856c77", &offline()),
            Ok(true)
        );
        assert_eq!(
            is_valid_medium_url("https://www.uxdesign.cc/t-0291df856c77", &offline()),
            Ok(true)
        );
    }

    #[test]
    fn known_bad_domains_are_rejected_loudly() {
        assert_eq!(
            is_valid_medium_url("https://github.com/x/y", &offline()),
            Err(NotValidMediumUrl)
        );
    }

    #[test]
    fn redirect_only_domains_are_accepted() {
        assert_eq!(
            is_valid_medium_url("https://google.com/url?url=x", &offline()),
            Ok(true)
        );
    }

    /// An unknown domain falls through to a resolve, which is the legacy
    /// behaviour and the reason the function is not just a list lookup.
    #[test]
    fn an_unknown_domain_falls_back_to_resolving() {
        assert_eq!(
            is_valid_medium_url("https://some-blog.test/@x/t-0291df856c77", &offline()),
            Ok(true)
        );
        assert_eq!(
            is_valid_medium_url("https://some-blog.test/@x/title", &offline()),
            Ok(false)
        );
    }
}
