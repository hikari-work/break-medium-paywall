//! The canonical form the render gate compares, and why it is not a tree
//! comparison.
//!
//! The Python and Rust renderers are allowed to disagree about *structure*. The
//! legacy pipeline splices markup templates into a paragraph string with a
//! position matrix, so overlapping ranges come out as **siblings** — one link
//! cut into three segments becomes three `<a>` tags with the same `href`
//! (`core.py` + `rl_string_helper/string_helper.py:141-...`) — while the IR
//! deliberately nests them into one anchor. Measured on the legacy code:
//!
//! ```text
//! LINK[0,10) + STRONG[2,5)  ->  <a>ab</a><strong><a>cde</a></strong><a>fghij</a>
//! LINK[0,10) + STRONG[0,10) ->  <strong><a>abcdefghij</a></strong>
//! ```
//!
//! A DOM comparison would call those two a mismatch on every article. So the
//! comparison is over the reading experience instead: the ordered sequence of
//!
//! ```text
//! (text of the node, the set of annotations in effect over it)
//! ```
//!
//! where an annotation is an ancestor element plus its attributes. That form is
//! invariant under sibling-vs-nested regrouping, and it is still strong enough
//! to catch what matters: dropped or reordered text, an annotation applied to
//! the wrong span, a wrong class, a missed escape. Both sides go through this
//! one function, so the parser's error recovery cannot favour either.
//!
//! Four deliberate normalisations, all symmetric:
//!
//! - The annotations over a text node are compared as a **set**, not a sequence,
//!   so which of two annotations ends up outermost does not matter. Only a
//!   difference you could see is a difference.
//! - Text nodes that are entirely whitespace are dropped. The legacy templates
//!   are multi-line string literals (`core.py:536-554` for the embed), so their
//!   indentation is text in the DOM but not in the output.
//! - Attribute order is sorted, because only the *set* of attributes carries
//!   meaning here.
//! - A `<meta charset>` value is compared case-insensitively. This one is not
//!   symmetry but a gap that Fase 4's shadow traffic closed: the legacy handler
//!   serialises every page through html5lib with an encoding, whose
//!   `inject_meta_charset` filter rewrites that attribute to the lowercase
//!   encoding string. See [`walk`] for the full reasoning and for what is
//!   deliberately left strict.
//!
//! What it does **not** do is relax the text. Whitespace inside a text node is
//! kept verbatim, so a code block whose newlines moved still fails.
//!
//! Text alone is not enough to compare, though: `<img>` and `<iframe>` carry no
//! text at all, so an image with the wrong `src` would reduce to nothing. A
//! text-less subtree therefore contributes an empty-text **marker** node holding
//! its annotation chain — see [`walk`].

use std::collections::BTreeMap;

use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// One element on the ancestor chain, reduced to what the gate compares.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Annotation {
    pub tag: String,
    /// Sorted, so attribute order in the source does not matter.
    pub attrs: Vec<(String, String)>,
}

impl std::fmt::Display for Annotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{}", self.tag)?;
        for (name, value) in &self.attrs {
            write!(f, " {name}=\"{value}\"")?;
        }
        write!(f, ">")
    }
}

/// One text node plus the annotations in effect over it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalNode {
    pub text: String,
    /// A **set**, stored sorted. See [`sorted`].
    pub annotations: Vec<Annotation>,
}

impl CanonicalNode {
    /// A one-line rendering for diagnostics.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for annotation in &self.annotations {
            out.push_str(&annotation.to_string());
        }
        out.push_str(&format!("{:?}", self.text));
        out
    }
}

/// Puts an annotation chain in its canonical order.
///
/// The chain arrives outermost-first, and that order is *not* compared: only the
/// set of annotations in effect over a run of text matters, because two
/// nestings of the same annotations render the same. `<strong><em>x</em></strong>`
/// and `<em><strong>x</strong></em>` are both bold italic; the legacy's splice
/// and the IR's tree disagree about which is outer, and neither is wrong. Sorting
/// is what makes the comparison invariant to that — and it is why a chain is
/// stored sorted rather than left in walk order.
fn sorted(mut chain: Vec<Annotation>) -> Vec<Annotation> {
    chain.sort();
    chain
}

/// Parses `html` and reduces it to the canonical sequence.
pub fn canonicalize(html: &str) -> Vec<CanonicalNode> {
    let dom = parse_document(RcDom::default(), Default::default()).one(html);
    let mut nodes = Vec::new();
    // The document node is not an annotation: it is bookkeeping html5ever adds
    // to both sides identically.
    for child in dom.document.children.borrow().iter() {
        walk(child, &mut Vec::new(), &mut nodes);
    }
    nodes
}

/// Walks `handle`, returning whether it contributed anything.
///
/// The return value is what makes `<img>` and `<iframe>` visible. Both are
/// elements with no text under them, so a walk that only ever emitted text nodes
/// would reduce `<img src="a">` and `<img src="b">` to the same empty sequence
/// and let a wrong image through. An element whose subtree yielded nothing
/// therefore emits a marker of its own: an empty-text node carrying its
/// annotation chain, which keeps its attributes in the comparison and keeps it
/// in document order. Because a marker counts as a contribution, only the
/// outermost text-less element emits one, so `<div><div><iframe/></div></div>`
/// produces a single marker whose chain names both wrappers — which is how the
/// gate notices a wrapper that one side has and the other does not.
fn walk(handle: &Handle, ancestors: &mut Vec<Annotation>, out: &mut Vec<CanonicalNode>) -> bool {
    match &handle.data {
        NodeData::Text { contents } => {
            let text = contents.borrow().to_string();
            // Template indentation lives in the DOM as text but not in the
            // output; anything with visible content is kept verbatim.
            if text.trim().is_empty() {
                return false;
            }
            out.push(CanonicalNode {
                text,
                annotations: sorted(ancestors.clone()),
            });
            true
        }

        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.to_string();

            let mut collected: BTreeMap<String, String> = BTreeMap::new();
            for attr in attrs.borrow().iter() {
                let name = attr.name.local.to_string();
                let mut value = attr.value.to_string();
                // The one attribute value this comparison normalises, and the
                // shadow run in Fase 4 is what found that it had to.
                //
                // `handlers/post.py:99-100` puts every served page through
                // html5lib's `parse`/`serialize`, and passing an `encoding`
                // turns on the serializer's `inject_meta_charset` filter, which
                // **replaces** the charset attribute with the encoding string it
                // was handed (`html5lib/filters/inject_meta_charset.py`). Python
                // therefore serves `charset=utf-8` where the template — and the
                // Rust port, which drops the round-trip entirely — say `UTF-8`.
                //
                // Per the Encoding Standard a charset label is case-insensitive,
                // and no browser can tell those two apart, so this difference is
                // exactly the kind the canonical form exists to forgive: it was
                // forgiving the round-trip's doctype, optional tags and attribute
                // quoting, and missing the one rewrite that touches a value.
                //
                // Only this attribute, and only on `<meta>`. Every other value —
                // `class`, `id`, `href`, `src`, `alt`, any `data-*` — is compared
                // verbatim, which is most of the point of holding the annotation
                // chain at all. The `http-equiv="Content-Type"` spelling of the
                // same declaration is deliberately **not** covered: html5lib
                // rewrites its `content` too, and a difference there should be
                // reported rather than normalised away through an attribute that
                // means other things on other elements.
                if tag == "meta" && name == "charset" {
                    value.make_ascii_lowercase();
                }
                collected.insert(name, value);
            }
            let annotation = Annotation {
                tag: tag.clone(),
                attrs: collected.into_iter().collect(),
            };

            ancestors.push(annotation.clone());
            let mut emitted = false;
            for child in handle.children.borrow().iter() {
                // `|=` rather than `||` so every child is walked, not just the
                // first one that emits.
                emitted |= walk(child, ancestors, out);
            }
            ancestors.pop();

            if emitted || is_parser_scaffolding(&tag) {
                return emitted;
            }

            let mut chain = ancestors.clone();
            chain.push(annotation);
            out.push(CanonicalNode {
                text: String::new(),
                annotations: sorted(chain),
            });
            true
        }

        // Comments carry no rendering, and the two sides emit different ones
        // (the legacy templates have none, the IR has none either, but a future
        // renderer might).
        NodeData::Comment { .. } => false,

        NodeData::Document => {
            let mut emitted = false;
            for child in handle.children.borrow().iter() {
                emitted |= walk(child, ancestors, out);
            }
            emitted
        }
        // Doctype and processing instructions.
        _ => false,
    }
}

/// The three elements html5ever always wraps a fragment in.
///
/// Neither side ever writes them — both render bare fragments, and the harness
/// concatenates those before parsing — so a marker for one would be pure noise,
/// and on empty input it would stop the sequence from being empty.
fn is_parser_scaffolding(tag: &str) -> bool {
    matches!(tag, "html" | "head" | "body")
}

#[cfg(test)]
mod tests {
    use super::canonicalize;

    fn describe(html: &str) -> Vec<String> {
        canonicalize(html).iter().map(|n| n.describe()).collect()
    }

    /// The property the gate depends on: a link split into siblings and the same
    /// link nested around its emphasis read identically.
    #[test]
    fn sibling_and_nested_markup_agree() {
        let siblings = r#"<a rel="r" title="t" href="h" target="_blank">ab</a><strong><a rel="r" title="t" href="h" target="_blank">cde</a></strong><a rel="r" title="t" href="h" target="_blank">fghij</a>"#;
        let nested =
            r#"<a rel="r" title="t" href="h" target="_blank">ab<strong>cde</strong>fghij</a>"#;
        assert_eq!(describe(siblings), describe(nested));
    }

    #[test]
    fn the_text_sequence_is_preserved() {
        let nodes = canonicalize("<p>one</p><p>two</p>");
        let texts: Vec<&str> = nodes.iter().map(|node| node.text.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
    }

    /// Attributes are compared as a set, so the two sides may emit them in
    /// different orders.
    #[test]
    fn attribute_order_does_not_matter() {
        assert_eq!(
            describe(r#"<img alt="a" src="b">"#),
            describe(r#"<img src="b" alt="a">"#)
        );
    }

    /// Fase 4's first finding, as a test. Python serves `utf-8` because
    /// html5lib's `inject_meta_charset` filter replaced it; the Rust port serves
    /// the template's `UTF-8`. Both are the same declaration.
    #[test]
    fn meta_charset_case_does_not_matter() {
        assert_eq!(
            describe(r#"<meta charset="UTF-8">"#),
            describe(r#"<meta charset="utf-8">"#)
        );
    }

    /// The normalisation is case-only, so a genuinely different encoding is
    /// still a difference. Without this, "forgive the case" could be read as
    /// "ignore the attribute".
    #[test]
    fn a_different_charset_is_still_a_difference() {
        assert_ne!(
            describe(r#"<meta charset="utf-8">"#),
            describe(r#"<meta charset="iso-8859-1">"#)
        );
    }

    /// And it is `<meta charset>` only. Attribute values are otherwise compared
    /// verbatim — `class` most of all, since that is what an article's
    /// presentation is made of.
    #[test]
    fn other_attribute_values_still_compare_exactly() {
        assert_ne!(
            describe(r#"<p class="font-bold">x</p>"#),
            describe(r#"<p class="Font-Bold">x</p>"#)
        );
        assert_ne!(
            describe(r#"<div charset="UTF-8">x</div>"#),
            describe(r#"<div charset="utf-8">x</div>"#)
        );
    }

    /// The reason markers exist: an element with no text must still be
    /// compared, or a wrong image source would pass silently.
    #[test]
    fn a_textless_element_is_still_compared() {
        assert_eq!(describe(r#"<img src="a">"#).len(), 1);
        assert_ne!(describe(r#"<img src="a">"#), describe(r#"<img src="b">"#));
        assert_ne!(
            describe(r#"<iframe src="a"></iframe>"#),
            describe(r#"<iframe src="b"></iframe>"#)
        );
    }

    /// And a wrapper one side has and the other does not is a difference even
    /// when the wrapper carries no attributes of its own — the marker's chain
    /// names every ancestor, so the extra `div` shows up.
    #[test]
    fn a_bare_wrapper_around_a_textless_element_is_a_difference() {
        assert_ne!(
            describe(r#"<div class="mt-7"><iframe src="a"></iframe></div>"#),
            describe(r#"<div class="mt-7"><div><iframe src="a"></iframe></div></div>"#)
        );
    }

    /// A marker keeps its place in the sequence, so an image that moved is a
    /// difference.
    #[test]
    fn a_marker_keeps_its_place_in_the_sequence() {
        assert_ne!(
            describe(r#"<img src="a"><p>x</p>"#),
            describe(r#"<p>x</p><img src="a">"#)
        );
    }

    /// An attribute *value* still matters, which is the whole point of keeping
    /// them.
    #[test]
    fn a_different_class_is_a_difference() {
        assert_ne!(
            describe("<p class=\"mt-3\">x</p>"),
            describe("<p class=\"mt-7\">x</p>")
        );
    }

    /// Template indentation is not output.
    #[test]
    fn whitespace_only_text_nodes_are_dropped() {
        assert_eq!(describe("\n    <div>x</div>\n  "), describe("<div>x</div>"));
    }

    /// But whitespace inside a text node is not touched — a code block that lost
    /// a newline must still fail.
    #[test]
    fn whitespace_inside_a_text_node_is_kept() {
        assert_ne!(describe("<pre>a\nb</pre>"), describe("<pre>ab</pre>"));
    }

    /// Two nestings of the same annotations render the same, so they agree.
    #[test]
    fn nestings_of_the_same_annotations_agree() {
        assert_eq!(
            describe(r#"<strong><em>x</em></strong>"#),
            describe(r#"<em><strong>x</strong></em>"#)
        );
    }

    /// But a genuinely different annotation is a difference, which is what keeps
    /// the set comparison from being vacuous.
    #[test]
    fn a_different_annotation_is_a_difference() {
        assert_ne!(describe("<strong>x</strong>"), describe("<em>x</em>"));
        assert_ne!(
            describe(r#"<a href="a">x</a>"#),
            describe(r#"<a href="b">x</a>"#)
        );
    }

    #[test]
    fn comments_are_ignored() {
        assert_eq!(describe("<p>x</p><!-- note -->"), describe("<p>x</p>"));
    }

    #[test]
    fn empty_input_has_no_nodes() {
        assert!(canonicalize("").is_empty());
    }
}
