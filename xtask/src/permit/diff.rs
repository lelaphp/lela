//! The diff judge: a set of files, each judged as the `Write` of its
//! content, read as one verdict by `done`'s condition 5 over the working
//! tree and by pre-commit's `permit --diff` over the index
//! (harness-design.md §4.4, §4.5, §6).
//!
//! Each file carries what the base has at its path beside what it would
//! carry, so a rule that fires on what a write adds is asked of the diff
//! and not of the file against itself (#510): the base is the index's
//! `HEAD` and the commit `done` measures since, and a path the base has
//! not, or a base that cannot be read, displaces nothing, which is the
//! reading that denies rather than excuses.

use std::collections::BTreeSet;
use std::collections::btree_map::{BTreeMap, Entry};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::adapters::{Adapter, record};
use super::call::Call;
use super::rules::{Finding, Judgement, Kind, judge};
use crate::manifest::Manifest;
use crate::owner::SessionId;
use crate::reading::{Status, Store};
use crate::{git, steer};

/// What permit found over a set of files, each judged as the `Write` of
/// its content: `done`'s condition 5 and pre-commit's `--diff` read one
/// verdict (harness-design.md §4.4, §4.5, §6). A denial under a rule the
/// manifest marks `deny = true`, the harness's own files, stands apart: a
/// person commits those, so it is reported as a note, except where an
/// agent's commit stages one no steer reading covers ([`Judged::refuse`],
/// #3644 ruling 3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Judged {
    /// Each file's judgement, in the order given.
    pub files: Vec<(String, Judgement)>,
    /// The ids of the rules the manifest marks `deny = true`.
    a_persons: Vec<String>,
    /// The harness files an agent's commit stages uncovered, refused.
    refused: Vec<String>,
}

impl Judged {
    /// The denials that refuse the diff: every `Deny` finding outside a
    /// person's rules.
    pub fn denials(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Deny).filter(|(path, f)| {
            !self.a_persons.contains(&f.rule) || self.refused.iter().any(|r| r == path)
        })
    }

    /// The harness's own files in the diff: a `Deny` under a person's rule.
    pub fn a_persons(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Deny).filter(|(path, f)| {
            self.a_persons.contains(&f.rule) && !self.refused.iter().any(|r| r == path)
        })
    }

    /// An agent's commit (`CLAUDE_CODE_SESSION_ID` set), refused for each
    /// harness file it stages that no steer reading covers: one that
    /// passed, wrote, and is stamped over the staged blob (#3644 ruling 3). This closes the gap where an agent clears `done`'s
    /// kept change by committing the file itself. A merge's file taken
    /// whole from one side is that side's commit and never staged here
    /// ([`staged_files`]); a person's commit is not asked.
    pub(crate) fn refuse(&mut self, root: &Path) -> Result<()> {
        let paths: BTreeSet<String> = self.a_persons().map(|(p, _)| p.to_owned()).collect();
        for path in paths {
            if !steered(root, &path)? {
                self.refused.push(path);
            }
        }
        Ok(())
    }

    /// The notes that ride on the diff.
    pub fn notes(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Note)
    }

    fn findings(&self, kind: Kind) -> impl Iterator<Item = (&str, &Finding)> {
        self.files.iter().flat_map(move |(path, judgement)| {
            judgement
                .findings
                .iter()
                .filter(move |f| f.kind == kind)
                .map(move |f| (path.as_str(), f))
        })
    }

    /// The lines pre-commit prints: one per finding, the rule id in each.
    pub fn render(&self) -> String {
        let denials = self.denials().map(|found| ("deny", found));
        let notes = self
            .a_persons()
            .chain(self.notes())
            .map(|found| ("note", found));
        let mut out = String::new();
        for (word, (path, finding)) in denials.chain(notes) {
            writeln!(out, "{word} {path}: {}", finding.reason()).ok();
            if word == "deny" && self.refused.iter().any(|r| r == path) {
                writeln!(
                    out,
                    "deny {path}: an agent's commit (CLAUDE_CODE_SESSION_ID is set) stages it, and \
                     no steer reading covers what is staged, so the commit is refused (#3644); a \
                     person commits it, a row a person settled lands by `cargo xtask steer`, and \
                     `git restore --staged -- {path}` takes it out of this commit"
                )
                .ok();
            }
        }
        out
    }

    /// Every finding as a reading, under pre-commit's own caller.
    pub fn record(&self, root: &Path) -> Result<()> {
        for (path, judgement) in &self.files {
            record(
                root,
                &Call::write_of(path, None),
                judgement,
                Adapter::PreCommit,
            )?;
        }
        Ok(())
    }
}

/// One file of a diff: where it is and what it would carry there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diffed {
    pub path: String,
    pub content: Content,
}

/// The extensions a file is text by whatever its bytes: what rustc,
/// cargo, php and the harness read as text, none of which this repository
/// tracks as binary. git's `-text` is a NUL or a lone CR anywhere, which
/// rustc takes in a comment, so a file of these types read as bytes was
/// one whose author took its content out of every rule's reach (833e084b7).
const TEXT: [&str; 17] = [
    "rs", "toml", "md", "php", "phpt", "inc", "sh", "py", "json", "txt", "yml", "yaml", "lock",
    "c", "h", "js", "ts",
];

/// Which of `paths` is judged by its path alone: those git reads as binary
/// on `side` ([`git::binary`]) whose path is no type of [`TEXT`].
fn binary(root: &Path, side: git::Side, paths: &[&str]) -> Result<BTreeSet<String>> {
    let mut binary = git::binary(root, side, paths)?;
    binary
        .retain(|path| (Path::new(path).extension()).is_none_or(|e| !TEXT.iter().any(|t| e == *t)));
    Ok(binary)
}

/// What a file of a diff carries, as its path and git read it ([`binary`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Content {
    /// Text, a byte that is not UTF-8 read as U+FFFD, beside what the
    /// base carries at the path, or in a merge what each parent does,
    /// which is what the write displaces.
    Text {
        text: String,
        displaced: Vec<String>,
    },
    /// Bytes git reads as binary, judged by the path alone as the `Write`
    /// of no text: what a rule reads is text, and a binary file read as
    /// text, a stand-in build in a fixture, is bytes no rule describes
    /// (#3458).
    Binary,
}

/// Judge `files`, each a path relative to `root` and the content that
/// would be committed there. A file in a workspace of its own under
/// `root`, a fixture that carries a manifest, is judged by that manifest
/// as the hook judges an edit to it (`Call::workspace`, #57); the
/// manifest is read once per workspace, and one that cannot be read is
/// the error, since a file no manifest describes is not a file nothing
/// denies.
pub fn judge_files(manifest: &Manifest, root: &Path, files: &[Diffed]) -> Result<Judged> {
    let mut a_persons: Vec<String> = a_persons_of(manifest).collect();
    let mut nested: BTreeMap<PathBuf, Manifest> = BTreeMap::new();
    let mut judged = Vec::new();
    for Diffed { path, content } in files {
        let at = root.join(path).to_string_lossy().into_owned();
        let call = match content {
            Content::Text { text, displaced } => Call::diff_of(&at, text, displaced),
            Content::Binary => Call::write_of(&at, None),
        };
        let judgement = match call.nested(root) {
            None => judge(manifest, root, &call)?,
            Some(workspace) => {
                let own = match nested.entry(workspace.clone()) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        let own = Manifest::load(&workspace).with_context(|| {
                            format!(
                                "{path} is judged by the manifest of {}",
                                workspace.display()
                            )
                        })?;
                        for id in a_persons_of(&own) {
                            if !a_persons.contains(&id) {
                                a_persons.push(id);
                            }
                        }
                        entry.insert(own)
                    }
                };
                judge(own, &workspace, &call)?
            }
        };
        judged.push((path.clone(), judgement));
    }
    Ok(Judged {
        files: judged,
        a_persons,
        refused: Vec::new(),
    })
}

/// Whether a steer reading covers the staged content at `path`: steer's
/// latest reading of the file passed, wrote, and is stamped over the
/// staged blob, as `permit --after` holds steer's write ([`super::after`]).
fn steered(root: &Path, path: &str) -> Result<bool> {
    let Some(reading) = Store::of_workspace(root).read(steer::INSTRUMENT, path)? else {
        return Ok(false);
    };
    let Ok(staged) = git::rev_parse(root, &["-q", "--verify", &format!(":{path}")]) else {
        return Ok(false);
    };
    Ok(reading.status == Status::Pass
        && !reading.message.contains("dry run")
        && reading.key() == git::file_hash(path, &staged))
}

/// The ids of the rules a manifest marks `deny = true`.
fn a_persons_of(manifest: &Manifest) -> impl Iterator<Item = String> + '_ {
    manifest
        .rules
        .iter()
        .filter(|r| r.predicate.deny == Some(true))
        .map(|r| r.id.clone())
}

/// What `rev` carries at `path`, the empty text where it carries nothing
/// and where the revision cannot be read at all: a base that answers with
/// an error is no evidence that a line stood there, and reading it as
/// empty is the answer that denies rather than excuses.
fn displaced(root: &Path, rev: &str, path: &str) -> String {
    git::show_bytes(root, rev, path)
        .ok()
        .flatten()
        .map(text)
        .unwrap_or_default()
}

/// Bytes as the judge reads them: text, a byte that is not UTF-8 as
/// U+FFFD.
fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|not| String::from_utf8_lossy(not.as_bytes()).into_owned())
}

/// The staged files with their staged content: what a commit would take,
/// against what `HEAD` carries. A deleted file has nothing to judge. In a
/// merge the commit displaces both parents, so it is judged against each
/// parent's text whole ([`super::rules::adds`]): a hit either parent
/// carried, such as an `exit=` the branch's `witness --accept` captured,
/// is that parent's and not the merge's (#1312), a block over several
/// lines among them, whose lines the merge base may carry apart (#2783);
/// and a file the merge takes whole from the merged parent is that
/// parent's, judged at its own commits, so the merge is not asked of it.
/// A hit neither parent carried as often is the merge's. A file git
/// reads as binary in the index is judged by its path alone.
fn staged_files(root: &Path) -> Result<Vec<Diffed>> {
    let theirs = merged(root);
    let staged: Vec<git::Change> = (git::staged(root)?.into_iter())
        .filter(|c| !c.deleted)
        .collect();
    let paths: Vec<&str> = staged.iter().map(|c| c.path.as_str()).collect();
    let binary = binary(root, git::Side::Index, &paths)?;
    let mut files = Vec::new();
    for git::Change { path, .. } in staged {
        if binary.contains(&path) {
            // A binary the merge takes whole from the merged parent is
            // that parent's, as a text file is.
            if let Some(theirs) = &theirs
                && git::show_bytes(root, theirs, &path)? == git::show_bytes(root, "", &path)?
            {
                continue;
            }
            files.push(Diffed {
                path,
                content: Content::Binary,
            });
            continue;
        }
        let Some(staged) = git::show_bytes(root, "", &path)?.map(text) else {
            continue;
        };
        let mut parents = vec![displaced(root, "HEAD", &path)];
        if let Some(theirs) = &theirs {
            let merged = displaced(root, theirs, &path);
            if merged == staged {
                continue;
            }
            parents.push(merged);
        }
        let content = Content::Text {
            text: staged,
            displaced: parents,
        };
        files.push(Diffed { path, content });
    }
    Ok(files)
}

/// The commit a merge takes in: `MERGE_HEAD`, which a merge stopped
/// before its commit keeps, or, for a merge git commits itself, whose
/// pre-merge-commit runs with no `MERGE_HEAD` written, the first name
/// `GIT_REFLOG_ACTION` gives after `merge` that is a commit (#3644).
fn merged(root: &Path) -> Option<String> {
    if let Ok(head) = git::rev_parse(root, &["-q", "--verify", "MERGE_HEAD"]) {
        return Some(head);
    }
    let action = std::env::var("GIT_REFLOG_ACTION").ok()?;
    (action.strip_prefix("merge ")?.split_whitespace())
        .filter(|word| !word.starts_with('-'))
        .find_map(|word| {
            let commit = format!("{word}^{{commit}}");
            git::rev_parse(root, &["-q", "--verify", &commit]).ok()
        })
}

/// The changed files with their working-tree content, as `done` judges
/// them, against what `base` carries; a deleted or vanished file has
/// nothing to judge, and one git reads as binary in the working tree is
/// judged by its path alone.
pub(crate) fn working_files(
    root: &Path,
    base: &str,
    changed: &[git::Change],
) -> Result<Vec<Diffed>> {
    let changed: Vec<&str> = (changed.iter())
        .filter(|c| !c.deleted && root.join(&c.path).is_file())
        .map(|c| c.path.as_str())
        .collect();
    let binary = binary(root, git::Side::Worktree, &changed)?;
    let mut files = Vec::new();
    for path in changed {
        let content = if binary.contains(path) {
            Content::Binary
        } else {
            let file = root.join(path);
            let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let displaced = vec![displaced(root, base, path)];
            Content::Text {
                text: text(bytes),
                displaced,
            }
        };
        let path = path.to_owned();
        files.push(Diffed { path, content });
    }
    Ok(files)
}

/// `permit --diff`: judge the staged diff, print the findings, fail while
/// anything is denied, so the commit is refused with the rule id.
pub(crate) fn run_diff(root: &Path, manifest: &Manifest) -> Result<()> {
    let files = staged_files(root)?;
    let mut diff = judge_files(manifest, root, &files)?;
    if SessionId::current().is_some() {
        diff.refuse(root)?;
    }
    diff.record(root)?;
    print!("{}", diff.render());
    let denied = diff.denials().count();
    if denied == 0 {
        Ok(())
    } else {
        bail!(
            "permit denies {denied} of {} file{} in the diff",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        )
    }
}
