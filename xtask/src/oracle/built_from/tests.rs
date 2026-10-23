//! What a binary is built from, read off a dep-info written as cargo
//! writes one beside a binary (#3109).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;
use crate::scratch::Tree;

/// The files of a workspace the dep-info below names, and the ones it does
/// not: a crate the binary does not link, the generated clippy
/// configuration, a crate directory's guide, a package's README and test.
const FILES: [&str; 11] = [
    "crates/php_engine/Cargo.toml",
    "crates/php_engine/src/lib.rs",
    "crates/php_engine/README.md",
    "crates/php_engine/tests/kernels.rs",
    "crates/timelib_sys/lib/timelib.c",
    "ops.toml",
    "sapi/cli/src/main.rs",
    "crates/php_jit/src/lib.rs",
    "ext/clippy.toml",
    "ext/AGENTS.md",
    "seams.toml",
];

/// What the binary's dep-info names: two sources, a directory a `build.rs`
/// reruns on, a file a `build.rs` reads through `..`, and what
/// one generated under `target`.
const LISTED: [&str; 5] = [
    "crates/php_engine/src/lib.rs",
    "crates/timelib_sys/lib",
    "crates/php_engine/../../ops.toml",
    "sapi/cli/src/main.rs",
    "target/release-da/build/php_unit-0/out/ops.rs",
];

/// A workspace whose [`FILES`] were written an hour ago, and a binary
/// written now beside a dep-info naming `listed` under it after `target`.
fn built(name: &str, target: Option<&Path>, listed: &[&str]) -> (Tree, PathBuf) {
    let tree = Tree::new(&format!("built-from-{name}")).unwrap();
    let ago = SystemTime::now() - Duration::from_hours(1);
    for file in FILES {
        written(&tree.join(file), Some(ago));
    }
    let binary = tree.join("target/release-da/php");
    written(&binary, None);
    let named = |path: &Path| path.display().to_string().replace(' ', "\\ ");
    let deps: Vec<String> = listed.iter().map(|f| named(&tree.join(f))).collect();
    let line = format!("{}: {}\n", named(target.unwrap_or(&binary)), deps.join(" "));
    fs::write(format!("{}.d", binary.display()), line).unwrap();
    (tree, binary)
}

/// The crate directories of a manifest laying crates down in `crates`,
/// `ext` and `sapi`.
fn layout() -> Layout {
    Layout::of(&["php_engine", "ext/standard", "sapi/cli"].map(crate::manifest::Crate::named))
}

/// `path` written, and dated `at` where named.
fn written(path: &Path, at: Option<SystemTime>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, path.display().to_string()).unwrap();
    if let Some(at) = at {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(at).unwrap();
    }
}

#[test]
fn a_dep_infos_words_read_an_escaped_space_as_part_of_the_path() {
    let words: Vec<String> = words("/w/my\\ php: /w/a.rs /w/b\\ c.rs\n").collect();
    assert_eq!(words, ["/w/my php:", "/w/a.rs", "/w/b c.rs"]);
}

/// A run is kept under each package the binary links, whole, every host,
/// the files a `build.rs` reads and the workspace's own, and under
/// nothing the binary is not built from: `crates/php_jit`, which it does
/// not link, `ext/clippy.toml` and `ext/AGENTS.md`, and what a
/// `build.rs` generated.
#[test]
fn a_run_is_kept_under_the_packages_the_binary_links_and_nothing_else() {
    let (tree, binary) = built("keyed", None, &LISTED);
    let built = BuiltFrom::of(&binary);
    assert!(matches!(built, BuiltFrom::Listed(_)), "{built:?}");
    let keyed: Vec<String> = (built.keyed(&tree, &layout()).iter())
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(
        keyed,
        [
            ".cargo/config.toml",
            "Cargo.lock",
            "Cargo.toml",
            "crates/php_engine",
            "crates/timelib_sys",
            "ops.toml",
            "rust-toolchain.toml",
            "sapi/cgi",
            "sapi/cli",
            "sapi/fpm",
            "sapi/makala",
            "sapi/phpdbg",
        ]
    );
}

/// A binary with no dep-info, or one whose dep-info is another binary's,
/// as a target directory copied from another checkout carries, is built
/// from every crate directory, the key every run had before #3109; and so
/// is one whose dep-info names no package of the workspace.
#[test]
fn a_binary_without_a_dep_info_of_its_own_is_built_from_every_crate_directory() {
    let (tree, binary) = built("copied", Some(Path::new("/elsewhere/php")), &LISTED);
    assert_eq!(BuiltFrom::of(&binary), BuiltFrom::Unlisted);
    fs::remove_file(format!("{}.d", binary.display())).unwrap();
    assert_eq!(BuiltFrom::of(&binary), BuiltFrom::Unlisted);
    let every = [
        "Cargo.toml",
        "Cargo.lock",
        "ops.toml",
        "crates",
        "ext",
        "sapi",
    ];
    let every: Vec<PathBuf> = every.into_iter().map(PathBuf::from).collect();
    assert_eq!(BuiltFrom::Unlisted.keyed(&tree, &layout()), every);
    let (tree, binary) = built("outside", None, &["/usr/include/zlib.h"]);
    assert_eq!(BuiltFrom::of(&binary).keyed(&tree, &layout()), every);
}

/// The binary is behind only what its build read, as cargo relinks it:
/// not a crate it does not link, `ext/clippy.toml`, a guide, a README or a
/// test, and a source it was compiled from, a file under a directory a
/// `build.rs` reruns on, and a linked package's manifest each.
#[test]
fn the_binary_is_behind_only_a_path_its_build_read() {
    let (tree, binary) = built("newest", None, &LISTED);
    let built = BuiltFrom::of(&binary);
    let made = binary.metadata().unwrap().modified().unwrap();
    let cli = Path::new("sapi/cli");
    for beside in [
        "crates/php_jit/src/lib.rs",
        "ext/clippy.toml",
        "ext/AGENTS.md",
        "seams.toml",
        "crates/php_engine/README.md",
        "crates/php_engine/tests/kernels.rs",
    ] {
        written(&tree.join(beside), Some(made + Duration::from_mins(1)));
        let (_, when) = built.newest(&tree, &layout(), cli).unwrap();
        assert!(when < made, "{beside} moved the binary");
    }
    for (moved, at) in [
        ("crates/php_engine/src/lib.rs", 120),
        ("crates/timelib_sys/lib/timelib.c", 180),
        ("crates/php_engine/Cargo.toml", 240),
    ] {
        written(&tree.join(moved), Some(made + Duration::from_secs(at)));
        let (newest, _) = built.newest(&tree, &layout(), cli).unwrap();
        assert_eq!(newest, tree.join(moved));
    }
}

/// With no dep-info, the binary is held to the engine's sources alone, as
/// before #3109: a generated `clippy.toml`, a guide or a test written after
/// it does not put it behind the tree, and a source does.
#[test]
fn a_binary_with_no_dep_info_is_behind_only_an_engine_source() {
    let (tree, binary) = built("unlisted", None, &LISTED);
    fs::remove_file(format!("{}.d", binary.display())).unwrap();
    let made = binary.metadata().unwrap().modified().unwrap();
    let later = Some(made + Duration::from_mins(1));
    for beside in [
        "ext/clippy.toml",
        "ext/AGENTS.md",
        "crates/php_engine/tests/kernels.rs",
    ] {
        written(&tree.join(beside), later);
    }
    let cli = Path::new("sapi/cli");
    let (_, when) = BuiltFrom::Unlisted.newest(&tree, &layout(), cli).unwrap();
    assert!(when < made);
    written(&tree.join("crates/php_jit/src/lib.rs"), later);
    let (newest, _) = BuiltFrom::Unlisted.newest(&tree, &layout(), cli).unwrap();
    assert_eq!(newest, tree.join("crates/php_jit/src/lib.rs"));
}
