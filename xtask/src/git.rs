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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

/// What a reading is stamped with: the scope's key
/// ([`crate::key::Snapshot::key`]) and the commit it was taken at.
#[derive(Debug, Clone)]
pub struct Stamp {
    pub key: String,
    pub commit: String,
}

/// The short id of `HEAD`, seven characters, read from the repository's
/// files; `git rev-parse --short=7` when the layout is one this reader does
/// not know. Seven is fixed, not git's own `--short`, which grows with the
/// repository (eight since `d32ff307`) and would make one commit two
/// stamps. A stamp is taken at every denial, so it spawns git once
/// (`ls-files`) and not four times. A repository with no commit yet has
/// no `HEAD` to stamp with, and is refused in a sentence rather than in
/// git's "Needed a single revision" (#3189).
pub fn head_commit(root: &Path) -> Result<String> {
    match read_head(root) {
        Some(id) => Ok(id),
        None => rev_parse(root, &["--short=7", "HEAD"]).map_err(|err| match git_dir(root) {
            Some(_) if !has_ref(root, "HEAD") => anyhow!(
                "{} has no commit yet; a reading is stamped with one",
                root.display()
            ),
            _ => err,
        }),
    }
}

/// The two directories git keeps for the checkout at `root`, read from
/// its `.git` alone: the checkout's own, where `HEAD` is, and the one
/// every worktree of the repository shares, where the refs are. `.git`
/// is that directory in the main worktree and a file naming it in a
/// linked one, whose `commondir` names the shared one.
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let line = fs::read_to_string(&dot_git).ok()?;
        root.join(line.strip_prefix("gitdir:")?.trim())
    };
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir.clone(),
    };
    Some((git_dir, common))
}

/// The directory every worktree of the repository `root` is checked out
/// from shares, canonical: two checkouts are worktrees of one repository
/// when theirs are equal, wherever either was laid down. `None` for a
/// directory whose `.git` does not say.
pub(crate) fn common_dir(root: &Path) -> Option<PathBuf> {
    git_dirs(root).and_then(|(_, common)| fs::canonicalize(common).ok())
}

/// `HEAD` through `.git` ([`git_dirs`]), a symbolic ref through `refs/`
/// and then `packed-refs`, shortened to seven characters, every stored
/// stamp's length.
fn read_head(root: &Path) -> Option<String> {
    let (git_dir, common) = git_dirs(root)?;
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
    files_under(root, &[scope])
}

/// [`files`] under each of `scopes` at once, one listing of their union.
pub(crate) fn files_under(root: &Path, scopes: &[impl AsRef<Path>]) -> Result<Vec<String>> {
    let mut paths = listed_under(root, &[], scopes)?;
    paths.retain(|path| root.join(path).is_file());
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// One object id over every file under `scopes` that git does not ignore,
/// tracked or not, with its content as it is on disk: the blob id of each
/// file, then the blob id of the sorted `<id> <path>` listing, which is
/// what `git hash-object` would print for the same bytes. A reading's key
/// was this over its scope's closure until #2163 made it a lookup on a
/// [`crate::key::Snapshot`]; it stays as the reference the tests hold
/// every snapshot's key to, file by file read and hashed.
#[cfg(test)]
pub(crate) fn tree_hash(root: &Path, scopes: &[PathBuf]) -> Result<String> {
    let mut listing = Vec::new();
    for path in files_under(root, scopes)? {
        let file = root.join(&path);
        let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        listing.push((path, blob_id(&bytes)));
    }
    Ok(listing_id(&listing))
}

/// The key ([`crate::key::Snapshot::key`]) of a scope that is the one
/// file `path`, from its blob id alone: what a reading stamped over that
/// file carries, for content that is no longer on disk.
pub(crate) fn file_hash(path: &str, blob: &str) -> String {
    listing_id(&[(path.to_owned(), blob.to_owned())])
}

/// The index as a commit takes it (#1143): each tracked file's staged
/// blob id by path, read once from `ls-files --stage`, under the
/// `GIT_INDEX_FILE` git hands a hook when it keeps one of its own.
#[derive(Debug, Clone, Default)]
pub struct Index(Vec<(String, String)>);

impl Index {
    pub fn read(root: &Path) -> Result<Index> {
        let mut entries: Vec<_> = (stage(root)?.into_iter())
            .filter_map(|(path, id)| Some((path, id?)))
            .collect();
        entries.sort_unstable();
        Ok(Index(entries))
    }

    /// Each staged file and its blob id, sorted by path: what
    /// [`crate::key::Snapshot::index`] keys a scope over.
    pub(crate) fn entries(&self) -> &[(String, String)] {
        &self.0
    }
}

/// Each path the index tracks and its blob id at stage 0, from
/// `ls-files --stage`: none for a submodule, which is no file of a
/// scope's, or a conflicted path, which has no stage 0, so no scope
/// holding one matches what is on disk.
pub(crate) fn stage(root: &Path) -> Result<Vec<(String, Option<String>)>> {
    let staged = entries(root, &["ls-files", "--stage", "-z"])?.into_iter();
    // Each head is `<mode> <id> <stage>`.
    let id = |head: &str| match head.split(' ').collect::<Vec<_>>()[..] {
        [mode, id, "0"] if mode != "160000" => Some(id.to_owned()),
        _ => None,
    };
    Ok(staged.map(|(head, path)| (path, id(&head))).collect())
}

/// Each file `commit` holds under `paths`, the whole tree for none, and
/// its blob id, from `ls-tree`.
pub(crate) fn blobs(root: &Path, commit: &str, paths: &[&str]) -> Result<Vec<(String, String)>> {
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", commit, "--"];
    args.extend(paths);
    let blob = |head: &str| match head.split(' ').collect::<Vec<_>>()[..] {
        [_, "blob", id] => Some(id.to_owned()),
        _ => None,
    };
    let listed = entries(root, &args)?.into_iter();
    Ok(listed
        .filter_map(|(head, path)| Some((path, blob(&head)?)))
        .collect())
}

/// What `git <args>` lists with `-z`, entry by entry.
pub(crate) fn listed(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let said = git(root, args)?;
    Ok(said
        .split('\0')
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect())
}

/// What `git ls-files` lists under `scopes`, tracked or not and not
/// ignored, with `flags` before the pathspec (`-t` tags each entry).
pub(crate) fn listed_under(
    root: &Path,
    flags: &[&str],
    scopes: &[impl AsRef<Path>],
) -> Result<Vec<String>> {
    let mut args = vec!["ls-files", "-z"];
    args.extend(flags);
    args.extend(["--cached", "--others", "--exclude-standard", "--"]);
    for scope in scopes {
        let scope = scope.as_ref().to_str();
        args.push(scope.context("the scope path is not UTF-8")?);
    }
    listed(root, &args)
}

/// The untracked files git does not ignore, relative to `root`.
pub(crate) fn untracked(root: &Path) -> Result<Vec<String>> {
    listed(root, &["ls-files", "-z", "--others", "--exclude-standard"])
}

/// What `git <args>` lists with `-z`, each entry's head and path parted
/// at its tab.
pub(crate) fn entries(root: &Path, args: &[&str]) -> Result<Vec<(String, String)>> {
    (listed(root, args)?.into_iter())
        .map(|entry| {
            let (head, path) = (entry.split_once('\t'))
                .with_context(|| format!("git {} wrote {entry:?}", args[0]))?;
            Ok((head.to_owned(), path.to_owned()))
        })
        .collect()
}

/// The blob id of the sorted `<id> <path>` listing of `(path, id)` pairs:
/// the one shape every hash of a scope here is taken in.
pub(crate) fn listing_id(listing: &[(String, String)]) -> String {
    let mut text = String::new();
    for (path, id) in listing {
        text.push_str(id);
        text.push(' ');
        text.push_str(path);
        text.push('\n');
    }
    blob_id(text.as_bytes())
}

/// One object id over the tracked files under `paths`, taken as
/// [`tree_hash`] is, from each file's blob id and path: their contents
/// on disk when `at` is `None`, and as the commit `at` holds them
/// otherwise, so a checkout of `at` with nothing under `paths` written
/// since hashes to the id `at` does. Tracked files alone, since an
/// untracked one the build reads is reached by an edit to a tracked one,
/// and an untracked `.DS_Store` would otherwise leave a clean checkout
/// matching no commit. A tracked file gone from disk is listed as gone.
/// The reference [`crate::key::Snapshot::tracked_over`] and
/// [`crate::key::Snapshot::commit`] are held to (#2163).
#[cfg(test)]
pub(crate) fn tracked_hash(root: &Path, paths: &[&str], at: Option<&str>) -> Result<String> {
    let mut listing = Vec::new();
    if let Some(commit) = at {
        listing = blobs(root, commit, paths)?;
    } else {
        let mut args = vec!["ls-files", "-z", "--cached", "--"];
        args.extend(paths);
        for path in listed(root, &args)? {
            let id = fs::read(root.join(&path)).map_or_else(|_| "gone".to_owned(), |b| blob_id(&b));
            listing.push((path, id));
        }
    }
    listing.sort_unstable();
    listing.dedup();
    Ok(listing_id(&listing))
}

/// The id git gives a blob: SHA-1 over `blob <len>\0<content>`.
pub fn blob_id(content: &[u8]) -> String {
    let mut hash = sha1_smol::Sha1::new();
    hash.update(format!("blob {}\0", content.len()).as_bytes());
    hash.update(content);
    hash.digest().to_string()
}

/// Whether `root` has the ref `name` (`refs/heads/x`, `refs/remotes/origin/x`).
pub(crate) fn has_ref(root: &Path, name: &str) -> bool {
    rev_parse(root, &["--verify", "--quiet", name]).is_ok()
}

/// `git rev-parse` with `args` in `root`: what it printed, trimmed.
pub(crate) fn rev_parse(root: &Path, args: &[&str]) -> Result<String> {
    let out = git(root, &[&["rev-parse"], args].concat())?;
    Ok(out.trim().to_owned())
}

/// The checkout's own git dir, a linked worktree's under the main one's
/// `worktrees/`, with its links resolved so `/tmp` and `/private/tmp` name
/// one; `None` outside a repository.
pub(crate) fn git_dir(checkout: &Path) -> Option<PathBuf> {
    let dir = PathBuf::from(rev_parse(checkout, &["--absolute-git-dir"]).ok()?);
    Some(std::fs::canonicalize(&dir).unwrap_or(dir))
}

/// `git` in a repository that is not the workspace's, such as the pinned
/// php-src tree, with every `GIT_` variable a hook inherits taken out
/// ([`crate::batch::without_git`]): pre-commit in a linked worktree hands
/// its instruments git's own `GIT_DIR`, under which `-C` names the
/// directory and the workspace still answers, so the pinned tree read as
/// standing at the workspace's `HEAD` and every commit there was refused.
pub(crate) fn elsewhere(root: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    crate::batch::without_git(&mut command);
    utf8(run(command, root, args, None)?)
}

/// Run git in `root` and return its stdout; a non-zero exit is an error
/// carrying git's stderr.
pub(crate) fn git(root: &Path, args: &[&str]) -> Result<String> {
    utf8(run(Command::new("git"), root, args, None)?)
}

/// [`git`] with `stdin` fed to it.
pub(crate) fn fed(root: &Path, args: &[&str], stdin: &str) -> Result<String> {
    utf8(run(Command::new("git"), root, args, Some(stdin))?)
}

/// `git`, as `command` stands, run in `root` with `stdin` fed to it: what
/// it wrote on stdout, or the error that carries what it wrote on stderr
/// when it exited non-zero.
pub(crate) fn run(
    mut command: Command,
    root: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<Vec<u8>> {
    #[cfg(test)]
    recorder::record(root, args);
    let mut child = command
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
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git {}: {}", args.join(" "), stderr.trim());
    }
    Ok(out.stdout)
}

/// The recorder a test counts git calls through ([`asked`]).
#[cfg(test)]
mod recorder {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// The questions asked of git in each directory `asked` listens at,
    /// or below it, each as its arguments.
    static ASKED: Mutex<BTreeMap<PathBuf, Vec<Vec<String>>>> = Mutex::new(BTreeMap::new());

    fn listening<T>(f: impl FnOnce(&mut BTreeMap<PathBuf, Vec<Vec<String>>>) -> T) -> T {
        f(&mut crate::held(&ASKED))
    }

    /// One question asked of git in `root`, for each listener above it.
    pub(super) fn record(root: &Path, args: &[&str]) {
        listening(|listeners| {
            for (at, asked) in listeners.iter_mut() {
                if root.starts_with(at) {
                    asked.push(args.iter().map(ToString::to_string).collect());
                }
            }
        });
    }

    /// What `f` returns and every question asked of git in `root` or below
    /// it while `f` ran, each as its arguments: the seam a test counts git
    /// calls through, at the one function every call spawns git from
    /// (#2972). Each test lays a repository of its own, so its `root` is
    /// its own and no other test's questions are counted, from whichever
    /// thread they are asked (#2994).
    #[cfg(tracked)]
    pub(crate) fn asked<T>(root: &Path, f: impl FnOnce() -> T) -> (T, Vec<Vec<String>>) {
        listening(|listeners| listeners.insert(root.to_path_buf(), Vec::new()));
        let out = f();
        let asked = listening(|listeners| listeners.remove(root));
        (out, asked.unwrap_or_default())
    }
}

#[cfg(test)]
#[cfg(tracked)]
pub(crate) use recorder::asked;

fn utf8(stdout: Vec<u8>) -> Result<String> {
    String::from_utf8(stdout).context("git wrote something that is not UTF-8")
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
    )?;
    let mut out = changes(&listing);
    let untracked = untracked(root)?;
    out.extend(untracked.into_iter().map(|path| Change {
        path,
        deleted: false,
    }));
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
    )?;
    let mut out = changes(&listing);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The changes a `git diff --name-status -z` listing names.
fn changes(listing: &str) -> Vec<Change> {
    let fields: Vec<&str> = listing.split('\0').filter(|f| !f.is_empty()).collect();
    let change = |pair: &[&str]| Change {
        path: pair[1].to_owned(),
        deleted: pair[0].starts_with('D'),
    };
    fields.chunks_exact(2).map(change).collect()
}

/// The content of `path` in the index: `show(root, "", path)`, since git
/// spells the index as an empty revision.
pub fn staged_content(root: &Path, path: &str) -> Result<Option<String>> {
    show(root, "", path)
}

/// The content of `path` at `rev`, or `None` when the commit has no such
/// file.
pub fn show(root: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    match git(root, &["show", &format!("{rev}:{path}")]) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.to_string().contains("does not exist") => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    /// A repository `git init` laid down and nothing committed has no
    /// commit to stamp a reading with, and says so (#3189).
    #[test]
    fn a_repository_with_no_commit_is_refused_in_a_sentence() {
        let dir = crate::scratch::Tree::new("git-unborn").unwrap();
        super::git(&dir, &["init", "-q"]).unwrap();
        let refused = super::head_commit(&dir).unwrap_err();
        assert_eq!(
            format!("{refused:#}"),
            format!(
                "{} has no commit yet; a reading is stamped with one",
                dir.display()
            )
        );
    }

    /// Run by [`elsewhere_reads_the_repository_it_names_under_a_hooks_git_dir`]
    /// with `GIT_DIR` naming another repository, as a hook in a linked
    /// worktree runs an instrument; run by nothing else, it does nothing.
    #[test]
    #[ignore = "elsewhere_reads_the_repository_it_names_under_a_hooks_git_dir runs it"]
    fn elsewhere_under_git_dir() {
        let Some(named) = std::env::var_os("XTASK_ELSEWHERE") else {
            return;
        };
        let head = super::elsewhere(Path::new(&named), &["rev-parse", "HEAD"]).unwrap();
        let expected = std::env::var("XTASK_ELSEWHERE_HEAD").unwrap();
        assert_eq!(head.trim(), expected);
    }

    /// A hook's `GIT_DIR` names the repository being committed to, and
    /// `git -C <tree>` under it reads that one; [`super::elsewhere`] reads
    /// the tree it names, the pinned php-src's in [`crate::oracle`].
    #[test]
    fn elsewhere_reads_the_repository_it_names_under_a_hooks_git_dir() {
        let dir = crate::scratch::Tree::new("git-elsewhere").unwrap();
        let mut heads = Vec::new();
        for name in ["committing", "pinned"] {
            let repo = dir.join(name);
            std::fs::create_dir_all(&repo).unwrap();
            for args in [
                &["init", "-q"][..],
                &[
                    "-c",
                    "user.name=x",
                    "-c",
                    "user.email=x@x",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    name,
                ],
            ] {
                let status = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_INDEX_FILE")
                    .status()
                    .unwrap();
                assert!(status.success());
            }
            heads.push(super::elsewhere(&repo, &["rev-parse", "HEAD"]).unwrap());
        }
        assert_ne!(heads[0], heads[1]);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git::tests::elsewhere_under_git_dir",
                "--ignored",
            ])
            .env("GIT_DIR", dir.join("committing/.git"))
            .env("XTASK_ELSEWHERE", dir.join("pinned"))
            .env("XTASK_ELSEWHERE_HEAD", heads[1].trim())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{said}");
        assert!(said.contains("1 passed"), "{said}");
    }

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
    use std::path::{Path, PathBuf};

    use super::{files, head_commit, tree_hash};
    use crate::built_root;
    use crate::key::Snapshot;
    use crate::manifest::Manifest;
    use crate::reading::{Outcome, Reading, Status, Store};

    #[test]
    fn a_stamp_has_a_tree_hash_and_a_commit() {
        let stamp = Snapshot::disk(&built_root())
            .unwrap()
            .stamp(Path::new("xtask"))
            .unwrap();
        assert_eq!(stamp.key.len(), 40, "{}", stamp.key);
        assert!(stamp.key.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!stamp.commit.is_empty());
    }

    #[test]
    fn the_tree_hash_follows_the_sources_it_covers() {
        let now = Snapshot::disk(&built_root()).unwrap();
        let a = now.stamp(Path::new("xtask")).unwrap();
        let b = now.stamp(Path::new("docs")).unwrap();
        assert_ne!(a.key, b.key);
        assert_eq!(a.key, now.stamp(Path::new("xtask")).unwrap().key);
    }

    /// The stamp computed in process is the one git would compute: the
    /// tree hash matches `hash-object` over the same listing and the
    /// commit is the first seven characters of `rev-parse HEAD`, a fixed
    /// width, where `--short=7` grows past seven once they are ambiguous.
    /// Over a scratch repository of its own, so a merge in progress in
    /// this one, or a file it adds under `docs/`, moves nothing (#1252).
    #[test]
    fn the_stamp_agrees_with_git() {
        let root = crate::scratch::Tree::new("stamp").unwrap();
        std::fs::create_dir_all(root.join("docs/deeper")).unwrap();
        std::fs::write(root.join("docs/one.md"), "one\n").unwrap();
        std::fs::write(root.join("docs/deeper/two.toml"), "two = 2\n").unwrap();
        std::fs::write(root.join("elsewhere.md"), "not under docs\n").unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"]] {
            super::git(&root, args).unwrap();
        }
        crate::scratch::committing(&root, &["commit", "-q", "-m", "scratch"]);
        let root = root.path();
        let out = |args: &[&str], stdin: Option<&str>| {
            let mut child = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
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
            head_commit(root).unwrap(),
            out(&["rev-parse", "HEAD"], None).trim()[..7]
        );
        let paths = files(root, Path::new("docs")).unwrap();
        assert_eq!(paths.len(), 2, "{paths:?}");
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
            tree_hash(root, &[PathBuf::from("docs")]).unwrap(),
            out(&["hash-object", "--stdin"], Some(&listing)).trim()
        );
    }

    /// The whole path an instrument takes: manifest, stamp, reading, store.
    #[test]
    fn a_test_instrument_writes_one_reading_end_to_end() {
        let workspace = built_root();
        let root = crate::scratch::Tree::new("instrument").unwrap();
        let manifest = Manifest::load(&workspace).unwrap();
        let mut reading = Reading::for_scope(
            "test",
            "xtask",
            &manifest,
            &crate::key::Snapshot::disk(&workspace).unwrap(),
        )
        .unwrap();
        reading.status = Status::Pass;
        assert_eq!(
            reading.key(),
            Snapshot::disk(&workspace)
                .unwrap()
                .stamp(&manifest.scope_path("xtask").unwrap())
                .unwrap()
                .key
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
