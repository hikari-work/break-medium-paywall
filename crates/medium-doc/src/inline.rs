//! Medium `markups` → the [`Inline`] tree.
//!
//! Replaces `parse_markups` + `split_overlapping_ranges` + the template splicing
//! in `rl_string_helper`. The input is a paragraph's text plus a list of
//! `{start, end, type}` ranges in UTF-16 code units; the output is a tree the
//! renderer walks once.
//!
//! ## The ordering rule, and why it is not obvious
//!
//! Medium's `markups` array is **not** ordered outermost-first or by position.
//! The legacy renderer applied the templates in reverse array order, so the
//! *last* markup in the array ends up as the outermost tag. Verified by running
//! the legacy code:
//!
//! ```text
//! LINK[0,10) + STRONG[2,5)  ->  <a>ab</a><strong><a>cde</a></strong><a>fghij</a>
//! STRONG[2,5) + LINK[0,10)  ->  <a>ab</a><a><strong>cde</strong></a><a>fghij</a>
//! LINK[0,10) + STRONG[0,10) ->  <strong><a>abcdefghij</a></strong>
//! ```
//!
//! That rule is per segment and therefore not a nesting *policy*: a link
//! spanning a `<strong>` is outermost in the text before the strong and inside
//! it in the text after, which is why the legacy output re-opens the anchor
//! three times. There is no tree-shaped IR that expresses "outside, then
//! inside, then outside" other than as three siblings, so [`build_inlines`]
//! replaces the rule with one that is globally consistent: **a markup whose
//! range encloses another's is the outer one, and payload order only breaks
//! ties** (equal ranges, or a partial overlap where no nesting exists at all).
//! See [`nesting_order`].
//!
//! ## Where the output differs from the legacy renderer
//!
//! Two kinds of difference, both deliberate, neither semantic:
//!
//! 1. **A link interrupted by another markup becomes one `<a>`** with the
//!    strong hoisted inside it, rather than three sibling `<a>` elements with
//!    identical attributes:
//!
//!    ```text
//!    legacy:  <a>ab</a><strong><a>cde</a></strong><a>fghij</a>
//!    here:    <a>ab<strong>cde</strong>fghij</a>
//!    ```
//!
//!    Same target, same emphasised text. This is the shape §2.1 of the rewrite
//!    plan asks for, and it is only expressible because [`Inline::Link`] holds
//!    children.
//! 2. **Nesting that payload order alone would have split is joined up.**
//!    `STRONG[0,5) + STRONG[1,3)` on `"abcde"` gives three siblings from the
//!    legacy renderer (`<strong>a</strong><strong><strong>bc</strong></strong>
//!    <strong>de</strong>`) and one nested
//!    `<strong>a<strong>bc</strong>de</strong>` here. Nested emphasis collapses
//!    to the same rendering.
//!
//! What does match byte for byte: markups with equal ranges, and any markup
//! nested strictly inside another with no third markup crossing it. Markups
//! that partially overlap still produce siblings, as they must — no tree
//! expresses that shape either.
//!
//! The differential harness normalises both sides to a canonical form before
//! comparing, so differences of this kind do not register as failures; see the
//! `run-render` subcommand.

use std::ops::Range;

use tracing::warn;

use crate::ir::Inline;
use crate::text::quote_symbol;
use crate::utf16::Utf16Map;

/// A markup as it arrives from the GraphQL payload, before offset resolution.
///
/// `start` and `end` are UTF-16 code-unit indices into the paragraph text, which
/// is what Medium's JavaScript editor counts. Unknown markup types are dropped
/// before this point, matching `markups.py:52-53`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Markup {
    pub start: usize,
    pub end: usize,
    pub kind: MarkupKind,
}

/// The markup types the renderer understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkupKind {
    /// An `A`/`LINK` markup. `new_tab` is derived, not stored: `markups.py:23`
    /// sets `target="_blank"` for everything except a `#` anchor.
    Link {
        href: String,
        rel: String,
        title: String,
    },
    /// An `A`/`USER` mention — a link with no `rel`, `title` or `target`.
    UserMention {
        user_id: String,
    },
    Strong,
    Emphasis,
    Code,
    /// The `<mark>` a reader highlight wraps (`core.py:314-321`).
    ///
    /// It carries no data — the class is a literal and the range is the markup's
    /// own — but it goes through this module rather than being wrapped around
    /// the finished tree because the legacy code set it as one more template on
    /// the same formatter, *after* the markup templates. That is what makes the
    /// mark the outermost tag when the ranges coincide, and what makes the
    /// offsets resolve in the same UTF-16 space as everything else.
    Highlight,
}

/// A markup paired with the byte range it covers in the paragraph text.
struct Resolved<'a> {
    kind: &'a MarkupKind,
    range: Range<usize>,
}

/// One node of the tree being assembled.
///
/// `path` is the chain of markup indices from the outermost node down to this
/// one, so the root frame's path is empty and a node's own markup is the last
/// element.
struct Frame {
    path: Vec<usize>,
    children: Vec<Child>,
}

enum Child {
    Frame(Frame),
    /// A byte range of the paragraph text.
    Text(Range<usize>),
}

/// Builds the inline tree for one paragraph.
///
/// `markups` must be in payload order, because that order breaks ties between
/// markups that enclose the same range; see [`nesting_order`] for the rule that
/// decides everything else.
pub fn build_inlines(text: &str, markups: &[Markup]) -> Vec<Inline> {
    // Typographic quotes are folded to ASCII before the offset map is built.
    // The substitution is one character for one character, so Medium's UTF-16
    // indices stay valid.
    let text = quote_symbol(text);
    let map = Utf16Map::new(&text);

    let mut resolved: Vec<Resolved<'_>> = Vec::with_capacity(markups.len());
    for markup in markups {
        // `None` covers `start == end` (skipped by the legacy code) and `start`
        // past the end of the text.
        let Some(range) = map.resolve(markup.start, markup.end) else {
            warn!(
                start = markup.start,
                end = markup.end,
                "markup range resolves to nothing; skipped"
            );
            continue;
        };
        if map.is_mid_surrogate(markup.start) || map.is_mid_surrogate(markup.end) {
            // The legacy renderer emitted *corrupted* markup for these: an
            // endpoint on the second unit of a surrogate pair ate the first
            // character of the tag being spliced in. Snapping to the character
            // boundary is the fix, and this keeps the fix observable.
            warn!(
                start = markup.start,
                end = markup.end,
                "markup range endpoint lands inside a surrogate pair"
            );
        }
        if range.is_empty() {
            // A range that starts on the second unit of a surrogate covers no
            // text at all. Emitting nothing beats an empty `<strong></strong>`.
            warn!(
                start = markup.start,
                end = markup.end,
                "markup range is empty after resolution; skipped"
            );
            continue;
        }
        resolved.push(Resolved {
            kind: &markup.kind,
            range,
        });
    }

    // Every markup boundary is a segment boundary. Characters outside any
    // markup fall into segments of their own and come out as `Text`.
    let mut points: Vec<usize> = Vec::with_capacity(resolved.len() * 2 + 2);
    points.push(0);
    points.push(text.len());
    for item in &resolved {
        points.push(item.range.start);
        points.push(item.range.end);
    }
    points.sort_unstable();
    points.dedup();

    // `path` is outermost-first, i.e. the order the tags are opened in.
    let segments: Vec<(Vec<usize>, Range<usize>)> = points
        .windows(2)
        .map(|window| {
            let (start, end) = (window[0], window[1]);
            let covering: Vec<usize> = resolved
                .iter()
                .enumerate()
                .filter(|(_, item)| item.range.start <= start && end <= item.range.end)
                .map(|(index, _)| index)
                .collect();
            (nesting_order(&covering, &resolved), start..end)
        })
        .collect();

    // Merge consecutive segments by longest common prefix: as long as the next
    // segment's path still starts with the open frame's path, that frame stays
    // open. This is what turns a link split across a `<strong>` into a single
    // anchor.
    //
    // The stack owns the frames that are still open. Closing a frame means
    // popping it and moving it into its parent's children, so the tree is
    // assembled in place and nothing is cloned.
    let mut stack: Vec<Frame> = vec![Frame {
        path: Vec::new(),
        children: Vec::new(),
    }];
    for (path, range) in segments {
        let common = {
            let open = &stack.last().expect("root frame is always present").path;
            let limit = open.len().min(path.len());
            let mut common = 0;
            while common < limit && open[common] == path[common] {
                common += 1;
            }
            common
        };
        close_frames(&mut stack, common + 1);
        for depth in common..path.len() {
            stack.push(Frame {
                path: path[..=depth].to_vec(),
                children: Vec::new(),
            });
        }
        stack
            .last_mut()
            .expect("root frame is always present")
            .children
            .push(Child::Text(range));
    }
    close_frames(&mut stack, 1);

    convert_children(&stack[0].children, &resolved, &text)
}

/// Orders the markups covering one segment, outermost first.
///
/// Nesting cannot be read off the payload order alone, because a segment's
/// covering set changes from one segment to the next: a link spanning the whole
/// paragraph is outermost in a segment it alone covers, but if a `<strong>` in
/// the middle of it sits *later* in the array, payload order would put the
/// strong outside the link there. That is exactly what makes the legacy
/// renderer emit three sibling anchors instead of one, and it leaves the tree
/// with no consistent nesting to merge segments on.
///
/// So containment decides: a markup whose range encloses another's is the outer
/// one, and payload order only breaks ties — equal ranges, or a partial overlap
/// where no nesting exists at all. The result is a nesting order that agrees
/// between any two segments that share both markups, which is what lets
/// [`build_inlines`] keep a tag open across a boundary.
///
/// The tie-break direction is the legacy one: the *last* markup in the array is
/// the outermost tag.
fn nesting_order(covering: &[usize], resolved: &[Resolved<'_>]) -> Vec<usize> {
    let mut order = covering.to_vec();
    order.sort_by_key(|&index| {
        let inner = &resolved[index].range;
        // How many of the others strictly enclose this markup.
        let depth = covering
            .iter()
            .filter(|&&other| {
                let outer = &resolved[other].range;
                other != index
                    && outer != inner
                    && outer.start <= inner.start
                    && inner.end <= outer.end
            })
            .count();
        (depth, std::cmp::Reverse(index))
    });
    order
}

/// Closes open frames until only `depth` of them remain, attaching each to its
/// parent as it goes.
fn close_frames(stack: &mut Vec<Frame>, depth: usize) {
    debug_assert!(depth >= 1, "the root frame is never closed");
    while stack.len() > depth {
        let finished = stack.pop().expect("checked by the loop condition");
        stack
            .last_mut()
            .expect("root frame is always present")
            .children
            .push(Child::Frame(finished));
    }
}

fn convert_children(children: &[Child], resolved: &[Resolved<'_>], text: &str) -> Vec<Inline> {
    children
        .iter()
        .map(|child| match child {
            // Text nodes are never merged: two adjacent `Text` values must stay
            // separate so the renderer's `&` lookahead can still see the
            // paragraph text either side of the boundary.
            Child::Text(range) => Inline::Text(text[range.clone()].to_string()),
            Child::Frame(frame) => frame_to_inline(frame, resolved, text),
        })
        .collect()
}

fn frame_to_inline(frame: &Frame, resolved: &[Resolved<'_>], text: &str) -> Inline {
    let children = convert_children(&frame.children, resolved, text);
    let inner = frame
        .path
        .last()
        .expect("only the root frame has an empty path");
    match &resolved[*inner].kind {
        MarkupKind::Strong => Inline::Strong(children),
        MarkupKind::Emphasis => Inline::Emphasis(children),
        MarkupKind::Code => Inline::Code(children),
        // `markups.py:23-24`: a same-page anchor gets no `target`.
        MarkupKind::Link { href, rel, title } => Inline::Link {
            new_tab: !href.starts_with('#'),
            href: href.clone(),
            rel: rel.clone(),
            title: title.clone(),
            children,
        },
        MarkupKind::UserMention { user_id } => Inline::UserMention {
            user_id: user_id.clone(),
            children,
        },
        MarkupKind::Highlight => Inline::Highlight(children),
    }
}

#[cfg(test)]
mod tests {
    use super::{Markup, MarkupKind, build_inlines};
    use crate::ir::Inline;

    fn link(href: &str) -> MarkupKind {
        MarkupKind::Link {
            href: href.to_string(),
            rel: String::new(),
            title: String::new(),
        }
    }

    fn markup(start: usize, end: usize, kind: MarkupKind) -> Markup {
        Markup { start, end, kind }
    }

    /// Renders a tree to a compact S-expression, so a failure shows the shape
    /// rather than one long HTML string. `a`, `s`, `e`, `c`, `u`, `h` are link,
    /// strong, emphasis, code, user-mention and highlight.
    fn dump(nodes: &[Inline]) -> String {
        fn one(node: &Inline) -> String {
            match node {
                Inline::Text(text) => format!("{text:?}"),
                Inline::Strong(kids) => format!("s({})", dump(kids)),
                Inline::Emphasis(kids) => format!("e({})", dump(kids)),
                Inline::Code(kids) => format!("c({})", dump(kids)),
                Inline::Link { href, children, .. } => {
                    format!("a[{href}]({})", dump(children))
                }
                Inline::UserMention { user_id, children } => {
                    format!("u[{user_id}]({})", dump(children))
                }
                Inline::Highlight(kids) => format!("h({})", dump(kids)),
            }
        }
        nodes.iter().map(one).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn no_markups_is_a_single_text_node() {
        let tree = build_inlines("plain text", &[]);
        assert_eq!(dump(&tree), "\"plain text\"");
    }

    /// The link encloses the strong, so it stays outer — and the three
    /// segments join into one anchor. Legacy emits three siblings here; see the
    /// module docs.
    #[test]
    fn overlapping_link_and_strong_yields_one_anchor() {
        let tree = build_inlines(
            "abcdefghij",
            &[
                markup(0, 10, link("https://x.test")),
                markup(2, 5, MarkupKind::Strong),
            ],
        );
        assert_eq!(
            dump(&tree),
            "a[https://x.test](\"ab\" s(\"cde\") \"fghij\")"
        );
    }

    /// Payload order does not change the result when containment already
    /// decides: the link encloses the strong, so it is outer either way. Only
    /// the legacy sibling-splitting made the two orders look different.
    #[test]
    fn containment_beats_payload_order() {
        let link_first = build_inlines(
            "abcdefghij",
            &[
                markup(0, 10, link("https://x.test")),
                markup(2, 5, MarkupKind::Strong),
            ],
        );
        let strong_first = build_inlines(
            "abcdefghij",
            &[
                markup(2, 5, MarkupKind::Strong),
                markup(0, 10, link("https://x.test")),
            ],
        );
        assert_eq!(dump(&link_first), dump(&strong_first));
    }

    /// Equal ranges leave containment no say, so payload order decides — and
    /// the last markup in the array is the outer one. Verified byte for byte
    /// against the legacy renderer:
    /// `<strong><a ...>abcdefghij</a></strong>`.
    #[test]
    fn identical_ranges_are_ordered_by_payload() {
        let tree = build_inlines(
            "abcdefghij",
            &[
                markup(0, 10, link("https://x.test")),
                markup(0, 10, MarkupKind::Strong),
            ],
        );
        assert_eq!(
            dump(&tree),
            "s(a[https://x.test](\"abcdefghij\"))",
            "the strong is last in the array, so it wraps the anchor"
        );
    }

    /// The parser appends the highlight's range *after* the payload markups,
    /// mirroring `core.py:317` setting the `<mark>` template last. Equal ranges
    /// then make the mark the outer tag, which is what the legacy renderer
    /// produced: `<mark ...><strong>abc</strong></mark>`.
    #[test]
    fn a_highlight_appended_last_wraps_an_equal_ranged_markup() {
        let tree = build_inlines(
            "abc",
            &[
                markup(0, 3, MarkupKind::Strong),
                markup(0, 3, MarkupKind::Highlight),
            ],
        );
        assert_eq!(dump(&tree), "h(s(\"abc\"))");
    }

    /// The mark lands in the same UTF-16 space as the markups, so it can enclose
    /// a markup that covers less of the paragraph.
    #[test]
    fn a_highlight_can_enclose_a_markup() {
        let tree = build_inlines(
            "abcdef",
            &[
                markup(1, 4, MarkupKind::Strong),
                markup(0, 6, MarkupKind::Highlight),
            ],
        );
        assert_eq!(dump(&tree), "h(\"a\" s(\"bcd\") \"ef\")");
    }

    /// Nesting the same markup type twice joins up instead of splitting at the
    /// inner range. Semantically identical to the legacy output, which was
    /// `<strong>a</strong><strong><strong>bc</strong></strong><strong>de</strong>`.
    #[test]
    fn repeated_strong_joins_into_one_nest() {
        let tree = build_inlines(
            "abcde",
            &[
                markup(0, 5, MarkupKind::Strong),
                markup(1, 3, MarkupKind::Strong),
            ],
        );
        assert_eq!(dump(&tree), "s(\"a\" s(\"bc\") \"de\")");
    }

    /// A markup partially overlapping another has no nesting relation with it.
    /// Neither can enclose the other, so the overlap stays two siblings and the
    /// second markup re-opens across it — the legacy renderer did the same,
    /// emitting `<strong>abc</strong><em><strong>de</strong></em><em>fghij</em>`.
    /// The emphasis joins back up here, which is the one difference.
    #[test]
    fn partially_overlapping_markups_stay_siblings() {
        let tree = build_inlines(
            "abcdefghij",
            &[
                markup(0, 5, MarkupKind::Strong),
                markup(3, 10, MarkupKind::Emphasis),
            ],
        );
        assert_eq!(
            dump(&tree),
            "s(\"abc\") e(s(\"de\") \"fghij\")",
            "neither markup encloses the other, so both must stay siblings"
        );
    }

    #[test]
    fn identical_ranges_of_the_same_type_nest() {
        let tree = build_inlines(
            "abc",
            &[
                markup(0, 3, MarkupKind::Strong),
                markup(0, 3, MarkupKind::Strong),
            ],
        );
        assert_eq!(dump(&tree), "s(s(\"abc\"))");
    }

    /// Verified against the legacy renderer: `STRONG[3,5)` on `"hi 😀 there"`
    /// renders `hi <strong>😀</strong> there`.
    #[test]
    fn utf16_range_selects_the_emoji() {
        let tree = build_inlines("hi \u{1f600} there", &[markup(3, 5, MarkupKind::Strong)]);
        assert_eq!(
            dump(&tree),
            "\"hi \" s(\"\u{1f600}\") \" there\"",
            "Medium counts the emoji as two units"
        );
    }

    /// A range ending on the emoji's *first* unit still takes the whole
    /// character. The legacy renderer corrupted its output here
    /// (`<strong>😀/strong>R`), so this is a fix rather than parity — hence no
    /// assertion against legacy bytes.
    #[test]
    fn range_ending_on_first_unit_of_emoji_takes_the_whole_character() {
        let tree = build_inlines("hi \u{1f600} there", &[markup(3, 4, MarkupKind::Strong)]);
        assert_eq!(dump(&tree), "\"hi \" s(\"\u{1f600}\") \" there\"");
    }

    /// A range that starts on the emoji's second unit selects nothing, so no
    /// node is emitted. The legacy renderer produced corrupted markup here.
    #[test]
    fn range_starting_on_second_unit_emits_no_node() {
        let tree = build_inlines("hi \u{1f600} there", &[markup(4, 5, MarkupKind::Strong)]);
        assert_eq!(dump(&tree), "\"hi 😀 there\"");
    }

    #[test]
    fn empty_range_is_dropped() {
        let tree = build_inlines("abcd", &[markup(2, 2, MarkupKind::Strong)]);
        assert_eq!(dump(&tree), "\"abcd\"");
    }

    #[test]
    fn range_past_the_end_clamps() {
        let tree = build_inlines("abcd", &[markup(0, 999, MarkupKind::Strong)]);
        assert_eq!(dump(&tree), "s(\"abcd\")");
    }

    /// `markups.py:23` — only a same-page anchor omits `target="_blank"`.
    #[test]
    fn same_page_anchors_do_not_open_a_new_tab() {
        let external = build_inlines("x", &[markup(0, 1, link("https://x.test"))]);
        let internal = build_inlines("x", &[markup(0, 1, link("#section"))]);
        assert!(matches!(&external[0], Inline::Link { new_tab: true, .. }));
        assert!(matches!(&internal[0], Inline::Link { new_tab: false, .. }));
    }

    #[test]
    fn user_mentions_carry_their_id() {
        let tree = build_inlines(
            "someone",
            &[markup(
                0,
                7,
                MarkupKind::UserMention {
                    user_id: "abc".into(),
                },
            )],
        );
        assert_eq!(dump(&tree), "u[abc](\"someone\")");
    }

    /// The offset map indexes the *normalised* text, which is why folding the
    /// quotes has to happen first.
    #[test]
    fn curly_quotes_are_folded_before_offsets_are_resolved() {
        let tree = build_inlines("\u{201c}hi\u{201d}", &[markup(0, 4, MarkupKind::Strong)]);
        assert_eq!(dump(&tree), "s(\"\\\"hi\\\"\")");
    }

    #[test]
    fn adjacent_segments_stay_separate_text_nodes() {
        let tree = build_inlines("abcdefghij", &[markup(2, 5, MarkupKind::Strong)]);
        assert_eq!(dump(&tree), "\"ab\" s(\"cde\") \"fghij\"");
    }
}
