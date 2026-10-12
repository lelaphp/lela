//! `xtask regions`, the one reader of what `computed run` answered (#43;
//! harness-design.md §5): each outcome against a stand-in `computed` on
//! `PATH` that answers with one shape, so the exit and the lines the
//! reader interprets are the ones the spec names, not this machine's
//! trust grant. `new-ext` and `worktree add` read the same value in
//! process, and pre-commit reads this subcommand's lines.

use std::fs;
use std::path::Path;
use std::process::Command;

use xtask::scratch::Tree;

/// A directory with one stand-in `computed` in it, which records the
/// arguments and the directory it was called from, prints `said` on
/// stderr, and exits with `code`.
fn stand_in(name: &str, said: &str, code: i32) -> Tree {
    let dir = Tree::new(&format!("computed-{name}")).unwrap();
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(dir.join("root")).unwrap();
    let body = format!(
        "#!/bin/sh\npwd > {}\nprintf '%s\\n' \"$*\" > {}\nprintf '%s' '{said}' >&2\nexit {code}\n",
        dir.join("cwd").display(),
        dir.join("argv").display(),
    );
    let path = bin.join("computed");
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// The subcommand against `dir/root`, with only `dir/bin` on `PATH`.
fn regions(dir: &Path, files: &[&str]) -> (i32, String, String) {
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(dir.join("root"))
        .arg("regions")
        .args(files)
        .env("PATH", dir.join("bin"))
        .env_remove("CARGO")
        .env_remove("CARGO_MANIFEST_DIR")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Exit 1 with a `written` line is a file written: named on its own line
/// so pre-commit can re-stage it, the file whose regions were all fresh
/// or kept named as unchanged, and the exit is 0.
#[test]
fn a_written_region_names_its_file_and_a_kept_one_does_not() {
    let dir = stand_in(
        "written",
        "AGENTS.md:62 design-of-record exec stale written\n\
         AGENTS.md:142 layout tree fresh\n\
         .claude/harness.local.md:142 permits exec volatile failed; body kept\n\
         \x20   exit status 1\n",
        1,
    );
    let (code, out, err) = regions(&dir, &["AGENTS.md", ".claude/harness.local.md"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(
        out,
        "AGENTS.md: written\n.claude/harness.local.md: unchanged\n"
    );
    // computed's own lines reach the person as they were, on stderr.
    assert!(err.contains("permits exec volatile failed"), "{err}");

    // `computed run <files>` from the root, so relative names are the root's.
    assert_eq!(
        fs::read_to_string(dir.join("argv")).unwrap().trim(),
        "run AGENTS.md .claude/harness.local.md"
    );
    let cwd = fs::read_to_string(dir.join("cwd")).unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        dir.join("root").canonicalize().unwrap()
    );
}

/// Exit 0 is every region fresh: nothing written, nothing to re-stage.
#[test]
fn every_region_fresh_is_unchanged() {
    let dir = stand_in("fresh", "", 0);
    let (code, out, _) = regions(&dir, &["AGENTS.md"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out, "AGENTS.md: unchanged\n");
}

/// Exit 2 is a marker `computed` could not read: a refusal that names
/// the marker's line, and no file line, since none was rendered.
#[test]
fn a_broken_marker_is_a_refusal_naming_the_line() {
    let dir = stand_in(
        "broken",
        "AGENTS.md:79: unterminated region opened at line 62\n",
        2,
    );
    let (code, out, err) = regions(&dir, &["AGENTS.md"]);
    assert_eq!(code, 1, "{out}{err}");
    assert_eq!(out, "");
    assert!(err.contains("a computed marker is broken"), "{err}");
    assert!(err.contains("AGENTS.md:79: unterminated"), "{err}");
}

/// No `computed` on `PATH` is a refusal that says how to install it.
#[test]
fn no_computed_on_path_is_a_refusal_naming_the_install() {
    let dir = stand_in("missing", "", 0);
    fs::remove_file(dir.join("bin/computed")).unwrap();
    let (code, out, err) = regions(&dir, &["AGENTS.md"]);
    assert_eq!(code, 1, "{out}{err}");
    assert_eq!(out, "");
    assert!(err.contains("computed did not run"), "{err}");
    assert!(err.contains("cargo install computed"), "{err}");
}

/// An `untrusted` line is the root without a trust grant: the refusal
/// carries the `computed trust <root>` line as a person's, and the tree
/// regions, which render without a grant, are still named as written.
#[test]
fn an_untrusted_region_is_a_refusal_carrying_the_trust_line() {
    let dir = stand_in(
        "untrusted",
        "AGENTS.md:62 design-of-record exec untrusted skipped; run `computed trust`\n\
         ext/AGENTS.md:9 extensions tree stale written\n",
        1,
    );
    let (code, out, err) = regions(&dir, &["AGENTS.md", "ext/AGENTS.md"]);
    assert_eq!(code, 1, "{out}{err}");
    assert_eq!(out, "AGENTS.md: unchanged\next/AGENTS.md: written\n");
    assert!(err.contains("untrusted"), "{err}");
    assert!(err.contains("a person runs, once"), "{err}");
    assert!(
        err.contains(&format!("computed trust {}", dir.join("root").display())),
        "{err}"
    );
}

/// The subcommand is in `xtask --help`, and no file is a usage error.
#[test]
fn regions_is_a_subcommand_and_needs_a_file() {
    let dir = stand_in("usage", "", 0);
    let (code, out, err) = regions(&dir, &[]);
    assert_eq!(code, 2, "{out}{err}");
    let help = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("\n  regions "), "{help}");
}

/// git in `dir`, as a test's scratch repository runs it.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=xtask", "-c", "user.email=xtask@scratch"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A scratch repository with one region file, MAP.md, rendered by the
/// real `computed` and committed: an `exec` region over x.lock at line 3
/// and a `tree` region over d, beside other.txt.
fn laid(name: &str) -> (Tree, String) {
    let dir = Tree::new(name).unwrap();
    let opener = "<!-- computed exec cmd=\"cat x.lock\" inputs=x.lock name=lock | do not edit; run computed -->";
    fs::write(dir.join("x.lock"), "one\n").unwrap();
    fs::write(dir.join("other.txt"), "a\n").unwrap();
    fs::create_dir_all(dir.join("d/a")).unwrap();
    fs::write(dir.join("d/a/f"), "").unwrap();
    fs::write(
        dir.join("MAP.md"),
        format!(
            "# map\n\n{opener}\n<!-- /computed -->\n\n\
             <!-- computed tree src=d depth=1 name=dirs | do not edit; run computed -->\n<!-- /computed -->\n"
        ),
    )
    .unwrap();
    git(&dir, &["init", "-q"]);
    let laid = Command::new("computed")
        .args(["run", "--trust", "MAP.md"])
        .current_dir(&*dir)
        .output()
        .expect("computed is on PATH, as CI installs it");
    assert!(laid.status.code().is_some_and(|c| c < 2), "{laid:?}");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "scratch"]);
    let rendered = fs::read_to_string(dir.join("MAP.md")).unwrap();
    assert!(rendered.contains("\none\n"), "{rendered}");
    (dir, rendered)
}

/// `xtask regions --staged` against the real `computed`, as pre-commit
/// runs it (#2236): a region it would write that reads a path whose
/// working copy the commit does not stage is refused before anything is
/// written, naming the region and the path, whether the region reads it
/// as an input, a lock, or as a directory a `tree` region lists; once the
/// path is staged, the refusal is gone.
#[test]
fn staged_refuses_a_region_that_would_carry_an_unstaged_change() {
    let (dir, rendered) = laid("computed-staged");
    let staged = || {
        let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
            .arg("--root")
            .arg(&*dir)
            .args(["regions", "--staged", "MAP.md"])
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // The lock changed in the working tree alone, beside a staged change.
    fs::write(dir.join("x.lock"), "two\n").unwrap();
    fs::write(dir.join("other.txt"), "b\n").unwrap();
    git(&dir, &["add", "--", "other.txt"]);
    let (code, err) = staged();
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("does not stage"), "{err}");
    assert!(err.contains("MAP.md:3 lock reads x.lock"), "{err}");
    assert!(
        !err.contains("other.txt"),
        "a staged path is no cause: {err}"
    );
    assert_eq!(fs::read_to_string(dir.join("MAP.md")).unwrap(), rendered);

    // A directory the tree region would list, untracked.
    git(&dir, &["add", "--", "x.lock"]);
    fs::create_dir_all(dir.join("d/b")).unwrap();
    fs::write(dir.join("d/b/f"), "").unwrap();
    let (code, err) = staged();
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("dirs reads d/b/"), "{err}");
    assert!(!err.contains("lock reads"), "x.lock is staged now: {err}");
    assert_eq!(fs::read_to_string(dir.join("MAP.md")).unwrap(), rendered);

    // Every path a region would read is staged: no refusal of this kind.
    git(&dir, &["add", "--", "d"]);
    let (_, err) = staged();
    assert!(!err.contains("does not stage"), "{err}");
}

/// The commit #2236 names, by pathspec through a pre-commit hook that
/// runs `xtask regions --staged`, as scripts/hooks/pre-commit does: git
/// hands the hook a temporary index of HEAD and the named paths. A region
/// rendered from an unstaged lock, its file named in the commit and the
/// lock not, is refused, and HEAD does not move; named with the lock, the
/// commit goes in.
#[test]
fn a_pathspec_commit_carries_no_region_of_an_unstaged_lock() {
    let (dir, _) = laid("computed-pathspec");
    let hooks = dir.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nexec '{}' --root '{}' regions --staged MAP.md\n",
            xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")).display(),
            dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&dir, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let head = || {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&*dir)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let before = head();

    // The region rendered from the lock as the working tree has it.
    fs::write(dir.join("x.lock"), "two\n").unwrap();
    let ran = Command::new("computed")
        .args(["run", "--trust", "MAP.md"])
        .current_dir(&*dir)
        .output()
        .unwrap();
    assert!(ran.status.code().is_some_and(|c| c < 2), "{ran:?}");
    fs::write(dir.join("other.txt"), "b\n").unwrap();

    let commit = |paths: &[&str]| {
        Command::new("git")
            .args(["-c", "user.name=xtask", "-c", "user.email=xtask@scratch"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "m",
                "--",
            ])
            .args(paths)
            .current_dir(&*dir)
            .output()
            .unwrap()
    };
    let refused = commit(&["MAP.md", "other.txt"]);
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{err}");
    assert!(err.contains("MAP.md:3 lock reads x.lock"), "{err}");
    assert_eq!(head(), before, "the commit was refused");

    let carried = commit(&["MAP.md", "other.txt", "x.lock"]);
    assert!(
        carried.status.success(),
        "{}",
        String::from_utf8_lossy(&carried.stderr)
    );
    assert_ne!(head(), before);
}
