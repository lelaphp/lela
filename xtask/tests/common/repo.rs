//! A scratch git repository a test lays out by hand, on the branch
//! `xtask::scratch::repository` lays its copy of the workspace on, `main`
//! unless a test names another, and every call under [`UNHOOKED`], the
//! identity, signing and hooks path that function commits under, so a
//! person's `commit.gpgsign`, `init.defaultBranch` or hooks are never the
//! test's. Where `repository` also points the clone's own hooks at
//! `scripts/hooks` for the calls a test makes through the binary, a `Repo`
//! holds no hooks of its own to point anywhere.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod repo; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use xtask::manifest::{self, Manifest};
use xtask::scratch::UNHOOKED;

/// A git repository a test lays out in a directory it owns. A call that
/// fails fails the test, naming the call, the directory and git's stderr.
/// It reads as the path it stands at, as a scratch `Tree` does.
#[derive(Debug, Clone)]
pub struct Repo {
    dir: PathBuf,
}

impl Repo {
    /// A new repository at `dir` on `main`, the directory made if it is
    /// not there. The branch is `main` by name: `git init` alone takes it
    /// from `init.defaultBranch`, which is `master` on a runner that never
    /// set it, and a repository with no `main` has no merge base for the
    /// verdicts that read one.
    pub fn init(dir: &Path) -> Repo {
        Repo::init_on(dir, "main")
    }

    /// A new repository at `dir` whose first branch is `branch`.
    pub fn init_on(dir: &Path, branch: &str) -> Repo {
        fs::create_dir_all(dir).unwrap_or_else(|err| panic!("creating {}: {err}", dir.display()));
        let repo = Repo::at(dir);
        repo.git(&["init", "-q", "-b", branch]);
        repo
    }

    /// The repository already standing at `dir`: one `init` laid, a clone
    /// or a worktree of it.
    pub fn at(dir: &Path) -> Repo {
        Repo {
            dir: dir.to_path_buf(),
        }
    }

    /// `text` written to `file`, a path under the repository, with the
    /// directories above it made.
    pub fn write(&self, file: &str, text: impl AsRef<[u8]>) -> &Repo {
        let path = self.dir.join(file);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("creating {}: {err}", parent.display()));
        }
        fs::write(&path, text).unwrap_or_else(|err| panic!("writing {}: {err}", path.display()));
        self
    }

    /// The working tree as it stands, everything in it not ignored, as
    /// one commit named `message`, and that commit's id. A tree that
    /// changed nothing is still a commit, as `--allow-empty` makes one.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// `git` with `args` in the repository under [`UNHOOKED`], and what
    /// it printed on stdout, the trailing newline gone.
    pub fn git(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "git {} in {}: {}\n{}",
            args.join(" "),
            self.dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
    }

    /// `git` with `args` as [`Repo::git`] runs it, where the test holds
    /// git to refusing, as a merge stops on its conflict: what it printed
    /// on stdout and stderr. A call that succeeds fails the test.
    pub fn refused(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let said = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "git {} in {} succeeded: {said}",
            args.join(" "),
            self.dir.display(),
        );
        said
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(UNHOOKED)
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap_or_else(|err| panic!("running git {}: {err}", args.join(" ")))
    }
}

impl Deref for Repo {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.dir
    }
}

impl AsRef<Path> for Repo {
    fn as_ref(&self) -> &Path {
        &self.dir
    }
}

/// A repository at `dir`, rebuilt from scratch, holding the workspace's
/// manifest, its pin, the divergence registry and a README as its one
/// commit: where a test provokes denials, so the readings land in its
/// store and not the workspace's. `permit --bench` builds the same
/// repository for the same reason, with the recipe stated there too.
pub fn scratch_workspace(root: &Path, dir: &Path) -> Repo {
    fs::remove_dir_all(dir).ok();
    let manifest = Manifest::load(root).unwrap();
    let repo = Repo::init(dir);
    for file in [
        manifest::FILE,
        manifest.oracle.pin.as_str(),
        "docs/divergence.toml",
        "README.md",
    ] {
        let text = fs::read(root.join(file)).unwrap_or_else(|err| panic!("reading {file}: {err}"));
        repo.write(file, text);
    }
    repo.commit("scratch");
    repo
}
