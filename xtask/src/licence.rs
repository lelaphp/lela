//! The licence header as `budget` holds it, under the manifest rule whose
//! `check` names this sensor ([`CHECK`]): one reading, scope `licence`.
//!
//! A Rust, C or C++ file the rule's `scope` covers and its `exempt` does
//! not states no licence where it begins: `NOTICE` says once what those
//! trees are derived from. Only a file the rule's `allow` names carries
//! the header, a file whose licence asks for a notice in the file itself:
//! four lines and an empty one, the SPDX expression, the two copyright
//! lines [`COPYRIGHT`] begins with, and [`DERIVED`]. Where a row of
//! [`THIRD_PARTY`]'s table of ports names such a file, it adds the row's
//! licence to the expression, says whose it is on its fifth line, and
//! carries each notice the row names, byte for byte as its file under
//! `LICENSES/` holds it; a file under an LGPL licence names each day it
//! changed. Each crate's `license` covers the expression of every file of
//! its own the rule covers, the header's and what a row adds, whether the
//! file carries the header or not.
//!
//! `LICENSES/` is the record: a file's notice is held to it with the
//! repository alone, always, and each notice there to the lines its row
//! names in the pinned php-src where that tree is laid, whether a file
//! carries the notice or not. Where the tree is not laid, the reading
//! names the notices it did not compare. No source file names a line of
//! php-src, so a move of the pin changes [`THIRD_PARTY`] and `LICENSES/`
//! and no header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;
use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest, Rule};
use crate::reading::{Reading, Status, civil_from_days, joined};

/// The `check` a rule names this sensor by.
pub(crate) const CHECK: &str = "licence-header";

/// The scope of the reading, beside `budget`'s of the workspace.
const SCOPE: &str = "licence";

/// How a file's first line begins, the expression after it.
const SPDX: &str = "// SPDX-License-Identifier: ";

/// The expression of a file no row names.
const BASE: &str = "MIT AND BSD-3-Clause";

/// The `license` of a crate with no file the rule covers.
const ALONE: &str = "MIT";

/// The fourth line.
const DERIVED: &str = "// Derived in part from php-src, used under BSD-3-Clause: see NOTICE.";

/// The file whose first two lines are the header's second and third.
const COPYRIGHT: &str = "LICENSES/php-src.txt";

/// The file that lists what this tree takes from others, and the heading
/// of its table of ports, each row a file that is a component's.
const THIRD_PARTY: &str = "THIRD-PARTY.md";
const PORTS: &str = "## 3.";

/// How a file under an LGPL licence says that it was changed, which
/// LGPL-2.1 section 2 asks of a changed file beside the date.
const CHANGED: &str = "// Changed from ";

/// The extensions of the files held: Rust, C and C++.
const HELD: [&str; 8] = ["rs", "c", "h", "cc", "cpp", "cxx", "hh", "hpp"];

/// How many failures a reading words, of the files and of what is wider
/// than one file, before it counts the rest: a tree with no header at all
/// prints one screen.
const WORDED: usize = 8;

/// The files `rule`'s `allow` names, each by its path: the ones that
/// carry the header, a list a person keeps. An entry that is a pattern or
/// a directory is refused naming the rule and the entry, since it would
/// list files nobody named.
pub(crate) fn listed(rule: &Rule) -> Result<BTreeSet<&str>> {
    (rule.predicate.allow.iter().flatten())
        .map(|entry| match entry.as_str() {
            path if path.is_empty() || path.contains('*') || path.ends_with('/') => bail!(
                "rule \"{}\" lists {entry:?}; an entry of the `allow` beside check = \"{CHECK}\" is one file by its path, never a pattern or a directory",
                rule.id
            ),
            path => Ok(path),
        })
        .collect()
}

/// The four lines over `expression`, the copyright lines read from
/// [`COPYRIGHT`] at `root` so that they are held to that file and to no
/// copy of it here.
fn four(root: &Path, expression: &str) -> Result<Vec<String>> {
    let text = crate::read(&root.join(COPYRIGHT))
        .with_context(|| format!("the licence header's copyright lines are {COPYRIGHT}'s"))?;
    let mut lines = vec![format!("{SPDX}{expression}")];
    lines.extend(text.lines().take(2).map(|line| format!("// {line}")));
    lines.push(DERIVED.to_owned());
    Ok(lines)
}

/// A notice a component's file carries: its file under `LICENSES/`, and
/// the lines of the php-src file its row says it stands at.
struct Notice {
    file: String,
    source: String,
    lines: (usize, usize),
}

/// One row of the table of ports: a file that is a component's.
struct Component {
    /// The file's fifth line, which says whose it is.
    said: String,
    /// What the row's licence adds to the expression: nothing for a work
    /// in the public domain, or under a licence [`BASE`] names already.
    adds: Option<String>,
    notices: Vec<Notice>,
}

/// The rows of the table of ports at `root`, each by its file's path: the
/// name is the first cell to its comma, the path the second cell's first
/// `` `…` ``, the licence the fourth cell, and a notice each
/// `` `LICENSES/<name>` (`<php-src file>:<first>-<last>` `` of the fifth.
fn components(root: &Path) -> Result<BTreeMap<String, Component>> {
    let text = std::fs::read_to_string(root.join(THIRD_PARTY)).unwrap_or_default();
    let cited =
        regex_lite::Regex::new(r"`(LICENSES/[A-Za-z0-9._-]+)` \(`([^`:]+):([0-9]+)-([0-9]+)`")?;
    let rows = (text.lines().skip_while(|line| !line.starts_with(PORTS)))
        .skip(1)
        .take_while(|line| !line.starts_with("## "));
    let mut out = BTreeMap::new();
    for row in rows {
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        let [_, name, place, _, licence, notice, ..] = cells[..] else {
            continue;
        };
        let Some(path) = place.split('`').nth(1) else {
            continue;
        };
        let name = name.split(',').next().unwrap_or(name);
        let (how, adds) = if licence.starts_with("public domain") {
            (", which is in the public domain", None)
        } else if BASE.split(" AND ").any(|id| id == licence) {
            (" as php-src carries it, an altered version", None)
        } else if licence.contains(" OR ") {
            (
                " as php-src carries it, an altered version",
                Some(format!("({licence})")),
            )
        } else {
            (
                " as php-src carries it, an altered version",
                Some(licence.to_owned()),
            )
        };
        let mut notices = Vec::new();
        for found in cited.captures_iter(notice) {
            notices.push(Notice {
                file: found[1].to_owned(),
                source: found[2].to_owned(),
                lines: (found[3].parse()?, found[4].parse()?),
            });
        }
        let said = format!("// A port of {name}{how}: see {THIRD_PARTY}.");
        out.insert(
            path.to_owned(),
            Component {
                said,
                adds,
                notices,
            },
        );
    }
    Ok(out)
}

/// A file's lines from where its header begins, under a byte-order mark
/// and an interpreter line, with a cursor: the line read next is counted
/// from 1 there.
struct Head<'a> {
    path: &'a str,
    lines: Vec<&'a [u8]>,
    at: usize,
}

impl<'a> Head<'a> {
    /// `#!` opens an interpreter line unless `[` follows it, whatever
    /// whitespace stands between, which opens an inner attribute.
    fn of(path: &'a str, bytes: &'a [u8]) -> Head<'a> {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        let mut lines: Vec<&[u8]> = (bytes.split(|byte| *byte == b'\n'))
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .collect();
        let interpreter = lines.first().and_then(|line| line.strip_prefix(b"#!"));
        if interpreter.is_some_and(|rest| rest.trim_ascii_start().first() != Some(&b'[')) {
            lines.remove(0);
        }
        Head { path, lines, at: 0 }
    }

    /// The next line, an empty one where the file has ended.
    fn next(&mut self) -> &'a [u8] {
        self.at += 1;
        self.lines.get(self.at - 1).copied().unwrap_or_default()
    }

    /// The next line held to `want`, or the clause naming both.
    fn is(&mut self, want: impl AsRef<[u8]>) -> Result<(), String> {
        let (found, want) = (self.next(), want.as_ref());
        if found == want {
            return Ok(());
        }
        Err(format!(
            "{}'s line {} is {:?} and the header's is {:?}",
            self.path,
            self.at,
            String::from_utf8_lossy(found),
            String::from_utf8_lossy(want)
        ))
    }

    /// The next lines to the first that is `//` alone or no comment, as
    /// one text, where they begin as [`CHANGED`] does: where a file under
    /// an LGPL licence names its days.
    fn changed(&mut self) -> String {
        let rest = self.lines.get(self.at..).unwrap_or_default();
        let taken = (rest.iter()).take_while(|line| line.starts_with(b"// "));
        let text = joined(taken.map(|line| String::from_utf8_lossy(line)), "\n");
        self.at += text.lines().count();
        if text.starts_with(CHANGED) {
            text
        } else {
            String::new()
        }
    }
}

/// What the sensor read once, and holds each file to.
struct Held<'a> {
    root: &'a Path,
    /// The files the rule's `allow` names: the ones that carry the header.
    headed: BTreeSet<&'a str>,
    components: BTreeMap<String, Component>,
    /// The four lines over [`BASE`]; the last three are every header's.
    four: Vec<String>,
}

impl Held<'_> {
    /// `path` against what the file is held to, `days` being the days a
    /// file under an LGPL licence names: the file's expression, the
    /// header's and what its row adds, or the clause that says what it
    /// fails by. A file the list does not name states no licence where it
    /// begins; one it names carries the header whole.
    fn file(&self, path: &str, bytes: &[u8], days: &BTreeSet<String>) -> Result<String, String> {
        let mut head = Head::of(path, bytes);
        let component = self.components.get(path);
        let want = match component.and_then(|component| component.adds.as_ref()) {
            Some(adds) => format!("{BASE} AND {adds}"),
            None => BASE.to_owned(),
        };
        let stated = head
            .lines
            .first()
            .and_then(|line| line.strip_prefix(SPDX.as_bytes()));
        if !self.headed.contains(path) {
            return match stated {
                Some(stated) => Err(format!(
                    "{path} states {:?} where it begins and the rule's allow does not name it; a file carries the header only where that list names its path",
                    String::from_utf8_lossy(stated)
                )),
                None => Ok(want),
            };
        }
        match stated {
            Some(stated) if stated != want.as_bytes() => {
                let stated = String::from_utf8_lossy(stated);
                return Err(format!(
                    "{path} states {stated:?} and its expression is {want:?}"
                ));
            }
            _ => head.is(format!("{SPDX}{want}"))?,
        }
        for line in &self.four[1..] {
            head.is(line)?;
        }
        let Some(component) = component else {
            if head
                .lines
                .get(head.at)
                .is_some_and(|line| line.starts_with(b"// A port of "))
            {
                return Err(format!(
                    "{path} says it is a port and no row of {THIRD_PARTY}'s ports names it"
                ));
            }
            return head.is("").map(|()| want);
        };
        head.is(&component.said)?;
        if want.contains("LGPL-") {
            let named = head.changed();
            let unnamed: Vec<&str> = (days.iter().map(String::as_str))
                .filter(|day| !named.contains(day))
                .collect();
            if !unnamed.is_empty() {
                return Err(format!(
                    "{path} is under an LGPL licence and changed on {}; `{CHANGED}` lines under its fifth line name each day it changed, and none names {}",
                    joined(days, ", "),
                    unnamed.join(", ")
                ));
            }
        }
        for notice in &component.notices {
            self.block(&mut head, notice)?;
        }
        head.is("").map(|()| want)
    }

    /// One notice as the file carries it: an empty comment line, the line
    /// that says whose it is, the notice's lines as its file under
    /// `LICENSES/` holds them, and the line that names that file. That
    /// last line closes the block because rustfmt takes the empty line off
    /// from under a line comment that ends in `*/`, as notices do.
    fn block(&self, head: &mut Head<'_>, notice: &Notice) -> Result<(), String> {
        let (file, path) = (&notice.file, head.path);
        let text = std::fs::read(self.root.join(file))
            .map_err(|err| format!("{file}, which {path} carries, is not read: {err}"))?;
        head.is("//")?;
        let whose = String::from_utf8_lossy(head.next());
        if !(whose.starts_with("// ") && whose.contains(" notice, as ") && whose.ends_with(" it:"))
        {
            return Err(format!(
                "{path}'s line {} is {whose:?} and a notice begins under the line that says whose it is, `// <whose> notice, as <who> carries it:`",
                head.at
            ));
        }
        let text = text.strip_suffix(b"\n").unwrap_or(&text);
        for line in text.split(|byte| *byte == b'\n') {
            match line {
                [] => head.is("//")?,
                line => head.is([&b"//   "[..], line].concat())?,
            }
        }
        head.is(format!("// It stands, byte for byte, in {file}."))
    }

    /// The days `path` changed, as the author dates of its history have
    /// them, and the day a commit made now would carry where the file
    /// differs from `HEAD`'s: a commit that does not touch the file is
    /// held to nothing of today.
    fn days(&self, path: &str, bytes: &[u8], today: &Today<'_>) -> Result<BTreeSet<String>> {
        let log = [
            "log",
            "--follow",
            "--format=%ad",
            "--date=short",
            "--",
            path,
        ];
        let log = git::git(self.root, &log)?;
        let mut days: BTreeSet<String> = log.split_whitespace().map(str::to_owned).collect();
        if git::show_bytes(self.root, "HEAD", &format!("./{path}"))?.as_deref() != Some(bytes) {
            days.insert(today()?);
        }
        Ok(days)
    }
}

/// The day a commit made now would carry, asked only where a file under
/// an LGPL licence differs from `HEAD`'s.
type Today<'a> = dyn Fn() -> Result<String> + 'a;

/// The php-src files the notices under `LICENSES/` are held to, by their
/// place in that tree: what a checkout of the pinned php-src holds beside
/// the corpus's roots, so a sparse one is compared as a whole one is.
pub(crate) fn sources(root: &Path) -> Result<BTreeSet<String>> {
    let notices = (components(root)?.into_values()).flat_map(|component| component.notices);
    Ok(notices.map(|notice| notice.source).collect())
}

/// A notice's file under `LICENSES/` against the lines its row names in
/// the pinned tree at `tree`: the clause that says it is not them. A file
/// that is not there is the failure of the file that carries it.
fn recorded(root: &Path, tree: &Path, notice: &Notice) -> Option<String> {
    let have = std::fs::read(root.join(&notice.file)).ok()?;
    let source = std::fs::read(tree.join(&notice.source)).unwrap_or_default();
    let lines: Vec<&[u8]> = source.split(|byte| *byte == b'\n').collect();
    let (first, last) = notice.lines;
    let want = lines.get(first.wrapping_sub(1)..last).map(|lines| {
        let mut want = lines.join(&b'\n');
        want.push(b'\n');
        want
    });
    (want.as_deref() != Some(&have[..])).then(|| {
        format!(
            "{} is not lines {first}-{last} of {} in the pinned php-src",
            notice.file, notice.source
        )
    })
}

/// The `license` the `[at]` table of the `Cargo.toml` at `path` states,
/// `[package]`'s or `[workspace.package]`'s: empty where it states none,
/// and none where what it states is no text, a crate's that inherits.
fn license(path: &Path, at: &[&str]) -> Result<Option<String>> {
    let table: toml::Table = crate::read_toml(path)?;
    let mut value = Some(&toml::Value::Table(table));
    for key in at.iter().chain(&["license"]) {
        value = value.and_then(|table| table.get(key));
    }
    Ok(match value {
        Some(toml::Value::String(license)) => Some(license.clone()),
        Some(_) => None,
        None => Some(String::new()),
    })
}

/// Each crate under `crates` whose `license` does not cover its files'
/// expressions, `stated` by the crate's directory, read through the
/// workspace's where the crate inherits it; and each crate with no file
/// the rule covers that inherits a `license` wider than [`ALONE`], as
/// the harness and a lane would.
fn uncovered(
    root: &Path,
    crates: &[PathBuf],
    stated: &BTreeMap<&PathBuf, BTreeSet<String>>,
) -> Result<Vec<String>> {
    let workspace = root.join("Cargo.toml");
    let workspace = if workspace.is_file() {
        license(&workspace, &["workspace", "package"])?
    } else {
        None
    };
    let mut out = Vec::new();
    for dir in crates {
        let at = dir.join("Cargo.toml");
        if !root.join(&at).is_file() {
            continue;
        }
        let own = license(&root.join(&at), &["package"])?;
        let whose = if own.is_none() {
            ", the workspace's,"
        } else {
            ""
        };
        let license = own.or_else(|| workspace.clone()).unwrap_or_default();
        let (at, ids) = (
            at.display(),
            license.split(" AND ").collect::<BTreeSet<_>>(),
        );
        let lacks: Vec<&str> = (stated.get(dir).into_iter().flatten())
            .map(String::as_str)
            .filter(|id| !ids.contains(id))
            .collect();
        if !lacks.is_empty() {
            let lacks = lacks.join(", ");
            out.push(format!(
                "{at} says license = {license:?}{whose} and its files are under {lacks}"
            ));
        } else if !whose.is_empty() && license != ALONE && !stated.contains_key(dir) {
            out.push(format!(
                "{at} inherits the workspace's license = {license:?} and the rule covers no file of the crate; such a crate says license = {ALONE:?} itself"
            ));
        }
    }
    Ok(out)
}

/// What the sensor found over one workspace.
#[derive(Debug, Default)]
struct Survey {
    /// The files held, and of them the components' and the ones the
    /// rule's `allow` names, which carry the header.
    files: usize,
    components: usize,
    headed: usize,
    /// Each file's failure, in the order of the paths, and each failure
    /// wider than one file: a list, a row, a crate, a notice's record.
    unmet: Vec<String>,
    wider: Vec<String>,
    /// The notices under `LICENSES/` held to the pinned tree, and the
    /// ones that were not, since no tree is laid.
    recorded: Vec<String>,
    unrecorded: Vec<String>,
}

/// `rule` held over the workspace at `root`: `tree` is the pinned php-src
/// where it is laid.
fn survey(
    root: &Path,
    manifest: &Manifest,
    rule: &Rule,
    tree: Option<&Path>,
    today: &Today<'_>,
) -> Result<Survey> {
    let held = Held {
        root,
        headed: listed(rule)?,
        components: components(root)?,
        four: four(root, BASE)?,
    };
    let mut survey = Survey::default();
    let layout = manifest.layout();
    let mut files = git::files_under(root, &layout.dirs().collect::<Vec<_>>())?;
    files.retain(|file| {
        let (path, extension) = (Path::new(file), Path::new(file).extension());
        extension.is_some_and(|extension| HELD.iter().any(|held| extension == *held))
            && rule.covers(path)
            && !rule.exempts(path)
    });
    let crates: Vec<PathBuf> = manifest.crates.iter().map(Crate::path).collect();
    let mut stated: BTreeMap<&PathBuf, BTreeSet<String>> = BTreeMap::new();
    for file in &files {
        let bytes = std::fs::read(root.join(file)).with_context(|| format!("reading {file}"))?;
        let component = held.components.get(file);
        let headed = held.headed.contains(file.as_str());
        let lgpl = component.and_then(|component| component.adds.as_ref());
        let days = if headed && lgpl.is_some_and(|adds| adds.contains("LGPL-")) {
            held.days(file, &bytes, today)?
        } else {
            BTreeSet::new()
        };
        match held.file(file, &bytes, &days) {
            Ok(expression) => {
                let owner = (crates.iter()).filter(|dir| Path::new(file).starts_with(dir));
                if let Some(owner) = owner.max_by_key(|dir| dir.as_os_str().len()) {
                    let ids = expression.split(" AND ").map(str::to_owned);
                    stated.entry(owner).or_default().extend(ids);
                }
            }
            Err(said) => survey.unmet.push(said),
        }
        survey.components += usize::from(component.is_some());
        survey.headed += usize::from(headed);
    }
    survey.files = files.len();
    let absent = |path: &str| !files.iter().any(|file| file == path);
    for path in held.headed.iter().filter(|path| absent(path)) {
        let said = format!("the rule's allow names {path}, which is no file the rule covers");
        survey.wider.push(said);
    }
    for (path, component) in &held.components {
        if absent(path) {
            let said =
                format!("{THIRD_PARTY}'s ports name {path}, which is no file the rule covers");
            survey.wider.push(said);
        } else if component.notices.is_empty() {
            survey.wider.push(format!(
                "{THIRD_PARTY}'s row for {path} names no notice as `LICENSES/<name>` (`<php-src file>:<first>-<last>`)"
            ));
        }
    }
    let notices: BTreeMap<&str, &Notice> = (held.components.values())
        .flat_map(|component| &component.notices)
        .map(|notice| (notice.file.as_str(), notice))
        .collect();
    for (file, notice) in notices {
        match tree.map(|tree| recorded(root, tree, notice)) {
            Some(Some(said)) => survey.wider.push(said),
            Some(None) => survey.recorded.push(file.to_owned()),
            None => survey.unrecorded.push(file.to_owned()),
        }
    }
    survey.wider.extend(uncovered(root, &crates, &stated)?);
    Ok(survey)
}

/// The day a commit made now would carry, in its author's own zone, which
/// is the day its history will name. Git hands a hook the commit's own as
/// `GIT_AUTHOR_DATE`, `@<seconds> <zone>`, and it is read off this process
/// since a tree laid apart is asked without the hook's variables; with no
/// commit in hand, `git var` ends the author's identity with the clock's.
fn today(root: &Path) -> Result<String> {
    match std::env::var("GIT_AUTHOR_DATE") {
        Ok(date) if date.starts_with('@') => day_of(&date),
        _ => day_of(&git::git(root, &["var", "GIT_AUTHOR_IDENT"])?),
    }
}

/// The day of the last two fields of `ident`, seconds since 1970 and the
/// zone as `+hhmm`, in that zone.
fn day_of(ident: &str) -> Result<String> {
    let mut fields = ident.trim().rsplit(' ');
    let (zone, seconds) = (fields.next(), fields.next());
    let field = |field: Option<&str>| -> Result<i64> {
        (field.unwrap_or_default().trim_start_matches('@').parse())
            .with_context(|| format!("{ident:?} does not end with a time and its zone"))
    };
    let (zone, seconds) = (field(zone)?, field(seconds)?);
    let local = seconds + (zone / 100) * 3600 + (zone % 100) * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

/// `budget`'s reading of the rule that names this sensor, over the
/// workspace `snapshot` holds: none where the manifest carries no such
/// rule. The pinned tree is the one the pin finds on this machine.
pub(crate) fn reading(snapshot: &Snapshot, manifest: &Manifest) -> Result<Option<Reading>> {
    let Some(rule) = manifest.rule_checked_by(CHECK) else {
        return Ok(None);
    };
    let root = snapshot.root();
    let tree = match &manifest.pin {
        Some(pin) => pin.source.tree(),
        None => Err(anyhow::anyhow!("the manifest names no pin")),
    };
    let survey = survey(root, manifest, rule, tree.as_deref().ok(), &|| today(root))?;
    let mut reading =
        Reading::stamped_over(crate::budget::INSTRUMENT, SCOPE, snapshot, Path::new("."))?;
    reading.rule.clone_from(&rule.id);
    reading.adr = rule.adr.adr_field();
    let counted = [
        ("files", survey.files),
        ("components", survey.components),
        ("headed", survey.headed),
        ("recorded", survey.recorded.len()),
    ];
    for (key, n) in counted {
        reading.count(key, u64::try_from(n)?)?;
    }
    if survey.unmet.is_empty() && survey.wider.is_empty() {
        reading.status = Status::Pass;
        let record = match &tree {
            Ok(_) => "each notice under LICENSES/ is the pinned php-src's lines".to_owned(),
            Err(why) => format!(
                "{} under LICENSES/ not held to the pinned php-src: {why:#}",
                survey.unrecorded.join(", ")
            ),
        };
        reading.message = format!(
            "licence: {} files, the header on the {} the rule's allow names and no licence line on the others; {} are a component's; {record}",
            survey.files, survey.headed, survey.components
        );
    } else {
        reading.status = Status::Fail;
        let worded = [&survey.unmet, &survey.wider].map(|said| {
            let more = said.len().saturating_sub(WORDED);
            let mut worded = said[..said.len() - more].to_vec();
            if more > 0 {
                worded.push(format!("and {more} more"));
            }
            worded
        });
        reading.message = format!(
            "{} (seams.toml rule {}: {})",
            worded.concat().join("; "),
            rule.id,
            rule.said()
        );
    }
    Ok(Some(reading))
}

#[cfg(test)]
mod tests;
