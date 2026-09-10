//! The two git hooks under `scripts/hooks` (harness-design.md §6 wiring,
//! ledger 11), run by git in a scratch repository: a stand-in `cargo` on
//! `PATH` records each call and answers as the instruments would, and
//! hands `xtask regions` to this crate's own binary against the scratch
//! root, so the hook's re-stage loop reads the real reader; a stand-in
//! `computed` answers with one shape per case. What is read back is what
//! the hook did, the commit it allowed or refused, the files it staged
//! and the base it handed `done`, since a hook that exits without doing
//! its job is invisible to a substring over its text.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// The region files pre-commit renders, in the order it names them, and
/// the local one it names beside them.
const REGIONS: [&str; 4] = [
    "AGENTS.md",
    "ext/AGENTS.md",
    "data/INDEX.md",
    "docs/divergence.md",
];
const LOCAL: &str = ".claude/harness.local.md";

/// A scratch repository with the two stand-ins beside it: `root` is the
/// repository, `bin` holds `cargo` and `computed`, `cargo.log` and
/// `computed.log` hold one line per call, and `refuse` names the one
/// `cargo` call that exits 1, when a test writes it.
struct Scratch {
    dir: PathBuf,
    root: PathBuf,
}

/// The stand-in `computed` for each case, as the shell that answers `run`
/// and `check`; `computed`'s own lines are the shapes its spec names.
enum Computed {
    /// `run` rewrites `AGENTS.md` and says so; `check` finds it fresh.
    Rendered,
    /// `run` finds every region fresh; `check` finds one hand-edited.
    HandEdited,
    /// `run` answers with an untrusted exec region; `check` finds it fresh.
    Untrusted,
}

impl Computed {
    fn body(&self) -> &'static str {
        match self {
            Computed::Rendered => {
                "if [ \"$1\" = run ]; then echo rendered >> AGENTS.md; printf 'AGENTS.md:3 layout tree stale written\\n' >&2; exit 1; fi\nexit 0\n"
            }
            Computed::HandEdited => {
                "if [ \"$1\" = check ]; then printf 'AGENTS.md:3 layout tree hand-edited\\n' >&2; exit 1; fi\nexit 0\n"
            }
            Computed::Untrusted => {
                "if [ \"$1\" = run ]; then printf 'AGENTS.md:3 layout exec untrusted skipped; run `computed trust`\\n' >&2; exit 1; fi\nexit 0\n"
            }
        }
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The scratch repository: the workspace's manifest and pin, the four
/// region files, one crate file, one commit, and the real hooks under
/// `scripts/hooks` as its `core.hooksPath`.
fn scratch(name: &str, computed: &Computed) -> Scratch {
    let dir = std::env::temp_dir().join(format!("xtask-hooks-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let root = dir.join("root");
    scratch_workspace(workspace(), &root).unwrap();
    for file in REGIONS {
        write(&root, file, "# a region file\n\n<!-- computed layout -->\n");
    }
    write(&root, "crates/php_heap/src/lib.rs", "pub fn alloc() {}\n");
    let scratch = Scratch {
        dir: dir.clone(),
        root: fs::canonicalize(&root).unwrap(),
    };
    scratch.git(&["add", "."], false);
    scratch.git(&["commit", "-q", "-m", "regions and a crate"], false);
    scratch.git(
        &[
            "config",
            "core.hooksPath",
            workspace().join("scripts/hooks").to_str().unwrap(),
        ],
        false,
    );

    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    executable(
        &bin.join("cargo"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ -f '{refuse}' ] && [ \"$(cat '{refuse}')\" = \"$1 $2\" ]; then exit 1; fi\n\
             if [ \"$1 $2\" = 'xtask regions' ]; then shift 2; exec '{xtask}' --root \"$PWD\" regions \"$@\"; fi\n\
             exit 0\n",
            log = dir.join("cargo.log").display(),
            refuse = dir.join("refuse").display(),
            xtask = env!("CARGO_BIN_EXE_xtask"),
        ),
    );
    executable(
        &bin.join("computed"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n{body}",
            log = dir.join("computed.log").display(),
            body = computed.body(),
        ),
    );
    scratch
}

impl Scratch {
    /// `git` in the repository, with the stand-ins first on `PATH` when
    /// `hooked`, so a hook git runs finds them; the exit and both streams.
    fn run(&self, args: &[&str], hooked: bool) -> (i32, String) {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut command = Command::new("git");
        command
            .args(["-c", "commit.gpgsign=false"])
            .current_dir(&self.root)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", "x@x")
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", "x@x");
        if hooked {
            command.env("PATH", format!("{}:{path}", self.dir.join("bin").display()));
        } else {
            command.args(["-c", "core.hooksPath=/var/empty"]);
        }
        command.args(args);
        let out = command.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// `git` that must succeed; its stdout.
    fn git(&self, args: &[&str], hooked: bool) -> String {
        let (code, out) = self.run(args, hooked);
        assert_eq!(code, 0, "git {args:?}: {out}");
        out
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"], false).trim().to_owned()
    }

    /// One line per call of the named stand-in.
    fn calls(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.dir.join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The one `cargo` call, `<subcommand> <first argument>`, that exits 1.
    fn refuse(&self, call: &str) {
        fs::write(self.dir.join("refuse"), call).unwrap();
    }

    /// A change under the crate, staged, and the commit through the hook.
    fn stage_and_commit(&self) -> (i32, String) {
        write(
            &self.root,
            "crates/php_heap/src/lib.rs",
            "pub fn alloc() {}\npub fn free() {}\n",
        );
        self.git(&["add", "--", "crates/php_heap/src/lib.rs"], false);
        self.run(&["commit", "-q", "-m", "a change under a crate"], true)
    }
}

// ---------------------------------------------------------------- pre-commit

/// The instruments in the order §6 lists them, `permit --diff`, then
/// `xtask regions` over the four region files and the local one by name,
/// which reaches `computed run`; the region file `computed` rewrote is
/// re-staged, so the commit carries it and the tree is clean after, and
/// `computed check` over the four holds the commit to a rendering.
#[test]
fn pre_commit_re_stages_the_region_it_rewrote_and_the_commit_is_allowed() {
    let scratch = scratch("rendered", &Computed::Rendered);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 0, "{out}");
    assert_ne!(scratch.head(), before, "the commit was allowed: {out}");

    let regions = format!("{} {LOCAL}", REGIONS.join(" "));
    assert_eq!(
        scratch.calls("cargo"),
        [
            "fmt --check".to_owned(),
            "xtask clippy --staged".to_owned(),
            "xtask api-check".to_owned(),
            "xtask budget".to_owned(),
            "xtask unsafe-audit".to_owned(),
            "xtask vocab".to_owned(),
            "xtask permit --diff".to_owned(),
            format!("xtask regions {regions}"),
        ]
    );
    assert_eq!(
        scratch.calls("computed"),
        [
            format!("run {regions}"),
            format!("check {}", REGIONS.join(" ")),
        ]
    );

    let committed = scratch.git(&["show", "--name-only", "--format=", "HEAD"], false);
    let mut files: Vec<_> = committed.lines().collect();
    files.sort_unstable();
    assert_eq!(files, ["AGENTS.md", "crates/php_heap/src/lib.rs"]);
    let agents = scratch.git(&["show", "HEAD:AGENTS.md"], false);
    assert!(agents.ends_with("rendered\n"), "{agents}");
    let status = scratch.git(&["status", "--porcelain"], false);
    assert_eq!(status, "", "what computed wrote is in the commit");
}

/// A region a hand edited is refused with the line naming
/// `computed check` and what it said; the change stays staged for the person.
#[test]
fn a_hand_edited_region_refuses_the_commit_naming_computed_check() {
    let scratch = scratch("hand-edited", &Computed::HandEdited);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(out.contains("run `computed check`"), "{out}");
    assert!(out.contains("AGENTS.md:3 layout tree hand-edited"), "{out}");
    let staged = scratch.git(&["diff", "--cached", "--name-only"], false);
    assert_eq!(staged.trim(), "crates/php_heap/src/lib.rs");
}

/// A root without a trust grant is refused with the `computed trust`
/// line as a person's, through `xtask regions`, and nothing is re-staged.
#[test]
fn an_untrusted_root_refuses_the_commit_naming_computed_trust() {
    let scratch = scratch("untrusted", &Computed::Untrusted);
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains(&format!("computed trust {}", scratch.root.display())),
        "{out}"
    );
    assert!(out.contains("the regions were not rendered"), "{out}");
    let staged = scratch.git(&["diff", "--cached", "--name-only"], false);
    assert_eq!(staged.trim(), "crates/php_heap/src/lib.rs");
}

/// An instrument that fails refuses the commit by name, and the ones
/// after it still run: every command's exit is read, none short-circuits.
#[test]
fn a_failing_instrument_refuses_the_commit_and_the_rest_still_run() {
    let scratch = scratch("failing", &Computed::Rendered);
    scratch.refuse("xtask budget");
    let before = scratch.head();
    let (code, out) = scratch.stage_and_commit();
    assert_eq!(code, 1, "{out}");
    assert_eq!(scratch.head(), before, "the commit was refused: {out}");
    assert!(
        out.contains("pre-commit: cargo xtask budget failed"),
        "{out}"
    );
    let calls = scratch.calls("cargo");
    assert!(calls.iter().any(|c| c == "xtask vocab"), "{calls:?}");
    assert!(
        calls.iter().any(|c| c.starts_with("xtask regions")),
        "{calls:?}"
    );
}

// ---------------------------------------------------------------- pre-push

/// A bare remote beside the repository, as `origin`.
fn remote(scratch: &Scratch) -> String {
    let bare = scratch.dir.join("remote.git");
    let out = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&bare)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    scratch.git(&["remote", "add", "origin", bare.to_str().unwrap()], false);
    scratch
        .git(&["symbolic-ref", "--short", "HEAD"], false)
        .trim()
        .to_owned()
}

/// The first push of a ref the remote lacks hands `done` no base, since
/// git's remote sha is all zeros; the next push hands it the sha the
/// remote has, so `done` judges what the push adds; and a `done` that
/// is unmet refuses the push, the remote ref unmoved.
#[test]
fn pre_push_hands_done_the_remote_sha_and_no_base_for_a_ref_the_remote_lacks() {
    let scratch = scratch("push", &Computed::Rendered);
    let branch = remote(&scratch);
    let first = scratch.head();

    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_eq!(code, 0, "{out}");
    assert_eq!(scratch.calls("cargo"), ["xtask done"]);

    scratch.git(&["commit", "-q", "--allow-empty", "-m", "one more"], false);
    let second = scratch.head();
    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.calls("cargo"),
        [
            "xtask done".to_owned(),
            format!("xtask done --base {first}")
        ]
    );

    scratch.git(&["commit", "-q", "--allow-empty", "-m", "unmet"], false);
    scratch.refuse("xtask done");
    let (code, out) = scratch.run(&["push", "-q", "origin", &branch], true);
    assert_ne!(code, 0, "{out}");
    assert_eq!(
        scratch.calls("cargo").last().unwrap(),
        &format!("xtask done --base {second}")
    );
    let held = scratch.git(&["rev-parse", &format!("origin/{branch}")], false);
    assert_eq!(held.trim(), second, "the push was refused");
}
