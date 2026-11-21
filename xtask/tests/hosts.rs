//! `xtask hosts --profile <name>`, the command that builds the hosts an
//! instrument judges and prints where each one is: over a scratch
//! workspace of stand-in host crates, one per host the oracle's profile
//! code lists, as `xtask status --section corpus` renders that list for
//! CI, and one host beside them the list does not name, which the command
//! leaves unbuilt. The stand-ins build in seconds; the real hosts are never
//! built here.

use std::fs;
use std::path::{Path, PathBuf};

use xtask::scratch::Tree;

mod common {
    pub mod run;
}
use common::run::{Ran, Run};

/// One host of the oracle's list: its crate's directory, its package and
/// its binary.
struct Host {
    krate: String,
    package: String,
    binary: String,
}

/// The oracle's list of hosts, read off the workspace this test builds
/// in, where `status --section corpus` renders it for CI to build.
fn listed() -> Vec<Host> {
    let workspace = xtask::package_dir().parent().unwrap().to_path_buf();
    let corpus = Run::at(&workspace)
        .args(["status", "--section", "corpus", "--format", "json"])
        .go()
        .ok();
    let corpus: serde_json::Value = serde_json::from_str(&corpus).unwrap();
    let text = |host: &serde_json::Value, key: &str| host[key].as_str().unwrap().to_owned();
    corpus["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|host| Host {
            krate: text(host, "crate"),
            package: text(host, "package"),
            binary: text(host, "binary"),
        })
        .collect()
}

/// A workspace of a stand-in crate per host of `hosts`, and one for a
/// host the list does not name, `sapi/unlisted`, each a binary that does
/// nothing, with the profiles the real workspace defines beyond cargo's.
fn workspace(name: &str, hosts: &[Host]) -> Tree {
    let root = Tree::new(&format!("hosts-{name}")).unwrap();
    let unlisted = Host {
        krate: "sapi/unlisted".into(),
        package: "php_sapi_unlisted".into(),
        binary: "unlisted".into(),
    };
    let mut members = Vec::new();
    for host in hosts.iter().chain([&unlisted]) {
        let dir = root.join(&host.krate);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"{}\"\npath = \"main.rs\"\n",
                host.package, host.binary
            ),
        )
        .unwrap();
        fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        members.push(format!("\"{}\"", host.krate));
    }
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nresolver = \"3\"\nmembers = [{}]\n\n[profile.release-da]\ninherits = \"release\"\ndebug-assertions = true\n",
            members.join(", ")
        ),
    )
    .unwrap();
    root
}

/// `xtask hosts` over `root` with `args`, cargo building into `root`'s own
/// target directory whatever the test's environment names.
fn hosts(root: &Path, args: &[&str]) -> Ran {
    Run::at(root)
        .args(["hosts"])
        .args(args)
        .env_remove("CARGO_TARGET_DIR")
        .go()
}

/// Every host of the oracle's list is built in the profile named and its
/// path printed, one a line, in the list's order, the binary under test
/// first; the host the list does not name is not built. The list the
/// command builds is the oracle's profile code's: the printed binaries are
/// the ones `status` renders of it, and no other.
#[test]
fn the_hosts_of_the_oracles_list_are_built_in_the_profile_named_and_each_path_printed() {
    let listed = listed();
    assert!(!listed.is_empty(), "the oracle lists its hosts");
    let root = workspace("built", &listed);
    for (profile, dir) in [("release-da", "release-da"), ("dev", "debug")] {
        let out = hosts(&root, &["--profile", profile]).ok();
        let printed: Vec<PathBuf> = out.lines().map(PathBuf::from).collect();
        let wanted: Vec<PathBuf> = listed
            .iter()
            .map(|host| root.join("target").join(dir).join(&host.binary))
            .collect();
        assert_eq!(printed, wanted, "{profile}: {out}");
        for path in &printed {
            assert!(path.is_file(), "{profile}: {} was built", path.display());
        }
        let unlisted = root.join("target").join(dir).join("unlisted");
        assert!(
            !unlisted.exists(),
            "{profile}: a host the list does not name is not built"
        );
    }
}

/// A profile the workspace does not define is a usage error that names
/// the ones it does, and nothing is built.
#[test]
fn a_profile_the_workspace_does_not_define_is_refused_naming_the_ones_it_does() {
    let root = workspace("refused", &listed());
    let ran = hosts(&root, &["--profile", "fast"]);
    assert_eq!(ran.code(), 2, "a usage error: {ran}");
    for name in ["dev", "release", "release-da"] {
        assert!(ran.stderr.contains(name), "names {name}: {ran}");
    }
    assert!(!root.join("target").exists(), "nothing is built: {ran}");
}

/// The command writes no reading: after a run that built every host, the
/// workspace's store holds nothing, and there is none to look for.
#[test]
fn it_writes_no_reading() {
    let root = workspace("no-reading", &listed());
    hosts(&root, &["--profile", "dev"]).ok();
    let store = xtask::reading::Store::of_workspace(&root);
    assert!(
        !store.root().exists(),
        "the store at {} holds a reading",
        store.root().display()
    );
}

/// Every host `--list` prints, one JSON object a line, as `dist/` reads
/// them: `json.loads` over each line.
fn list(root: &Path, args: &[&str]) -> Vec<serde_json::Value> {
    let out = hosts(root, &[&["--list"], args].concat()).ok();
    out.lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}")))
        .collect()
}

/// `--list` prints every host with where a release ships it, as Mitchell
/// ruled on 21 November 2026: all four in the tarball, the debugger host
/// in the `cli` image and in no other, the CLI and CGI in every image,
/// the FPM in the two that serve through it. Its hosts are the ones
/// `status` renders of the oracle's list, in that order.
#[test]
fn the_list_says_where_a_release_ships_each_host() {
    let root = Tree::new("hosts-list").unwrap();
    let printed = list(&root, &[]);
    let wanted = [
        r#"{"binary":"lela","crate":"sapi/cli","package":"php_sapi_cli","tarball":true,"images":["cli","fpm","apache"]}"#,
        r#"{"binary":"lela-cgi","crate":"sapi/cgi","package":"php_sapi_cgi","tarball":true,"images":["cli","fpm","apache"]}"#,
        r#"{"binary":"lela-fpm","crate":"sapi/fpm","package":"php_sapi_fpm","tarball":true,"images":["fpm","apache"]}"#,
        r#"{"binary":"lela-dbg","crate":"sapi/phpdbg","package":"php_sapi_phpdbg","tarball":true,"images":["cli"]}"#,
    ]
    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
    assert_eq!(printed, wanted);
    let binaries: Vec<String> = listed().into_iter().map(|host| host.binary).collect();
    let named: Vec<&str> = printed
        .iter()
        .map(|h| h["binary"].as_str().unwrap())
        .collect();
    assert_eq!(named, binaries, "the list is the oracle's");
}

/// `--list` builds nothing, takes no seat and writes no reading, with
/// `--profile` or without: over a workspace with no `target/` it leaves
/// none, the seat directory it is pointed at is never made, and the store
/// holds nothing. With `--profile` each line names the path the build
/// would leave. The control: the building mode, pointed at the same seat
/// directory, makes it.
#[test]
fn the_list_builds_nothing_takes_no_seat_and_writes_no_reading() {
    let root = workspace("list", &listed());
    let seats = root.join("seats");
    let seated = |args: &[&str]| {
        Run::at(&root)
            .args(["hosts"])
            .args(args)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("XTASK_SEATED")
            .env("XTASK_SEATS", "1")
            .env("XTASK_SEAT_DIR", &seats)
            .go()
    };
    seated(&["--list"]).ok();
    let out = seated(&["--list", "--profile", "release-da"]).ok();
    let paths: Vec<PathBuf> = out
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .map(|host| PathBuf::from(host["path"].as_str().unwrap()))
        .collect();
    let wanted: Vec<PathBuf> = (listed().iter())
        .map(|host| root.join("target").join("release-da").join(&host.binary))
        .collect();
    assert_eq!(paths, wanted, "{out}");
    assert!(!root.join("target").exists(), "--list built nothing");
    assert!(!seats.exists(), "--list took no seat");
    let store = xtask::reading::Store::of_workspace(&root);
    assert!(!store.root().exists(), "--list wrote no reading");
    seated(&["--profile", "dev"]).ok();
    assert!(seats.join("0").is_file(), "the building mode takes a seat");
}
