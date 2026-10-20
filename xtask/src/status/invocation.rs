//! The `invocation` section: the fields of `oracle::Invocation` past the
//! binary and the program, read from the type's own source, so the region
//! under harness-design.md §4.3 is the type and not a sentence about it
//! (#1305, #1319). Each row is a field's name, its type, which of §4.3's
//! two groups it is in and the first sentence of its rustdoc; the group is
//! the field's rustdoc's last paragraph, one of [`Group`]'s two sentences,
//! so a field that names neither fails the rendering, naming it.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use syn::{Fields, GenericArgument, Item, PathArguments, Type};

use super::{Table, cell, grid};

/// The file the type is read from, relative to the workspace root: the
/// region's input beside this renderer.
const SOURCE: &str = "xtask/src/oracle/mod.rs";
/// The type whose fields are rendered.
const TYPE: &str = "Invocation";
/// The fields §4.3's sentence names itself, which the region leaves out.
const NAMED: [&str; 2] = ["binary", "program"];

/// Which of §4.3's two a field is: what a `.phpt` section adds to a run,
/// or what no section gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Section,
    NoSection,
}

impl Group {
    const BOTH: [Group; 2] = [Group::Section, Group::NoSection];

    /// The last paragraph of a field's rustdoc that puts it in the group.
    const fn marker(self) -> &'static str {
        match self {
            Group::Section => "A `.phpt` section adds it.",
            Group::NoSection => "No section gives it.",
        }
    }

    /// The group as the region's cell says it: the marker as a clause,
    /// lowercase and without its stop.
    fn said(self) -> String {
        let marker = self.marker().trim_end_matches('.');
        let mut chars = marker.chars();
        chars.next().map_or_else(String::new, |first| {
            first.to_lowercase().chain(chars).collect()
        })
    }
}

/// One field as the region renders it.
#[derive(Debug, PartialEq, Eq)]
struct Field {
    name: String,
    ty: String,
    group: Group,
    sentence: String,
}

/// The section over the workspace at `root`, in `format`.
pub(super) fn render(root: &Path, format: Table) -> Result<String> {
    let text =
        fs::read_to_string(root.join(SOURCE)).with_context(|| format!("reading {SOURCE}"))?;
    let fields = fields(&text).with_context(|| format!("{SOURCE}: oracle::{TYPE}"))?;
    Ok(match format {
        Table::Json => serde_json::to_string_pretty(
            &fields
                .iter()
                .map(|f| {
                    json!({
                        "field": f.name,
                        "type": f.ty,
                        "group": f.group.said(),
                        "sentence": f.sentence,
                    })
                })
                .collect::<Vec<Value>>(),
        )?,
        Table::Md => grid(
            &["field", "type", "group", "what it is"],
            fields.iter().map(|f| {
                let (name, ty) = (format!("`{}`", f.name), format!("`{}`", f.ty));
                [name, ty, f.group.said(), cell(&f.sentence)]
            }),
        ),
    })
}

/// The public fields of [`TYPE`] in `source` past [`NAMED`], in the
/// order the type declares them.
fn fields(source: &str) -> Result<Vec<Field>> {
    let file = syn::parse_file(source).context("parsing the source")?;
    let Some(fields) = file.items.iter().find_map(|item| match item {
        Item::Struct(s) if s.ident == TYPE => Some(&s.fields),
        _ => None,
    }) else {
        bail!("no struct {TYPE}");
    };
    let Fields::Named(fields) = fields else {
        bail!("{TYPE} has no named fields");
    };
    fields
        .named
        .iter()
        .filter(|f| matches!(f.vis, syn::Visibility::Public(_)))
        .filter_map(|f| f.ident.as_ref().map(|name| (name.to_string(), f)))
        .filter(|(name, _)| !NAMED.contains(&name.as_str()))
        .map(|(name, f)| {
            let doc = doc(&f.attrs);
            let (said, group) = grouped(&doc).with_context(|| {
                format!(
                    "the field {name} ends its rustdoc with neither \"{}\" nor \"{}\", the two groups of harness-design.md §4.3",
                    Group::Section.marker(),
                    Group::NoSection.marker()
                )
            })?;
            let sentence = first_sentence(said);
            if sentence.is_empty() {
                bail!("the field {name} says what it is before its group");
            }
            Ok(Field {
                ty: type_name(&f.ty)
                    .with_context(|| format!("the field {name}'s type is one this section does not spell"))?,
                name,
                group,
                sentence: sentence.to_owned(),
            })
        })
        .collect()
}

/// A field's rustdoc, its lines joined, each without the one space
/// `///` leaves.
pub(super) fn doc(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        })
        .map(|line| line.strip_prefix(' ').map_or(line.clone(), str::to_owned))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rustdoc before its last paragraph, and the group that paragraph
/// names; none when it names neither.
fn grouped(doc: &str) -> Option<(&str, Group)> {
    let doc = doc.trim_end();
    let (said, last) = doc.rsplit_once("\n\n").unwrap_or(("", doc));
    let group = Group::BOTH
        .into_iter()
        .find(|g| last.trim() == g.marker())?;
    Some((said, group))
}

/// The first sentence of `text`, its lines joined: up to the first `.`
/// outside backticks followed by whitespace or the end.
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let mut quoted = false;
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'`' => quoted = !quoted,
            // `.` is ASCII, so `i + 1` is a char boundary.
            b'.' if !quoted && bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace) => {
                return &text[..=i];
            }
            _ => {}
        }
    }
    text
}

/// A field's type as a person writes it, without its lifetimes: `&[String]`
/// for `&'a [String]`; none for a shape no field of the type has.
fn type_name(ty: &Type) -> Option<String> {
    let all = |types: &mut dyn Iterator<Item = &Type>| -> Option<Vec<String>> {
        types.map(type_name).collect()
    };
    Some(match ty {
        Type::Reference(r) if r.mutability.is_none() => format!("&{}", type_name(&r.elem)?),
        Type::Slice(s) => format!("[{}]", type_name(&s.elem)?),
        Type::Tuple(t) => format!("({})", all(&mut t.elems.iter())?.join(", ")),
        Type::Path(p) => {
            let mut segments = Vec::new();
            for seg in &p.path.segments {
                let args = match &seg.arguments {
                    PathArguments::AngleBracketed(a) => {
                        all(&mut a.args.iter().filter_map(|arg| match arg {
                            GenericArgument::Type(t) => Some(t),
                            _ => None,
                        }))?
                    }
                    _ => Vec::new(),
                };
                segments.push(if args.is_empty() {
                    seg.ident.to_string()
                } else {
                    format!("{}<{}>", seg.ident, args.join(", "))
                });
            }
            segments.join("::")
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{Field, Group, fields};

    const SOURCE: &str = "
        pub struct Other { pub ini: bool }
        pub struct Invocation<'a> {
            /// The binary.
            pub binary: &'a Path,
            pub program: Program<'a>,
            /// Ini after the recipe's: a test's `--INI--`. More after.
            ///
            /// A `.phpt` section adds it.
            pub ini: &'a [String],
            /// The working directory, `php-src` or `None`.
            /// Two lines of it.
            ///
            /// No section gives it.
            pub cwd: Option<&'a Path>,
            hidden: (),
            /// Entries, as `proc_open.c:174` hands them.
            ///
            /// A `.phpt` section adds it.
            pub env: &'a [(String, OsString)],
        }
    ";

    /// The public fields past the binary and the program, in order, each
    /// with its type without lifetimes, its group and its first sentence.
    #[test]
    fn the_fields_past_the_binary_and_the_program_render_with_their_group() {
        let got = fields(SOURCE).unwrap();
        let want = [
            (
                "ini",
                "&[String]",
                Group::Section,
                "Ini after the recipe's: a test's `--INI--`.",
            ),
            (
                "cwd",
                "Option<&Path>",
                Group::NoSection,
                "The working directory, `php-src` or `None`.",
            ),
            (
                "env",
                "&[(String, OsString)]",
                Group::Section,
                "Entries, as `proc_open.c:174` hands them.",
            ),
        ]
        .map(|(name, ty, group, sentence)| Field {
            name: name.to_owned(),
            ty: ty.to_owned(),
            group,
            sentence: sentence.to_owned(),
        });
        assert_eq!(got, want);
    }

    /// A field whose rustdoc names no group fails the rendering, naming
    /// the field, so the type cannot gain one the region would misfile.
    #[test]
    fn a_field_naming_no_group_fails_naming_it() {
        let source = SOURCE.replace("hidden: (),", "/// What it is.\npub raw: bool,");
        let err = format!("{:#}", fields(&source).unwrap_err());
        assert!(
            err.contains("the field raw ends its rustdoc with neither"),
            "{err}"
        );
        let err = format!("{:#}", fields("pub struct Other;").unwrap_err());
        assert!(err.contains("no struct Invocation"), "{err}");
    }
}
