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

use std::collections::btree_map::{BTreeMap, Entry};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::adapters::{Adapter, record};
use super::call::Call;
use super::rules::{Finding, Judgement, Kind, judge};
use crate::git;
use crate::manifest::Manifest;

/// What permit found over a set of files, each judged as the `Write` of
/// its content: `done`'s condition 5 and pre-commit's `--diff` read one
/// verdict (harness-design.md §4.4, §4.5, §6). A denial under a rule the
/// manifest marks `deny = true`, the harness's own files, stands apart: a
/// person commits those, and the hook cannot tell a person from an agent,
/// so it refuses nothing and is reported as a note.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Judged {
    /// Each file's judgement, in the order given.
    pub files: Vec<(String, Judgement)>,
    /// The ids of the rules the manifest marks `deny = true`.
    a_persons: Vec<String>,
}

impl Judged {
    /// The denials that refuse the diff: every `Deny` finding outside a
    /// person's rules.
    pub fn denials(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Deny)
            .filter(|(_, f)| !self.a_persons.contains(&f.rule))
    }

    /// The harness's own files in the diff: a `Deny` under a person's rule.
    pub fn a_persons(&self) -> impl Iterator<Item = (&str, &Finding)> {
        self.findings(Kind::Deny)
            .filter(|(_, f)| self.a_persons.contains(&f.rule))
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
        }
        out
    }

    /// Every finding as a reading, as the hook records one call's.
    pub fn record(&self, root: &Path) -> Result<()> {
        for (path, judgement) in &self.files {
            record(
                root,
                &Call::write_of(path, None),
                judgement,
                Adapter::CommandHook,
            )?;
        }
        Ok(())
    }
}

/// One file of a diff: where it is, what it would carry there, and what
/// the base carries at that path, which is what the write displaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diffed {
    pub path: String,
    pub content: String,
    /// What the base carries at the path, or in a merge what each parent
    /// does.
    pub displaced: Vec<String>,
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
    for Diffed {
        path,
        content,
        displaced,
    } in files
    {
        let call = Call::diff_of(&root.join(path).to_string_lossy(), content, displaced);
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
    })
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
    git::show(root, rev, path)
        .ok()
        .flatten()
        .unwrap_or_default()
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
/// A hit neither parent carried as often is the merge's.
fn staged_files(root: &Path) -> Result<Vec<Diffed>> {
    let theirs = git::git(root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"], None).ok();
    let mut files = Vec::new();
    for change in git::staged(root)?.into_iter().filter(|c| !c.deleted) {
        if let Some(content) = git::staged_content(root, &change.path)? {
            let mut parents = vec![displaced(root, "HEAD", &change.path)];
            if let Some(theirs) = &theirs {
                let merged = displaced(root, theirs.trim(), &change.path);
                if merged == content {
                    continue;
                }
                parents.push(merged);
            }
            files.push(Diffed {
                path: change.path,
                content,
                displaced: parents,
            });
        }
    }
    Ok(files)
}

/// The changed files with their working-tree content, as `done` judges
/// them, against what `base` carries; a deleted or vanished file has
/// nothing to judge.
pub(crate) fn working_files(
    root: &Path,
    base: &str,
    changed: &[git::Change],
) -> Result<Vec<Diffed>> {
    let mut files = Vec::new();
    for change in changed.iter().filter(|c| !c.deleted) {
        let file = root.join(&change.path);
        if !file.is_file() {
            continue;
        }
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        files.push(Diffed {
            path: change.path.clone(),
            content: String::from_utf8_lossy(&bytes).into_owned(),
            displaced: vec![displaced(root, base, &change.path)],
        });
    }
    Ok(files)
}

/// `permit --diff`: judge the staged diff, print the findings, fail while
/// anything is denied, so the commit is refused with the rule id.
pub(crate) fn run_diff(root: &Path, manifest: &Manifest) -> Result<()> {
    let files = staged_files(root)?;
    let diff = judge_files(manifest, root, &files)?;
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
