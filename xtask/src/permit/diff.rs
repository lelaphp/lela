//! The diff judge: a set of files, each judged as the `Write` of its
//! content, read as one verdict by `done`'s condition 5 over the working
//! tree and by pre-commit's `permit --diff` over the index
//! (harness-design.md §4.4, §4.5, §6).

use std::collections::btree_map::{BTreeMap, Entry};
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
        let mut out = String::new();
        for (path, finding) in self.denials() {
            out.push_str("deny ");
            out.push_str(path);
            out.push_str(": ");
            out.push_str(&finding.reason());
            out.push('\n');
        }
        for (path, finding) in self.a_persons().chain(self.notes()) {
            out.push_str("note ");
            out.push_str(path);
            out.push_str(": ");
            out.push_str(&finding.reason());
            out.push('\n');
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

/// Judge `files`, each a path relative to `root` and the content that
/// would be committed there. A file in a workspace of its own under
/// `root`, a fixture that carries a manifest, is judged by that manifest
/// as the hook judges an edit to it (`Call::workspace`, #57); the
/// manifest is read once per workspace, and one that cannot be read is
/// the error, since a file no manifest describes is not a file nothing
/// denies.
pub fn judge_files(manifest: &Manifest, root: &Path, files: &[(String, String)]) -> Result<Judged> {
    let mut a_persons: Vec<String> = a_persons_of(manifest).collect();
    let mut nested: BTreeMap<PathBuf, Manifest> = BTreeMap::new();
    let mut judged = Vec::new();
    for (path, content) in files {
        let call = Call::write_of(&root.join(path).to_string_lossy(), Some(content));
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

/// The staged files with their staged content: what a commit would take.
/// A deleted file has nothing to judge.
fn staged_files(root: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for change in git::staged(root)?.into_iter().filter(|c| !c.deleted) {
        if let Some(content) = git::staged_content(root, &change.path)? {
            files.push((change.path, content));
        }
    }
    Ok(files)
}

/// The changed files with their working-tree content, as `done` judges
/// them; a deleted or vanished file has nothing to judge.
pub(crate) fn working_files(root: &Path, changed: &[git::Change]) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for change in changed.iter().filter(|c| !c.deleted) {
        let file = root.join(&change.path);
        if !file.is_file() {
            continue;
        }
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        files.push((
            change.path.clone(),
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
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
