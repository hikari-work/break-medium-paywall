//! The public JSON contract: what `/api/v1` promises to a machine.
//!
//! This crate is **types only**. It has no axum, no `Document`, no Redis and no
//! rendering — which is what lets `freedium-web` and `medium-render` both name
//! these types without either owning them, and what keeps the contract from
//! drifting towards whatever is convenient for the current handler.
//!
//! # Two gates, and they are not the same question
//!
//! - The `openapi` **feature** (build time, default **off**) decides whether the
//!   `utoipa::ToSchema` impls exist at all. A binary that never serves the spec
//!   pays no proc-macro cost and carries no schema code.
//! - `DISABLE_EXTERNAL_DOCS` (runtime, default **true**) decides whether this
//!   *process* serves it. See `freedium-web`'s `config`/`api::openapi`.
//!
//! One flag for a build-time concern would normally be over-engineering, but the
//! runtime flag already existed in `Config` and was documented as inert until
//! Fase 6. This crate makes it load-bearing rather than inventing it.
//!
//! # The contract does not expose Medium's vocabulary
//!
//! [`BlockDto`] and [`InlineDto`] are tagged with Freedium's own names
//! (`heading`, `paragraph`, `code`, …), never Medium's `"H2"`/`"P"`/`"PRE"`.
//! Those are Medium's internal names for its own storage format, they are not a
//! stable interface, and a consumer that switches on them inherits every rename
//! Medium makes. The same reasoning is why every string here is plain text and
//! never HTML-escaped: [`MetaDto::title`] and friends come from the raw
//! payload, **not** from `medium-doc`'s `PostMetadata`, whose strings are
//! pre-escaped for HTML and whose `description` is escaped twice by a legacy bug.
//!
//! # What is deliberately *not* here
//!
//! - No `serde_json` in `[dependencies]` — see `Cargo.toml`. This is the
//!   structural reason Medium's raw shape cannot leak in.
//! - No `Hash`/`Eq`. The ETag hashes the response *bytes*, never this tree; see
//!   `freedium-web`'s `api::http_cache`.
//! - No deployment-specific absolute URLs. A client knows which host it called;
//!   embedding one in a versioned contract would make the same content hash
//!   differently per deployment. Image URLs are the exception because their
//!   resize parameters are not reconstructible by a client.

pub mod feed;
pub mod health;
pub mod post;
pub mod problem;
pub mod resolve;

pub use feed::FeedDto;
pub use health::{CheckDto, HealthDto, HealthStatus};
pub use post::{
    BlockDto, CollectionDto, CreatorDto, ImageDto, InlineDto, MetaDto, PostDto, QuoteStyleDto,
    TagDto,
};
pub use problem::{Problem, ProblemKind};
pub use resolve::ResolveDto;

/// The version of the contract every root object in this crate carries.
///
/// It is on the root of *every* JSON body, [`Problem`] included, so a client can
/// read it without knowing which endpoint answered.
///
/// `/api/v1/posts/{id}/meta` serves [`MetaDto`] **as its root**, which is why
/// that type carries the field even though it is also nested inside
/// [`PostDto`] and [`FeedDto`] — see its docs for why the repetition is accepted
/// rather than wrapped.
///
/// `/html` and `/markdown` cannot carry it in the body without corrupting the
/// representation, so both send it as `X-Freedium-Schema-Version` instead. Every
/// representation advertises the version somehow.
pub const SCHEMA_VERSION: u8 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// The metadata every shape test starts from, with each optional populated
    /// so a test that cares about one does not have to build the other twelve.
    fn meta() -> MetaDto {
        MetaDto {
            schema_version: SCHEMA_VERSION,
            post_id: "0291df856c77".to_string(),
            title: "A post".to_string(),
            subtitle: Some("a subtitle".to_string()),
            description: "a description".to_string(),
            preview_image_url: Some(
                "https://miro.medium.com/v2/resize:fit:700/1*a.png".to_string(),
            ),
            reading_time_minutes: 5,
            is_locked: false,
            medium_url: Some("https://medium.com/@ada/a-post-0291df856c77".to_string()),
            first_published_at_unix_ms: Some(1_690_000_000_000),
            updated_at_unix_ms: None,
            tags: vec![TagDto {
                display_title: "Rust".to_string(),
                slug: Some("rust".to_string()),
            }],
            creator: Some(CreatorDto {
                id: "c1".to_string(),
                name: "Ada".to_string(),
                username: "ada".to_string(),
                bio: "b".to_string(),
                image_url: None,
            }),
            collection: None,
        }
    }

    fn every_block() -> Vec<BlockDto> {
        vec![
            BlockDto::Heading {
                level: 2,
                id: "h1".to_string(),
                text: "A heading".to_string(),
                content: vec![InlineDto::Text {
                    text: "A heading".to_string(),
                }],
            },
            BlockDto::Paragraph {
                content: vec![
                    InlineDto::Text {
                        text: "text".to_string(),
                    },
                    InlineDto::Strong {
                        content: vec![InlineDto::Text {
                            text: "bold".to_string(),
                        }],
                    },
                    InlineDto::Emphasis {
                        content: vec![InlineDto::Text {
                            text: "italic".to_string(),
                        }],
                    },
                    InlineDto::Code {
                        content: vec![InlineDto::Text {
                            text: "code".to_string(),
                        }],
                    },
                    InlineDto::Link {
                        href: "https://x.test".to_string(),
                        rel: "noopener".to_string(),
                        title: String::new(),
                        new_tab: true,
                        content: vec![InlineDto::Text {
                            text: "link".to_string(),
                        }],
                    },
                    InlineDto::UserMention {
                        user_id: "u1".to_string(),
                        content: vec![InlineDto::Text {
                            text: "@ada".to_string(),
                        }],
                    },
                    InlineDto::Highlight {
                        content: vec![InlineDto::Text {
                            text: "marked".to_string(),
                        }],
                    },
                ],
                drop_cap: false,
            },
            BlockDto::List {
                ordered: true,
                items: vec![vec![InlineDto::Text {
                    text: "one".to_string(),
                }]],
            },
            BlockDto::Code {
                language: Some("python".to_string()),
                lines: vec!["x = 1".to_string()],
            },
            BlockDto::Blockquote {
                style: QuoteStyleDto::Inset,
                content: vec![InlineDto::Text {
                    text: "quoted".to_string(),
                }],
            },
            BlockDto::Image {
                url: "https://miro.medium.com/v2/resize:fit:700/i.png".to_string(),
                alt: "an image".to_string(),
                caption: Some(vec![InlineDto::Text {
                    text: "a caption".to_string(),
                }]),
            },
            BlockDto::ImageRow {
                images: vec![ImageDto {
                    url: "https://miro.medium.com/v2/resize:fit:700/i.png".to_string(),
                    alt: String::new(),
                }],
            },
            BlockDto::Embed {
                url: "https://example.com/post".to_string(),
                title: "Title".to_string(),
                description: "Desc".to_string(),
                site: "example.com".to_string(),
                thumbnail_url: Some("https://miro.medium.com/v2/resize:fit:320/t.png".to_string()),
            },
            BlockDto::Iframe {
                src: "https://www.youtube.com/embed/x".to_string(),
                width: Some(640),
                height: Some(360),
            },
        ]
    }

    fn to_value<T: serde::Serialize>(value: &T) -> Value {
        serde_json::to_value(value).expect("the contract serialises")
    }

    /// **The public vocabulary, pinned.**
    ///
    /// Medium's own names for these are `"H2"`, `"P"`, `"PRE"`, `"BQ"`,
    /// `"IMG"`, `"IFRAME"` — leaking any of them would make Medium's storage
    /// format the public interface. This fails on a stray `rename_all`, a
    /// renamed variant, or a new block that forgot the attribute.
    #[test]
    fn a_block_tag_is_the_variant_name_in_camel_case() {
        let expected = [
            "heading",
            "paragraph",
            "list",
            "code",
            "blockquote",
            "image",
            "imageRow",
            "embed",
            "iframe",
        ];

        let tags: Vec<String> = every_block()
            .iter()
            .map(|block| {
                to_value(block)["type"]
                    .as_str()
                    .expect("every block carries a `type` tag")
                    .to_string()
            })
            .collect();

        assert_eq!(tags, expected);
        for tag in &tags {
            assert!(
                !["H2", "H3", "H4", "P", "PRE", "BQ", "IMG", "IFRAME"].contains(&tag.as_str()),
                "`{tag}` is Medium's name, not ours"
            );
        }
    }

    /// Same rule for the inline vocabulary. `em` rather than `emphasis` is
    /// deliberate: it is the HTML element name, matching `strong`'s relationship
    /// to `<strong>`.
    #[test]
    fn an_inline_tag_is_the_variant_name_in_camel_case() {
        let block = BlockDto::Paragraph {
            content: vec![
                InlineDto::Text {
                    text: "a".to_string(),
                },
                InlineDto::Strong { content: vec![] },
                InlineDto::Emphasis { content: vec![] },
                InlineDto::Code { content: vec![] },
                InlineDto::Link {
                    href: "h".to_string(),
                    rel: String::new(),
                    title: String::new(),
                    new_tab: false,
                    content: vec![],
                },
                InlineDto::UserMention {
                    user_id: "u".to_string(),
                    content: vec![],
                },
                InlineDto::Highlight { content: vec![] },
            ],
            drop_cap: false,
        };

        let tags: Vec<String> = to_value(&block)["content"]
            .as_array()
            .expect("the paragraph carries its content")
            .iter()
            .map(|inline| inline["type"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(
            tags,
            [
                "text",
                "strong",
                "em",
                "code",
                "link",
                "userMention",
                "highlight"
            ]
        );
    }

    /// Field **keys** stay snake_case while the tag values above are camelCase.
    /// One test so the mixing is a decision on record rather than an accident:
    /// a tag is a name, a key is a key.
    #[test]
    fn the_field_keys_are_snake_case() {
        let value = to_value(&BlockDto::Paragraph {
            content: vec![],
            drop_cap: true,
        });
        assert!(value.get("drop_cap").is_some(), "{value}");
        assert!(value.get("dropCap").is_none());

        let meta = to_value(&meta());
        for key in [
            "post_id",
            "preview_image_url",
            "reading_time_minutes",
            "is_locked",
            "medium_url",
            "first_published_at_unix_ms",
            "updated_at_unix_ms",
        ] {
            assert!(meta.get(key).is_some(), "`{key}` is missing from {meta}");
        }
    }

    /// A client should be able to read the contract version off any JSON body it
    /// gets, including a failure.
    #[test]
    fn every_json_root_carries_the_schema_version() {
        let roots: Vec<(&str, Value)> = vec![
            ("PostDto", to_value(&PostDto::new(meta(), every_block()))),
            ("MetaDto", to_value(&meta())),
            ("FeedDto", to_value(&FeedDto::new(vec![meta()], None))),
            ("ResolveDto", to_value(&ResolveDto::new("0291df856c77"))),
            (
                "HealthDto",
                to_value(&HealthDto::new(
                    HealthStatus::Ok,
                    "0.1.0",
                    false,
                    false,
                    vec![CheckDto::ok("postgres")],
                )),
            ),
            (
                "Problem",
                to_value(&Problem::new(
                    ProblemKind::NotFound,
                    "d",
                    "/api/v1/posts/x",
                    "a-b-c",
                )),
            ),
        ];

        for (name, value) in roots {
            assert_eq!(
                value["schema_version"],
                json!(SCHEMA_VERSION),
                "`{name}` does not carry the schema version at its root"
            );
        }
    }

    /// **Optional fields serialise as `null`, never as an absent key.**
    ///
    /// This keeps the key set of every response stable, so a consumer can
    /// destructure without a presence check and a schema differs between two
    /// responses only where the *values* differ. It is pinned because
    /// `skip_serializing_if` is the idiomatic thing to reach for, and adding it
    /// would silently change the contract for every client at once.
    #[test]
    fn an_absent_optional_is_null_not_missing() {
        let bare = MetaDto {
            schema_version: SCHEMA_VERSION,
            post_id: "0291df856c77".to_string(),
            title: String::new(),
            subtitle: None,
            description: String::new(),
            preview_image_url: None,
            reading_time_minutes: 0,
            is_locked: true,
            medium_url: None,
            first_published_at_unix_ms: None,
            updated_at_unix_ms: None,
            tags: Vec::new(),
            creator: None,
            collection: None,
        };

        let value = to_value(&bare);
        for key in [
            "subtitle",
            "preview_image_url",
            "medium_url",
            "first_published_at_unix_ms",
            "updated_at_unix_ms",
            "creator",
            "collection",
        ] {
            let field = value.get(key).unwrap_or_else(|| {
                panic!("`{key}` is missing; a consumer cannot destructure that")
            });
            assert!(field.is_null(), "`{key}` is {field}, not null");
        }

        // Same rule one level down: a block's optional members.
        let image = to_value(&BlockDto::Image {
            url: "u".to_string(),
            alt: String::new(),
            caption: None,
        });
        assert!(image.get("caption").is_some() && image["caption"].is_null());

        let iframe = to_value(&BlockDto::Iframe {
            src: "s".to_string(),
            width: None,
            height: None,
        });
        assert!(iframe["width"].is_null() && iframe["height"].is_null());

        let feed = to_value(&FeedDto::new(vec![], None));
        assert!(feed.get("next_cursor").is_some());
        assert!(feed["next_cursor"].is_null());
    }

    /// `is_locked` is the payload's own polarity. The legacy's `free_access` is
    /// the string `"Yes"`/`"No"` and its name is inverted; mirroring that into a
    /// public contract would hand every consumer a boolean to get backwards.
    #[test]
    fn locked_is_a_boolean_with_the_payloads_polarity() {
        let locked = to_value(&MetaDto {
            is_locked: true,
            ..meta()
        });
        assert_eq!(locked["is_locked"], json!(true));

        let value = to_value(&meta());
        assert!(
            value.get("free_access").is_none(),
            "the legacy's inverted string field must not reach the contract"
        );
    }

    /// `Text` carries raw text. If an HTML escape were ever applied on the way
    /// into the DTO, a client would see `&#39;` where the author typed an
    /// apostrophe — and the legacy's double-escape makes that `&amp;#39;`.
    #[test]
    fn text_is_not_html_escaped() {
        let value = to_value(&BlockDto::Paragraph {
            content: vec![InlineDto::Text {
                text: "it's a <b>test</b> & more".to_string(),
            }],
            drop_cap: false,
        });

        let text = value["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "it's a <b>test</b> & more");
        assert!(!text.contains("&#39;"));
        assert!(!text.contains("&amp;"));
        assert!(!text.contains("&lt;"));
    }

    /// `ResolveDto::new` builds the id-only URL, which is valid for every post
    /// and needs no fetch. `meta.medium_url` is the article's own canonical URL
    /// and is a different value; a test so the two are not confused later.
    #[test]
    fn resolve_builds_the_id_only_url() {
        let resolved = ResolveDto::new("0291df856c77");
        assert_eq!(resolved.resolved_url, "https://medium.com/p/0291df856c77");
        assert_ne!(
            Some(resolved.resolved_url.clone()),
            meta().medium_url,
            "the id-only URL is not the article's canonical URL"
        );
    }

    /// The contract round-trips. Every type derives `Deserialize` for the
    /// consumer's benefit; this is what proves the derives are consistent with
    /// the `Serialize` side rather than half-declared.
    #[test]
    fn every_type_round_trips() {
        let post = PostDto::new(meta(), every_block());
        let json = serde_json::to_string(&post).unwrap();
        assert_eq!(serde_json::from_str::<PostDto>(&json).unwrap(), post);

        let problem = Problem::new(
            ProblemKind::RateLimited,
            "slow down",
            "/api/v1/feed",
            "a-b-c",
        );
        let json = serde_json::to_string(&problem).unwrap();
        assert_eq!(serde_json::from_str::<Problem>(&json).unwrap(), problem);
    }
}
