//! Design §4.10's list of surfaces, read as the names it writes, which an
//! `ext/*` crate's `pub` items beyond `extension()` are held to (the
//! manifest's `ext.pub-beyond-extension`, as #1781 ruled its text; #1826).
//!
//! The list is read where a person wrote it, and nowhere else: each row of
//! the section's table whose Surface cell opens with the producing
//! extension (`` | `pcre` → `spl` | ``) gives its Items cell, and each
//! paragraph opening `` **The `libxml` surface.** `` gives its own text.
//! A code span there that reads as a Rust path names an item: `Regex`,
//! `connector::Connection`, `XML_PARSE_*`, whose `*` stands for any rest
//! of its last segment. A computed region in the section is skipped, so a
//! name rendered from the tree lists nothing: the hold is of the tree
//! against what a person typed, which is #2020's question to settle.
//!
//! A printed item is listed when a name names it, or names the type or
//! module it stands under, a name naming a path whose last segments are
//! its own (`Connection` names `connector::Connection`, as a row writes
//! `` `connector::Connector`, `Connection` and `Statement` ``). A type
//! written `` `Regex`, with `compile`, `capture_count` and `is_match` `` is
//! closed to the members its rows write: a member of it no row writes is
//! not listed, though the type is. The members are the spans after
//! `, with ` that only `, ` and ` and ` part; any other text between two
//! spans (`` `release`, and `Proxy` ``, `` `named`, handed to ``) ends the
//! list, and a member names nothing on its own.
//!
//! An export on no row fails its crate's reading, save one [`WAITING`]
//! names: an export a filed ticket is to put on a row, named in the reading
//! with its ticket until the row lands.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};

/// The design of record, at the workspace root.
const DESIGN: &str = "third-engine-design.md";

/// The section of [`DESIGN`] that carries the list of surfaces.
const SECTION: &str = "4.10";

/// One producer's exports on no row of the tracked design, which a filed
/// ticket is to put on one.
struct Waiting {
    producer: &'static str,
    ticket: u32,
    /// Paths from the crate's root, each naming the item and every item
    /// under it.
    names: &'static [&'static str],
}

/// The exports on no row of the tracked design when the hold was laid
/// down, each with the ticket that lists it. An entry is dropped in the
/// commit that lands its ticket's row; an entry whose export the design
/// lists already is named in the reading for that drop. Nothing is added
/// here: a new export on no row fails.
const WAITING: &[Waiting] = &[
    Waiting {
        producer: "standard",
        ticket: 1347,
        names: &["php_next_utf8_char"],
    },
    Waiting {
        producer: "random",
        ticket: 2021,
        names: &["Rng"],
    },
    Waiting {
        producer: "pdo_sqlite",
        ticket: 2020,
        names: &[
            "BlobStream",
            "ffi::Aggregate",
            "ffi::Argument",
            "ffi::Blob",
            "ffi::Callbacks",
            "ffi::Db",
            "ffi::Function",
            "ffi::Handler",
            "ffi::Invocation",
            "ffi::Registration",
            "ffi::SQLITE_*",
            "ffi::Stmt",
            "ffi::Unclosed",
            "ffi::drop_aggregate",
            "ffi::version",
            "ffi::version_number",
        ],
    },
    Waiting {
        producer: "libxml",
        ticket: 2020,
        names: &[
            "ffi::At",
            "ffi::AttributeNs",
            "ffi::Emitter",
            "ffi::Expanded",
            "ffi::Position",
            "ffi::PushParser",
            "ffi::RelaxNg",
            "ffi::Sax",
            "ffi::TextReader",
        ],
    },
    Waiting {
        producer: "libxml",
        ticket: 2022,
        names: &[
            "ffi::XML_ATTRIBUTE_ID",
            "ffi::XML_EXTERNAL_GENERAL_UNPARSED_ENTITY",
        ],
    },
];

/// The names §4.10 writes, by the extension that exports them.
#[derive(Debug, Default)]
pub(crate) struct Surfaces {
    /// Whether [`DESIGN`] has a §4.10 at all: without one, nothing is
    /// listed and the reading says why.
    found: bool,
    by_producer: BTreeMap<String, Names>,
}

/// A path as its segments, the last of which may end in `*`.
type Segments = Vec<String>;

/// A type written `` `T`, with `a`, `b` ``, and the members written after
/// it.
#[derive(Debug)]
struct Closed {
    owner: Segments,
    members: BTreeSet<String>,
}

/// The names the rows of one producing extension write.
#[derive(Debug, Default)]
struct Names {
    /// Every name outside a member list.
    named: Vec<Segments>,
    /// Each closed type; one written so in two rows is two entries, and
    /// its members are the two lists together.
    closed: Vec<Closed>,
}

/// What the hold makes of one crate's exports: a problem, which fails the
/// reading, and notes, which it carries either way.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(crate) problem: Option<String>,
    pub(crate) notes: Vec<String>,
}

impl Surfaces {
    /// §4.10 of `root`'s [`DESIGN`]; a workspace without the file has no
    /// list, and every printed item is unlisted there.
    pub(crate) fn read(root: &Path) -> Result<Surfaces> {
        let path = root.join(DESIGN);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Surfaces::parse(&text)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Surfaces::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The list as `text`, a design document, writes it in its §4.10.
    fn parse(text: &str) -> Surfaces {
        let mut out = Surfaces::default();
        let heading = format!("### {SECTION} ");
        let mut lines = text.lines();
        if !lines.by_ref().any(|line| line.starts_with(&heading)) {
            return out;
        }
        out.found = true;
        let mut rendered = false;
        for line in lines {
            if ends_section(line) {
                break;
            }
            if line.starts_with("<!-- computed") {
                rendered = true;
            } else if line.starts_with("<!-- /computed") {
                rendered = false;
            } else if !rendered
                && let Some((producer, text)) = row(line).or_else(|| paragraph(line))
            {
                out.by_producer.entry(producer).or_default().scan(text);
            }
        }
        out
    }

    /// Hold `items`, the `pub` items of `ext/<producer>` beyond
    /// `extension()`, to the list.
    pub(crate) fn hold(&self, producer: &str, items: &[String]) -> Held {
        let names = self.by_producer.get(producer);
        let waiting: Vec<&Waiting> = WAITING.iter().filter(|w| w.producer == producer).collect();
        let waits_on = |item: &str| {
            waiting
                .iter()
                .find(|w| w.names.iter().any(|name| covers(name, item)))
                .map(|w| w.ticket)
        };
        let mut listed = Vec::new();
        let mut unlisted = Vec::new();
        let mut waits: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
        let mut listed_and_waiting: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
        for item in items.iter().map(String::as_str) {
            let on_list = names.is_some_and(|n| n.lists(item));
            match (on_list, waits_on(item)) {
                (true, None) => listed.push(item),
                (true, Some(ticket)) => {
                    listed.push(item);
                    listed_and_waiting.entry(ticket).or_default().push(item);
                }
                (false, Some(ticket)) => waits.entry(ticket).or_default().push(item),
                (false, None) => unlisted.push(item),
            }
        }

        let mut held = Held::default();
        if !listed.is_empty() {
            let listed: Vec<String> = listed.iter().map(|s| (*s).to_owned()).collect();
            held.notes.push(format!(
                "pub beyond extension(): {}, each on design §{SECTION}'s list of surfaces",
                super::list(&listed, 12)
            ));
        }
        for (ticket, items) in &waits {
            held.notes.push(format!(
                "pub beyond extension(), on no row and waiting on #{ticket}: {}",
                items.join(", ")
            ));
        }
        for (ticket, items) in &listed_and_waiting {
            held.notes.push(format!(
                "on design §{SECTION}'s list now, so its entry waiting on #{ticket} is dropped: {}",
                items.join(", ")
            ));
        }
        if !unlisted.is_empty() {
            held.problem = Some(if self.found {
                format!(
                    "pub beyond extension() and on no row of design §{SECTION}'s list of surfaces, {} of {}: {}",
                    unlisted.len(),
                    items.len(),
                    unlisted.join(", ")
                )
            } else {
                format!(
                    "pub beyond extension(), on no list, {DESIGN} having no §{SECTION} here: {}",
                    unlisted.join(", ")
                )
            });
        }
        held
    }
}

impl Names {
    /// Take the names `text`, one Items cell or surface paragraph, writes.
    fn scan(&mut self, text: &str) {
        let mut rest = text;
        // The closed type whose member list is open.
        let mut open: Option<Closed> = None;
        while let Some(start) = rest.find('`') {
            let (between, after) = rest.split_at(start);
            let after = &after[1..];
            let Some(end) = after.find('`') else {
                break;
            };
            let span = &after[..end];
            rest = &after[end + 1..];
            // Right after `, with `, or parted from the last member by a
            // list's own separators.
            let member = open.is_some() && matches!(between, "" | ", " | " and ");
            if !member && let Some(closed) = open.take() {
                self.closed.push(closed);
            }
            let Some(path) = path(span) else {
                continue;
            };
            match (&mut open, path.as_slice()) {
                (Some(closed), [name]) if member => {
                    closed.members.insert(name.clone());
                }
                _ if rest.starts_with(", with `") => {
                    open = Some(Closed {
                        owner: path.clone(),
                        members: BTreeSet::new(),
                    });
                    rest = &rest[", with ".len()..];
                    self.named.push(path);
                }
                _ => self.named.push(path),
            }
        }
        self.closed.extend(open);
    }

    /// Whether `item`, a lock line's path, is listed.
    fn lists(&self, item: &str) -> bool {
        let path: Vec<&str> = item.split("::").collect();
        // The nearest type above it that a row closes decides alone.
        for k in (1..path.len()).rev() {
            let owner = &path[..k];
            let mut closing = self
                .closed
                .iter()
                .filter(|closed| names(&closed.owner, owner))
                .peekable();
            if closing.peek().is_some() {
                return closing.any(|closed| closed.members.contains(path[k]));
            }
        }
        (1..=path.len()).any(|k| self.named.iter().any(|name| names(name, &path[..k])))
    }
}

/// Whether `name`'s segments are the last of `path`'s, a trailing `*`
/// standing for any rest of the last segment.
fn names(name: &[String], path: &[&str]) -> bool {
    let Some(from) = path.len().checked_sub(name.len()) else {
        return false;
    };
    name.iter().zip(&path[from..]).all(|(n, p)| segment(n, p))
}

/// Whether a [`WAITING`] name, a path from the crate's root, is `item` or
/// stands above it.
fn covers(name: &str, item: &str) -> bool {
    let name: Vec<&str> = name.split("::").collect();
    let item: Vec<&str> = item.split("::").collect();
    name.len() <= item.len() && name.iter().zip(&item).all(|(n, p)| segment(n, p))
}

/// Whether one segment of a name, `*` ending it or not, is `p`.
fn segment(n: &str, p: &str) -> bool {
    match n.strip_suffix('*') {
        Some(prefix) => p.starts_with(prefix),
        None => n == p,
    }
}

/// A code span's text as a Rust path's segments, `*` allowed at the end of
/// the last; any other span (`php_pcre.h`, `pcre_get_compiled_regex_cache()`)
/// names nothing.
fn path(span: &str) -> Option<Segments> {
    let segments: Vec<&str> = span.split("::").collect();
    let last = segments.len() - 1;
    segments
        .iter()
        .enumerate()
        .all(|(i, s)| {
            let s = if i == last {
                s.strip_suffix('*').unwrap_or(s)
            } else {
                s
            };
            !s.is_empty()
                && !s.starts_with(|c: char| c.is_ascii_digit())
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .then(|| segments.iter().map(|s| (*s).to_owned()).collect())
}

/// `` | `pcre` → `spl` | items | mirrors | ``: the producer and the Items
/// cell; the header and the rule beneath it open with no code span.
fn row(line: &str) -> Option<(String, &str)> {
    let (producer, _) = line.strip_prefix("| `")?.split_once('`')?;
    let items = line.split(" | ").nth(1)?;
    extension_name(producer).then(|| (producer.to_owned(), items))
}

/// `` **The `libxml` surface.** … ``: the producer and the paragraph.
fn paragraph(line: &str) -> Option<(String, &str)> {
    let (producer, text) = line.strip_prefix("**The `")?.split_once("` surface.**")?;
    extension_name(producer).then(|| (producer.to_owned(), text))
}

/// An extension's name as `ext/<name>` spells it.
fn extension_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether `line` is a heading of level one to three, which ends a `###`
/// section; a `####` heading stands inside it.
fn ends_section(line: &str) -> bool {
    let hashes = line.len() - line.trim_start_matches('#').len();
    (1..=3).contains(&hashes) && line[hashes..].starts_with(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Design

### 4.9 Before

| Surface | Items | Mirrors |
|---|---|---|
| `pcre` → `spl` | `Elsewhere` | not this section |

### 4.10 Extension crates

| Surface | Items | Mirrors |
|---|---|---|
| `pcre` → `spl`, `zip` (#1) | `Regex`, with `compile` and `is_match`, and `split`; `Escape`, with `new`, handed to `Other` | `php_pcre.h`'s `php_pcre_split_impl` |
| `pcre` → `mbstring` | `Regex`, with `capture_count` | `php_pcre.h` |
| `pdo` → the connectors | `connector::Connection`, the types it passes (`Source`), and `ffi::SQLITE_*` | `php_pdo_driver.h` |

**The `libxml` surface.** `ext/libxml` exports `ffi::Node`, with its calls, and `NodeObject::hold_node`.

<!-- computed exec cmd=\"cargo xtask status --section surfaces\" name=surfaces | do not edit; run computed -->
| `pcre` → `filter` | `Rendered` | rendered |
<!-- /computed in=0 out=0 -->

#### 4.10.1 A subsection stays in the section

| `random` → `standard` | `Rng` | `php_random.h` |

### 4.11 After

| `pcre` → `spl` | `After` | not this section |
";

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn unlisted(surfaces: &Surfaces, producer: &str, items: &[&str]) -> Vec<String> {
        let items = lines(items);
        let names = surfaces.by_producer.get(producer);
        items
            .into_iter()
            .filter(|item| !names.is_some_and(|n| n.lists(item)))
            .collect()
    }

    #[test]
    fn a_closed_type_lists_the_members_its_rows_write_and_no_other() {
        let surfaces = Surfaces::parse(DOC);
        let items = [
            "Regex",
            "Regex::compile",
            "Regex::is_match",
            "Regex::capture_count",
            "Regex::replace",
            "Regex::split",
            "split",
            "Escape::new",
            "Escape::Other",
            "Other",
            "sub::new",
        ];
        assert_eq!(
            unlisted(&surfaces, "pcre", &items),
            [
                "Regex::replace",
                "Regex::split",
                "Escape::Other",
                "sub::new"
            ],
            "`, and` and `, handed to` end a member list, and a member names nothing alone"
        );
    }

    #[test]
    fn an_open_type_lists_its_members_and_a_name_matches_by_its_last_segments() {
        let surfaces = Surfaces::parse(DOC);
        let items = [
            "connector::Connection",
            "connector::Connection::prepare",
            "connector::Source",
            "connector::Statement",
            "ffi::SQLITE_OK",
            "ffi::SQLITE_ROW",
            "ffi::OTHER_OK",
        ];
        assert_eq!(
            unlisted(&surfaces, "pdo", &items),
            ["connector::Statement", "ffi::OTHER_OK"],
            "`SQLITE_*` names `SQLITE_OK` and `SQLITE_ROW` and nothing else"
        );
    }

    #[test]
    fn a_surface_paragraph_lists_for_its_producer() {
        let surfaces = Surfaces::parse(DOC);
        let items = [
            "ffi::Node",
            "ffi::Node::new_child",
            "ffi::NodeObject::hold_node",
            "ffi::NodeObject",
        ];
        assert_eq!(
            unlisted(&surfaces, "libxml", &items),
            ["ffi::NodeObject"],
            "`, with` and prose closes nothing"
        );
    }

    #[test]
    fn only_the_section_s_typed_rows_list_and_a_rendered_region_does_not() {
        let surfaces = Surfaces::parse(DOC);
        assert_eq!(
            unlisted(&surfaces, "pcre", &["Elsewhere", "After", "Rendered"]),
            ["Elsewhere", "After", "Rendered"]
        );
        assert!(
            unlisted(&surfaces, "random", &["Rng"]).is_empty(),
            "a deeper heading is still §4.10"
        );
        assert_eq!(unlisted(&surfaces, "hash", &["Digest"]), ["Digest"]);
    }

    #[test]
    fn a_span_that_is_no_path_names_nothing() {
        assert_eq!(path("Regex"), Some(lines(&["Regex"])));
        assert_eq!(path("XML_PARSE_*"), Some(lines(&["XML_PARSE_*"])));
        for span in [
            "php_pcre.h",
            "f(x)",
            "a::",
            "::a",
            "1a",
            "a*b",
            "*",
            "Dom\\HTMLDocument",
            "",
        ] {
            assert_eq!(path(span), None, "{span}");
        }
    }

    #[test]
    fn an_unlisted_export_is_a_problem_and_one_waiting_on_its_ticket_a_note() {
        let surfaces = Surfaces::parse(DOC);
        let held = surfaces.hold("pcre", &lines(&["Regex", "unlisted"]));
        assert_eq!(
            held.problem.as_deref(),
            Some(
                "pub beyond extension() and on no row of design §4.10's list of surfaces, 1 of 2: unlisted"
            )
        );
        let held = surfaces.hold("pcre", &lines(&["Regex", "Regex::compile"]));
        assert_eq!(held.problem, None);
        assert_eq!(
            held.notes,
            [
                "pub beyond extension(): Regex, Regex::compile, each on design §4.10's list of surfaces"
            ]
        );
        let held =
            Surfaces::parse(DOC).hold("standard", &lines(&["php_next_utf8_char", "php_basename"]));
        assert_eq!(
            held.problem.as_deref(),
            Some(
                "pub beyond extension() and on no row of design §4.10's list of surfaces, 1 of 2: php_basename"
            )
        );
        assert_eq!(
            held.notes,
            ["pub beyond extension(), on no row and waiting on #1347: php_next_utf8_char"]
        );
    }

    #[test]
    fn a_waiting_entry_the_design_lists_now_is_named_for_its_drop() {
        let doc = "### 4.10 Extension crates\n\n| `standard` → `xml` | `php_next_utf8_char` | `html.h` |\n";
        let held = Surfaces::parse(doc).hold("standard", &lines(&["php_next_utf8_char"]));
        assert_eq!(held.problem, None);
        assert_eq!(
            held.notes,
            [
                "pub beyond extension(): php_next_utf8_char, each on design §4.10's list of surfaces",
                "on design §4.10's list now, so its entry waiting on #1347 is dropped: php_next_utf8_char",
            ]
        );
    }

    #[test]
    fn a_waiting_name_covers_the_item_it_names_and_those_under_it() {
        assert!(covers("ffi::Db", "ffi::Db"));
        assert!(covers("ffi::Db", "ffi::Db::open"));
        assert!(covers("ffi::SQLITE_*", "ffi::SQLITE_OK"));
        assert!(!covers("ffi::Db", "ffi::Dbx"));
        assert!(!covers("ffi::Db", "other::ffi::Db"));
    }

    #[test]
    fn a_design_without_the_section_lists_nothing_and_says_so() {
        let surfaces = Surfaces::parse("### 4.9 Before\n\n| `pcre` → `spl` | `Regex` | x |\n");
        let held = surfaces.hold("pcre", &lines(&["Regex"]));
        assert!(
            held.problem.is_some_and(|p| p.contains("having no §4.10")),
            "a missing section fails what it cannot list"
        );
    }
}
