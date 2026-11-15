//! The `conditions` section: the conditions of done, read from
//! `done::Condition`'s own source, so the list under harness-design.md
//! §4.5 is what `done` judges and not a copy of it. Each item is
//! a variant's first rustdoc paragraph, numbered in the order the enum
//! declares them; a variant without one fails the rendering, naming it.

use std::fmt::Write;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use syn::Item;

use super::Table;
use super::invocation::doc;

/// The file the conditions are read from, relative to the workspace root:
/// the region's input beside this renderer.
const SOURCE: &str = "xtask/src/done.rs";
/// The type whose variants are the conditions.
const TYPE: &str = "Condition";

/// The section over the workspace at `root`, in `format`.
pub(super) fn render(root: &Path, format: Table) -> Result<String> {
    let text =
        fs::read_to_string(root.join(SOURCE)).with_context(|| format!("reading {SOURCE}"))?;
    let said = sentences(&text).with_context(|| format!("{SOURCE}: done::{TYPE}"))?;
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(&said)?,
        Table::Md => said.iter().zip(1..).fold(String::new(), |mut out, (s, n)| {
            writeln!(out, "{n}. {s}").ok();
            out
        }),
    })
}

/// Each variant's first rustdoc paragraph, its lines joined, in order.
fn sentences(source: &str) -> Result<Vec<String>> {
    let file = syn::parse_file(source).context("parsing the source")?;
    let Some(variants) = file.items.iter().find_map(|item| match item {
        Item::Enum(e) if e.ident == TYPE => Some(&e.variants),
        _ => None,
    }) else {
        bail!("no enum {TYPE}");
    };
    variants
        .iter()
        .map(|v| {
            let doc = doc(&v.attrs);
            let first = doc.trim().split("\n\n").next().unwrap_or_default();
            if first.is_empty() {
                bail!("the condition {} has no rustdoc to render", v.ident);
            }
            Ok(first.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sentences;

    /// Each variant renders as its first paragraph, its lines joined, and
    /// what follows the paragraph is the code's account and stays out.
    #[test]
    fn a_condition_is_its_variant_s_first_rustdoc_paragraph() {
        let source = "
            enum Other { A }
            enum Condition {
                /// The first, over
                /// two lines.
                ///
                /// Not rendered.
                Fresh,
                /// The second.
                Second,
            }
        ";
        assert_eq!(
            sentences(source).unwrap(),
            ["The first, over two lines.", "The second."]
        );
    }

    /// A variant with no rustdoc fails the rendering, naming it, so the
    /// list cannot gain a condition it does not say.
    #[test]
    fn a_condition_without_rustdoc_fails_naming_it() {
        let err = format!(
            "{:#}",
            sentences("enum Condition { /// Said.\nA, B }").unwrap_err()
        );
        assert!(err.contains("the condition B has no rustdoc"), "{err}");
        let err = format!("{:#}", sentences("enum Other {}").unwrap_err());
        assert!(err.contains("no enum Condition"), "{err}");
    }
}
