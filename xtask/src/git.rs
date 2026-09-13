//! What git says about the tree a reading was taken over: the stamp, the
//! tree hash of its scope's sources and the commit; the files of a scope;
//! the git process; and the diffs `done` and pre-commit read. The tree
//! hash is what makes a reading fresh or stale without a turn counter
//! (harness-design.md §4.2): it covers the working tree, tracked and
//! untracked files alike, so an unstaged edit changes it. The time a
//! reading carries is the reading's own; the track and its base are
//! `track`'s.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Stamp {
    pub tree: String,
    pub commit: String,
}

impl Stamp {
    /// Stamp the sources under `scope` (relative to `root`) now.
    pub fn take(root: &Path, scope: &Path) -> Result<Stamp> {
        Ok(Stamp {
            tree: tree_hash(root, scope)?,
            commit: head_commit(root)?,
        })
    }
}

/// The short id of `HEAD`, read from the repository's files; `git
/// rev-parse` when the layout is one this reader does not know. A stamp is
/// taken at every denial, so it spawns git once (`ls-files`) and not four
/// times.
pub fn head_commit(root: &Path) -> Result<String> {
    match read_head(root) {
        Some(id) => Ok(id),
        None => Ok(git(root, &["rev-parse", "--short", "HEAD"], None)?
            .trim()
            .to_owned()),
    }
}

/// `HEAD` through `.git` (a directory, or a worktree's file naming one),
/// a symbolic ref through `refs/` and then `packed-refs`, shortened to the
/// seven characters `rev-parse --short` prints on a small repository.
fn read_head(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let line = fs::read_to_string(&dot_git).ok()?;
        let target = line.strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            root.join(target)
        }
    };
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir.clone(),
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let full = match head.strip_prefix("ref:") {
        None => head.to_owned(),
        Some(name) => {
            let name = name.trim();
            match fs::read_to_string(common.join(name)) {
                Ok(id) => id.trim().to_owned(),
                Err(_) => fs::read_to_string(common.join("packed-refs"))
                    .ok()?
                    .lines()
                    .find_map(|l| l.strip_suffix(name).map(|id| id.trim().to_owned()))?,
            }
        }
    };
    (full.len() == 40 && full.chars().all(|c| c.is_ascii_hexdigit())).then(|| full[..7].to_owned())
}

/// Every file under `scope` (relative to `root`) that git does not ignore,
/// tracked or not, as paths relative to `root`, sorted. What an instrument
/// means by "the sources" of a scope; `tree_hash` is one hash over them.
pub fn files(root: &Path, scope: &Path) -> Result<Vec<String>> {
    let scope = scope.to_str().context("the scope path is not UTF-8")?;
    let listing = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            scope,
        ],
        None,
    )?;
    let mut paths: Vec<String> = listing
        .split('\0')
        .filter(|p| !p.is_empty() && root.join(p).is_file())
        .map(str::to_owned)
        .collect();
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// One object id over every file under `scope` that git does not ignore,
/// tracked or not, with its content as it is on disk: the blob id of each
/// file, then the blob id of the sorted `<id> <path>` listing, which is
/// what `git hash-object` would print for the same bytes.
pub fn tree_hash(root: &Path, scope: &Path) -> Result<String> {
    let mut manifest = String::new();
    for path in files(root, scope)? {
        let file = root.join(&path);
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        manifest.push_str(&blob_id(&bytes));
        manifest.push(' ');
        manifest.push_str(&path);
        manifest.push('\n');
    }
    Ok(blob_id(manifest.as_bytes()))
}

/// The id git gives a blob: SHA-1 over `blob <len>\0<content>`.
pub fn blob_id(content: &[u8]) -> String {
    let mut hash = sha1_smol::Sha1::new();
    hash.update(format!("blob {}\0", content.len()).as_bytes());
    hash.update(content);
    hash.digest().to_string()
}

/// Whether `ancestor` is an ancestor of `commit`, or the same commit:
/// `merge-base --is-ancestor`, whose exit is the answer.
pub(crate) fn is_ancestor(root: &Path, ancestor: &str, commit: &str) -> bool {
    git(
        root,
        &["merge-base", "--is-ancestor", ancestor, commit],
        None,
    )
    .is_ok()
}

/// Whether `root` has the ref `name` (`refs/heads/x`, `refs/remotes/origin/x`).
pub(crate) fn has_ref(root: &Path, name: &str) -> bool {
    git(root, &["rev-parse", "--verify", "--quiet", name], None).is_ok()
}

/// Run git in `root` and return its stdout; a non-zero exit is an error
/// carrying git's stderr.
pub(crate) fn git(root: &Path, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).context("feeding git")?;
    }
    let out = child.wait_with_output().context("waiting for git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git wrote something that is not UTF-8")
}

/// One file the working tree differs in from a base commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Relative to the root.
    pub path: String,
    /// The file is gone from the working tree.
    pub deleted: bool,
}

/// Every file that differs between `base` and the working tree, committed
/// since the base or not, plus every untracked file git does not ignore;
/// sorted by path. A rename is its deletion and its addition.
pub fn changed_since(root: &Path, base: &str) -> Result<Vec<Change>> {
    let listing = git(
        root,
        &["diff", "--name-status", "-z", "--no-renames", base, "--"],
        None,
    )?;
    let mut fields = listing.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        out.push(Change {
            path: path.to_owned(),
            deleted: status.starts_with('D'),
        });
    }
    let untracked = git(
        root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        None,
    )?;
    out.extend(
        untracked
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| Change {
                path: p.to_owned(),
                deleted: false,
            }),
    );
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup();
    Ok(out)
}

/// Every file the index differs in from `HEAD`: what a commit would take,
/// and what pre-commit judges (harness-design.md §6). Sorted by path; a
/// rename is its deletion and its addition.
pub fn staged(root: &Path) -> Result<Vec<Change>> {
    let listing = git(
        root,
        &[
            "diff",
            "--cached",
            "--name-status",
            "-z",
            "--no-renames",
            "--",
        ],
        None,
    )?;
    let mut fields = listing.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        out.push(Change {
            path: path.to_owned(),
            deleted: status.starts_with('D'),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The content of `path` in the index: `show(root, "", path)`, since git
/// spells the index as an empty revision.
pub fn staged_content(root: &Path, path: &str) -> Result<Option<String>> {
    show(root, "", path)
}

/// The content of `path` at `rev`, or `None` when the commit has no such
/// file.
pub fn show(root: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    match git(root, &["show", &format!("{rev}:{path}")], None) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.to_string().contains("does not exist") => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn blob_ids_are_git_object_ids() {
        assert_eq!(
            super::blob_id(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            super::blob_id(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
    use std::path::Path;

    use super::{Stamp, files, head_commit, tree_hash};
    use crate::built_root;
    use crate::manifest::Manifest;
    use crate::reading::{Outcome, Reading, Status, Store};

    #[test]
    fn a_stamp_has_a_tree_hash_and_a_commit() {
        let stamp = Stamp::take(&built_root(), Path::new("xtask")).unwrap();
        assert_eq!(stamp.tree.len(), 40, "{}", stamp.tree);
        assert!(stamp.tree.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!stamp.commit.is_empty());
    }

    #[test]
    fn the_tree_hash_follows_the_sources_it_covers() {
        let root = built_root();
        let a = Stamp::take(&root, Path::new("xtask")).unwrap();
        let b = Stamp::take(&root, Path::new("docs")).unwrap();
        assert_ne!(a.tree, b.tree);
        assert_eq!(a.tree, Stamp::take(&root, Path::new("xtask")).unwrap().tree);
    }

    /// The stamp computed in process is the one git would compute: the
    /// tree hash matches `hash-object` over the same listing and the
    /// commit matches `rev-parse --short HEAD`.
    #[test]
    fn the_stamp_agrees_with_git() {
        let root = built_root();
        let out = |args: &[&str], stdin: Option<&str>| {
            let mut child = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            if let Some(text) = stdin {
                std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes())
                    .unwrap();
            }
            String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
        };
        assert_eq!(
            head_commit(&root).unwrap(),
            out(&["rev-parse", "--short", "HEAD"], None).trim()
        );
        let paths = files(&root, Path::new("docs")).unwrap();
        let absolute = paths
            .iter()
            .map(|p| root.join(p).display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let ids = out(&["hash-object", "--stdin-paths"], Some(&absolute));
        let mut listing = String::new();
        for (p, id) in paths.iter().zip(ids.lines()) {
            listing.push_str(id);
            listing.push(' ');
            listing.push_str(p);
            listing.push('\n');
        }
        assert_eq!(
            tree_hash(&root, Path::new("docs")).unwrap(),
            out(&["hash-object", "--stdin"], Some(&listing)).trim()
        );
    }

    /// The whole path an instrument takes: manifest, stamp, reading, store.
    #[test]
    fn a_test_instrument_writes_one_reading_end_to_end() {
        let workspace = built_root();
        let root = crate::scratch::Tree::new("instrument").unwrap();
        let manifest = Manifest::load(&workspace).unwrap();
        let mut reading = Reading::for_scope("test", "xtask", &manifest, &workspace).unwrap();
        reading.status = Status::Pass;
        assert_eq!(
            reading.tree(),
            Stamp::take(&workspace, &manifest.scope_path("xtask").unwrap())
                .unwrap()
                .tree
        );
        let store = Store::at(root.path());
        assert_eq!(
            store.record(std::slice::from_ref(&reading)).unwrap(),
            Outcome::Met
        );
        assert!(root.join("test/xtask.toml").is_file());
        assert_eq!(store.read("test", "xtask").unwrap().unwrap(), reading);
    }
}
