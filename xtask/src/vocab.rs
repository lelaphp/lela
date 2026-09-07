//! `xtask vocab`, the glossary as a sensor (harness-design.md §5). For every
//! manifest rule with `avoid_from`, the glossary it names is read for its
//! `_Avoid_` lists, and every crate the rule's scope covers is scanned: the
//! names of its public items, the prose of its rustdoc, and any lock file
//! the scope names under the crate's path. One reading per crate, of
//! instrument `vocab`; a crate whose sources do not exist yet gets none.
//!
//! The scan is textual, not a parse: a public item is a line beginning `pub`
//! followed by an item keyword and a name; rustdoc prose is the text of
//! `///` and `//!` lines outside fenced code blocks and inline code spans.
//! A name is split into its words (`get_or_build`, `ExtensionBuilder`) and
//! also taken whole, so `ReturnTypeWillChange` is found as written.
//! Matching is case-sensitive (`glossary`): `Result` the type, in a name or
//! in prose, is not `result` the avoided word.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git::Stamp;
use crate::glossary::{self, Avoided, Glossary};
use crate::manifest::{Crate, Manifest, Rule};
use crate::reading::{Reading, Status, Store};

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "vocab";

/// Where an avoided word was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where {
    /// The name of a public item.
    Item,
    /// Rustdoc prose.
    Prose,
    /// A line of a lock file.
    Lock,
}

/// One avoided word found: where, in what, and the terms the glossary
/// offers instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Relative to the crate's directory (or the lock's own name).
    pub file: PathBuf,
    pub line: usize,
    pub site: Where,
    /// The avoided phrase as the glossary wrote it.
    pub word: String,
    /// The public item or lock line that carries it; empty for prose.
    pub subject: String,
    pub terms: Vec<String>,
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
            Where::Lock => write!(f, "in the lock line `{}`", self.subject)?,
        }
        write!(f, "; the glossary says {}", quoted(&self.terms))
    }
}

fn quoted(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Run the instrument over the manifest's crates, or over `only` when it
/// names some, and write a reading per crate that exists. `Ok(true)` when
/// every reading passed.
pub fn run(root: &Path, manifest: &Manifest, store: &Store, only: &[String]) -> Result<bool> {
    let rules: Vec<&Rule> = manifest
        .rules
        .iter()
        .filter(|r| r.predicate.avoid_from.is_some())
        .collect();
    if rules.is_empty() {
        bail!("the manifest has no rule with `avoid_from`; nothing for vocab to read");
    }
    for name in only {
        if manifest.crate_named(name).is_none() {
            bail!("the manifest has no crate named {name:?}");
        }
    }
    let mut all_pass = true;
    for rule in rules {
        let glossary_file = rule.predicate.avoid_from.as_deref().unwrap_or_default();
        let glossary = Glossary::load(&root.join(glossary_file))?;
        for krate in &manifest.crates {
            if !only.is_empty() && !only.iter().any(|n| n == &krate.name) {
                continue;
            }
            let Some(scan) = Scan::of(root, rule, krate, &glossary)? else {
                continue;
            };
            let reading = scan.reading(root, rule, krate)?;
            for finding in &scan.findings {
                println!("{}/{finding}", krate.path().display());
            }
            all_pass &= reading.status == Status::Pass;
            store.write(&reading)?;
        }
    }
    Ok(all_pass)
}

/// What one crate's scan counted and found.
struct Scan {
    items: i64,
    prose: i64,
    lock: i64,
    findings: Vec<Finding>,
}

impl Scan {
    /// Scan `krate` under `rule`'s scope; `None` when the scope does not
    /// cover it or its directory does not exist.
    fn of(root: &Path, rule: &Rule, krate: &Crate, glossary: &Glossary) -> Result<Option<Scan>> {
        let path = krate.path();
        let dir = root.join(&path);
        let covered = rule.scope.iter().any(|pattern| matches(pattern, &path));
        if !covered || !dir.is_dir() {
            return Ok(None);
        }
        let mut scan = Scan {
            items: 0,
            prose: 0,
            lock: 0,
            findings: Vec::new(),
        };
        let avoided = glossary.avoided_phrases();
        for file in rust_files(&dir)? {
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let relative = file.strip_prefix(&dir).unwrap_or(&file).to_path_buf();
            let counts = scan_source(&relative, &text, &avoided, &mut scan.findings);
            scan.items += counts.0;
            scan.prose += counts.1;
        }
        for lock in rule.scope.iter().filter(|s| !s.contains('*')) {
            let lock_path = Path::new(lock);
            if !lock_path.starts_with(&path) {
                continue;
            }
            let file = root.join(lock_path);
            if !file.is_file() {
                continue;
            }
            let text =
                fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let relative = lock_path.strip_prefix(&path).unwrap_or(lock_path);
            scan.lock += scan_lock_text(relative, &text, &avoided, &mut scan.findings);
        }
        Ok(Some(scan))
    }

    fn reading(&self, root: &Path, rule: &Rule, krate: &Crate) -> Result<Reading> {
        let mut reading = Reading::new(INSTRUMENT, &krate.name);
        reading.rule.clone_from(&rule.id);
        rule.adr
            .iter()
            .next()
            .unwrap_or_default()
            .clone_into(&mut reading.adr);
        reading.numbers = BTreeMap::from([
            ("items".to_owned(), self.items.into()),
            ("prose".to_owned(), self.prose.into()),
            ("lock".to_owned(), self.lock.into()),
            (
                "hits".to_owned(),
                i64::try_from(self.findings.len())
                    .unwrap_or(i64::MAX)
                    .into(),
            ),
        ]);
        reading.status = if self.findings.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        };
        reading.message = if self.findings.is_empty() {
            format!("{}: no avoided word", krate.name)
        } else {
            let list = self
                .findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{}: {}\n{list}", krate.name, rule.text)
        };
        reading.stamp(&Stamp::take(root, &krate.path())?);
        Ok(reading)
    }
}

/// Scan one crate directory as the instrument would: every `.rs` file under
/// it. The findings' files are relative to `dir`.
pub fn scan_crate(dir: &Path, glossary: &Glossary) -> Result<Vec<Finding>> {
    let avoided = glossary.avoided_phrases();
    let mut findings = Vec::new();
    for file in rust_files(dir)? {
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let relative = file.strip_prefix(dir).unwrap_or(&file).to_path_buf();
        scan_source(&relative, &text, &avoided, &mut findings);
    }
    Ok(findings)
}

/// Scan one lock file: each item line's path, split into words. Comment
/// lines are the lock's own prose and not asserted on.
pub fn scan_lock(file: &Path, glossary: &Glossary) -> Result<Vec<Finding>> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let mut findings = Vec::new();
    let name = file
        .file_name()
        .map_or_else(|| file.to_path_buf(), PathBuf::from);
    scan_lock_text(&name, &text, &glossary.avoided_phrases(), &mut findings);
    Ok(findings)
}

fn scan_lock_text(file: &Path, text: &str, avoided: &[Avoided], out: &mut Vec<Finding>) -> i64 {
    let mut lines = 0;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        lines += 1;
        let mut words = Vec::new();
        for segment in line.split("::") {
            words.extend(identifier_words(segment));
            words.push(segment.to_owned());
        }
        for hit in hits(&words, avoided) {
            out.push(Finding {
                file: file.to_path_buf(),
                line: index + 1,
                site: Where::Lock,
                word: hit.text.clone(),
                subject: line.to_owned(),
                terms: hit.terms.clone(),
            });
        }
    }
    lines
}

/// Scan one source file; `(public items, prose lines)` counted.
fn scan_source(file: &Path, text: &str, avoided: &[Avoided], out: &mut Vec<Finding>) -> (i64, i64) {
    let (mut items, mut prose) = (0, 0);
    let mut in_fence = false;
    for (index, line) in text.lines().enumerate() {
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
            prose += 1;
            let words = glossary::words(&without_code_spans(doc));
            for hit in hits(&words, avoided) {
                out.push(Finding {
                    file: file.to_path_buf(),
                    line: index + 1,
                    site: Where::Prose,
                    word: hit.text.clone(),
                    subject: String::new(),
                    terms: hit.terms.clone(),
                });
            }
        } else if let Some((subject, name)) = public_item(line) {
            items += 1;
            let mut words = identifier_words(&name);
            words.push(name);
            for hit in hits(&words, avoided) {
                out.push(Finding {
                    file: file.to_path_buf(),
                    line: index + 1,
                    site: Where::Item,
                    word: hit.text.clone(),
                    subject: subject.clone(),
                    terms: hit.terms.clone(),
                });
            }
        }
    }
    (items, prose)
}

/// The avoided phrases that occur in `words`, each once, in the order the
/// glossary states them.
fn hits<'a>(words: &[String], avoided: &'a [Avoided]) -> Vec<&'a Avoided> {
    avoided
        .iter()
        .filter(|a| {
            words
                .windows(a.words.len())
                .any(|w| w == a.words.as_slice())
        })
        .collect()
}

/// The item keywords a `pub` may introduce.
const ITEM_KEYWORDS: [&str; 11] = [
    "fn", "struct", "enum", "union", "trait", "type", "mod", "const", "static", "macro", "use",
];

/// A public item on this line: its declaration head (`pub fn run`) and
/// its name. `pub(crate)` and narrower are not public.
fn public_item(line: &str) -> Option<(String, String)> {
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
            _ => return None,
        }
    }
    let keyword = keyword?;
    let tail = tokens.collect::<Vec<_>>().join(" ");
    let name = if keyword == "use" {
        use_name(&tail)?
    } else {
        tail.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .filter(|n| !n.is_empty())?
    };
    Some((format!("pub {keyword} {name}"), name.to_owned()))
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

/// Whether a manifest scope pattern covers a crate path: segment by segment,
/// `*` matching one segment and `**` the rest.
fn matches(pattern: &str, path: &Path) -> bool {
    let segments: Vec<&str> = path
        .iter()
        .filter_map(|s| s.to_str())
        .filter(|s| *s != ".")
        .collect();
    let mut pattern = pattern.split('/').filter(|p| !p.is_empty()).peekable();
    let mut segments = segments.into_iter().peekable();
    loop {
        match (pattern.next(), segments.next()) {
            (None, None) | (Some("**"), _) => return true,
            (Some("*"), Some(_)) => {}
            (Some(p), Some(s)) if p == s => {}
            _ => return false,
        }
    }
}

/// Every `.rs` file under `dir`, in path order; `target/` is skipped.
fn rust_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{identifier_words, matches, public_item, use_name};

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
        let head = |l: &str| public_item(l).map(|(h, _)| h);
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
        assert_eq!(head("pub use a::*;"), None);
        assert_eq!(head("pub(crate) fn run() {}"), None);
        assert_eq!(head("fn run() {}"), None);
        assert_eq!(use_name("a::{b, c};"), None);
    }

    #[test]
    fn scope_patterns_cover_crate_paths() {
        assert!(matches("crates/*", Path::new("crates/php_heap")));
        assert!(matches("ext/*", Path::new("ext/pcre")));
        assert!(!matches("crates/*", Path::new("ext/pcre")));
        assert!(!matches("crates/*", Path::new("xtask")));
        assert!(!matches(
            "crates/php_ext/api.lock",
            Path::new("crates/php_ext")
        ));
        assert!(matches("crates/**", Path::new("crates/php_ext/src")));
    }
}
