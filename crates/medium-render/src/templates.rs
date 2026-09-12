//! The page templates, embedded, with the escaping turned off.
//!
//! Six files, byte-identical copies of `legacy/web/server/templates/*.html`.
//! The copies are not symlinks and `legacy/` is not read at runtime: the
//! differential harness renders the *legacy* copies through Jinja2 and this crate
//! renders *these* through minijinja, and comparing the two byte for byte is what
//! catches the copies drifting apart. A symlink would make that comparison
//! vacuous — it would be one file compared against itself.
//!
//! # Autoescape must be off, and minijinja's default is on
//!
//! `server/services/jinja.py:5` builds `Environment(loader=...)` with no
//! `autoescape` argument, and Jinja2's default for a bare `Environment()` is
//! `False`. Every value the templates interpolate is escaped by hand *before* it
//! gets there — see [`medium_doc::metadata`] — and the body is HTML that must not
//! be escaped again.
//!
//! minijinja disagrees with that default: it auto-escapes `.html` templates. So
//! the callback is overridden for every template, not just the ones that look
//! dangerous. Left at the default, `{{ body_template }}` in `base.html` would
//! turn the entire page into visible source, and `{{ paragraph }}` would do the
//! same to the article.
//!
//! # Why the legacy two-pass render is not reproduced
//!
//! `services/jinja.py:9-17` renders `main.html` and `error.html` *twice*: once
//! with `DebugUndefined` and no context, then compiles the resulting string with
//! the ordinary environment. The only thing that survives the first pass is a
//! literal `{{ postleter }}` or `{{ error_msg }}` — `DebugUndefined` prints the
//! expression instead of the empty string — and the `{% include 'url_box.html' %}`
//! gets expanded.
//!
//! `url_box.html` interpolates nothing, and neither `main.html` nor `error.html`
//! has an expression that needs to survive a first pass, so a single pass with
//! the real context produces the same bytes. The tests below assert exactly that,
//! against Jinja2's own output, rather than trusting the argument.

use minijinja::{AutoEscape, Environment, UndefinedBehavior};

/// The shared environment: every template, autoescape off, lenient undefined.
///
/// One environment per call, which is what the templates need and no more.
/// `freedium-web` builds it once at boot and keeps it in its state; the
/// differential harness builds one per case. Construction is cheap — the
/// templates are already compiled into the binary.
pub fn environment() -> Environment<'static> {
    let mut env = Environment::new();

    // Before the templates are loaded, not after. minijinja resolves a
    // template's escape mode when it is *compiled*, so a callback installed
    // afterwards never runs for the embedded templates — they keep the `.html`
    // default and the page comes out with its own markup visible as text.
    env.set_auto_escape_callback(|_| AutoEscape::None);

    // The other default that disagrees with Jinja2. Jinja2's bare
    // `Environment()` uses `Undefined`, which prints as the empty string, is
    // falsy in a condition, and **raises on further attribute access**:
    // `{{ a.b.c }}` with `b` missing gives `a.b` as an `Undefined` and then
    // `Undefined.__getattr__("c")` raises, which is what `homepage.html:24`
    // does with `{{ post.collection.avatar.id }}`.
    //
    // minijinja has four settings near that and only one matches. `Lenient` is
    // it — its "attribute access of undefined values: fails" line is exactly
    // Jinja2's `Undefined`. `Chainable` (which allows the chain to keep
    // returning undefined) is Jinja2's *`ChainableUndefined`*, a different
    // class that `Environment()` does not use; reaching for it here would make
    // this side render a page where production returns a 500, which is a
    // divergence the Fase 4 mirror would report as Rust being "better" on a
    // payload Python cannot serve. Parity means raising too.
    //
    // The matrix that pins this — missing key, explicit `null`, non-dict parent,
    // empty object — is compared against real Jinja2 by the throwaway harness in
    // this phase and by `xtask/difftest` afterwards.
    env.set_undefined_behavior(UndefinedBehavior::Lenient);

    minijinja_embed::load_templates!(&mut env);

    env
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::environment;

    /// The `*.html` names in `dir`, sorted.
    fn html_files(dir: &Path) -> Vec<String> {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()));
        let mut names: Vec<String> = entries
            .map(|entry| {
                entry
                    .unwrap_or_else(|err| panic!("cannot read {}: {err}", dir.display()))
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.ends_with(".html"))
            .collect();
        names.sort();
        names
    }

    /// The anti-drift gate for the six template copies.
    ///
    /// `legacy/web/server/templates/` is the original: it is what the Python
    /// server serves and what `xtask/difftest`'s reference renders through
    /// Jinja2. This crate carries copies so the Rust binary needs no template
    /// directory at runtime, and a copy is a second thing that can be wrong. The
    /// gate compares the two directly, for the same reason `medium-client`'s
    /// `the_query_matches_the_legacy_client` compares `query.graphql` to
    /// `api.py:59`: a duplicated source is tolerable exactly as long as something
    /// fails when the two disagree.
    ///
    /// Two checks, because they catch different mistakes. The set comparison
    /// catches a template *added* to one side and forgotten on the other — which
    /// the byte comparison cannot see, since it only visits names that exist. The
    /// byte comparison catches an edit.
    ///
    /// The bytes compared are the **embedded** source, from
    /// `env.get_template(name).source()`, not a re-read of the file in this
    /// crate's `templates/`. That is the string `minijinja_embed::load_templates!`
    /// actually compiles, so the test verifies what the binary renders rather than
    /// what happens to sit next to it.
    ///
    /// This test is scaffolding, and it dies with its subject: Fase 5 deletes
    /// `legacy/`, and then the only correct thing to do with this function is
    /// delete it too. Until then, `legacy/` is not going anywhere.
    #[test]
    fn the_copies_have_not_drifted_from_the_originals() {
        let ours_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let legacy_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/web/server/templates");

        let ours = html_files(&ours_dir);
        let theirs = html_files(&legacy_dir);
        assert_eq!(
            ours,
            theirs,
            "{} and {} no longer hold the same templates — one side has a file the \
             other does not",
            ours_dir.display(),
            legacy_dir.display()
        );

        let env = environment();
        for name in &ours {
            let path = legacy_dir.join(name);
            let original = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
            let embedded = env
                .get_template(name)
                .unwrap_or_else(|err| panic!("{name} is not embedded: {err}"))
                .source()
                .to_string();
            assert_eq!(
                embedded,
                original,
                "{name} has drifted from {}; the parity gate renders the legacy copy \
                 through Jinja2 and the embedded one through minijinja, so the two \
                 must stay the same file",
                path.display()
            );
        }
    }
}
