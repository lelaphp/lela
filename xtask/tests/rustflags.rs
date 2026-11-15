//! The negative control of `php_engine`'s refusal of a release-like build
//! without the LLVM option every profile passes (design §10 Profiles):
//! `xtask/fixtures/rustflags`, a crate whose `build.rs` is the
//! engine's rule, built from inside the workspace, so under its cargo
//! configuration, with flags that replace the workspace's rustflags table
//! as a `RUSTFLAGS` variable would. Each build is its own cargo run in a
//! scratch target directory.
#![cfg(tracked)]

use std::path::Path;
use std::process::{Command, Output};

/// The option, as the rule names it in what it prints.
const OPTION: &str = "--cgp-max-address-users-to-scan";

/// Flags with no `llvm-args` among them.
const WITHOUT: &str = "--cfg\x1fwithout_the_option";

/// The fixture built in `profile` into `target`, with
/// `CARGO_ENCODED_RUSTFLAGS` set to `flags` where they are named, which
/// replaces the workspace's table, and `PHP_ENGINE_FOLD` to `ask` where it
/// is named; where it is not, the workspace's `[env]` alone says it.
fn built(target: &Path, profile: &str, flags: Option<&str>, ask: Option<&str>) -> Output {
    let fixture = xtask::package_dir().join("fixtures/rustflags");
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cargo
        .args(["build", "--locked", "--profile", profile, "--target-dir"])
        .arg(target)
        .current_dir(&fixture)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("PHP_ENGINE_FOLD");
    if let Some(flags) = flags {
        cargo.env("CARGO_ENCODED_RUSTFLAGS", flags);
    }
    if let Some(ask) = ask {
        cargo.env("PHP_ENGINE_FOLD", ask);
    }
    cargo.output().unwrap()
}

fn said(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Inside the workspace a release build, and a release-da one, whose
/// flags lack the option are refused, naming it and the line that adds
/// flags without replacing the table: the workspace's `[env]` asks, and a
/// release build that passed under the table before, in the same target
/// directory, is judged again, cargo running `build.rs` anew for the new
/// rustflags. One whose last value is under the floor is refused, the
/// last standing where a higher came before it. One that sets the
/// variable to `warn` is built with a warning naming it, as one from
/// outside the workspace is; a `dev` build is neither; and a value the
/// rule does not know fails naming the two it does.
#[test]
fn a_release_build_without_the_option_is_refused_where_the_workspace_asks() {
    let target = xtask::scratch::Tree::new("rustflags").unwrap();
    let warm = built(&target, "release", None, None);
    assert!(warm.status.success(), "{}", said(&warm));
    for profile in ["release", "release-da"] {
        let out = built(&target, profile, Some(WITHOUT), None);
        let said = said(&out);
        assert!(!out.status.success(), "{profile} was built: {said}");
        assert!(
            said.contains(&format!("error: rustflags@0.0.0: this release build of php_engine passes rustc no LLVM {OPTION}"))
                && said.contains("--config \"target.'cfg(all())'.rustflags=[...]\""),
            "{profile}: {said}"
        );
    }
    for low in [
        format!("-C\x1fllvm-args={OPTION}=100"),
        format!("-Cllvm-args={OPTION}=100000\x1f-C\x1fllvm-args={OPTION}=100"),
    ] {
        let out = built(&target, "release", Some(&low), None);
        assert!(!out.status.success(), "{low:?}: {}", said(&out));
        assert!(
            said(&out).contains(&format!("{OPTION}=100, under the 1000")),
            "{low:?}: {}",
            said(&out)
        );
    }

    let warned = built(&target, "release", Some(WITHOUT), Some("warn"));
    assert!(warned.status.success(), "{}", said(&warned));
    assert!(
        said(&warned).contains(&format!("warning: rustflags@0.0.0: this release build of php_engine passes rustc no LLVM {OPTION}")),
        "{}",
        said(&warned)
    );

    let dev = built(&target, "dev", Some(WITHOUT), None);
    assert!(dev.status.success(), "{}", said(&dev));
    assert!(!said(&dev).contains(OPTION), "{}", said(&dev));

    let unknown = built(&target, "release", None, Some("maybe"));
    assert!(!unknown.status.success(), "{}", said(&unknown));
    assert!(
        said(&unknown).contains("is neither `refuse` nor `warn`"),
        "{}",
        said(&unknown)
    );
}

/// The positive control: the workspace's own rustflags table passes the
/// option, so a release build is built with nothing said of it, as one
/// whose flags pass it in another of rustc's spellings is, `--codegen`
/// and its argument apart among them and LLVM's one-dash spelling of the
/// option, the last value standing where several are passed.
#[test]
fn a_release_build_under_the_workspaces_table_is_built() {
    let target = xtask::scratch::Tree::new("rustflags-table").unwrap();
    let plain = built(&target, "release", None, None);
    assert!(plain.status.success(), "{}", said(&plain));
    assert!(!said(&plain).contains(OPTION), "{}", said(&plain));
    for flags in [
        format!("-Cllvm-args={OPTION}=1000"),
        format!("--codegen=llvm-args={OPTION}=100\x1f-C\x1fllvm-args={OPTION}=100000"),
        format!("--codegen\x1fllvm-args={OPTION}=1000"),
        format!("-Cllvm-args=-{}=1000", OPTION.trim_start_matches('-')),
    ] {
        let out = built(&target, "release", Some(&flags), None);
        assert!(out.status.success(), "{flags:?}: {}", said(&out));
    }
}
