//! `xtask vocab`, the glossary as a sensor (harness-design.md §5). Each
//! `avoid_from` rule of the manifest names the glossary, whole or one
//! heading of it, which is read for its `_Avoid_` lists, and every crate
//! the rule's scope covers is scanned: the names of its public items and
//! the prose of its rustdoc. A crate is held by one rule; two rules that
//! cover one crate are refused, since the store holds one vocab reading
//! per crate. A lock file is not read: its lines mirror php's own names,
//! which the engine copies on purpose. A `fixtures/` directory under a
//! crate is not read either: what is under it is another crate's sources,
//! laid there to fail some instrument on purpose. One reading per crate,
//! of instrument `vocab`; a crate whose sources do not exist yet gets none,
//! and where the manifest has no `avoid_from` rule each crate's is a skip
//! that says so (#3182).
//!
//! The scan is textual, not a parse. A public item is a line beginning
//! `pub` followed by an item keyword and a name, a `pub` field, a variant of
//! a `pub enum`, or a `macro_rules!` under `#[macro_export]`; `pub(crate)`
//! and narrower are not public. Rustdoc prose is the text of `///` and `//!`
//! lines outside fenced code blocks and inline code spans; `/** */` and
//! `#[doc]` are not read. A name is split into its words (`get_or_build`,
//! `ExtensionBuilder`) and also taken whole, so `ReturnTypeWillChange` is
//! found as written. Case is ignored except for the spellings the glossary
//! marks as code (`glossary`): `Registry` is `registry`, `Result` is not
//! `result`.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::glossary::{self, Avoided, Glossary};
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest, Rule};
use crate::reading::{Outcome, Reading, Status, Store, joined};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "vocab";

/// Where an avoided word was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Where {
    /// The name of a public item.
    Item,
    /// Rustdoc prose.
    Prose,
}

/// One avoided word found: where, in what, and the terms the glossary
/// offers instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// Relative to the crate's directory.
    pub file: PathBuf,
    pub line: usize,
    pub site: Where,
    /// The avoided phrase as the glossary wrote it.
    pub word: String,
    /// The public item that carries it; empty for prose.
    pub subject: String,
    pub terms: Vec<String>,
}

impl Finding {
    fn at(file: &Path, line: usize, site: Where, subject: &str, hit: &Avoided) -> Finding {
        Finding {
            file: file.to_path_buf(),
            line,
            site,
            word: hit.phrase.text.clone(),
            subject: subject.to_owned(),
            terms: hit.terms.clone(),
        }
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}: \"{}\" ",
            self.file.display(),
            self.line,
            self.word
        )?;
        match self.site {
            Where::Item => write!(f, "names `{}`", self.subject)?,
            Where::Prose => write!(f, "in rustdoc prose")?,
        }
        write!(f, "; the glossary says {}", quoted(&self.terms))
    }
}

fn quoted(terms: &[String]) -> String {
    joined(terms.iter().map(|t| format!("\"{t}\"")), " or ")
}

/// What one scan counted and found.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Public items whose names were read.
    pub items: i64,
    /// Rustdoc prose lines read.
    pub prose: i64,
    pub findings: Vec<Finding>,
}

/// Run the instrument over the manifest's crates, or over `only` when it
/// names some, and record a reading per crate that exists; the outcome is
/// the exit.
pub fn run(root: &Path, manifest: &Manifest, store: &Store, only: &[String]) -> Result<Outcome> {
    store.record(&hold(root, manifest, only)?)
}

/// Every reading of one run, each finding printed as it is found: one per
/// crate some rule's scope covers and that exists, or per crate `only`
/// names. The library entry, as `api_check::hold` is; `run` records.
pub fn hold(root: &Path, manifest: &Manifest, only: &[String]) -> Result<Vec<Reading>> {
    let rules: Vec<(&Rule, Glossary)> = manifest
        .rules
        .iter()
        .filter_map(|r| r.predicate.avoid_from.as_ref().map(|from| (r, from)))
        .map(|(rule, from)| {
            let glossary = Glossary::load(&root.join(from.file()))?;
            let glossary = match from.section() {
                Some(section) => glossary
                    .section(section)
                    .with_context(|| format!("rule {}: {}", rule.id, from.file()))?,
                None => glossary,
            };
            Ok((rule, glossary))
        })
        .collect::<Result<_>>()?;
    let crates = manifest.crates_named(only)?;
    let snapshot = Snapshot::disk(root)?;
    let mut readings = Vec::new();
    for krate in crates {
        let dir = root.join(krate.path());
        let over = rule_over(&rules, krate)?;
        if !dir.is_dir() || (over.is_none() && !rules.is_empty()) {
            continue;
        }
        let Some((rule, glossary)) = over else {
            let mut reading = Reading::for_crate(INSTRUMENT, krate, &snapshot)?;
            reading.message = format!(
                "{}: the manifest has no rule with `avoid_from`, so no glossary names a word to avoid",
                krate.name
            );
            readings.push(reading);
            continue;
        };
        let scan = scan_crate(&dir, glossary)?;
        for finding in &scan.findings {
            println!("{}/{finding}", krate.path().display());
        }
        readings.push(reading_of(&scan, &snapshot, rule, krate)?);
    }
    Ok(readings)
}

/// The one rule whose scope covers `krate`, with its glossary; `None`
/// when no rule does, and two are refused by name.
fn rule_over<'a>(
    rules: &'a [(&'a Rule, Glossary)],
    krate: &Crate,
) -> Result<Option<(&'a Rule, &'a Glossary)>> {
    let path = krate.path();
    let covering: Vec<(&Rule, &Glossary)> = rules
        .iter()
        .filter(|(rule, _)| rule.covers(&path))
        .map(|(rule, glossary)| (*rule, glossary))
        .collect();
    match covering.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(*one)),
        many => bail!(
            "{} rules with `avoid_from` cover {} ({}); the store holds one vocab reading per crate, so one rule holds it",
            many.len(),
            path.display(),
            joined(many.iter().map(|(r, _)| r.id.as_str()), ", ")
        ),
    }
}

/// The reading of one crate's scan: `numbers` are the counts and the hits,
/// `message` lists every finding under the rule's text.
fn reading_of(scan: &Scan, snapshot: &Snapshot, rule: &Rule, krate: &Crate) -> Result<Reading> {
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    reading.rule.clone_from(&rule.id);
    reading.adr = rule.adr.adr_field();
    reading.set("items", scan.items);
    reading.set("prose", scan.prose);
    reading.count("hits", u64::try_from(scan.findings.len())?)?;
    reading.status = Status::of(scan.findings.is_empty());
    reading.message = if scan.findings.is_empty() {
        format!("{}: no avoided word", krate.name)
    } else {
        let list = joined(scan.findings.iter(), "\n");
        format!("{}: {}\n{list}", krate.name, rule.text)
    };
    Ok(reading)
}

/// Scan one crate directory: every `.rs` file under it, `target/` and
/// `fixtures/` aside. The findings' files are relative to `dir`.
pub(crate) fn scan_crate(dir: &Path, glossary: &Glossary) -> Result<Scan> {
    let avoided = glossary.avoided_phrases();
    let mut scan = Scan::default();
    for file in rust_files(dir)? {
        let text = crate::read(&file)?;
        let relative = file.strip_prefix(dir).unwrap_or(&file);
        scan_source(relative, &text, &avoided, &mut scan);
    }
    Ok(scan)
}

/// Scan one source file into `scan`.
fn scan_source(file: &Path, text: &str, avoided: &[Avoided], scan: &mut Scan) {
    let mut in_fence = false;
    let mut items = Items::default();
    for (index, line) in text.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        let line = line.trim_start();
        if let Some(doc) = line
            .strip_prefix("///")
            .or_else(|| line.strip_prefix("//!"))
        {
            let doc = doc.trim();
            if doc.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            scan.prose += 1;
            let words = glossary::words(&without_code_spans(doc));
            for hit in hits(&words, avoided) {
                scan.findings
                    .push(Finding::at(file, index + 1, Where::Prose, "", hit));
            }
        } else if let Some((subject, name)) = items.public_item(line, indent) {
            scan.items += 1;
            let mut words = identifier_words(&name);
            words.push(name.clone());
            // A name the glossary marks as code (`Result`, and an
            // identifier a parenthetical writes in backticks) is not the
            // word; a spelling written as a path marks the item its last
            // segment names.
            for hit in hits(&words, avoided).into_iter().filter(|hit| {
                !hit.phrase
                    .code
                    .iter()
                    .any(|spelling| spelling.rsplit("::").next() == Some(name.as_str()))
            }) {
                scan.findings
                    .push(Finding::at(file, index + 1, Where::Item, &subject, hit));
            }
        }
    }
}

/// The avoided phrases that occur in `words`, each once, in the order the
/// glossary states them.
fn hits<'a>(words: &[String], avoided: &'a [Avoided]) -> Vec<&'a Avoided> {
    avoided
        .iter()
        .filter(|a| a.phrase.occurs_in(words))
        .collect()
}

/// The item keywords a `pub` may introduce.
const ITEM_KEYWORDS: [&str; 11] = [
    "fn", "struct", "enum", "union", "trait", "type", "mod", "const", "static", "macro", "use",
];

/// The line-to-line state of the public-item scan: the `pub enum` whose
/// variants are being read, by the indentation its `}` will have, and
/// whether the last attribute line was `#[macro_export]`.
#[derive(Default)]
struct Items {
    enum_indent: Option<usize>,
    exported: bool,
}

impl Items {
    /// The public item this line declares: its head (`pub fn run`) and its
    /// name.
    fn public_item(&mut self, line: &str, indent: usize) -> Option<(String, String)> {
        if let Some(enum_indent) = self.enum_indent {
            if line.starts_with('}') && indent <= enum_indent {
                self.enum_indent = None;
            } else if let Some(name) = variant(line) {
                return Some((format!("variant {name}"), name.to_owned()));
            }
        }
        let exported = std::mem::replace(&mut self.exported, line.starts_with("#[macro_export]"));
        if exported && let Some(rest) = line.strip_prefix("macro_rules!") {
            let name = identifier(rest)?;
            return Some((format!("macro_rules! {name}"), name.to_owned()));
        }
        let rest = line.strip_prefix("pub ")?;
        let mut tokens = rest.split_whitespace().peekable();
        let mut keyword = None;
        while let Some(token) = tokens.next() {
            match token {
                "unsafe" | "async" | "extern" => {}
                t if t.starts_with('"') => {} // the ABI of `extern "C"`
                "const" if tokens.peek() == Some(&"fn") => {}
                t if ITEM_KEYWORDS.contains(&t) => {
                    keyword = Some(t);
                    break;
                }
                t => {
                    // A `pub` field: `pub name: Type,`.
                    let name = identifier(t)?;
                    return (t[name.len()..].starts_with(':'))
                        .then(|| (format!("pub field {name}"), name.to_owned()));
                }
            }
        }
        let keyword = keyword?;
        let tail = tokens.collect::<Vec<_>>().join(" ");
        let name = if keyword == "use" {
            use_name(&tail)?
        } else {
            identifier(&tail)?
        };
        if keyword == "enum" && tail.contains('{') && !tail.contains('}') {
            self.enum_indent = Some(indent);
        }
        Some((format!("pub {keyword} {name}"), name.to_owned()))
    }
}

/// A variant line of an enum body: a capitalised identifier followed by
/// `,`, `(`, `{`, `=` or nothing.
fn variant(line: &str) -> Option<&str> {
    let name = identifier(line)?;
    let next = line[name.len()..].trim_start().chars().next();
    (name.starts_with(|c: char| c.is_uppercase())
        && matches!(next, None | Some(',' | '(' | '{' | '=')))
    .then_some(name)
}

/// The identifier at the start of `text`, if there is one.
fn identifier(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    (end > 0).then(|| &text[..end])
}

/// The name a `pub use` brings in: the `as` alias, else the last path
/// segment. A glob or a brace list names nothing on its own.
fn use_name(tail: &str) -> Option<&str> {
    let tail = tail.trim_end_matches(';').trim();
    if let Some((_, alias)) = tail.rsplit_once(" as ") {
        return Some(alias.trim());
    }
    let last = tail.rsplit("::").next()?.trim();
    (!last.is_empty() && last.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(last)
}

/// The words of an identifier: split at `_` and at CamelCase boundaries,
/// case kept, so `ExtensionBuilder` is `Extension`, `Builder` and
/// `HTTPRequest` is `HTTP`, `Request`.
fn identifier_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in name.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = part.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let boundary = chars[i].is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || chars[i - 1].is_ascii_digit()
                    || chars.get(i + 1).is_some_and(|c| c.is_lowercase()));
            if boundary {
                words.push(chars[start..i].iter().collect());
                start = i;
            }
        }
        if start < chars.len() {
            words.push(chars[start..].iter().collect());
        }
    }
    words
}

/// Prose with its inline code spans removed: what is between backticks is
/// code, not a word.
fn without_code_spans(text: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for c in text.chars() {
        if c == '`' {
            in_code = !in_code;
            out.push(' ');
        } else if !in_code {
            out.push(c);
        }
    }
    out
}

/// Every `.rs` file under `dir`, in path order; `target/` is skipped, and
/// so is `fixtures/`, whose files are other crates' sources.
fn rust_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    crate::tally::files_under(dir, &mut files)?;
    files.retain(|p| {
        p.extension().is_some_and(|e| e == "rs")
            && !p
                .strip_prefix(dir)
                .unwrap_or(p)
                .iter()
                .any(|part| part == "target" || part == "fixtures")
    });
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Items, Scan, Where, identifier_words, scan_source, use_name};
    use crate::glossary::Glossary;

    #[test]
    fn a_public_item_whose_whole_name_the_glossary_marks_as_code_is_not_a_hit() {
        let glossary = Glossary::parse(
            "**Instrument**:\n_Avoid_: tool (the hook's `tool_name` is Claude Code's)\n",
        )
        .unwrap();
        let source = "pub tool_name: String,\npub tool: u8,\n/// The tool's name.\npub fn f() {}\n";
        let mut scan = Scan::default();
        scan_source(
            Path::new("a.rs"),
            source,
            &glossary.avoided_phrases(),
            &mut scan,
        );
        let sites: Vec<(usize, Where)> = scan.findings.iter().map(|f| (f.line, f.site)).collect();
        assert_eq!(sites, [(2, Where::Item), (3, Where::Prose)]);
    }

    #[test]
    fn a_public_item_a_path_spelling_names_is_not_a_hit_and_the_word_in_prose_is() {
        let glossary =
            Glossary::parse("**Divergence**:\n_Avoid_: exception (php's `Thrown::exception`)\n")
                .unwrap();
        let source = "pub fn exception() {}\n/// Throws an exception.\npub fn f() {}\n";
        let mut scan = Scan::default();
        scan_source(
            Path::new("a.rs"),
            source,
            &glossary.avoided_phrases(),
            &mut scan,
        );
        let sites: Vec<(usize, Where)> = scan.findings.iter().map(|f| (f.line, f.site)).collect();
        assert_eq!(sites, [(2, Where::Prose)]);
    }

    #[test]
    fn identifiers_split_into_their_words() {
        assert_eq!(identifier_words("get_or_build"), ["get", "or", "build"]);
        assert_eq!(
            identifier_words("ExtensionBuilder"),
            ["Extension", "Builder"]
        );
        assert_eq!(identifier_words("HTTPRequest"), ["HTTP", "Request"]);
        assert_eq!(identifier_words("Result"), ["Result"]);
        assert_eq!(identifier_words("vm_interrupt2"), ["vm", "interrupt2"]);
    }

    #[test]
    fn public_items_are_named_and_narrower_visibility_is_not() {
        let head = |l: &str| Items::default().public_item(l, 0).map(|(h, _)| h);
        assert_eq!(head("pub fn run() {}"), Some("pub fn run".into()));
        assert_eq!(head("pub const fn run() {}"), Some("pub fn run".into()));
        assert_eq!(
            head("pub unsafe extern \"C\" fn go()"),
            Some("pub fn go".into())
        );
        assert_eq!(head("pub const MAX: u8 = 1;"), Some("pub const MAX".into()));
        assert_eq!(
            head("pub type Result<T> = X;"),
            Some("pub type Result".into())
        );
        assert_eq!(head("pub struct Return;"), Some("pub struct Return".into()));
        assert_eq!(head("pub use a::b::run;"), Some("pub use run".into()));
        assert_eq!(head("pub use a::b as serve;"), Some("pub use serve".into()));
        assert_eq!(head("pub run: u32,"), Some("pub field run".into()));
        assert_eq!(head("pub use a::*;"), None);
        assert_eq!(head("pub(crate) fn run() {}"), None);
        assert_eq!(head("fn run() {}"), None);
        assert_eq!(head("pub if x {}"), None);
        assert_eq!(use_name("a::{b, c};"), None);
    }

    #[test]
    fn enum_variants_and_exported_macros_are_public_items() {
        let mut items = Items::default();
        let mut heads = Vec::new();
        for (line, indent) in [
            ("pub enum Kind {", 0),
            ("/// A run.", 4),
            ("Run,", 4),
            ("Handle(u8),", 4),
            ("}", 0),
            ("Other,", 0),
            ("#[macro_export]", 0),
            ("macro_rules! script {", 0),
            ("macro_rules! private {", 0),
        ] {
            if let Some((head, _)) = items.public_item(line, indent) {
                heads.push(head);
            }
        }
        assert_eq!(
            heads,
            [
                "pub enum Kind",
                "variant Run",
                "variant Handle",
                "macro_rules! script"
            ]
        );
    }

    /// A rule's scope covers a crate path in the manifest's one grammar,
    /// which `rule_over` reads through `Rule::covers`.
    #[test]
    fn scope_patterns_cover_crate_paths() {
        let matches = |pattern, path| crate::manifest::scope_matches(pattern, Path::new(path));
        assert!(matches("crates/*", "crates/php_heap"));
        assert!(matches("ext/*", "ext/pcre"));
        assert!(!matches("crates/*", "ext/pcre"));
        assert!(!matches("crates/*", "xtask"));
        assert!(!matches("crates/php_ext/api.lock", "crates/php_ext"));
        assert!(matches("crates/**", "crates/php_ext/src"));
    }
}
