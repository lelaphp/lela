//! `xtask regions`, the one reader of what `computed run` answered (#43;
//! harness-design.md §5): each outcome against a stand-in `computed` on
//! `PATH` that answers with one shape, so the exit and the lines the
//! reader interprets are the ones the spec names, not this machine's
//! trust grant. `new-ext` and `worktree add` read the same value in
//! process, and pre-commit reads this subcommand's lines.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A directory with one stand-in `computed` in it, which records the
/// arguments and the directory it was called from, prints `said` on
/// stderr, and exits with `code`.
fn stand_in(name: &str, said: &str, code: i32) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-computed-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
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
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
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
    let _ = fs::remove_dir_all(&dir);
}

/// Exit 0 is every region fresh: nothing written, nothing to re-stage.
#[test]
fn every_region_fresh_is_unchanged() {
    let dir = stand_in("fresh", "", 0);
    let (code, out, _) = regions(&dir, &["AGENTS.md"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out, "AGENTS.md: unchanged\n");
    let _ = fs::remove_dir_all(&dir);
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
    let _ = fs::remove_dir_all(&dir);
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
    let _ = fs::remove_dir_all(&dir);
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
    let _ = fs::remove_dir_all(&dir);
}

/// The subcommand is in `xtask --help`, and no file is a usage error.
#[test]
fn regions_is_a_subcommand_and_needs_a_file() {
    let dir = stand_in("usage", "", 0);
    let (code, out, err) = regions(&dir, &[]);
    assert_eq!(code, 2, "{out}{err}");
    let help = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("\n  regions "), "{help}");
    let _ = fs::remove_dir_all(&dir);
}
