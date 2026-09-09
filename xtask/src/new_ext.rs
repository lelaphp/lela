//! `xtask new-ext <name>` (harness-design.md §5, §11; design §4.10; ADR-0004,
//! ADR-0005): one extension crate laid down in the template topology, so
//! the shape is made by the harness and not copied by hand. The crate has
//! one dependency, the facade; one entry, `extension()`; its state on the
//! value; a test through the `Capture` sink; a witness in rustdoc with the
//! bare `exit` opt-in for `xtask witness --accept` to fill (design §8); the
//! lints the manifest denies under `ext/*` at its root. The
//! generated `ext/clippy.toml` is rewritten so it covers the new crate, the
//! crate joins the workspace members, and the `extensions` region of
//! `ext/AGENTS.md` is rendered. Nothing is measured here: the readings are
//! `budget`'s, `unsafe-audit`'s and `api-check`'s, which the crate passes
//! untouched.
//!
//! Three refusals, each before anything is written: a name with no
//! `[[crate]]` row (the ceiling is the manifest's, and the row is a
//! person's), a workspace without `crates/php_ext` (the one dependency
//! must exist), and a directory already at `ext/<name>` (the crate is laid
//! down once and never overwritten).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::computed;
use crate::generated;
use crate::manifest::{Manifest, Rule};
use crate::phpt;

/// The facade every extension depends on (ADR-0004), relative to the root.
const FACADE: &str = "crates/php_ext";

/// The per-directory guidance whose `extensions` region lists the crates.
const GUIDANCE: &str = "ext/AGENTS.md";

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
    if manifest.crate_named(&row).is_none() {
        bail!(
            "seams.toml has no [[crate]] row for {row}, so the crate would have no ceiling; the row is a person's (harness §11): propose it in the commit message"
        );
    }
    if !root.join(FACADE).join("Cargo.toml").is_file() {
        bail!("{FACADE} is not a crate yet; the facade lands before any extension (ADR-0004)");
    }
    let dir = root.join(&row);
    if dir.exists() {
        bail!("{row} exists; new-ext lays a crate down once and never overwrites");
    }

    let mut laid = Laid {
        dir: dir.clone(),
        written: Vec::new(),
        notes: Vec::new(),
    };
    for (rel, text) in [
        ("Cargo.toml", cargo_toml(name)),
        ("src/lib.rs", lib_rs(manifest, name)),
        ("tests/capture.rs", capture_rs(name)),
    ] {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        laid.written.push(format!("{row}/{rel}"));
    }
    if add_member(root, &row)? {
        laid.written.push("Cargo.toml".to_owned());
    }
    for path in generated::write_all(root, manifest)? {
        laid.written.push(relative(root, &path));
    }
    render_guidance(root, &mut laid)?;

    // Which subsets are the crate's is the `[[crate]]` row's sentence and
    // is read there, never spelled again here (#18, harness §4.1).
    let krate = manifest.crate_named(&row);
    let owned: Vec<String> = krate
        .map(|krate| {
            krate
                .subsets()
                .iter()
                .map(|under| format!("{}/{}", phpt::CORPUS, under.display()))
                .collect()
        })
        .unwrap_or_default();
    let named: Vec<&str> = owned.iter().map(String::as_str).collect();
    let subset = phpt::listed(&named);
    if krate.is_some_and(|krate| phpt::Subset::of(manifest, krate).is_some()) {
        laid.notes
            .push(format!("phpt subset: {subset} at the pinned tag"));
    } else {
        laid.notes.push(format!(
            "phpt subset: {subset} is not in the pinned php-src tree ({}); `xtask phpt --subset` has nothing to run",
            manifest.pin.source.tag
        ));
    }
    laid.notes.push(format!(
        "left: the `php_ext_{name}` line in php_bundle's manifest and its entry in `enabled()`, in module-startup order (ADR-0004); the witness's verdict through `xtask witness --accept`"
    ));
    Ok(laid)
}

/// The crate's manifest: the facade as its one dependency, the workspace's
/// lints and package fields.
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
         php_ext = {{ path = \"../../{FACADE}\" }}\n"
    )
}

/// The crate root: `extension()` with its request state declared on it,
/// and one witness asking the oracle whether the extension is loaded.
fn lib_rs(manifest: &Manifest, name: &str) -> String {
    let mut out = format!(
        "//! The {name} extension: `php-src/ext/{name}` as one crate under `ext/`\n\
         //! (ADR-0004). One dependency, the facade; one entry, `extension()`; its\n\
         //! state on the value (ADR-0005). The phpt subset is\n\
         //! `php-src/ext/{name}/tests`, through `xtask phpt --subset`.\n\
         #![forbid(unsafe_code)]\n"
    );
    for rule in denied_lints(manifest, name) {
        let _ = writeln!(out, "// seams.toml rule {}: {}", rule.id, rule.text);
        let lints = rule.predicate.deny_lints.iter().flatten().cloned();
        let _ = writeln!(out, "#![deny({})]", lints.collect::<Vec<_>>().join(", "));
    }
    out.push_str(
        "// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).\n\
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
    let _ = writeln!(out, "/// var_dump(extension_loaded('{name}'));");
    out.push_str(
        "/// ```\n\
         /// ```expect exit\n\
         /// bool(true)\n\
         /// ```\n\
         pub fn extension() -> Extension {\n\
         \x20   ExtensionBuilder::default()\n\
         \x20       .request_state::<State>()\n",
    );
    let _ = writeln!(out, "        .build(\"{name}\")");
    out.push_str("}\n");
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

/// The manifest's `deny_lints` rules whose scope reaches `ext/<name>`.
fn denied_lints<'a>(manifest: &'a Manifest, name: &str) -> Vec<&'a Rule> {
    let own = format!("ext/{name}");
    manifest
        .rules
        .iter()
        .filter(|r| r.predicate.deny_lints.is_some())
        .filter(|r| {
            r.scope.iter().any(|scope| {
                scope == own || scope == "ext/*" || scope.strip_suffix("/**") == Some(&own)
            })
        })
        .collect()
}

/// Add `ext/<name>` to the root manifest's `[workspace] members` unless a
/// member or a glob already covers it; whether the file changed. The edit
/// is textual, so the comments stay.
fn add_member(root: &Path, row: &str) -> Result<bool> {
    let path = root.join("Cargo.toml");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("{}", path.display()))?;
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
    let mut at = 0;
    let start = text
        .lines()
        .map(|line| {
            let here = at;
            at += line.len() + 1;
            (here, line)
        })
        .find(|(_, line)| {
            line.trim_start()
                .strip_prefix("members")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
        .map(|(here, _)| here)
        .context("the root Cargo.toml has no `members =` line")?;
    let open = start + text[start..].find('[').context("members has no `[`")?;
    let close = open + text[open..].find(']').context("members has no `]`")?;
    let inside = &text[open + 1..close];
    let last = inside.trim_end();
    let comma = if last.is_empty() || last.ends_with(',') {
        ""
    } else {
        ","
    };
    let (at, insert) = if inside.contains('\n') {
        // One member per line: after the last one, with its indent.
        let indent = inside
            .lines()
            .find(|l| !l.trim().is_empty())
            .map_or("    ", |l| &l[..l.len() - l.trim_start().len()]);
        (
            open + 1 + last.len(),
            format!("{comma}\n{indent}\"{row}\","),
        )
    } else if last.is_empty() {
        (close, format!("\"{row}\""))
    } else {
        (open + 1 + last.len(), format!("{comma} \"{row}\""))
    };
    let mut out = text.clone();
    out.insert_str(at, &insert);
    fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
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

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::add_member;

    fn root(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xtask-new-ext-member-{name}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
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
