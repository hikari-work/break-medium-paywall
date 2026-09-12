//! Whole pages: the templates plus the contexts the legacy server hands them.
//!
//! Ports the render half of `legacy/web/server/` — `handlers/post.py`,
//! `handlers/main.py` and `utils/error.py` — as pure functions. Nothing here
//! touches a database, a socket or a clock, which is what lets
//! `xtask/difftest` call [`render_post`] on a fixture and compare it with the
//! real Jinja2 output for the same payload.
//!
//! # The base template is rendered with a narrower context than you would expect
//!
//! `base.html` has `{% if creator %}<meta name="author" ...>{% endif %}`, and it
//! looks like the post page ought to pass `creator`. It does not:
//! `handlers/post.py:89-98` builds a base context of `host_address`,
//! `enable_ads_header`, `body_template`, `title` and `description`, and nothing
//! else. `creator` is therefore `Undefined` and the author meta tag is **never
//! emitted** on any page.
//!
//! That is preserved rather than fixed. The `<meta name="author">` is not what
//! the parity gate is checking today, but it will change the bytes of every page
//! the moment someone "fixes" it here alone, and a silent divergence in the tag
//! is worse than an absent one. If it should be emitted, that is a change to make
//! on both sides deliberately.
//!
//! # `post.html` never sees `enable_ads_header`
//!
//! Same shape of quirk. The post body is rendered by `MediumParser`, whose own
//! Jinja environment has no such variable, so `{% if enable_ads_header %}` at the
//! top of `post.html` is always false — the ad spacer never appears inside an
//! article. `base.html` *does* get the flag and renders the banner there. Only
//! `render_post` reproduces this, and it does so by simply not putting the key in
//! the context.

use minijinja::{Environment, Error};
use serde_json::{Value, json};

use medium_doc::ir::Document;
use medium_doc::metadata::PostMetadata;

use crate::html::render_blocks;
use crate::post::RenderedPost;

/// `config.HOST_ADDRESS` (`config.py:5`).
pub const DEFAULT_HOST_ADDRESS: &str = "https://freedium.cfd";

/// What `base.html` needs beyond the body: where the site is, and whether to
/// show the ad banner (`config.HOST_ADDRESS`, `config.ENABLE_ADS_BANNER`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageConfig {
    /// Interpolated into the page's scripts as the base for `/@miro/v2/`.
    pub host_address: String,
    /// The banner at the top of `base.html`, not the spacer in `post.html`.
    pub enable_ads_header: bool,
}

impl Default for PageConfig {
    fn default() -> Self {
        Self {
            host_address: DEFAULT_HOST_ADDRESS.to_string(),
            enable_ads_header: false,
        }
    }
}

impl PageConfig {
    pub fn new(host_address: impl Into<String>) -> Self {
        Self {
            host_address: host_address.into(),
            enable_ads_header: false,
        }
    }

    #[must_use]
    pub fn with_ads_header(mut self, enabled: bool) -> Self {
        self.enable_ads_header = enabled;
        self
    }
}

/// `base.html`, the document every page is wrapped in.
///
/// `title` and `description` are the *page* values, not the article's:
/// `handlers/post.py:95-96` passes `RenderedPost.title` (which is the
/// `"{title} | by {creator}"` string) as `title`, so the `<title>` tag reads
/// `My Post | by Ada - Freedium`.
///
/// Both are optional in the template — `{{ title or "Breaking Medium paywall!" }}`
/// — and an empty string takes the same branch a missing key does, because
/// Python's `or` and minijinja's agree that `""` is falsy.
pub fn render_base(
    env: &Environment<'_>,
    body_html: &str,
    title: &str,
    description: &str,
    config: &PageConfig,
) -> Result<String, Error> {
    env.get_template("base.html")?.render(json!({
        "host_address": config.host_address,
        "enable_ads_header": config.enable_ads_header,
        "body_template": body_html,
        "title": title,
        "description": description,
        // `creator` is deliberately absent — see the module docs.
    }))
}

/// `render_medium_post_link`'s second half (`handlers/post.py:88-98`).
///
/// Renders `post.html` around the article body and wraps it in `base.html`. The
/// returned [`RenderedPost`] is the cacheable unit: `title` holds the page title
/// rather than the article's, matching `HtmlResult`'s field order
/// (`models/html_result.py:4-9`).
///
/// # `content` is a list of fragments, not one string
///
/// `post.html:68` is `{% for paragraph in content %}{{ paragraph }}{% endfor %}`.
/// The loop is why [`render_blocks`] returns a `Vec<String>` — the legacy
/// `out_paragraphs` is a list too, and keeping the shape means a divergence can
/// be reported as "block 7" instead of "the page".
pub fn render_post(
    env: &Environment<'_>,
    document: &Document,
    metadata: &PostMetadata,
    config: &PageConfig,
) -> Result<RenderedPost, Error> {
    let content = render_blocks(document);

    let page_title = metadata.page_title();
    let body = env
        .get_template("post.html")?
        .render(post_context(metadata, &content))?;
    let html = render_base(env, &body, &page_title, &metadata.description, config)?;

    Ok(RenderedPost {
        title: page_title,
        description: metadata.description.clone(),
        url: metadata.url.clone(),
        html,
    })
}

/// The context `core.py:795-807` builds for `post.html`, key for key.
///
/// Separate from [`render_post`] so the differential harness can render the body
/// on its own and diff it against `_parse_and_render_content_html_post`'s
/// `post_template`, which is what isolates a template bug from a renderer bug.
pub fn post_context(metadata: &PostMetadata, content: &[String]) -> Value {
    json!({
        "subtitle": metadata.subtitle,
        "title": metadata.title,
        "url": metadata.url,
        "creator": metadata.creator,
        "collection": metadata.collection,
        "readingTime": metadata.reading_time,
        "freeAccess": metadata.free_access,
        "updatedAt": metadata.updated_at,
        "firstPublishedAt": metadata.first_published_at,
        "previewImageId": metadata.preview_image_id,
        "content": content,
        "tags": metadata.tags,
        // `enable_ads_header` is deliberately absent — see the module docs.
    })
}

/// `render_homepage` (`handlers/post.py:16-47`), minus the fetching.
///
/// `post_list` is a list of [`PostMetadata`] as dictionaries, which is what
/// `generate_metadata(..., as_dict=True)` returns. The template reads
/// `post.post_id`, `post.reading_time`, `post.preview_image_id` and
/// `post.first_published_at` in snake case, so the field names here are
/// load-bearing in a way the article's camelCase context is not.
///
/// No [`PageConfig`]: `homepage.html` interpolates neither `host_address` nor
/// `enable_ads_header`, and passing a config the template never reads would
/// suggest otherwise. The wrapper is what needs it.
pub fn render_homepage(env: &Environment<'_>, posts: &[PostMetadata]) -> Result<String, Error> {
    let post_list: Vec<Value> = posts.iter().map(metadata_context).collect();
    env.get_template("homepage.html")?
        .render(json!({ "post_list": post_list }))
}

/// One row of `homepage.html`'s `post_list`.
fn metadata_context(metadata: &PostMetadata) -> Value {
    json!({
        "post_id": metadata.post_id,
        "title": metadata.title,
        "subtitle": metadata.subtitle,
        "description": metadata.description,
        "url": metadata.url,
        "creator": metadata.creator,
        "collection": metadata.collection,
        "reading_time": metadata.reading_time,
        "free_access": metadata.free_access,
        "updated_at": metadata.updated_at,
        "first_published_at": metadata.first_published_at,
        "preview_image_id": metadata.preview_image_id,
        "tags": metadata.tags,
    })
}

/// `main.html` with the homepage in it (`handlers/main.py:50-52`).
///
/// The single pass where the legacy does two. See [`crate::templates`] for why
/// that is the same output: `main.html` interpolates `{{ postleter }}` and
/// nothing else, and the only other thing the first pass does is expand
/// `{% include 'url_box.html' %}`, which minijinja's include does here.
pub fn render_main(env: &Environment<'_>, homepage_html: &str) -> Result<String, Error> {
    env.get_template("main.html")?
        .render(json!({ "postleter": homepage_html }))
}

/// The whole of `main_page()` (`handlers/main.py:49-63`): homepage → main → base.
///
/// This is `/`, and it is what the compose healthcheck fetches.
pub fn render_index(
    env: &Environment<'_>,
    posts: &[PostMetadata],
    config: &PageConfig,
) -> Result<String, Error> {
    let homepage = render_homepage(env, posts)?;
    let main = render_main(env, &homepage)?;
    // No title and no description: `handlers/main.py:57-58` passes only the body
    // and the host, so both tags fall back to their defaults.
    render_base(env, &main, "", "", config)
}

/// `generate_error`'s rendering half (`utils/error.py:38-50`).
///
/// `error_msg` and `title` are already resolved — the caller owns the
/// [`ERROR_MSG_LIST`](crate::page::ERROR_MSG_LIST) choice, because that choice is
/// random and the gate must not be.
///
/// The transponder code is the one piece of the page that is *meant* to differ
/// between two renders of the same input: it is a correlation token that also
/// goes to Telegram. A comparison has to normalise it out.
pub fn render_error(
    env: &Environment<'_>,
    error_msg: &str,
    transponder_code: &str,
    title: &str,
    config: &PageConfig,
) -> Result<String, Error> {
    let body = env.get_template("error.html")?.render(json!({
        "error_msg": error_msg,
        "transponder_code": transponder_code,
    }))?;
    // No description, matching `utils/error.py:44-49` — the tag falls back.
    render_base(env, &body, title, "", config)
}

/// `ERROR_MSG_LIST` (`utils/error.py:14-30`), verbatim, emoji included.
///
/// Kept here rather than in the server so that the two error paths — the handler
/// and the timeout layer — cannot pick from different lists.
pub const ERROR_MSG_LIST: [&str; 15] = [
    "Oops! 🙈 Looks like we stumbled into a little problem!",
    "Sorry to hear that, but our problem factory is working overtime! 😅",
    "Oh no! 😱 We've cooked up some problems again!",
    "Whoops! Did someone order a problem? 🍕",
    "Yikes! 🤪 We've hit a snag bigger than my coffee addiction!",
    "Uh oh! 🚨 We've brewed a fresh pot of problems!",
    "Hold tight! 🎢 Our problem rollercoaster has just begun!",
    "Alert! 📢 We've encountered a wild problem in its natural habitat!",
    "Bummer! 😜 We've tripped over a problem cord!",
    "Oh dear! 🐻 Looks like we've poked the problem bear!",
    "Guess what? 🤔 We've got a problem, but we're smiling through it!",
    "Surprise! 🎉 We found a problem you didn't even know you needed!",
    "Heads up! 🙆‍♂️ We're dancing with a few problems today!",
    "Sorry to hear that, but it's just another manic problem day! 🎶",
    "Well, well, well... if it isn't another problem joining the party! 🥳",
];

/// `title = "Opppps.."` (`utils/error.py:35`).
pub const DEFAULT_ERROR_TITLE: &str = "Opppps..";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::environment;
    use medium_doc::ir::{Document, PostMeta};

    /// A metadata value with every field populated, so a test that cares about
    /// one of them does not have to read the other twelve.
    fn metadata() -> PostMetadata {
        PostMetadata {
            post_id: "0291df856c77".to_string(),
            title: "My &lt;b&gt;Post&lt;/b&gt;".to_string(),
            subtitle: "a subtitle".to_string(),
            description: "a &lt;description&gt;".to_string(),
            url: "https://medium.com/@ada/my-post-0291df856c77".to_string(),
            creator: json!({ "name": "Ada", "username": "ada", "bio": "b", "imageId": "i.png" }),
            // `avatar` is not decoration: `post.html:55` and `homepage.html:24`
            // read `collection.avatar.id`, and a collection *without* that key
            // raises on both sides — see `a_missing_avatar_raises_like_jinja2`.
            collection: json!({ "name": "Coll", "slug": "coll", "avatar": { "id": "a.png" } }),
            reading_time: 5,
            free_access: "Yes",
            updated_at: "August 14, 2023".to_string(),
            first_published_at: "August 1, 2023".to_string(),
            preview_image_id: "1*abc.png".to_string(),
            tags: vec![json!({ "displayTitle": "Rust", "normalizedTagSlug": "rust" })],
        }
    }

    /// A document with no blocks. `render_post` only cares about the body here,
    /// and every other test in this module renders a template directly.
    fn empty_document() -> Document {
        Document {
            meta: PostMeta::default(),
            blocks: Vec::new(),
        }
    }

    fn body() -> Vec<String> {
        vec![
            "<p>First paragraph.</p>".to_string(),
            "<pre><code>fn main() {}</code></pre>".to_string(),
        ]
    }

    /// **The assertion the whole module exists for.**
    ///
    /// minijinja auto-escapes `.html` templates; Jinja2's bare `Environment()`
    /// does not. If the callback in [`crate::templates`] is ever removed, or
    /// installed after the templates are loaded, the body of every page turns
    /// into visible markup — and this fails rather than the deploy.
    #[test]
    fn autoescape_is_off_in_every_template() {
        let env = environment();
        let config = PageConfig::default();

        // The article body reaches `post.html` through `{{ paragraph }}` inside a
        // `{% for %}`. `post_context` and the template are called directly here
        // because the point is the splice, not the block rendering `html.rs`
        // already covers.
        let m = metadata();
        let body_html = env
            .get_template("post.html")
            .unwrap()
            .render(post_context(&m, &body()))
            .unwrap();
        assert!(
            body_html.contains("<p>First paragraph.</p>"),
            "the article body must be spliced in as HTML, not escaped"
        );
        assert!(!body_html.contains("&lt;p&gt;"), "the body was escaped");
        // `title` is the one field that reaches a template pre-escaped, so the
        // entity must survive *unchanged* — a second escape would give
        // `&amp;lt;b&amp;gt;`.
        assert!(body_html.contains("My &lt;b&gt;Post&lt;/b&gt;"));
        assert!(!body_html.contains("&amp;lt;"));

        // The same for `body_template` in base.html, which is the other place a
        // page is spliced into a page.
        let page = render_base(&env, &body_html, "t", "d", &config).unwrap();
        assert!(page.contains("<p>First paragraph.</p>"));
        assert!(page.contains("<pre><code>fn main() {}</code></pre>"));

        let homepage = render_homepage(&env, &[metadata()]).unwrap();
        assert!(homepage.contains("<div class=\"grid w-full"));
        assert!(!homepage.contains("&lt;div class="));
        assert!(
            render_index(&env, &[metadata()], &config)
                .unwrap()
                .contains("<div class=\"grid")
        );
    }

    /// `post.html` is rendered by `MediumParser`, whose Jinja environment has no
    /// `enable_ads_header` — so the ad spacer at the top of the article is
    /// **always** absent, however the banner is configured. `base.html` is a
    /// different story and does render it.
    #[test]
    fn the_ad_spacer_and_the_ad_banner_are_not_the_same_flag() {
        let env = environment();
        let with_ads = PageConfig::default().with_ads_header(true);

        let page = render_post(&env, &empty_document(), &metadata(), &with_ads).unwrap();
        assert!(
            !page.html.contains("<div class=\"pt-8\"></div>"),
            "the post body must never carry the spacer — see the module docs"
        );
        assert!(
            page.html
                .contains("Advertise here and support our project!"),
            "but base.html must carry the banner when the flag is on"
        );

        let off =
            render_post(&env, &empty_document(), &metadata(), &PageConfig::default()).unwrap();
        assert!(!off.html.contains("Advertise here and support our project!"));
    }

    /// `base.html` has `{% if creator %}` around `<meta name="author">`, and no
    /// caller ever passes `creator`. Faithful, and worth failing loudly if
    /// someone "fixes" it on one side only.
    #[test]
    fn the_author_meta_tag_is_never_emitted() {
        let env = environment();
        let page =
            render_post(&env, &empty_document(), &metadata(), &PageConfig::default()).unwrap();
        assert!(!page.html.contains("name=\"author\""));
    }

    /// The `<title>` is the *page* title, not the article's: `handlers/post.py`
    /// passes `RenderedPost.title` straight into `base.html`.
    #[test]
    fn the_title_tag_is_the_page_title() {
        let env = environment();
        let page =
            render_post(&env, &empty_document(), &metadata(), &PageConfig::default()).unwrap();

        assert_eq!(page.title, "My &lt;b&gt;Post&lt;/b&gt; | by Ada | in Coll");
        assert!(
            page.html.contains(
                "<title>My &lt;b&gt;Post&lt;/b&gt; | by Ada | in Coll - Freedium</title>"
            )
        );
    }

    /// `{{ title or "Breaking Medium paywall!" }}` — the homepage and the error
    /// page pass no title, so both fall back.
    #[test]
    fn an_absent_title_and_description_fall_back_to_the_defaults() {
        let env = environment();
        let page = render_index(&env, &[], &PageConfig::default()).unwrap();
        assert!(page.contains("<title>Breaking Medium paywall! - Freedium</title>"));
        assert!(page.contains("content=\"Your paywall breakthrough for Medium!\""));

        let error = render_error(
            &env,
            "boom",
            "code",
            DEFAULT_ERROR_TITLE,
            &PageConfig::default(),
        )
        .unwrap();
        // The error page passes a *title* but no description, so exactly one of
        // the two falls back.
        assert!(error.contains("<title>Opppps.. - Freedium</title>"));
        assert!(error.contains("content=\"Your paywall breakthrough for Medium!\""));
    }

    /// The legacy renders `main.html` twice — once with `DebugUndefined` to leave
    /// `{{ postleter }}` intact, then again with the homepage. This does it in one
    /// pass, and the assertion is that the placeholder is *gone* and the homepage
    /// is not escaped.
    #[test]
    fn main_html_is_substituted_in_one_pass() {
        let env = environment();
        let page = render_index(&env, &[metadata()], &PageConfig::default()).unwrap();

        assert!(
            !page.contains("postleter"),
            "the placeholder leaked through"
        );
        assert!(
            !page.contains("{{"),
            "an unresolved expression leaked through"
        );
        assert!(page.contains("Freedium: Your paywall breakthrough for Medium!"));
        assert!(
            page.contains("Enter Medium post link"),
            "url_box.html was not included"
        );
        assert!(
            page.contains("<div class=\"grid w-full"),
            "the homepage was escaped"
        );
    }

    /// `homepage.html` reads the *snake_case* keys `generate_metadata` produces —
    /// `reading_time`, `preview_image_id`, `first_published_at` — not the
    /// camelCase ones `post.html` uses. Getting the case wrong renders silently
    /// empty fields, which is exactly the bug a byte-level gate would report as
    /// a large diff with no obvious cause.
    #[test]
    fn homepage_rows_use_the_snake_case_keys() {
        let env = environment();
        let page = render_homepage(&env, &[metadata()]).unwrap();

        assert!(page.contains("~5 min read"));
        assert!(page.contains("Free: Yes"));
        assert!(page.contains("August 1, 2023 (Updated: August 14, 2023)"));
        assert!(page.contains("resize:fit:700/1*abc.png"));
        assert!(page.contains("href=\"/0291df856c77\""));
        assert!(
            page.contains("a &lt;description&gt;"),
            "the description is pre-escaped"
        );
        assert!(!page.contains("{{"));
    }

    /// A locked post reports `No`, and the field the template reads is
    /// `free_access` — the inverted name is the easy thing to get backwards.
    #[test]
    fn a_locked_post_reports_not_free() {
        let env = environment();
        let locked = PostMetadata {
            free_access: "No",
            ..metadata()
        };
        assert!(
            render_homepage(&env, &[locked])
                .unwrap()
                .contains("Free: No")
        );

        let page = env
            .get_template("post.html")
            .unwrap()
            .render(post_context(
                &PostMetadata {
                    free_access: "No",
                    ..metadata()
                },
                &[],
            ))
            .unwrap();
        assert!(page.contains("Free: No"));
    }

    /// `render_post` hands back the three values `HtmlResult` carries, in the
    /// field order `RenderedPost` pins.
    #[test]
    fn render_post_packages_the_cacheable_fields() {
        let env = environment();
        let m = metadata();
        let page = render_post(&env, &empty_document(), &m, &PageConfig::default()).unwrap();

        assert_eq!(page.title, m.page_title());
        assert_eq!(page.description, m.description);
        assert_eq!(page.url, m.url);
        assert!(page.html.contains(&format!("href=\"{}#bypass\"", m.url)));
        // The body itself is empty here — `empty_document()` has no blocks — so
        // the splice is asserted in `autoescape_is_off_in_every_template`.
    }

    /// The error page's two placeholders, and the transponder code that makes it
    /// byte-unstable across renders. Anything comparing two error pages has to
    /// normalise that code out; this pins that it is there to normalise.
    #[test]
    fn the_error_page_carries_the_message_and_the_transponder_code() {
        let env = environment();
        let page = render_error(
            &env,
            "Oops! 🙈 Looks like we stumbled into a little problem!",
            "steady-violet-anchor",
            DEFAULT_ERROR_TITLE,
            &PageConfig::default(),
        )
        .unwrap();

        assert!(page.contains("Oops! 🙈 Looks like we stumbled into a little problem!"));
        assert!(page.contains("Your emergency transponder code: steady-violet-anchor"));
        assert!(page.contains("We are aware of this error."));
        assert!(!page.contains("{{"));
    }

    /// The list is the error page's whole visible payload and is copied verbatim
    /// from `utils/error.py`; a dropped emoji would be invisible in review.
    #[test]
    fn the_error_message_list_matches_the_legacy() {
        assert_eq!(ERROR_MSG_LIST.len(), 15);
        assert_eq!(
            ERROR_MSG_LIST[0],
            "Oops! 🙈 Looks like we stumbled into a little problem!"
        );
        assert_eq!(
            ERROR_MSG_LIST[14],
            "Well, well, well... if it isn't another problem joining the party! 🥳"
        );
        assert!(
            ERROR_MSG_LIST.iter().all(|message| !message.is_empty()),
            "a blank entry would render an empty error page"
        );
    }

    /// `host_address` is interpolated into `base.html`'s inline script as the
    /// base for `/@miro/v2/`. It is the only thing [`PageConfig`] changes on a
    /// post page.
    #[test]
    fn the_host_address_reaches_the_page_script() {
        let env = environment();
        let config = PageConfig::new("http://localhost:7080");

        let page = render_post(&env, &empty_document(), &metadata(), &config).unwrap();
        assert!(page.html.contains("http://localhost:7080/@miro/v2/"));
        assert!(!page.html.contains("https://freedium.cfd/@miro/v2/"));

        assert_eq!(
            PageConfig::default().host_address,
            DEFAULT_HOST_ADDRESS,
            "the default must be `config.py:5`'s"
        );
        assert!(!PageConfig::default().enable_ads_header);
    }

    /// Every template must be present and loadable. `minijinja-embed` fails the
    /// *build* on a syntax error, so this only catches a missing file or a glob
    /// that stopped matching — which is worth knowing before the first request.
    #[test]
    fn every_template_is_embedded() {
        let env = environment();
        for name in [
            "base.html",
            "error.html",
            "homepage.html",
            "main.html",
            "post.html",
            "url_box.html",
        ] {
            assert!(env.get_template(name).is_ok(), "{name} is not embedded");
        }
    }

    /// **The invariant behind the one place minijinja and Jinja2 still differ.**
    ///
    /// Both templates loop over a context value — `{% for tag in tags %}` and
    /// `{% for post in post_list %}`. Handed an explicit `null`, Jinja2 raises
    /// `TypeError: 'NoneType' object is not iterable` while minijinja's `Lenient`
    /// iterates it as empty. That is a real divergence and it is not fixable by
    /// an `UndefinedBehavior` setting, because the value is `none`, not
    /// `undefined`.
    ///
    /// It is also unreachable: both keys are built from `Vec`s in this module, so
    /// they are arrays whatever the payload said. This test is what keeps that
    /// true — if someone changes `tags` to `Option<Vec<_>>` and passes the
    /// `None` through, the pages stop being comparable and this says so.
    #[test]
    fn the_looped_context_keys_are_always_arrays() {
        let empty = PostMetadata {
            creator: Value::Null,
            collection: Value::Null,
            tags: Vec::new(),
            ..metadata()
        };

        let context = post_context(&empty, &[]);
        assert!(
            context["tags"].is_array(),
            "`tags` must be an array, never null"
        );
        assert!(
            context["content"].is_array(),
            "`content` must be an array, never null"
        );

        let env = environment();

        // `{% for post in post_list %}` lives in `homepage.html`, and the value
        // is a slice we serialise here — an array by construction, so the loop
        // can never see `null`. Rendering it anyway proves the empty list and
        // the tagless row both get through the rest of the template rather than
        // only through the loop.
        assert!(render_homepage(&env, &[]).is_ok(), "an empty list is fine");
        assert!(render_homepage(&env, std::slice::from_ref(&empty)).is_ok());

        // The tag chips are `post.html`'s, not the homepage's: with no tags the
        // loop body must simply not appear.
        let body = env
            .get_template("post.html")
            .unwrap()
            .render(post_context(&empty, &[]))
            .unwrap();
        assert!(!body.contains("medium.com/tag/"));
    }

    /// A collection without an `avatar` key is a **500 on both sides**, and the
    /// point of this test is that Rust raises too rather than papering over it.
    ///
    /// This is the case that motivated `UndefinedBehavior::Lenient`.
    /// `Chainable` — which also renders every other case identically — would
    /// return an empty string here and serve a page where production returns an
    /// error. On a payload Python cannot serve, "Rust works" is a divergence, not
    /// an improvement, and the Fase 4 mirror would report it as one.
    #[test]
    fn a_missing_avatar_raises_like_jinja2() {
        let env = environment();
        let broken = PostMetadata {
            collection: json!({ "name": "Coll", "slug": "coll" }),
            ..metadata()
        };

        let error = env
            .get_template("post.html")
            .unwrap()
            .render(post_context(&broken, &[]))
            .unwrap_err();
        assert_eq!(
            error.kind(),
            minijinja::ErrorKind::UndefinedError,
            "Jinja2 raises `UndefinedError: 'dict object' has no attribute \
             'avatar'` here; minijinja must raise, not render"
        );

        // Same key, present but null. `collection.avatar` is then `none` rather
        // than undefined, the chain resolves, and both sides render — so this is
        // the shape a fixture should use when it wants to exercise a collection
        // that has no picture.
        let no_picture = PostMetadata {
            collection: json!({ "name": "Coll", "slug": "coll", "avatar": Value::Null }),
            ..metadata()
        };
        assert!(
            env.get_template("post.html")
                .unwrap()
                .render(post_context(&no_picture, &[]))
                .is_ok()
        );

        // `homepage.html:24` reads the same chain, so the homepage row has the
        // same behaviour on the same payload.
        assert!(render_homepage(&env, &[broken]).is_err());
    }
}
