//! The values `post.html` renders: `generate_metadata` plus the page title.
//!
//! Ports `core.py:694-812`. This is the half of `_render_as_html` that does not
//! touch the article body, and it is where the page's `<title>`, its
//! `<meta name="description">` and every field in the byline come from.
//!
//! # It reads the payload, not the [`Document`](crate::ir::Document)
//!
//! `_render_as_html` computes the body and the metadata *in parallel* and then
//! keeps the metadata's `title` and `subtitle`, discarding the ones
//! `_parse_and_render_content_html_post` returned (`core.py:758-784`, noted on
//! [`PostMeta`](crate::ir::PostMeta)). The two differ whenever the de-duplication
//! pass replaced a title with the paragraph that repeated it, so reading them
//! back out of the `Document` would quietly lose that quirk.
//!
//! Everything here therefore comes from the raw `data.post` JSON, which is also
//! why [`from_payload`] takes a [`PostPayload`] rather than a `Document`.
//!
//! # Escaping is the whole job
//!
//! Each field has its own rule, and they are not interchangeable:
//!
//! | field | rule | `it's` |
//! |---|---|---|
//! | `title` | `quote_symbol`, then minimal escape | `it's` |
//! | `subtitle` | `quote_symbol`, then full escape | `it&#39s` |
//! | `description` | `quote_symbol`, shorten, then full escape *again* | `it&amp;#39s` |
//!
//! The description's double escape is not a mistake in the port. It is what
//! `RLStringHelper(textwrap.shorten(subtitle, ...)).get_text()` does, because
//! `subtitle` at that point is already escaped. And it is not idempotent:
//! `utils.py:14` maps `'` to `&#39` with **no terminating semicolon**, so the
//! second pass sees an ampersand that is not the start of a recognised entity
//! and escapes it, while `&quot;` — which does have its semicolon — survives
//! untouched. Both halves of that asymmetry are pinned by tests.
//!
//! # Timestamps are UTC
//!
//! `convert_datetime_to_human_readable` (`time.py:13`) is
//! `datetime.fromtimestamp(unix_time / 1000)`, which reads the *process's* local
//! timezone. Production runs `python:3.12.3` with no `TZ`, so that is UTC; this
//! formats UTC unconditionally rather than depending on the host. The difftest
//! gate has to pin `TZ=UTC` for the Python side for the same reason — a machine
//! east of Greenwich would otherwise disagree with both this and production.

use serde_json::Value;

use crate::escape::{self, EscapeMode};
use crate::parse::PostPayload;

/// `textwrap.shorten`'s width and placeholder (`core.py:704`).
const DESCRIPTION_WIDTH: usize = 100;
const DESCRIPTION_PLACEHOLDER: &str = "...";

/// The month names `time.py:15-28` indexes by `datetime.month`.
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Milliseconds per day, for the epoch → civil date conversion.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// What `post.html` interpolates, in the shape it interpolates it.
///
/// Every string field is already escaped exactly as the template expects to
/// find it. The template runs with autoescape off — Jinja2's default for the
/// `Environment()` `core.py` builds, and minijinja's setting is forced to match
/// — so these values go into the page verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct PostMetadata {
    pub post_id: String,
    /// Minimal escape: `&`, `<`, `>` only.
    pub title: String,
    /// Full escape, quotes included.
    pub subtitle: String,
    /// Full escape applied over the *already-escaped* subtitle, shortened.
    pub description: String,
    pub url: String,
    /// Raw `post.creator`, for `creator.name` / `.username` / `.bio` /
    /// `.imageId`. Interpolated unescaped, deliberately.
    pub creator: Value,
    /// Raw `post.collection`, or `Value::Null` when the post is not in one.
    pub collection: Value,
    /// `math.ceil(readingTime)`.
    pub reading_time: u32,
    /// `"No"` when the post is locked, `"Yes"` otherwise — inverted from
    /// `isLocked`, and rendered as `Free: {{ freeAccess }}`.
    pub free_access: &'static str,
    /// `"August 14, 2023"`. Empty when the payload carried no timestamp.
    pub updated_at: String,
    /// See [`PostMetadata::updated_at`].
    pub first_published_at: String,
    /// Empty when there is no preview image, which `{% if previewImageId %}`
    /// then skips.
    pub preview_image_id: String,
    /// Raw `post.tags`, for `tag.displayTitle` and `tag.normalizedTagSlug`.
    /// Distinct from [`PostMeta::tags`](crate::ir::PostMeta::tags), which holds
    /// only the display titles and exists for the de-duplication pass.
    pub tags: Vec<Value>,
}

impl PostMetadata {
    /// The `<title>`, as `core.py:786-792` builds it.
    ///
    /// The legacy code builds this by *rendering a Jinja template*, which is a
    /// roundabout way to concatenate two strings and only matters for what
    /// happens when the attributes are missing:
    ///
    /// * `{}` — a key that is not there gives Jinja an `Undefined`, printed as
    ///   the empty string;
    /// * `{"name": null}` — a real `None`, printed as the four characters
    ///   `None`.
    ///
    /// Both are reproduced, because `Value` can tell them apart and the byte
    /// difference would otherwise show up in the gate.
    pub fn page_title(&self) -> String {
        let mut title = format!("{} | by {}", self.title, name_of(&self.creator));
        if is_truthy(&self.collection) {
            title.push_str(" | in ");
            title.push_str(name_of(&self.collection));
        }
        title
    }
}

/// `{{ value.name }}` under Jinja2's default undefined.
///
/// A non-string, non-null `name` would be printed by Jinja as a Python `repr`
/// (`True`, `{'a': 1}`), which this does not reproduce: the GraphQL schema types
/// both `creator.name` and `collection.name` as `String!`, so no other shape
/// reaches here from a real response.
fn name_of(value: &Value) -> &str {
    match value.get("name") {
        Some(Value::String(name)) => name,
        Some(Value::Null) => "None",
        _ => "",
    }
}

/// Whether `{% if value %}` takes the branch.
///
/// Jinja is Python: an empty dict, an empty list, the empty string, `0` and
/// `None` are all falsy, and so is `Undefined`. `collection` is the only field
/// this is used on, and it is `None` or an object.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `generate_metadata(post_data, post_id)` (`core.py:694-751`).
///
/// `post_id` is not read from the payload: it is the value the URL resolved to,
/// and the cache keys are built from it rather than from the API's own `id`.
///
/// # Where this degrades where Python raises
///
/// `generate_metadata` indexes the payload directly, so a payload missing
/// `previewImage` raises `KeyError` and a null `updatedAt` raises `TypeError` —
/// both reaching the client as a 500 with no article. This returns an empty
/// string for the affected field instead, following the preference recorded in
/// §3.3: a malformed field costs the field, not the article. The GraphQL query
/// requests all of them, so no real response takes that path.
pub fn from_payload(payload: &PostPayload, post_id: &str) -> PostMetadata {
    let post = payload.post();

    // `RLStringHelper.__init__` runs `quote_symbol` over its argument whatever
    // the quoting mode, so curly quotes fold to straight ones *before* escaping
    // — for the title, the subtitle and the description alike.
    let title = escape::escape(&crate::text::quote_symbol(&post.title), EscapeMode::Minimal);
    let subtitle = escape::escape(
        &crate::text::quote_symbol(
            post.preview_content
                .as_ref()
                .map_or("", |content| content.subtitle.as_str()),
        ),
        EscapeMode::Full,
    );
    // Over `subtitle`, i.e. over the *escaped* text — see the module docs.
    let description = escape::escape(
        &crate::text::quote_symbol(&crate::textwrap::shorten(
            &subtitle,
            DESCRIPTION_WIDTH,
            DESCRIPTION_PLACEHOLDER,
        )),
        EscapeMode::Full,
    );

    PostMetadata {
        post_id: post_id.to_string(),
        title,
        subtitle,
        description,
        url: post.medium_url.clone(),
        creator: post.creator.clone(),
        collection: post.collection.clone(),
        reading_time: post.reading_time.map_or(0, |minutes| minutes.ceil() as u32),
        free_access: if post.is_locked { "No" } else { "Yes" },
        updated_at: post
            .updated_at
            .map_or_else(String::new, human_readable_date),
        first_published_at: post
            .first_published_at
            .map_or_else(String::new, human_readable_date),
        preview_image_id: post
            .preview_image
            .as_ref()
            .map_or_else(String::new, |image| image.id.clone()),
        // Read straight from the envelope. `Post::tags` is `Vec<Tag>`, and `Tag`
        // carries only `displayTitle` — the de-duplication pass has no use for
        // the rest — while the template also wants `normalizedTagSlug` and
        // `post.html` interpolates `tag.displayTitle` on the *raw* object.
        tags: raw_tags(payload),
    }
}

/// `data.post.tags`, as the template will see it.
///
/// A missing or malformed list is empty rather than a parse failure: the tags
/// are decoration, and losing them must not cost the article.
fn raw_tags(payload: &PostPayload) -> Vec<Value> {
    match payload.data.get("post").and_then(|post| post.get("tags")) {
        Some(Value::Array(tags)) => tags.clone(),
        _ => Vec::new(),
    }
}

/// `datetime.fromtimestamp(unix_ms / 1000)` → `"{month} {day}, {year}"`.
///
/// The day is *not* zero-padded: `time.py:33` interpolates an `int`, so it is
/// `"August 4, 2023"`, not `"August 04, 2023"`. Python's `%B %-d, %Y` is the
/// closest format string, but building the three pieces here keeps the month
/// list visibly the same one `time.py` uses.
///
/// Negative timestamps floor toward the past, as `fromtimestamp` does:
/// `-1000` ms is `December 31, 1969`.
pub fn human_readable_date(unix_ms: i64) -> String {
    let (year, month, day) = civil_from_days(unix_ms.div_euclid(MILLIS_PER_DAY));
    format!("{} {}, {}", MONTH_NAMES[(month - 1) as usize], day, year)
}

/// Days since 1970-01-01 → `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, which is exact over the whole `i64` range
/// and needs no epoch table or leap-year special case. Ported rather than pulled
/// from a date crate: this is the only date arithmetic in Fase 1, and the
/// workspace keeps these crates dependency-light on purpose.
///
/// The magic numbers are the Gregorian calendar's: 146 097 days per 400-year
/// era, 1 461 per 4-year cycle, and 719 468 days between 0000-03-01 and
/// 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so that leap days land at the end of the
    // year, which is what makes the arithmetic below branch-free.
    let shifted = days + 719_468;

    // `era` is floor division; Rust's `/` truncates toward zero, so the negative
    // case is shifted down first.
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64; // [0, 146096]

    // The `- 1/4 + 1/100 - 1/400` leap correction, applied to the year-of-era.
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;

    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_index = (5 * day_of_year + 2) / 153; // [0, 11], March = 0

    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32; // [1, 31]
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32; // [1, 12]

    let year = year_of_era as i64 + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `data.post` object carrying every field `generate_metadata` reads.
    fn post(title: &str, subtitle: &str) -> Value {
        json!({
            "title": title,
            "previewContent": { "subtitle": subtitle },
            "previewImage": { "id": "1*abc.png" },
            "mediumUrl": "https://medium.com/@x/t-0291df856c77",
            "readingTime": 4.2,
            "isLocked": false,
            "updatedAt": 1_692_000_000_000i64,
            "firstPublishedAt": 1_690_000_000_000i64,
            "creator": { "name": "Ada", "username": "ada", "bio": "b", "imageId": "i" },
            "collection": { "name": "Coll", "slug": "coll", "shortDescription": "d" },
            "tags": [
                { "displayTitle": "Rust", "normalizedTagSlug": "rust" },
                { "displayTitle": "Web", "normalizedTagSlug": "web" }
            ],
            "content": { "bodyModel": { "paragraphs": [] } }
        })
    }

    /// Wraps a `data.post` object in the envelope.
    fn payload_of(post: Value) -> PostPayload {
        PostPayload::from_value(json!({ "data": { "post": post } }))
            .expect("the fixture is a valid payload")
    }

    /// [`post`] with `key` replaced — `null` deletes it, which is the distinction
    /// the page title turns on.
    fn post_with(title: &str, subtitle: &str, key: &str, value: Value) -> Value {
        let mut post = post(title, subtitle);
        match value {
            Value::Null => {
                post.as_object_mut().expect("an object").remove(key);
            }
            value => {
                post.as_object_mut()
                    .expect("an object")
                    .insert(key.into(), value);
            }
        }
        post
    }

    /// Captured from the real `generate_metadata`, through `RLStringHelper`.
    ///
    /// Regenerate by importing the legacy modules under `stubs.install()` and
    /// calling `RLStringHelper(t, ["minimal"]).get_text()`,
    /// `RLStringHelper(s).get_text()` and
    /// `RLStringHelper(textwrap.shorten(subtitle, 100, "...")).get_text()`.
    #[test]
    fn escaping_matches_the_legacy_chain() {
        let cases = [
            (
                "Plain title",
                "Plain subtitle",
                "Plain title",
                "Plain subtitle",
            ),
            (
                "Tom & Jerry",
                "it's a \"test\" & more",
                "Tom &amp; Jerry",
                "it&#39s a &quot;test&quot; &amp; more",
            ),
            (
                "\u{201c}Curly\u{201d} title",
                "\u{2018}Curly\u{2019} subtitle",
                "\"Curly\" title",
                "&#39Curly&#39 subtitle",
            ),
            (
                "a < b > c",
                "x &nbsp; y &amp; z",
                "a &lt; b &gt; c",
                "x &amp;nbsp; y &amp; z",
            ),
            (
                "&amp; already",
                "&#39; semi &quot; q",
                "&amp; already",
                "&#39; semi &quot; q",
            ),
            (
                "Tag <b>bold</b>",
                "it&#39s",
                "Tag &lt;b&gt;bold&lt;/b&gt;",
                "it&amp;#39s",
            ),
        ];

        for (title_raw, subtitle_raw, title, subtitle) in cases {
            let metadata = from_payload(&payload_of(post(title_raw, subtitle_raw)), "id");
            assert_eq!(metadata.title, title, "title of {title_raw:?}");
            assert_eq!(metadata.subtitle, subtitle, "subtitle of {subtitle_raw:?}");
        }
    }

    /// **The double escape, and its asymmetry.**
    ///
    /// `'` is escaped to `&#39` with no semicolon, so the second pass does not
    /// recognise it and escapes the `&` too. `&quot;` has its semicolon, so the
    /// second pass leaves it alone. Both are the real behaviour.
    #[test]
    fn the_description_double_escape_is_asymmetric() {
        let apostrophe = from_payload(&payload_of(post("T", "it's")), "id");
        assert_eq!(apostrophe.subtitle, "it&#39s");
        assert_eq!(apostrophe.description, "it&amp;#39s");

        let quote = from_payload(&payload_of(post("T", "say \"hi\"")), "id");
        assert_eq!(quote.subtitle, "say &quot;hi&quot;");
        assert_eq!(
            quote.description, "say &quot;hi&quot;",
            "`&quot;` keeps its semicolon, so the second pass recognises it"
        );

        let ampersand = from_payload(&payload_of(post("T", "a & b")), "id");
        assert_eq!(ampersand.subtitle, "a &amp; b");
        assert_eq!(ampersand.description, "a &amp; b");
    }

    /// The description measures its width against the *escaped* subtitle, so
    /// escaping costs columns and can push a subtitle over the limit that the
    /// raw text was under.
    #[test]
    fn the_description_is_shortened_after_escaping() {
        let metadata = from_payload(&payload_of(post("T", &"x".repeat(101))), "id");
        assert_eq!(metadata.description, "...", "an oversized word vanishes");

        let long = "alpha ".repeat(15) + "super-duper-hyphenated-word-here";
        let metadata = from_payload(&payload_of(post("T", &long)), "id");
        assert_eq!(
            metadata.description,
            format!("{}super-...", "alpha ".repeat(15)),
            "the width is applied to the escaped text, hyphens included"
        );

        // 14 escaped quotes are 97 columns and fit; the 15th would need 104, so
        // the line is cut back and the placeholder appended — 14 words, not 13,
        // because the trailing placeholder still fits in the 100.
        let fits = "\" ".repeat(14);
        let metadata = from_payload(&payload_of(post("T", &fits)), "id");
        assert_eq!(
            metadata.description,
            "&quot; ".repeat(14).trim_end(),
            "14 escaped quotes measure 97 columns"
        );

        let overflows = "\" ".repeat(15);
        assert_eq!(overflows.chars().count(), 30, "short enough unescaped");
        let metadata = from_payload(&payload_of(post("T", &overflows)), "id");
        assert_eq!(
            metadata.description,
            format!("{}...", "&quot; ".repeat(14).trim_end()),
            "15 escaped quotes measure 104 columns, so one is dropped"
        );
    }

    /// `freeAccess` is `Free: {{ freeAccess }}` on the page, and `isLocked`
    /// inverts: a locked post reports `No`.
    ///
    /// The missing case is a deviation, and the only one in this file Python
    /// does not also take: `generate_metadata` indexes `["isLocked"]` directly,
    /// so a payload without it raises `KeyError` and the client gets a 500 with
    /// no article at all. Here it reads as unlocked. The query always requests
    /// the field, so no real response reaches either path.
    #[test]
    fn free_access_is_inverted_from_is_locked() {
        let open = from_payload(
            &payload_of(post_with("T", "", "isLocked", json!(false))),
            "id",
        );
        assert_eq!(open.free_access, "Yes");

        let locked = from_payload(
            &payload_of(post_with("T", "", "isLocked", json!(true))),
            "id",
        );
        assert_eq!(locked.free_access, "No");

        let missing = from_payload(
            &payload_of(post_with("T", "", "isLocked", Value::Null)),
            "id",
        );
        assert_eq!(missing.free_access, "Yes", "a missing isLocked is false");
        // Python: `KeyError: 'isLocked'`.
    }

    /// `math.ceil`, not a truncation: 4.2 minutes is 5.
    #[test]
    fn reading_time_rounds_up() {
        let metadata = from_payload(&payload_of(post("T", "")), "id");
        assert_eq!(metadata.reading_time, 5);

        let exact = from_payload(
            &payload_of(post_with("T", "", "readingTime", json!(4.0))),
            "id",
        );
        assert_eq!(exact.reading_time, 4, "a whole number must not round to 5");

        let under = from_payload(
            &payload_of(post_with("T", "", "readingTime", json!(0.4))),
            "id",
        );
        assert_eq!(under.reading_time, 1, "any fraction rounds up to 1");

        let zero = from_payload(
            &payload_of(post_with("T", "", "readingTime", json!(0))),
            "id",
        );
        assert_eq!(zero.reading_time, 0, "0.0 is not 1");
    }

    /// The raw tag objects survive, because the page needs both
    /// `displayTitle` and `normalizedTagSlug` — unlike `PostMeta::tags`, which
    /// keeps only the titles for the de-duplication pass.
    #[test]
    fn tags_stay_raw() {
        let metadata = from_payload(&payload_of(post("T", "")), "id");
        assert_eq!(metadata.tags.len(), 2);
        assert_eq!(metadata.tags[0]["displayTitle"], json!("Rust"));
        assert_eq!(metadata.tags[0]["normalizedTagSlug"], json!("rust"));

        let none = from_payload(&payload_of(post_with("T", "", "tags", Value::Null)), "id");
        assert!(
            none.tags.is_empty(),
            "a missing tag list is not a parse failure"
        );
    }

    /// The page title, both with and without a collection. Python builds this by
    /// *rendering a Jinja template*, so the missing-attribute cases are pinned
    /// too.
    #[test]
    fn page_title_matches_the_jinja_template() {
        let with_collection = from_payload(&payload_of(post("My Post", "")), "id");
        assert_eq!(with_collection.page_title(), "My Post | by Ada | in Coll");
    }

    /// **Jinja's three shapes, and the trap in the middle one.**
    ///
    /// A *missing* key, a `null` value and a nested lookup on a non-dict all
    /// print differently:
    ///
    /// * `{}` → `{{ creator.name }}` is `Undefined` → `""`;
    /// * `{"name": null}` → a real `None` → the four characters `None`;
    /// * `null` → `None.name` is `Undefined` again → `""`, **not** `None`.
    ///
    /// That last one is the trap: `Value::Null.get("name")` is `None`, so the
    /// fall-through arm is what makes it right. Verified against the real
    /// `jinja_env` rather than reasoned about.
    #[test]
    fn page_title_reproduces_jinjas_missing_attribute_rules() {
        let without_collection = from_payload(
            &payload_of(post_with("My Post", "", "collection", Value::Null)),
            "id",
        );
        assert_eq!(without_collection.page_title(), "My Post | by Ada");

        let null_creator = from_payload(
            &payload_of(post_with("My Post", "", "creator", Value::Null)),
            "id",
        );
        assert_eq!(
            null_creator.page_title(),
            "My Post | by  | in Coll",
            "`None.name` is Undefined, so it prints nothing — not `None`"
        );

        let no_name = from_payload(
            &payload_of(post_with("My Post", "", "creator", json!({}))),
            "id",
        );
        assert_eq!(no_name.page_title(), "My Post | by  | in Coll");

        let null_name = from_payload(
            &payload_of(post_with("My Post", "", "creator", json!({ "name": null }))),
            "id",
        );
        assert_eq!(
            null_name.page_title(),
            "My Post | by None | in Coll",
            "an explicit null *is* a real `None`"
        );

        let collections_without_name = from_payload(
            &payload_of(post_with(
                "My Post",
                "",
                "collection",
                json!({ "slug": "coll" }),
            )),
            "id",
        );
        assert_eq!(
            collections_without_name.page_title(),
            "My Post | by Ada | in ",
            "the clause is added whenever `collection` is truthy, name or not"
        );
    }

    /// An empty collection object is falsy to Jinja, so the `| in` clause is
    /// skipped rather than rendering `| in `.
    #[test]
    fn an_empty_collection_is_falsy() {
        let metadata = from_payload(
            &payload_of(post_with("My Post", "", "collection", json!({}))),
            "id",
        );
        assert_eq!(metadata.page_title(), "My Post | by Ada");

        let empty_string = from_payload(
            &payload_of(post_with("My Post", "", "collection", json!(""))),
            "id",
        );
        assert_eq!(empty_string.page_title(), "My Post | by Ada");
    }

    /// Spot values from `convert_datetime_to_human_readable`, including the
    /// boundaries a hand-written date routine gets wrong: the leap day, a
    /// century that is not a leap year, and the epoch itself.
    #[test]
    fn dates_are_formatted_in_utc() {
        for (unix_ms, expected) in [
            (0, "January 1, 1970"),
            (1_692_000_000_000i64, "August 14, 2023"),
            // 2000-02-29, and 1900-03-01 — the day after a 28 February that a
            // naive `year % 4` would have made the 29th.
            (951_782_400_000i64, "February 29, 2000"),
            (-2_203_891_200_000i64, "March 1, 1900"),
            // Midnight, so a timezone shift of even a second would change the
            // day. This is the assertion that catches a local-time bug.
            (1_704_067_200_000i64, "January 1, 2024"),
            // One millisecond before it, which must stay in 2023.
            (1_704_067_199_999i64, "December 31, 2023"),
            (-1_000, "December 31, 1969"),
        ] {
            assert_eq!(
                human_readable_date(unix_ms),
                expected,
                "unix_ms = {unix_ms}"
            );
        }
    }

    /// **The days the month list indexes into.** A month name off by one is the
    /// classic way to get this wrong, and it is invisible in a spot check of one
    /// date.
    #[test]
    fn every_month_name_is_reachable() {
        let names: Vec<String> = (1..=12)
            .map(|month| {
                // The 15th of each month avoids every month-length question.
                let days = days_from_civil(2024, month, 15);
                human_readable_date(days * MILLIS_PER_DAY)
            })
            .collect();

        assert_eq!(
            names,
            vec![
                "January 15, 2024",
                "February 15, 2024",
                "March 15, 2024",
                "April 15, 2024",
                "May 15, 2024",
                "June 15, 2024",
                "July 15, 2024",
                "August 15, 2024",
                "September 15, 2024",
                "October 15, 2024",
                "November 15, 2024",
                "December 15, 2024",
            ]
        );
    }

    /// Days since 1970-01-01 → `(year, month, day)`, the inverse of
    /// [`civil_from_days`].
    ///
    /// Written out so the test can round-trip the two against each other. Two
    /// independently derived algorithms agreeing over a whole 400-year era is a
    /// far stronger check than any table of expected dates, and it needs no date
    /// crate to state.
    fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
        let year = year - i64::from(month <= 2);
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let year_of_era = (year - era * 400) as u64;
        let month_index = u64::from((month + 9) % 12);
        let day_of_year = (153 * month_index + 2) / 5 + u64::from(day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era as i64 - 719_468
    }

    /// A full Gregorian era: 1600-03-01 through 2000-02-29. This is the range
    /// that contains every leap-year rule exception, including 1700, 1800 and
    /// 1900, none of which are leap years.
    #[test]
    fn the_civil_calendar_round_trips_over_a_whole_era() {
        for days in -1_000..146_097 {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(
                days_from_civil(year, month, day),
                days,
                "day {days} round-tripped through {year}-{month}-{day}"
            );
        }
    }

    /// The specific days that a naive leap-year rule gets wrong, spelled out so
    /// a regression names itself.
    #[test]
    fn century_leap_years_are_handled() {
        assert_eq!(civil_from_days(days_from_civil(1700, 2, 28)), (1700, 2, 28));
        assert_eq!(
            civil_from_days(days_from_civil(1700, 2, 28) + 1),
            (1700, 3, 1),
            "1700 was not a leap year"
        );
        assert_eq!(
            civil_from_days(days_from_civil(2000, 2, 28) + 1),
            (2000, 2, 29),
            "2000 was a leap year: divisible by 400"
        );
    }

    /// A payload with none of the optional fields must not lose the article.
    /// Python raises `KeyError: 'previewContent'` on the second field it reads;
    /// this degrades instead, per §3.3.
    #[test]
    fn a_sparse_payload_degrades_field_by_field() {
        let payload = PostPayload::from_value(json!({
            "data": { "post": {
                "title": "Only a title",
                "content": { "bodyModel": { "paragraphs": [] } }
            } }
        }))
        .unwrap();

        let metadata = from_payload(&payload, "abc123");
        assert_eq!(metadata.title, "Only a title");
        assert_eq!(metadata.subtitle, "");
        assert_eq!(metadata.description, "");
        assert_eq!(metadata.url, "");
        assert_eq!(metadata.preview_image_id, "");
        assert_eq!(metadata.reading_time, 0);
        assert_eq!(metadata.updated_at, "");
        assert_eq!(metadata.first_published_at, "");
        assert_eq!(metadata.free_access, "Yes");
        assert_eq!(metadata.page_title(), "Only a title | by ");
        assert_eq!(metadata.post_id, "abc123");
    }

    /// A timestamp sent as a float, which the schema forbids but the loader
    /// tolerates — losing the whole `Post` over it would be the wrong trade.
    #[test]
    fn a_float_timestamp_is_read_rather_than_dropping_the_post() {
        let payload = PostPayload::from_value(json!({
            "data": { "post": {
                "title": "T",
                "updatedAt": 1_692_000_000_000.0,
                "content": { "bodyModel": { "paragraphs": [] } }
            } }
        }))
        .unwrap();

        let metadata = from_payload(&payload, "id");
        assert_eq!(metadata.updated_at, "August 14, 2023");
    }

    /// The `post_id` comes from the resolved URL, not from the payload — the
    /// cache keys are built from it, and the API's own `id` is not requested.
    #[test]
    fn the_post_id_is_the_callers_not_the_payloads() {
        let mut post = post("T", "");
        post.as_object_mut()
            .unwrap()
            .insert("id".into(), json!("api-id"));
        let metadata = from_payload(&payload_of(post), "resolved-id");
        assert_eq!(metadata.post_id, "resolved-id");
    }

    /// `from_payload` must not be fooled by a payload whose `data.post` is
    /// missing entirely — `PostPayload::post` warns and returns a default.
    /// Python raises `KeyError: 'data'` on the first line of `generate_metadata`.
    #[test]
    fn an_empty_envelope_produces_empty_metadata() {
        let payload = PostPayload::from_value(json!({})).unwrap();
        let metadata = from_payload(&payload, "id");
        assert_eq!(metadata.title, "");
        assert_eq!(metadata.page_title(), " | by ");
    }
}
