//! `xtask new-ext <name>` (harness-design.md §5, §11; design §4.10; ADR-0004,
//! ADR-0005): one extension crate laid down in the template topology, so
//! the shape is made by the harness and not copied by hand. The crate has
//! one dependency, the facade; one entry, `extension()`; its state on the
//! value; a test through the `Capture` sink; a witness in rustdoc whose
//! `expect` fence names no `exit`, which asserts zero, the status the
//! snippet has, so `--accept` has nothing to write and the crate carries
//! no `exit=` (design §8); the lints the manifest denies under `ext/*` at
//! its root. The
//! generated `ext/clippy.toml` is rewritten so it covers the new crate, the
//! crate joins the workspace members, the bundle gains the crate as an
//! optional dependency behind a feature of its name and one line in
//! `enabled()` (ADR-0004; design §4.10), and the `extensions` region of
//! `ext/AGENTS.md` is rendered. Nothing is measured here: the readings are
//! `budget`'s, `unsafe-audit`'s and `api-check`'s, which the crate passes
//! untouched, and `api-check` holds the bundle's order.
//!
//! What the template is exercised on is the fixture at
//! `xtask/fixtures/new-ext`, whose own `seams.toml` carries the `[[crate]]`
//! rows for `ext/demo` and `ext/other`: `cargo test -p xtask --test new_ext`
//! lays `ext/demo` down there and holds it to those three instruments, the
//! shipped manifest carrying no row for a name nobody ships. The generated
//! `tests/capture.rs` is held there as text, the fixture's facade being
//! empty by its own line; it is run where a real extension stands instead,
//! `cargo test -p php_ext_<name> --test capture` over the twenty ported
//! crates (harness-design.md §5).
//!
//! Four refusals, each before anything is written: a name with no
//! `[[crate]]` row (the ceiling is the manifest's, and the row is a
//! person's), a workspace without `crates/php_ext` (the one dependency
//! must exist), one without `crates/php_bundle` (the crate the extension
//! reaches), and a directory already at `ext/<name>` (the crate is laid
//! down once and never overwritten).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::computed;
use crate::generated;
use crate::manifest::{Crate, Manifest, Rule};
use crate::phpt;
use crate::reading::listed;

/// The facade every extension depends on (ADR-0004), relative to the root.
const FACADE: &str = "crates/php_ext";

/// The per-directory guidance whose `extensions` region lists the crates.
const GUIDANCE: &str = "ext/AGENTS.md";

/// The enabled set every extension reaches (ADR-0004), relative to the root.
pub(crate) const BUNDLE: &str = "crates/php_bundle";

/// What one run laid down.
#[derive(Debug)]
pub struct Laid {
    /// The crate's directory, absolute.
    pub dir: PathBuf,
    /// Every file written or rewritten, relative to the root, in order.
    pub written: Vec<String>,
    /// What is left, or what was found: one line each.
    pub notes: Vec<String>,
}

impl Laid {
    /// `files` written at their paths under `dir`, the crate `row` of the
    /// workspace at `root`, and the crate made one of its members.
    pub(crate) fn files<const N: usize>(
        root: &Path,
        dir: PathBuf,
        row: &str,
        files: [(&str, String); N],
    ) -> Result<Laid> {
        let mut written = Vec::new();
        for (rel, text) in files {
            let path = dir.join(rel);
            crate::make_parent(&path)?;
            crate::write(&path, text)?;
            written.push(format!("{row}/{rel}"));
        }
        if add_member(root, row)? {
            written.push("Cargo.toml".to_owned());
        }
        let notes = Vec::new();
        Ok(Laid {
            dir,
            written,
            notes,
        })
    }
}

/// One file's edit, computed before it is made: the text the file is to
/// hold and one line saying what changed in it.
struct Edit {
    path: PathBuf,
    before: String,
    after: String,
    note: String,
}

impl Edit {
    /// Write the file when the edit changed it; whether it did.
    fn write(&self) -> Result<bool> {
        if self.after == self.before {
            return Ok(false);
        }
        crate::write(&self.path, &self.after)?;
        Ok(true)
    }
}

/// The refusals before anything is written, as `instrument` lays down a
/// crate at `row` beside `facade` and the bundle: no `[[crate]]` row (the
/// ceiling is the manifest's, and the row is a person's), the facade or
/// the bundle not yet a crate, or the crate's directory already there.
/// The row's crate and the directory otherwise.
pub(crate) fn unrefused<'m>(
    root: &Path,
    manifest: &'m Manifest,
    row: &str,
    facade: &str,
    instrument: &str,
    one: &str,
) -> Result<(&'m Crate, PathBuf)> {
    let Some(krate) = manifest.crate_named(row) else {
        bail!(
            "seams.toml has no [[crate]] row for {row}, so the crate would have no ceiling; the row is a person's (harness §11): propose it in the commit message"
        );
    };
    for (dir, what) in [(facade, "the facade"), (BUNDLE, "the bundle")] {
        if !root.join(dir).join("Cargo.toml").is_file() {
            bail!("{dir} is not a crate yet; {what} lands before any {one}");
        }
    }
    let dir = root.join(row);
    if dir.exists() {
        bail!("{row} exists; {instrument} lays a crate down once and never overwrites");
    }
    Ok((krate, dir))
}

/// The phpt subsets the `[[crate]]` row names, under the pinned corpus:
/// the row's sentence, never spelled again here (#18, harness §4.1).
pub(crate) fn subsets(krate: &Crate) -> Vec<String> {
    (krate.subsets().iter())
        .map(|under| format!("{}/{}", phpt::CORPUS, under.display()))
        .collect()
}

/// Lay `ext/<name>` down, or refuse and write nothing.
pub fn run(root: &Path, manifest: &Manifest, name: &str) -> Result<Laid> {
    if name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        bail!(
            "{name}: an extension name is lowercase ascii letters, digits and `_`, as php-src spells it"
        );
    }
    let row = format!("ext/{name}");
    let (krate, dir) = unrefused(
        root,
        manifest,
        &row,
        FACADE,
        "new-ext",
        "extension (ADR-0004)",
    )?;
    // The bundle's two edits are computed here, so a bundle whose shape the
    // edit cannot read refuses before anything is written; they are
    // written after the crate is.
    let feature = bundle_feature(root, name)?;
    let line = bundle_line(root, name)?;

    let files = [
        ("Cargo.toml", cargo_toml(name)),
        ("src/lib.rs", lib_rs(manifest, name)),
        ("tests/capture.rs", capture_rs(name)),
    ];
    let mut laid = Laid::files(root, dir, &row, files)?;
    for edit in [feature, line] {
        if edit.write()? {
            laid.written.push(relative(root, &edit.path));
        }
        laid.notes.push(edit.note);
    }
    for path in generated::write_all(root, manifest)? {
        laid.written.push(relative(root, &path));
    }
    render_guidance(root, &mut laid)?;

    let owned = subsets(krate);
    let subset = listed(&owned);
    laid.notes.push(if owned.is_empty() {
        format!(
            "phpt subset: {subset} is not in the pinned php-src tree ({}); `xtask phpt --subset` has nothing to run",
            manifest.pin.source.tag
        )
    } else {
        format!("phpt subset: {subset} at the pinned tag")
    });
    laid.notes
        .push("left: nothing of the witness; `xtask witness` holds it to the oracle".to_owned());
    Ok(laid)
}

/// The crate's manifest: the facade as its one dependency, the same crate
/// with its `test` feature for the tests alone, the workspace's lints and
/// package fields.
fn cargo_toml(name: &str) -> String {
    format!(
        "# ext/{name}: one crate per extension (ADR-0004), laid down by `xtask new-ext`.\n\
         [package]\n\
         name = \"php_ext_{name}\"\n\
         version = \"0.0.0\"\n\
         edition.workspace = true\n\
         license.workspace = true\n\
         rust-version.workspace = true\n\
         publish = false\n\
         description = \"The {name} extension, php-src/ext/{name}\"\n\
         \n\
         [lints]\n\
         workspace = true\n\
         \n\
         # The one dependency: the facade. The engine is out of reach (`[[edge]]` ext/*).\n\
         [dependencies]\n\
         php_ext = {{ path = \"../../{FACADE}\" }}\n\
         \n\
         # The test face, `php_ext::test`, for tests/ alone (design §4.3 seam 4).\n\
         [dev-dependencies]\n\
         php_ext = {{ path = \"../../{FACADE}\", features = [\"test\"] }}\n"
    )
}

/// The crate root: `extension()` with its request state declared on it,
/// and one witness asking the oracle whether the extension is loaded.
pub(crate) fn lib_rs(manifest: &Manifest, name: &str) -> String {
    let mut out = format!(
        "//! The `{name}` extension: `php-src/ext/{name}` as one crate under `ext/`\n\
         //! (ADR-0004). One dependency, the facade; one entry, `extension()`; its\n\
         //! state on the value (ADR-0005). The phpt subset is every `.phpt`\n\
         //! under `php-src/ext/{name}`, through `xtask phpt --subset`.\n\
         #![forbid(unsafe_code)]\n"
    );
    out.push_str(&lint_attributes(manifest, &format!("ext/{name}")));
    out.push_str(
        "// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).\n\
         #![allow(rustdoc::invalid_codeblock_attributes)]\n\
         \n\
         use php_ext::abi::{Extension, ExtensionBuilder};\n\
         \n\
         /// Request-scoped: built at request start, dropped after `rshutdown`,\n\
         /// private to this crate (ADR-0005); nothing in it is shared or synchronised.\n\
         #[derive(Default)]\n\
         struct State {}\n\
         \n\
         /// The extension value (design §7.1): declarations paired with entries,\n\
         /// and the state declared on it.\n\
         ///\n\
         /// ```php\n",
    );
    writeln!(out, "/// var_dump(extension_loaded('{name}'));").ok();
    out.push_str(
        "/// ```\n\
         /// ```expect\n\
         /// bool(true)\n\
         /// ```\n\
         pub fn extension() -> Extension {\n\
         \x20   ExtensionBuilder::default()\n\
         \x20       .request_state::<State>()\n",
    );
    writeln!(out, "        .build(\"{name}\")").ok();
    out.push_str("}\n");
    out
}

/// The crate root's `#![deny(..)]` for each of the manifest's
/// `deny_lints` rules whose scope reaches the crate named `row`, each under
/// the comment naming its rule: what a crate laid down by `new-ext` or
/// `new-sapi` carries, as the moved crates carry it by hand.
pub(crate) fn lint_attributes(manifest: &Manifest, row: &str) -> String {
    let mut out = String::new();
    for rule in denied_lints(manifest, row) {
        writeln!(out, "// seams.toml rule {}: {}", rule.id, rule.text).ok();
        let lints: Vec<String> = rule
            .predicate
            .deny_lints
            .iter()
            .flatten()
            .cloned()
            .collect();
        out.push_str(&deny(&lints));
    }
    out
}

/// One `#![deny(..)]` attribute in the shape `cargo fmt --check` holds
/// the file to: one line while it fits the workspace's width, and one
/// lint a line, the last without its comma, when it does not. The crate
/// is laid down formatted, since pre-commit runs `cargo fmt --check`
/// over it and the template is written and not formatted.
fn deny(lints: &[String]) -> String {
    /// rustfmt's `max_width`, the workspace's default.
    const WIDTH: usize = 100;
    let line = format!("#![deny({})]\n", lints.join(", "));
    if line.len() - 1 <= WIDTH {
        return line;
    }
    let mut out = String::from("#![deny(\n");
    for (at, lint) in lints.iter().enumerate() {
        let comma = if at + 1 == lints.len() { "" } else { "," };
        writeln!(out, "    {lint}{comma}").ok();
    }
    out.push_str(")]\n");
    out
}

/// The integration test: a fresh engine with this crate alone and the
/// `Capture` sink (design §4.10, ADR-0002); never the bundle.
fn capture_rs(name: &str) -> String {
    format!(
        "//! The extension through the `Capture` sink (design §4.10, ADR-0002): a\n\
         //! fresh engine with this crate alone, never the bundle.\n\
         \n\
         use php_ext::test::{{Capture, Engine}};\n\
         \n\
         #[test]\n\
         fn the_extension_is_loaded() {{\n\
         \x20   let engine = Engine::builder()\n\
         \x20       .extensions(&[php_ext_{name}::extension()])\n\
         \x20       .build();\n\
         \x20   let sink = Capture::default();\n\
         \x20   let served = engine.serve_capture(\"<?php var_dump(extension_loaded('{name}'));\", sink);\n\
         \x20   assert_eq!(served.stdout, b\"bool(true)\\n\");\n\
         }}\n"
    )
}

/// The manifest's `deny_lints` rules whose scope reaches the crate named
/// `row` (`ext/<name>`, `sapi/<name>`): the row itself, its directory's
/// glob (`ext/*`), or everything under it.
fn denied_lints<'a>(manifest: &'a Manifest, row: &str) -> Vec<&'a Rule> {
    let glob = row.split_once('/').map(|(dir, _)| format!("{dir}/*"));
    manifest
        .rules
        .iter()
        .filter(|r| r.predicate.deny_lints.is_some())
        .filter(|r| {
            r.scope.iter().any(|scope| {
                scope == row
                    || glob.as_deref() == Some(scope)
                    || scope.strip_suffix("/**") == Some(row)
            })
        })
        .collect()
}

/// Add `ext/<name>` to the root manifest's `[workspace] members` unless a
/// member or a glob already covers it; whether the file changed. The edit
/// is textual, so the comments stay.
fn add_member(root: &Path, row: &str) -> Result<bool> {
    let path = root.join("Cargo.toml");
    let (text, table) = (crate::read(&path)?, crate::read_toml::<toml::Table>(&path)?);
    let members = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(toml::Value::as_array)
        .context("the root Cargo.toml has no [workspace] members")?;
    let covered = members.iter().filter_map(toml::Value::as_str).any(|m| {
        m == row
            || m.strip_suffix("/*")
                .is_some_and(|prefix| row.starts_with(prefix))
    });
    if covered {
        return Ok(false);
    }
    // The `members =` line itself, not the word in a comment above it.
    let start = key_line(&text, "workspace", "members")
        .context("the root Cargo.toml has no `members =` line")?;
    let (at, insert) = push_array(&text, start, row)?;
    let mut out = text.clone();
    out.insert_str(at, &insert);
    crate::write(&path, out)?;
    Ok(true)
}

/// Where and what to insert so `item` joins the array whose `[` follows
/// `start`: after the last element with its comma and, in a one-per-line
/// list, on its own line with the list's indent.
fn push_array(text: &str, start: usize, item: &str) -> Result<(usize, String)> {
    let open = start + text[start..].find('[').context("no `[`")?;
    let close = open + text[open..].find(']').context("no `]`")?;
    let inside = &text[open + 1..close];
    let last = inside.trim_end();
    let comma = if last.is_empty() || last.ends_with(',') {
        ""
    } else {
        ","
    };
    Ok(if inside.contains('\n') {
        let indent = indent_of(inside).unwrap_or("    ");
        (
            open + 1 + last.len(),
            format!("{comma}\n{indent}\"{item}\","),
        )
    } else if last.is_empty() {
        (close, format!("\"{item}\""))
    } else {
        (open + 1 + last.len(), format!("{comma} \"{item}\""))
    })
}

/// The indent of the first line of `inside` that is not blank.
fn indent_of(inside: &str) -> Option<&str> {
    let line = inside.lines().find(|l| !l.trim().is_empty())?;
    Some(&line[..line.len() - line.trim_start().len()])
}

/// The offset of the `key =` line inside the `[table]` of a TOML text,
/// found textually so the word in a comment is not the line.
fn key_line(text: &str, table: &str, key: &str) -> Option<usize> {
    let mut inside = false;
    for (here, line) in offsets(text) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = header(trimmed) == format!("[{table}]");
            continue;
        }
        if inside
            && trimmed
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            return Some(here);
        }
    }
    None
}

/// Each line of `text` with the offset it starts at, a line ending
/// taken for one byte.
fn offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().scan(0, |at, line| {
        let here = *at;
        *at += line.len() + 1;
        Some((here, line))
    })
}

/// A TOML header line without the comment after it.
fn header(line: &str) -> &str {
    line.split('#').next().unwrap_or(line).trim()
}

/// The offset just after the last entry of the `[table]` of a TOML text,
/// where one more line goes; blank lines and the comment above the next
/// header stay with the next header.
fn table_end(text: &str, table: &str) -> Option<usize> {
    let mut end = None;
    for (here, line) in offsets(text) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if end.is_some() {
                break;
            }
            if header(trimmed) == format!("[{table}]") {
                end = Some(here + line.len());
            }
            continue;
        }
        if end.is_some() && !trimmed.is_empty() && !trimmed.starts_with('#') {
            end = Some(here + line.len());
        }
    }
    end
}

/// The features of the bundle a new extension is listed in: `default`, the
/// build a person prunes a non-default extension from when porting it
/// (design §4.10), and `host`, the set every host links, which
/// `one_thread` and `linked` are built of (#3081), so a branch that adds an
/// extension links it into the hosts and `php-linked` in the same commit,
/// its merge breaks no harness test (#1252), and the pruning leaves it
/// linked.
const LISTS: [&str; 2] = ["default", "host"];

/// The bundle's manifest gains `php_ext_<name>` as an optional path
/// dependency and the feature `<name> = ["dep:php_ext_<name>"]`, listed in
/// each of [`LISTS`]. The edit is textual, so the comments stay; nothing is
/// written here.
fn bundle_feature(root: &Path, name: &str) -> Result<Edit> {
    let path = root.join(BUNDLE).join("Cargo.toml");
    let (text, table) = (crate::read(&path)?, crate::read_toml::<toml::Table>(&path)?);
    let package = format!("php_ext_{name}");
    let has = |section: &str, key: &str| {
        table
            .get(section)
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.contains_key(key))
    };
    let mut out = text.clone();
    let mut did: Vec<String> = Vec::new();
    if !has("features", name) {
        let at = table_end(&out, "features")
            .with_context(|| format!("{} has no [features] table", path.display()))?;
        out.insert_str(at, &format!("\n{name} = [\"dep:{package}\"]"));
        did.push(format!("the feature {name} = [\"dep:{package}\"]"));
    }
    for list in LISTS {
        let listed = table
            .get("features")
            .and_then(|f| f.get(list))
            .and_then(toml::Value::as_array)
            .is_some_and(|d| d.iter().any(|v| v.as_str() == Some(name)));
        if !listed {
            let start = key_line(&out, "features", list)
                .with_context(|| format!("{} has no `{list} =` line", path.display()))?;
            let (at, insert) = push_array(&out, start, name)?;
            out.insert_str(at, &insert);
            did.push(format!("in {list}"));
        }
    }
    if !has("dependencies", &package) {
        let at = table_end(&out, "dependencies")
            .with_context(|| format!("{} has no [dependencies] table", path.display()))?;
        out.insert_str(
            at,
            &format!("\n{package} = {{ path = \"../../ext/{name}\", optional = true }}"),
        );
        did.push(format!("{package} as an optional dependency at ext/{name}"));
    }
    let note = format!(
        "{BUNDLE}/Cargo.toml: {}",
        if did.is_empty() {
            format!("already carries the feature {name} and {package}")
        } else {
            did.join(", ")
        }
    );
    Ok(Edit {
        path,
        before: text,
        after: out,
        note,
    })
}

/// The bundle's `enabled()` gains `php_ext_<name>::extension` behind
/// `#[cfg(feature = "<name>")]`, appended last, where the witness on
/// `enabled()` names it until a person moves the line to its place in
/// php's registration order (#1778). The tier's alias is never touched. The edit is
/// textual, so the comments stay; nothing is written here.
fn bundle_line(root: &Path, name: &str) -> Result<Edit> {
    let path = root.join(BUNDLE).join("src/lib.rs");
    let text = crate::read(&path)?;
    let package = format!("php_ext_{name}");
    let entry = format!("{package}::extension");
    let head = offsets(&text)
        .map(|(here, line)| (here, line.trim_start()))
        .find(|(_, line)| line.starts_with("pub fn enabled(") || line.starts_with("fn enabled("))
        .map(|(here, _)| here)
        .with_context(|| format!("{} has no `fn enabled(`", path.display()))?;
    let body = head + text[head..].find('{').context("enabled() has no body")?;
    let open = body
        + text[body..]
            .find("&[")
            .context("enabled() returns no slice")?
        + 1;
    let mut depth = 1usize;
    let mut close = None;
    for (i, c) in text[open + 1..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + 1 + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close.context("enabled()'s slice has no `]`")?;
    let inside = &text[open + 1..close];
    if inside.contains(&entry) {
        return Ok(Edit {
            path,
            after: text.clone(),
            before: text,
            note: format!("{BUNDLE}/src/lib.rs: enabled() already lists {entry}"),
        });
    }
    let line_start = text[..open].rfind('\n').map_or(0, |i| i + 1);
    let outer = &text[line_start..open];
    let outer = &outer[..outer.len() - outer.trim_start().len()];
    let last = inside.trim_end();
    let (at, insert) = if last.is_empty() {
        let indent = format!("{outer}    ");
        (
            close,
            format!("\n{indent}#[cfg(feature = \"{name}\")]\n{indent}{entry},\n{outer}"),
        )
    } else {
        let indent = indent_of(inside).map_or(format!("{outer}    "), str::to_owned);
        let comma = if last.ends_with(',') { "" } else { "," };
        (
            open + 1 + last.len(),
            format!("{comma}\n{indent}#[cfg(feature = \"{name}\")]\n{indent}{entry},"),
        )
    };
    let mut out = text.clone();
    out.insert_str(at, &insert);
    Ok(Edit {
        path,
        before: text,
        after: out,
        note: format!(
            "{BUNDLE}/src/lib.rs: enabled() gains {entry} behind feature {name}, last; api-check holds the order to the crate graph (ADR-0004)"
        ),
    })
}

/// The guidance file's regions rendered, read as [`computed::Answer`]:
/// the file is written when its extensions region was; an untrusted
/// checkout and a missing `computed` are notes, since the crate is laid
/// down either way; a broken marker is a refusal.
fn render_guidance(root: &Path, laid: &mut Laid) -> Result<()> {
    if !root.join(GUIDANCE).is_file() {
        return Ok(());
    }
    let files = [GUIDANCE.to_owned()];
    let answer = computed::render(root, &files)?;
    if let computed::Answer::BrokenMarker(said) = &answer {
        bail!("computed run {GUIDANCE}: {said}");
    }
    if let Some(rendered) = answer.rendered() {
        laid.written.extend(rendered.written.iter().cloned());
    }
    if let Some(refusal) = answer.refusal() {
        laid.notes.push(format!(
            "{GUIDANCE}: {refusal}; the extensions region is rendered by `computed run {GUIDANCE}`"
        ));
    }
    Ok(())
}

pub(crate) fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{add_member, deny, lib_rs};
    use crate::manifest::Manifest;

    /// The crate is laid down formatted: pre-commit runs `cargo fmt
    /// --check` over what is staged, and the six lints the manifest
    /// denies do not fit one line.
    #[test]
    fn the_denied_lints_are_wrapped_when_one_line_is_too_wide() {
        let short = ["clippy::todo".to_owned()];
        assert_eq!(deny(&short), "#![deny(clippy::todo)]\n");
        let six: Vec<String> = [
            "clippy::unwrap_used",
            "clippy::expect_used",
            "clippy::panic",
            "clippy::unreachable",
            "clippy::todo",
            "clippy::unimplemented",
        ]
        .iter()
        .map(|lint| (*lint).to_owned())
        .collect();
        assert_eq!(
            deny(&six),
            "#![deny(\n    clippy::unwrap_used,\n    clippy::expect_used,\n    clippy::panic,\n    \
             clippy::unreachable,\n    clippy::todo,\n    clippy::unimplemented\n)]\n"
        );
    }

    /// The verdict of a witness is captured and never authored
    /// (`witness.exit-authored`), and an `expect` fence with no `exit`
    /// asserts zero, which is the status of the template's snippet: so
    /// the crate is laid down with nothing for `--accept` to write, and
    /// `permit` over the diff reads no `exit=` in it.
    #[test]
    fn the_template_witness_pins_no_exit_for_accept_to_write() {
        let root = lib_rs(&Manifest::named("ext/standard"), "standard");
        let fence = root
            .lines()
            .find(|line| line.trim_start().starts_with("/// ```expect"))
            .unwrap();
        assert_eq!(fence, "/// ```expect", "{root}");
    }

    #[test]
    fn the_crate_root_names_the_extension_in_backticks() {
        let root = lib_rs(&Manifest::named("ext/pdo_mysql"), "pdo_mysql");
        let first = root.lines().next().unwrap();
        assert!(
            first.contains("`pdo_mysql`"),
            "clippy::doc_markdown reads a name with an `_` as an item unless the prose \
             backticks it, and the crate is laid down clippy-clean: {first}"
        );
    }

    fn root(name: &str, text: &str) -> crate::scratch::Tree {
        let dir = crate::scratch::Tree::new(&format!("new-ext-member-{name}")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), text).unwrap();
        dir
    }

    #[test]
    fn a_member_joins_a_one_line_list_and_a_glob_covers_it() {
        let dir = root(
            "one-line",
            "# The members are `crates/*`, `ext/*` and `xtask`; listed as created.\n\
             [workspace]\nmembers = [\"xtask\"] # the harness\n",
        );
        assert!(add_member(&dir, "ext/demo").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(),
            "# The members are `crates/*`, `ext/*` and `xtask`; listed as created.\n\
             [workspace]\nmembers = [\"xtask\", \"ext/demo\"] # the harness\n",
            "the word in the comment is not the list"
        );
        assert!(!add_member(&dir, "ext/demo").unwrap(), "already a member");
        let dir = root("glob", "[workspace]\nmembers = [\"crates/*\", \"ext/*\"]\n");
        assert!(!add_member(&dir, "ext/demo").unwrap(), "the glob covers it");
    }

    #[test]
    fn a_member_joins_a_multi_line_list_on_its_own_line() {
        let dir = root(
            "multi-line",
            "[workspace]\nmembers = [\n    \"xtask\",\n]\n",
        );
        assert!(add_member(&dir, "ext/demo").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(),
            "[workspace]\nmembers = [\n    \"xtask\",\n    \"ext/demo\",\n]\n"
        );
    }
}
