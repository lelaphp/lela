//! `xtask status`, the one renderer (harness-design.md §4.6): it reads the
//! manifest and the store and computes nothing else.
#![cfg(tracked)]

use std::path::Path;

use xtask::manifest::Manifest;
use xtask::oracle::{PROMISE_VARIABLE, Platform, TREE_VARIABLE};
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::sha256::Sha256;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::{Tracked, pin, with_source_path};
use common::repo::Repo;
use common::run::{Ran, Run};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// A scratch workspace: a manifest of no row ([`Tracked::minimal`]), the
/// tracked pin, a registry with no row, and no store.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("status-{name}")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::copy(workspace().join("oracle.toml"), dir.join("oracle.toml")).unwrap();
    Tracked::minimal().write(&dir);
    let empty = xtask::package_dir().join("fixtures/registry/empty.toml");
    std::fs::copy(empty, dir.join("docs/divergence.toml")).unwrap();
    dir
}

/// A scratch workspace holding the tracked manifest itself, for the tests
/// in [`tracked`], whose subject is how a section renders the tracked
/// file's rows.
fn tracked_scratch(name: &str) -> Tree {
    let dir = scratch(name);
    std::fs::copy(workspace().join("seams.toml"), dir.join("seams.toml")).unwrap();
    dir
}

fn status(root: &Path, args: &[&str]) -> Ran {
    Run::at(root).args(["status"]).args(args).go()
}

/// `status` with the hook's stdin, the JSON head every hook event carries.
fn status_with_stdin(root: &Path, args: &[&str], stdin: &str) -> Ran {
    Run::at(root).args(["status"]).args(args).stdin(stdin).go()
}

#[test]
fn every_section_reports_no_reading_when_the_store_is_gone() {
    let root = scratch("empty");
    for section in ["rules", "locks", "budget"] {
        let ran = status(&root, &["--section", section]);
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let md = ran.stdout;
        assert!(md.contains("no reading"), "{section}: {md}");
    }
    assert!(
        !root.join("target").exists(),
        "status ran nothing and wrote nothing"
    );
}

/// `status` with the two variables of the pin as the test wants them,
/// set or removed, since the process running the tests may carry either
/// (CI sets both for the suite).
fn status_with_pin(root: &Path, args: &[&str], tree: Option<&Path>, promise: Option<&str>) -> Ran {
    let run = Run::at(root).args(["status"]).args(args);
    let run = match tree {
        Some(tree) => run.env(TREE_VARIABLE, tree),
        None => run.env_remove(TREE_VARIABLE),
    };
    match promise {
        Some(promise) => run.env(PROMISE_VARIABLE, promise),
        None => run.env_remove(PROMISE_VARIABLE),
    }
    .go()
}

/// A scratch workspace whose pin's `[source].path` is `path`: the tracked
/// `oracle.toml`, which states none, with that one line stated in the
/// copy, the tracked file untouched.
fn scratch_with_pin_path(name: &str, path: &Path) -> Tree {
    let root = scratch(name);
    let pin = root.join("oracle.toml");
    let text = std::fs::read_to_string(&pin).unwrap();
    std::fs::write(&pin, with_source_path(&text, path)).unwrap();
    root
}

/// A directory standing where the pinned php-src would.
fn fixture_tree(name: &str) -> Tree {
    Tree::new(&format!("status-oracle-{name}")).unwrap()
}

/// The `oracle` section as JSON, under the two variables as given.
fn oracle_json(root: &Path, tree: Option<&Path>, promise: Option<&str>) -> serde_json::Value {
    let json = status_with_pin(
        root,
        &["--section", "oracle", "--format", "json"],
        tree,
        promise,
    )
    .ok();
    serde_json::from_str(&json).unwrap()
}

/// The `oracle` section: the pin as this machine has it, the
/// variables by name and whether set, the pin's path, and where the tree
/// was found. Here the variable names a tree: found there, ahead of the
/// pin's path, which is a directory too.
#[test]
fn oracle_finds_the_tree_the_variable_names_and_states_both_names() {
    let named = fixture_tree("named");
    let pinned = fixture_tree("named-pinned");
    let root = scratch_with_pin_path("oracle-named", &pinned);
    let md = status_with_pin(&root, &["--section", "oracle"], Some(&named), Some("tree")).ok();
    let named_text = named.display().to_string();
    assert!(
        md.contains(&format!("| tree | {TREE_VARIABLE} | {named_text} | set |")),
        "{md}"
    );
    assert!(
        md.contains(&format!("| promise | {PROMISE_VARIABLE} | tree | set |")),
        "{md}"
    );
    assert!(
        md.contains(&format!(
            "| found |  | {named_text} | named by {TREE_VARIABLE} |"
        )),
        "{md}"
    );
    let json = oracle_json(&root, Some(&named), Some("tree"));
    assert_eq!(json["tree"]["variable"], TREE_VARIABLE);
    assert_eq!(json["tree"]["set"], true);
    assert_eq!(json["tree"]["value"], named_text);
    assert_eq!(json["promise"]["variable"], PROMISE_VARIABLE);
    assert_eq!(json["promise"]["set"], true);
    assert_eq!(json["promise"]["value"], "tree");
    assert_eq!(json["pin"]["path"], pinned.display().to_string());
    assert_eq!(json["pin"]["directory"], true);
    assert_eq!(json["found"]["at"], "variable");
    assert_eq!(json["found"]["path"], named_text);
    assert_eq!(json["found"]["error"], serde_json::Value::Null);
}

/// With the variable unset, the pin's path is the tree, and both rows
/// say unset.
#[test]
fn oracle_falls_back_to_the_pins_path_when_the_variable_is_unset() {
    let pinned = fixture_tree("pinned");
    let root = scratch_with_pin_path("oracle-pinned", &pinned);
    let md = status_with_pin(&root, &["--section", "oracle"], None, None).ok();
    assert!(
        md.contains(&format!("| tree | {TREE_VARIABLE} |  | unset |")),
        "{md}"
    );
    assert!(
        md.contains(&format!("| promise | {PROMISE_VARIABLE} |  | unset |")),
        "{md}"
    );
    assert!(
        md.contains(&format!("| pin |  | {} | a directory |", pinned.display())),
        "{md}"
    );
    assert!(
        md.contains(&format!(
            "| found |  | {} | the pin's path |",
            pinned.display()
        )),
        "{md}"
    );
    let json = oracle_json(&root, None, None);
    assert_eq!(json["tree"]["set"], false);
    assert_eq!(json["tree"]["value"], serde_json::Value::Null);
    assert_eq!(json["promise"]["set"], false);
    assert_eq!(json["promise"]["value"], serde_json::Value::Null);
    assert_eq!(json["found"]["at"], "pin");
    assert_eq!(json["found"]["path"], pinned.display().to_string());
}

/// With neither present, the found row names the variable, its state and
/// the pin's path, and the section renders rather than failing, writing
/// nothing.
#[test]
fn oracle_names_both_places_when_the_tree_is_at_neither() {
    let nowhere = Path::new("/nowhere/pinned/php-src");
    let root = scratch_with_pin_path("oracle-absent", nowhere);
    let md = status_with_pin(&root, &["--section", "oracle"], None, None).ok();
    assert!(
        md.contains(&format!(
            "| pin |  | {} | not a directory |",
            nowhere.display()
        )),
        "{md}"
    );
    let found = md.lines().find(|l| l.starts_with("| found |")).unwrap();
    assert!(found.contains("absent:"), "{found}");
    assert!(
        found.contains(&format!("{TREE_VARIABLE} is unset")),
        "{found}"
    );
    assert!(found.contains(&nowhere.display().to_string()), "{found}");
    let json = oracle_json(&root, None, None);
    assert_eq!(json["found"]["at"], serde_json::Value::Null);
    assert_eq!(json["found"]["path"], serde_json::Value::Null);
    let error = json["found"]["error"].as_str().unwrap();
    assert!(
        error.contains(TREE_VARIABLE) && error.contains("/nowhere/pinned/php-src"),
        "{error}"
    );
    assert!(
        !root.join("target").exists(),
        "status ran nothing and wrote nothing"
    );
}

/// The pinned php a runner pours: its version, keg path and SHA-256 as the
/// pin states them, and each variant's build for this machine a runner
/// lays by its SHA-256, so the oracle job reads the pin off this section
/// and never parses `oracle.toml` a second time. No binary is named by an
/// md5, which no pin carries.
#[test]
fn oracle_states_the_pinned_php_a_runner_pours() {
    let root = scratch("oracle-binary");
    let pin = pin();
    let oracle = pin.oracle.clone();
    let path = oracle.path.display().to_string();
    let json = oracle_json(&root, None, None);
    assert_eq!(json["binary"]["version"], oracle.version.as_str());
    assert_eq!(json["binary"]["path"], path.as_str());
    assert_eq!(json["binary"]["sha256"], oracle.sha256.to_string());
    assert!(json["binary"].get("md5").is_none(), "{json}");
    // The formula the keg is poured from, which is the keg's name
    // in the path the section states beside it, and the bottle of it, the
    // directory the keg stands in.
    let formula = json["binary"]["formula"].as_str().unwrap();
    let bottle = json["binary"]["bottle"].as_str();
    let mut parts = oracle.path.iter().skip_while(|part| *part != "Cellar");
    assert_eq!(parts.nth(1).and_then(|keg| keg.to_str()), Some(formula));
    assert_eq!(parts.next().and_then(|at| at.to_str()), bottle);
    assert!(bottle.unwrap().starts_with(&oracle.version), "{bottle:?}");
    for name in pin.variant.keys() {
        let built = pin.variant(name).unwrap().oracle;
        let laid = &json["variants"][name.as_str()];
        if built.sha256 == Sha256::default() {
            assert!(laid.is_null(), "{name} pins no build here: {laid}");
            continue;
        }
        assert_eq!(laid["sha256"], built.sha256.to_string(), "{name}");
        assert!(laid.get("md5").is_none(), "{name}: {laid}");
        assert_eq!(laid["path"], built.path.to_str().unwrap(), "{name}");
        assert_eq!(laid["platform"], Platform::this().unwrap().key(), "{name}");
        let pinned = pin.variant[name].build(Platform::this().unwrap()).unwrap();
        let closure = pinned.provenance.closure.as_ref().map(ToString::to_string);
        assert_eq!(laid["closure"], serde_json::json!(closure), "{name}");
    }
    // The record's digest, SHA-256 over each `name=version\n` in name
    // order, computed here from the kegs the section names.
    if let Some(kegs) = json["closure"].as_object() {
        use sha2::Digest as _;
        let mut sha = sha2::Sha256::new();
        for (keg, version) in kegs {
            sha.update(format!("{keg}={}\n", version.as_str().unwrap()));
        }
        let hex: Vec<String> = sha.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(json["closure_digest"], format!("sha256:{}", hex.concat()));
    }
    let pinned_here = pin
        .variant
        .keys()
        .filter(|n| pin.variant(n).unwrap().oracle.sha256 != Sha256::default());
    assert_eq!(
        json["variants"].as_object().unwrap().len(),
        pinned_here.count()
    );
    let md = status_with_pin(&root, &["--section", "oracle"], None, None).ok();
    assert!(
        md.contains(&format!(
            "| binary |  | {path} | php {}, {}, poured from the formula {formula} |",
            oracle.version, oracle.sha256
        )),
        "{md}"
    );
    for (name, laid) in json["variants"].as_object().unwrap() {
        let row = md
            .lines()
            .find(|l| l.starts_with(&format!("| variant {name} |")));
        let digest = pin.variant(name).unwrap().oracle.sha256.to_string();
        let state = match laid["closure"].as_str() {
            Some(closure) => format!("{digest}, closure {closure} |"),
            None => format!("{digest} |"),
        };
        assert!(row.is_some_and(|row| row.ends_with(&state)), "{md}");
    }
}

/// A scratch workspace whose pin is `fixture` of
/// `xtask/fixtures/oracle/formula/`, in place of the tracked one, which
/// pins php by the SHA-256 of 64 zeros the stand-in [`SHASUM`] prints.
fn scratch_poured(name: &str, fixture: &str) -> Tree {
    let root = scratch(name);
    let fixture = xtask::package_dir()
        .join("fixtures/oracle/formula")
        .join(fixture);
    std::fs::copy(fixture, root.join("oracle.toml")).unwrap();
    root
}

/// The digest the scratch pin of `root` holds php to, as
/// `scripts/ci/pinned-php.sh` names it: `sha256:<hex>`.
fn held(root: &Path) -> String {
    let pin = xtask::oracle::Pin::load(&root.join("oracle.toml")).unwrap();
    pin.oracle.sha256.to_string()
}

/// The stand-in `brew` of [`pinned_php`], `@prefix@` its Homebrew prefix.
/// It answers `--prefix` and `--cache`, `@prefix@/cache`, and under
/// `DRY_RUN=1` nothing else. Outside a
/// dry run it writes each call to `@prefix@/calls` and answers as far as
/// a pour goes: a pin, an unpin and a tap listing no tap succeed, the
/// forced uninstall of `php@8.5` drops the pin of `php`, as brew's
/// `Uninstall.rm_pin` does while `php@8.5` is homebrew-core's alias of
/// `php`, `tap-new` and `--repository` give the tap `@prefix@/tap`,
/// and `install` lays the keg the tap's formula file bottles,
/// `Cellar/<formula>/<bottle>/bin/php`, as brew lands a bottle where its
/// formula and its version say, with the receipt brew writes beside it
/// naming the tap it was poured from. Any other call fails.
const BREW: &str = r#"#!/bin/sh
[ "$*" = --prefix ] && echo "@prefix@" && exit 0
[ "$*" = --cache ] && echo "@prefix@/cache" && exit 0
if [ -z "$DRY_RUN" ]; then
    echo "brew $*" >>"@prefix@/calls"
    case "$*" in
    "pin "* | "unpin "* | tap) exit 0 ;;
    "uninstall --ignore-dependencies --force php@8.5") rm -f "@prefix@/var/homebrew/pinned/php" && exit 0 ;;
    "tap-new --no-git dax/pin") mkdir -p "@prefix@/tap/Formula" && exit 0 ;;
    "--repository dax/pin") echo "@prefix@/tap" && exit 0 ;;
    "install --ignore-dependencies dax/pin/"*)
        name=${3#dax/pin/}
        bottle=$(sed -n 's/^# bottled-//p' "@prefix@/tap/Formula/$name.rb")
        keg="@prefix@/Cellar/$name/$bottle"
        [ -n "$bottle" ] && mkdir -p "$keg/bin" && : >"$keg/bin/php" &&
            echo '{"source":{"tap":"dax/pin"}}' >"$keg/INSTALL_RECEIPT.json" && exit 0
        ;;
    esac
fi
echo "brew $*" >&2
exit 1
"#;

/// The stand-in `curl` of [`pinned_php`]: nothing under `DRY_RUN=1`.
/// Outside a dry run it writes each call to `@prefix@/calls`, serves
/// the commits GitHub lists for a formula from
/// `@prefix@/api/<formula>.json`, where the test laid them, and for a
/// formula file at a commit writes one whose first line names the
/// commit, so the stand-in `brew` pours what that commit bottles. Any
/// other call fails, a formula the test laid no commits for among them.
const CURL: &str = r#"#!/bin/sh
if [ -z "$DRY_RUN" ]; then
    echo "curl $*" >>"@prefix@/calls"
    for url; do :; done
    case "$url" in
    "https://api.github.com/repos/Homebrew/homebrew-core/commits?path=Formula/"*"&per_page=100")
        name=${url#*path=Formula/*/}
        name=${name%".rb&per_page=100"}
        [ -f "@prefix@/api/$name.json" ] && cat "@prefix@/api/$name.json" && exit 0
        ;;
    "https://raw.githubusercontent.com/Homebrew/homebrew-core/"*)
        sha=${url#https://raw.githubusercontent.com/Homebrew/homebrew-core/}
        [ "$2" = -o ] &&
            printf '# %s\nclass Php < Formula\n  no_autobump! because: :requires_manual_review\n  bottle do\n  end\nend\n' "${sha%%/*}" >"$3" &&
            exit 0
        ;;
    esac
fi
echo "curl $*" >&2
exit 1
"#;

/// The stand-in `shasum` of [`pinned_php`]: 64 zeros for whatever
/// file stands, as `shasum -a 256` prints one.
const SHASUM: &str = r#"#!/bin/sh
[ "$#" = 3 ] && [ "$1" = -a ] && [ "$2" = 256 ] && [ -f "$3" ] &&
    echo "0000000000000000000000000000000000000000000000000000000000000000  $3" && exit 0
echo "shasum $*" >&2
exit 1
"#;

/// `scripts/ci/pinned-php.sh` over the pin of `root` as `status --section
/// oracle --format json` renders it, on a machine of stand-ins whose
/// Homebrew prefix is `<root>/brew` ([`BREW`], [`CURL`], [`SHASUM`]), and
/// what it printed on each stream. So nothing here pours, fetches or pins
/// a keg of this machine, and what it asked of `brew` and `curl`
/// outside a dry run is `<root>/brew/calls`.
fn pinned_php(root: &Path, dry: bool) -> (serde_json::Value, String, String) {
    let section = ["--section", "oracle", "--format", "json"];
    let rendered = status_with_pin(root, &section, None, None).ok();
    let json: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let pin = root.join("oracle.json");
    std::fs::write(&pin, rendered).unwrap();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join("brew")).unwrap();
    let prefix = root.join("brew").display().to_string();
    for (name, body) in [("brew", BREW), ("curl", CURL), ("shasum", SHASUM)] {
        std::fs::write(bin.join(name), body.replace("@prefix@", &prefix)).unwrap();
        xtask::scratch::executable(&bin.join(name)).unwrap();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)));
    let mut script = std::process::Command::new("bash");
    script
        .arg(workspace().join("scripts/ci/pinned-php.sh"))
        .arg(&pin)
        .env("PATH", path.unwrap())
        .env_remove("GH_TOKEN");
    if dry {
        script.env("DRY_RUN", "1");
    } else {
        script.env_remove("DRY_RUN");
    }
    let out = script.output().unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    assert!(out.status.success(), "{stdout}{stderr}");
    (json, stdout, stderr)
}

/// A scratch workspace whose pin is `fixture` with its Cellar under the
/// stand-in prefix `<root>/brew`, in place of `/nowhere`, and the one keg
/// of its `[closure]` linked there at its record, poured from
/// homebrew-core as its receipt says, so `scripts/ci/pinned-php.sh` finds
/// that one poured and php wherever the test lays it.
fn scratch_kegged(name: &str, fixture: &str) -> Tree {
    let root = scratch_poured(name, fixture);
    let prefix = root.join("brew");
    let pin = std::fs::read_to_string(root.join("oracle.toml")).unwrap();
    let cellar = prefix.join("Cellar").display().to_string();
    let pin = pin.replace("/nowhere/Cellar", &cellar);
    std::fs::write(root.join("oracle.toml"), pin).unwrap();
    std::fs::create_dir_all(prefix.join("opt")).unwrap();
    receipt(&prefix.join("Cellar/zstd/1.5.7_1"), "homebrew/core");
    std::os::unix::fs::symlink("../Cellar/zstd/1.5.7_1", prefix.join("opt/zstd")).unwrap();
    root
}

/// The receipt brew writes in a keg it pours, `INSTALL_RECEIPT.json`,
/// naming the tap the formula was poured from, which a pin names the
/// formula by.
fn receipt(keg: &Path, tap: &str) {
    std::fs::create_dir_all(keg).unwrap();
    let json = serde_json::json!({ "source": { "tap": tap } }).to_string();
    std::fs::write(keg.join("INSTALL_RECEIPT.json"), json).unwrap();
}

/// What a runner pours php from: the section names the formula
/// beside the keg path, `php` for a pin that names none and `php@8.5` for
/// one that names it, and the bottle the path names, `8.5.13_1` where the
/// keg is the formula's first revision and the pin's version says 8.5.13;
/// and `scripts/ci/pinned-php.sh` under `DRY_RUN=1` says it would pour
/// that formula at that bottle and the pin's path, after the one keg of
/// the fixture's `[closure]`. The dry run asks `brew` for its prefix and
/// nothing else, and fetches nothing ([`pinned_php`]). No keg stands
/// under the fixtures' `/nowhere`, so nothing here reads this machine's
/// Cellar.
#[test]
fn oracle_names_the_formula_a_runner_pours_and_the_dry_run_says_it() {
    for (fixture, formula, bottle, php) in [
        (
            "php.toml",
            "php",
            "8.5.12",
            "/nowhere/Cellar/php/8.5.12/bin/php",
        ),
        (
            "line.toml",
            "php@8.5",
            "8.5.13",
            "/nowhere/Cellar/php@8.5/8.5.13/bin/php",
        ),
        (
            "revised.toml",
            "php@8.5",
            "8.5.13_1",
            "/nowhere/Cellar/php@8.5/8.5.13_1/bin/php",
        ),
    ] {
        let root = scratch_poured(&format!("oracle-poured-{fixture}"), fixture);
        let (json, stdout, stderr) = pinned_php(&root, true);
        assert_eq!(json["binary"]["formula"], formula, "{fixture}");
        assert_eq!(json["binary"]["bottle"], bottle, "{fixture}");
        assert_eq!(
            stdout,
            format!(
                "zstd: would pour 1.5.7_1 in place of nothing\n{formula}: would pour {bottle} at {php}\n"
            ),
            "{fixture}: {stderr}"
        );
        assert_eq!(stderr, "", "{fixture}");
    }
    // The negative control: the paths under `Cellar/php@8.5/` and no
    // formula named. The section renders none of it, so a runner is
    // handed no pin to pour `php` from into a keg the paths do not name.
    let root = scratch_poured("oracle-poured-moved", "moved.toml");
    let section = ["--section", "oracle", "--format", "json"];
    let refused = status_with_pin(&root, &section, None, None).failed();
    assert!(
        refused.contains(
            "[oracle].path /nowhere/Cellar/php@8.5/8.5.13/bin/php stands in the keg php@8.5, and the pin is poured from the formula php,"
        ),
        "{refused}"
    );
    // So is a pin whose `[recipe.env]` still exports php from the keg
    // before: no runner pours a pin that would run two.
    let root = scratch_poured("oracle-poured-exported", "exported.toml");
    let refused = status_with_pin(&root, &section, None, None).failed();
    assert!(
        refused.contains(
            "[recipe.env].TEST_PHP_EXECUTABLE /nowhere/Cellar/php/8.5.12/bin/php stands in the keg php, and the pin is poured from the formula php@8.5,"
        ),
        "{refused}"
    );
}

/// php's own keg is `brew pin`ned as each keg of `[closure]` is,
/// on a machine where it is already poured too: the fixture pin's keg
/// stands under the stand-in prefix at the pin's SHA-256, and the one keg of
/// its `[closure]` at its record, so `scripts/ci/pinned-php.sh` pours
/// neither, says so, and asks `brew` for two pins and nothing else, the
/// closure's keg and then the formula the pin names. Each is named as
/// brew names the formula its receipt says it was poured from:
/// `zstd`, homebrew-core's, bare, and php's by the tap, `dax/pin/php@8.5`,
/// where it was poured from the tap, so homebrew-core's alias `php@8.5`
/// of `php` cannot stand in for it; and bare where its receipt names
/// homebrew-core, as a php poured by `brew install php` is. Under
/// `DRY_RUN=1` it says the same and asks nothing ([`pinned_php`]).
#[test]
fn the_keg_of_the_pinned_php_is_brew_pinned_where_it_is_already_poured() {
    let root = scratch_kegged("oracle-pinned", "revised.toml");
    let prefix = root.join("brew");
    let keg = prefix.join("Cellar/php@8.5/8.5.13_1");
    let php = keg.join("bin/php");
    std::fs::create_dir_all(php.parent().unwrap()).unwrap();
    std::fs::write(&php, "").unwrap();
    receipt(&keg, "dax/pin");
    let poured = format!(
        "zstd 1.5.7_1: the record's; already poured\n{}: {}, the pin's; already poured\n",
        php.display(),
        held(&root)
    );
    let calls = prefix.join("calls");
    let (_, stdout, stderr) = pinned_php(&root, true);
    assert_eq!((stdout.as_str(), stderr.as_str()), (poured.as_str(), ""));
    assert!(!calls.exists(), "the dry run asked brew or curl for more");
    let (json, stdout, stderr) = pinned_php(&root, false);
    assert_eq!(json["binary"]["formula"], "php@8.5");
    assert_eq!((stdout.as_str(), stderr.as_str()), (poured.as_str(), ""));
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap(),
        "brew pin zstd\nbrew pin dax/pin/php@8.5\n"
    );
    std::fs::remove_file(&calls).unwrap();
    receipt(&keg, "homebrew/core");
    let (_, stdout, stderr) = pinned_php(&root, false);
    assert_eq!((stdout.as_str(), stderr.as_str()), (poured.as_str(), ""));
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap(),
        "brew pin zstd\nbrew pin php@8.5\n"
    );
}

/// The pour itself, which a dry run stops short of: on a machine
/// where php is not poured, `scripts/ci/pinned-php.sh` asks GitHub for
/// the commits of the formula the pin names and no other, takes the one
/// that bottles the keg the pin's path names, an `update` for the revised
/// bottle of `revised.toml` and the formula's `add` for the first bottle
/// of `line.toml`, writes that formula into the local tap with
/// homebrew/core's packages as its `root_url`, and unpins, uninstalls,
/// installs and pins it, in that order. Each call names the keg so that
/// homebrew-core's alias `php@8.5` of `php` cannot stand in for it:
/// the unpin, the install and the pin name the tap's formula,
/// `dax/pin/php@8.5`, and the uninstall, which takes every keg of the
/// name whichever tap poured it, is asked by the bare name only where a
/// keg of that name stands, `8.5.13` before the revised bottle, since
/// brew finds the rack of a name before any alias and resolves a name
/// with no rack through the aliases. The pin of homebrew-core's `php`,
/// which brew's own unpin after that uninstall drops through the alias,
/// is put back by its bare name. The stand-in `brew` lands the keg where
/// the formula and the commit it was handed say ([`BREW`]), so a pour
/// of another formula or another bottle misses the pin's path and the
/// SHA-256 that follows the pour stops it. Before the install the pour drops
/// Homebrew's cached choice of curl, `<cache>/tool-selection/curl`, which
/// a keg the uninstall took can leave naming a curl that cannot start.
/// The formula the tap holds drops homebrew-core's `no_autobump!`, which
/// Homebrew refuses in any other tap.
/// The pour is macOS's, `sed -i ''` in it, so
/// this runs where the oracle runner's `cargo test -p xtask` does.
#[cfg(target_os = "macos")]
#[test]
fn the_formula_and_the_bottle_the_pin_names_are_what_a_runner_pours() {
    const COMMITS: &str = r#"[
        {"sha": "bumped", "commit": {"message": "php@8.5 8.5.13"}},
        {"sha": "bottled-8.5.13_1", "commit": {"message": "php@8.5: update 8.5.13_1 bottle."}},
        {"sha": "bottled-8.5.13", "commit": {"message": "php@8.5: add 8.5.13 bottle."}}
    ]"#;
    for (fixture, bottle, before) in [
        ("revised.toml", "8.5.13_1", Some("8.5.13")),
        ("line.toml", "8.5.13", None),
    ] {
        let root = scratch_kegged(&format!("oracle-pour-{fixture}"), fixture);
        let prefix = root.join("brew");
        std::fs::create_dir_all(prefix.join("api")).unwrap();
        std::fs::write(prefix.join("api/php@8.5.json"), COMMITS).unwrap();
        // homebrew-core's php stands pinned beside it, the keg the alias
        // names.
        receipt(&prefix.join("Cellar/php/8.5.12"), "homebrew/core");
        std::os::unix::fs::symlink("../Cellar/php/8.5.12", prefix.join("opt/php")).unwrap();
        let pinned = prefix.join("var/homebrew/pinned");
        std::fs::create_dir_all(&pinned).unwrap();
        std::os::unix::fs::symlink("../../../Cellar/php/8.5.12", pinned.join("php")).unwrap();
        let uninstall = match before {
            Some(at) => {
                receipt(&prefix.join("Cellar/php@8.5").join(at), "dax/pin");
                "brew uninstall --ignore-dependencies --force php@8.5\nbrew pin php\n"
            }
            None => "",
        };
        let php = prefix.join(format!("Cellar/php@8.5/{bottle}/bin/php"));
        let php = php.display();
        let calls = prefix.join("calls");
        let chosen = prefix.join("cache/tool-selection/curl");
        std::fs::create_dir_all(chosen.parent().unwrap()).unwrap();
        std::fs::write(&chosen, "brewed").unwrap();
        let (_, stdout, stderr) = pinned_php(&root, true);
        let would = format!(
            "zstd 1.5.7_1: the record's; already poured\nphp@8.5: would pour {bottle} at {php}\n"
        );
        assert_eq!((stdout.as_str(), stderr.as_str()), (would.as_str(), ""));
        assert!(!calls.exists(), "{fixture}: the dry run asked for more");
        assert!(
            chosen.exists(),
            "{fixture}: the dry run dropped the choice of curl"
        );
        let (_, stdout, stderr) = pinned_php(&root, false);
        let poured = format!(
            "zstd 1.5.7_1: the record's; already poured\n\
             php@8.5 {bottle} from homebrew-core bottled-{bottle}\n\
             {php}: {}, the pin's\n",
            held(&root)
        );
        assert_eq!((stdout.as_str(), stderr.as_str()), (poured.as_str(), ""));
        let file = prefix.join("tap/Formula/php@8.5.rb");
        let asked = format!(
            "brew pin zstd\n\
             curl -sSfL https://api.github.com/repos/Homebrew/homebrew-core/commits?path=Formula/p/php@8.5.rb&per_page=100\n\
             brew tap\n\
             brew tap-new --no-git dax/pin\n\
             brew --repository dax/pin\n\
             curl -sSfL -o {} https://raw.githubusercontent.com/Homebrew/homebrew-core/bottled-{bottle}/Formula/p/php@8.5.rb\n\
             brew unpin dax/pin/php@8.5\n\
             {uninstall}\
             brew install --ignore-dependencies dax/pin/php@8.5\n\
             brew pin dax/pin/php@8.5\n",
            file.display()
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), asked, "{fixture}");
        assert!(
            !chosen.exists(),
            "{fixture}: brew's choice of curl outlived the pour"
        );
        let formula = std::fs::read_to_string(&file).unwrap();
        assert!(
            formula.contains("  bottle do\n    root_url \"https://ghcr.io/v2/homebrew/core\"\n"),
            "{fixture}: {formula}"
        );
        assert!(!formula.contains("no_autobump!"), "{fixture}: {formula}");
    }
}

/// The pin's tag and commit as `[source]` states them, beside its
/// path: the section names the pin whole, which the corpus section clones
/// at, so no reader parses `oracle.toml` for either.
#[test]
fn oracle_states_the_pins_tag_and_commit() {
    let root = scratch("oracle-source");
    let source = pin().source;
    let json = oracle_json(&root, None, None);
    assert_eq!(json["pin"]["tag"], source.tag.as_str());
    assert_eq!(json["pin"]["commit"], source.commit.as_str());
    let md = status_with_pin(&root, &["--section", "oracle"], None, None).ok();
    assert!(
        md.contains(&format!("| tag |  | {} | the pin's |", source.tag)),
        "{md}"
    );
    assert!(
        md.contains(&format!("| commit |  | {} | the pin's |", source.commit)),
        "{md}"
    );
}

/// The corpus CI checks out: the pin's tag, commit and count of
/// `.phpt` files, the sparse patterns of the corpus's roots and of php-src's
/// own runner, which `phpt --upstream` runs out of that checkout, and a cache
/// key of the commit and a digest of the patterns, the same on every call,
/// where the targets under the pin are kept, and the hosts the lane
/// runs a test through, which CI builds; the Markdown carries the
/// same facts, and nothing is written.
#[test]
fn corpus_states_the_pin_the_patterns_and_the_key_they_make() {
    let root = scratch("corpus");
    let source = pin().source;
    let json = status(&root, &["--section", "corpus", "--format", "json"]).ok();
    let corpus: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(corpus["tag"], source.tag.as_str());
    assert_eq!(corpus["commit"], source.commit.as_str());
    assert_eq!(corpus["phpt_files"], source.phpt_files);
    let patterns: Vec<&str> = corpus["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    let roots = ["/Zend/tests/", "/tests/", "/ext/*/", "/sapi/*/tests/"];
    for pattern in roots.into_iter().chain(["/run-tests.php"]) {
        assert!(patterns.contains(&pattern), "{pattern}: {patterns:?}");
    }
    let key = corpus["key"].as_str().unwrap();
    let digest = key
        .strip_prefix(&format!("php-src-{}-", source.commit))
        .unwrap_or_else(|| panic!("{key} names the commit"));
    assert!(
        digest.len() == 12 && digest.chars().all(|c| c.is_ascii_hexdigit()),
        "{key}"
    );
    let pin = pin();
    let variants = pin
        .variant
        .keys()
        .map(|name| pin.variant(name).unwrap().oracle.sha256)
        .filter(|digest| *digest != Sha256::default());
    let kept: Vec<String> = std::iter::once(pin.oracle.sha256)
        .chain(variants)
        .map(|digest| digest.key())
        .map(|key| format!("target/harness/phpt-runs/pin-{}-{key}", pin.source.tag))
        .collect();
    assert_eq!(
        corpus["targets"],
        serde_json::json!(kept),
        "the targets are kept under the pin's tag and each oracle's digest"
    );
    // The hosts the lane hands a test to, the binary under test
    // first: each `sapi/*` crate of the workspace, by the package and the
    // binary its own Cargo.toml names, which CI builds and spells nowhere.
    let hosts: Vec<(String, String, String)> = corpus["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let field = |k: &str| h[k].as_str().unwrap().to_owned();
            (field("crate"), field("package"), field("binary"))
        })
        .collect();
    assert_eq!(
        hosts.first().map(|h| h.0.as_str()),
        Some("sapi/cli"),
        "the binary under test first: {hosts:?}"
    );
    for (krate, package, binary) in &hosts {
        let cargo: toml::Table = toml::from_str(
            &std::fs::read_to_string(workspace().join(krate).join("Cargo.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cargo["package"]["name"].as_str(), Some(package.as_str()));
        let bins = cargo["bin"].as_array().unwrap();
        assert!(
            bins.iter()
                .any(|b| b["name"].as_str() == Some(binary.as_str())),
            "{krate} builds {binary}"
        );
    }
    let again = status(&root, &["--section", "corpus", "--format", "json"]).stdout;
    assert_eq!(again, json, "the key is the patterns', not the call's");

    let md = status(&root, &["--section", "corpus"]).ok();
    assert!(md.contains(&format!("| key | {key} |")), "{md}");
    assert!(md.contains("/sapi/*/tests/"), "{md}");
    assert!(
        md.contains(&format!(
            "| hosts | {} |",
            hosts
                .iter()
                .map(|h| h.1.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )),
        "{md}"
    );
    assert!(
        !root.join("target").exists(),
        "status ran nothing and wrote nothing"
    );
}

/// A php-src file a notice under `LICENSES/` is held to is a pattern of
/// the corpus after the roots, so the sparse checkout CI makes holds the
/// lines the licence check compares; the key moves with the patterns, and
/// this workspace's own table names files no root holds.
#[test]
fn corpus_takes_the_files_the_notices_under_licenses_are_held_to() {
    let root = scratch("corpus-notices");
    let corpus = |root: &Path| -> serde_json::Value {
        let json = status(root, &["--section", "corpus", "--format", "json"]).ok();
        serde_json::from_str(&json).unwrap()
    };
    let holds = |corpus: &serde_json::Value, pattern: &str| {
        let patterns = corpus["patterns"].as_array().unwrap();
        patterns.iter().any(|p| p.as_str() == Some(pattern))
    };
    let before = corpus(&root);
    assert!(!holds(&before, "/Zend/zend_strtod.c"), "{before}");
    let ports = "## 3. Ports\n\n\
        | component | where in this project | origin | licence | notice text |\n\
        |---|---|---|---|---|\n\
        | dtoa | `crates/a/src/strtod.rs` | `Zend/zend_strtod.c` | MIT | `LICENSES/dtoa.txt` (`Zend/zend_strtod.c:1-18`) |\n";
    std::fs::write(root.join("THIRD-PARTY.md"), ports).unwrap();
    let after = corpus(&root);
    assert!(holds(&after, "/Zend/zend_strtod.c"), "{after}");
    assert_eq!(
        after["patterns"].as_array().unwrap().last(),
        Some(&serde_json::json!("/Zend/zend_strtod.c")),
        "after the roots: {after}"
    );
    assert_ne!(before["key"], after["key"], "the key is the patterns'");
    let own = corpus(workspace());
    for pattern in ["/README.REDIST.BINS", "/main/php_glob.c"] {
        assert!(holds(&own, pattern), "{pattern}: {own}");
    }
}

#[test]
fn a_section_that_has_no_renderer_yet_is_refused() {
    let root = scratch("unknown");
    let err = status(&root, &["--section", "gates"]).failed();
    assert!(err.contains("gates"), "{err}");
}

/// A Markdown row is what a session scans at start, eight tables under
/// the engine's cap on one block: its message is one line cut at
/// 120 characters and a number at zero is left out. What the row leaves
/// out is the same section's JSON, the reading whole.
#[test]
fn a_row_clips_its_message_and_leaves_zeros_out_and_the_json_has_them() {
    let root = scratch("clipped");
    let store = Store::of_workspace(&root);
    let mut phpt = Reading::new("phpt", "ext/dom");
    phpt.status = Status::Fail;
    phpt.numbers.insert("failed".into(), 691.into());
    phpt.numbers.insert("passed".into(), 0.into());
    phpt.numbers.insert("percent".into(), 0.0.into());
    let names = (0..40).map(|n| format!("t{n:03}.phpt")).collect::<Vec<_>>();
    phpt.message = format!("ext/dom: 691 of 871 missed:\n{}", names.join(", "));
    store.record(std::slice::from_ref(&phpt)).unwrap();

    let md = status(&root, &["--section", "phpt"]).ok();
    let row = md.lines().find(|l| l.starts_with("| ext/dom |")).unwrap();
    let cells: Vec<&str> = row.trim_matches('|').split(" | ").collect();
    assert_eq!(cells[2], "failed=691", "{row}");
    let message = cells[5].trim_end();
    assert_eq!(message.chars().count(), 120, "{message}");
    assert!(
        message.starts_with("691 of 871 missed: t000.phpt"),
        "{message}"
    );
    assert!(
        message.ends_with('…') && !message.contains("t039"),
        "{message}"
    );

    let json = status(&root, &["--section", "phpt", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let reading = &rows[0]["reading"];
    assert_eq!(reading["message"], phpt.message.as_str());
    assert_eq!(reading["numbers"]["passed"], 0);
    assert_eq!(reading["numbers"]["percent"], 0.0);
}

/// The week's ticket closes, from the stores alone: a close is
/// written into the main worktree's store by the merge that lands it, and
/// a withdrawal into the checkout's where its session ran, so a worktree
/// counts both. A ready record, a read, a refused close and a close older
/// than the week count nothing.
#[test]
fn tickets_counts_the_weeks_closes_in_the_main_worktrees_store_and_the_checkouts() {
    let main = scratch("tickets");
    let repo = Repo::init(&main);
    repo.git(&["commit", "-q", "--allow-empty", "-m", "first"]);
    repo.git(&["worktree", "add", "-q", "-b", "t1", "wt"]);
    let checkout = main.join("wt");
    for file in ["seams.toml", "oracle.toml", "docs/divergence.toml"] {
        std::fs::create_dir_all(checkout.join(file).parent().unwrap()).unwrap();
        std::fs::copy(main.join(file), checkout.join(file)).unwrap();
    }
    let now = xtask::reading::now().unwrap();
    let ticket = |scope: &str, status: Status, numbers: &[(&str, i64)], at: &str| {
        let mut r = Reading::new("ticket", scope);
        r.status = status;
        r.at = if at.is_empty() {
            now
        } else {
            at.parse().unwrap()
        };
        for (key, n) in numbers {
            r.numbers.insert((*key).to_owned(), (*n).into());
        }
        r
    };
    Store::of_workspace(&main)
        .record(&[
            ticket("1", Status::Pass, &[("opens", 2), ("withdrawn", 0)], ""),
            ticket("2", Status::Pass, &[("opens", 1), ("withdrawn", 0)], ""),
            ticket("3", Status::Pass, &[("opens", 4)], ""),
            ticket("4", Status::Fail, &[("opens", 3), ("withdrawn", 0)], ""),
            ticket(
                "5",
                Status::Pass,
                &[("opens", 9), ("withdrawn", 0)],
                "2020-01-01T00:00:00Z",
            ),
        ])
        .unwrap();
    Store::of_workspace(&checkout)
        .record(&[
            ticket("6", Status::Pass, &[("opens", 0), ("withdrawn", 1)], ""),
            ticket("7", Status::Pass, &[("criteria", 2)], ""),
        ])
        .unwrap();

    let md = status(&checkout, &["--section", "tickets"]).ok();
    // The tracker's line follows, unknown where the scratch has no
    // GitHub to ask.
    assert_eq!(
        md,
        "the last 7 days: 3 closes, 1 of them withdrawals, opened 3 tickets, 1.00 per close\n\
         on the tracker: unknown: GitHub could not be asked\n"
    );
    let json = status(&checkout, &["--section", "tickets", "--format", "json"]).ok();
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["closes"], 3);
    assert_eq!(json["withdrawn"], 1);
    assert_eq!(json["opens"], 3);
    assert_eq!(json["opens_per_close"], 1.0);
    // The main worktree reads its own store once, and has no withdrawal.
    let md = status(&main, &["--section", "tickets"]).ok();
    assert!(
        md.contains(": 2 closes, 0 of them withdrawals, opened 3 tickets"),
        "{md}"
    );
    let err = status(&main, &["--section", "tickets", "--tracked"]).failed();
    assert!(err.contains("--tracked renders the tree alone"), "{err}");
}

/// `git` is volatile: the log and the status of the working tree.
#[test]
fn git_prints_the_log_and_the_status() {
    let root = scratch("git");
    Repo::init(&root).git(&["commit", "-q", "--allow-empty", "-m", "the first commit"]);
    std::fs::write(root.join("untracked.txt"), "x").unwrap();
    let md = status(&root, &["--section", "git"]).ok();
    assert!(md.contains("the first commit"), "{md}");
    assert!(md.contains("?? untracked.txt"), "{md}");
}

/// A working tree with more changed paths than the section prints keeps
/// twenty of them, `STATUS_LINES` in `status.rs`, below the branch's line
/// and names how many it left out; the JSON keeps the same lines with the
/// number beside them.
#[test]
fn git_cuts_the_status_at_twenty_paths_and_names_the_rest() {
    let root = scratch("git-cut");
    Repo::init(&root).git(&["commit", "-q", "--allow-empty", "-m", "cut"]);
    for i in 0..40 {
        std::fs::write(root.join(format!("untracked-{i:02}.txt")), "x").unwrap();
    }
    // The scratch's own seams.toml, oracle.toml and docs/ are untracked too.
    let md = status(&root, &["--section", "git"]).ok();
    let paths = md.lines().filter(|l| l.starts_with("?? ")).count();
    assert_eq!(paths, 20, "{md}");
    assert!(
        md.contains("\n… and 23 more; git status --short\n```"),
        "{md}"
    );
    let json = status(&root, &["--section", "git", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["status"].as_array().map(Vec::len), Some(21), "{json}");
    assert_eq!(value["status_omitted"], 23, "{json}");
}

/// A track's reading of a subset is the one at its tip or its nearest
/// ancestor on the track. A branch cut behind the tip, whose worktree's
/// log `worktree retire` appends into the track's store after the track's
/// own readings, stands for nothing, and neither does a reading of an
/// older commit written last; the JSON the dashboard's data step reads
/// says the same, with the commit each reading was taken at.
#[test]
fn phpt_takes_the_reading_at_the_track_tip_and_never_a_retired_branchs() {
    let root = scratch("chain");
    let repo = Repo::init_on(&root, "t-x");
    repo.git(&["commit", "-q", "--allow-empty", "-m", "cut"]);
    let cut = repo.git(&["rev-parse", "--short", "HEAD"]);
    repo.git(&["switch", "-q", "-c", "behind"]);
    repo.git(&["commit", "-q", "--allow-empty", "-m", "the branch"]);
    let branch = repo.git(&["rev-parse", "--short", "HEAD"]);
    repo.git(&["switch", "-q", "t-x"]);
    repo.git(&["commit", "-q", "--allow-empty", "-m", "the tip"]);
    let tip = repo.git(&["rev-parse", "--short", "HEAD"]);

    let phpt = |scope: &str, commit: &str, at: &str, passed: i64| {
        let mut r = Reading::new("phpt", scope);
        r.status = Status::Fail;
        r.commit = commit.into();
        r.at = at.parse().unwrap();
        r.numbers.insert("passed".into(), passed.into());
        r
    };
    let track = Store::of_workspace(&root);
    let worktree = Store::at(root.join("worktree-store"));
    track
        .record(&[phpt("ext/date", &tip, "2026-09-24T16:44:00Z", 613)])
        .unwrap();
    track
        .record(&[phpt("ext/date", &cut, "2026-09-24T16:50:00Z", 400)])
        .unwrap();
    worktree
        .record(&[
            phpt("ext/date", &branch, "2026-09-24T17:35:00Z", 0),
            phpt("ext/spl", &branch, "2026-09-24T17:36:00Z", 1),
        ])
        .unwrap();
    assert_eq!(track.land(&worktree).unwrap(), 2);

    let json = status(&root, &["--section", "phpt", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the branch's ext/spl stands for nothing: {json}"
    );
    assert_eq!(rows[0]["scope"], "ext/date");
    assert_eq!(rows[0]["reading"]["commit"], tip.as_str(), "{json}");
    assert_eq!(rows[0]["reading"]["numbers"]["passed"], 613, "{json}");
    let md = status(&root, &["--section", "phpt"]).ok();
    assert!(md.contains("passed=613") && !md.contains("ext/spl"), "{md}");
}

/// `--every --log` renders every phpt reading of a log it is handed, one
/// versioned line each, in the log's order (harness-design.md §4.6):
/// a whole line with a field this build does not know, a lean row
/// of the published history, a torn line counted on stderr and skipped, a
/// scope outside the grammar, and the other instruments' lines left out.
#[test]
fn every_phpt_reading_of_a_log_handed_in_is_one_versioned_line() {
    let root = scratch("every-log");
    let fixture = xtask::package_dir().join("fixtures/status-phpt");
    let log = fixture.join("log.jsonl");
    let expected = std::fs::read_to_string(fixture.join("expected.jsonl")).unwrap();
    let every = ["--section", "phpt", "--format", "jsonl", "--every", "--log"];

    let ran = status(&root, &[&every[..], &[log.to_str().unwrap()]].concat());
    assert_eq!(ran.code(), 0, "{ran}");
    let lines = ran.stdout;
    let err = ran.stderr;
    assert_eq!(lines, expected);
    assert!(err.contains("1 line unreadable at 4"), "{err}");
    for line in lines.lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["v"], 1, "{line}");
    }

    let text = std::fs::read_to_string(&log).unwrap();
    let piped = status_with_stdin(&root, &[&every[..], &["-"]].concat(), &text).ok();
    assert_eq!(piped, expected, "the standard input reads as the file does");

    let err = status(&root, &[&every[..], &["no-such-log.jsonl"]].concat()).failed();
    assert!(err.contains("no-such-log.jsonl"), "{err}");
}

/// `--every --log` renders every budget, api-check (`locks` by its other
/// name) and witness reading of
/// a log it is handed as one versioned line each, in the log's order,
/// as it does a phpt reading: a whole line, a lean row of the
/// published history, a torn line counted on stderr and skipped, and the
/// other instruments' lines left out. Each fixture's `expected.jsonl` is
/// what the dashboard's tests read as the rows xtask renders.
#[test]
fn every_budget_api_check_and_witness_reading_of_a_log_handed_in_is_one_versioned_line() {
    let root = scratch("every-sections");
    for (section, instrument, torn) in [
        ("budget", "budget", 8),
        ("api-check", "api-check", 6),
        ("witness", "witness", 5),
    ] {
        let fixture = xtask::package_dir().join(format!("fixtures/status-{section}"));
        let log = fixture.join("log.jsonl");
        let expected = std::fs::read_to_string(fixture.join("expected.jsonl")).unwrap();
        let every = [
            "--section",
            section,
            "--format",
            "jsonl",
            "--every",
            "--log",
        ];

        let ran = status(&root, &[&every[..], &[log.to_str().unwrap()]].concat());
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let lines = ran.stdout;
        let err = ran.stderr;
        assert_eq!(lines, expected, "{section}");
        assert!(
            err.contains(&format!("1 line unreadable at {torn}")),
            "{section}: {err}"
        );
        for line in lines.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                (&row["v"], &row["instrument"]),
                (&serde_json::json!(1), &serde_json::json!(instrument)),
                "{line}"
            );
        }

        let text = std::fs::read_to_string(&log).unwrap();
        let ran = status_with_stdin(&root, &[&every[..], &["-"]].concat(), &text);
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let piped = ran.stdout;
        assert_eq!(
            piped, expected,
            "{section}: the standard input reads as the file does"
        );
    }
}

/// Without `--log`, `--every` renders the store's own log, every reading
/// and none picked, where the section without it renders the one that
/// stands, as it did; and each flag is refused where it means nothing.
#[test]
fn every_renders_the_stores_log_and_the_section_without_it_is_unchanged() {
    let root = scratch("every-store");
    let store = Store::of_workspace(&root);
    let phpt = |commit: &str, at: &str, passed: i64| {
        let mut r = Reading::new("phpt", "ext/date");
        r.status = Status::Pass;
        r.commit = commit.into();
        r.at = at.parse().unwrap();
        r.numbers.insert("passed".into(), passed.into());
        r.numbers.insert("tests".into(), passed.into());
        r.numbers.insert("run".into(), passed.into());
        r
    };
    store
        .record(&[
            phpt("", "2026-09-24T16:44:00Z", 400),
            phpt("", "2026-09-24T16:50:00Z", 613),
        ])
        .unwrap();

    let lines = status(
        &root,
        &["--section", "phpt", "--format", "jsonl", "--every"],
    )
    .ok();
    let passed: Vec<i64> = lines
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["p"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert_eq!(passed, [400, 613], "{lines}");

    let json = status(&root, &["--section", "phpt", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1, "{json}");
    assert_eq!(rows[0]["reading"]["numbers"]["passed"], 613, "{json}");

    for (args, says) in [
        (&["--section", "phpt", "--format", "jsonl"][..], "--every"),
        (&["--section", "phpt", "--every"][..], "--format jsonl"),
        (
            &["--section", "test", "--format", "jsonl", "--every"][..],
            "test",
        ),
        (&["--section", "phpt", "--log", "-"][..], "--every"),
    ] {
        let err = status(&root, args).failed();
        assert!(err.contains(says), "{args:?}: {err}");
    }
}

/// `--log --run <id>` lists every reading the run named wrote, whatever
/// its instrument, in the log's order, and no other run's and none that
/// names no run: as the line each instrument prints, or under `--format
/// jsonl` each reading whole; and from a log handed in as from the store's.
#[test]
fn log_run_lists_that_runs_readings_and_no_other() {
    let root = scratch("log-run");
    let store = Store::of_workspace(&root);
    let of = |instrument: &str, scope: &str, run: Option<&str>, minute: u32| {
        let mut r = Reading::new(instrument, scope);
        r.status = Status::Pass;
        r.run = run.map(str::to_owned);
        r.message = format!("{scope} at {minute}");
        r.at = format!("2026-11-21T10:{minute:02}:00Z").parse().unwrap();
        r
    };
    store
        .record(&[
            of("phpt", "php-src", Some("r-1"), 1),
            of("budget", "xtask", Some("r-2"), 2),
            of("phpt", "ext/date", Some("r-1"), 3),
            of("budget", "ext/pcre", None, 4),
            of("budget", "ext/json", Some("r-1"), 5),
        ])
        .unwrap();
    let lines = "pass phpt php-src: php-src at 1\n\
                 pass phpt ext/date: ext/date at 3\n\
                 pass budget ext/json: ext/json at 5\n";
    assert_eq!(status(&root, &["--log", "--run", "r-1"]).ok(), lines);
    assert_eq!(
        status(&root, &["--run", "r-2"]).ok(),
        "pass budget xtask: xtask at 2\n"
    );
    assert_eq!(status(&root, &["--log", "--run", "r-3"]).ok(), "");

    let jsonl = status(&root, &["--log", "--run", "r-1", "--format", "jsonl"]).ok();
    let scopes: Vec<String> = (jsonl.lines())
        .map(|l| Reading::from_json(l).unwrap())
        .inspect(|r| assert_eq!(r.run.as_deref(), Some("r-1"), "{jsonl}"))
        .map(|r| r.scope)
        .collect();
    assert_eq!(scopes, ["php-src", "ext/date", "ext/json"], "{jsonl}");

    let log = root.join("handed.jsonl");
    let text = std::fs::read_to_string(root.join("target/harness/log/phpt.jsonl")).unwrap();
    std::fs::write(&log, text).unwrap();
    let handed = status(&root, &["--log", log.to_str().unwrap(), "--run", "r-1"]).ok();
    assert_eq!(
        handed,
        "pass phpt php-src: php-src at 1\npass phpt ext/date: ext/date at 3\n"
    );
}

/// A scratch workspace whose store holds the phpt readings the parity
/// tests read, and whose pin's path is `pin`.
fn parity_store(name: &str, pin: &Path) -> Tree {
    let root = scratch_with_pin_path(name, pin);
    // The section's rows are the tracked manifest's crates and subsets.
    std::fs::copy(workspace().join("seams.toml"), root.join("seams.toml")).unwrap();
    let phpt = |scope: &str, counts: &[(&str, i64)]| {
        let mut r = Reading::new("phpt", scope);
        r.status = Status::Pass;
        r.at = "2026-10-03T12:00:00Z".parse().unwrap();
        for (key, n) in counts {
            r.numbers.insert((*key).into(), (*n).into());
        }
        r
    };
    let held = |n: i64, met: i64| {
        [
            ("tests", n + 10),
            ("run", n + 10),
            ("target", n),
            ("met", met),
            ("outside", 10),
        ]
    };
    Store::of_workspace(&root)
        .record(&[
            phpt("Zend/tests", &held(100, 90)),
            phpt(
                "ext/ctype",
                &[&held(48, 47)[..], &[("predated", 2)]].concat(),
            ),
            phpt("ext/json", &[("tests", 88), ("run", 88), ("passed", 88)]),
            phpt("ext/ctype@control", &[("tests", 50), ("run", 50)]),
            phpt("ext/ctype@against", &[("held", 47)]),
            phpt("ext/ctype@force", &held(1000, 1000)),
            phpt("ext/standard/tests/serialize", &held(500, 500)),
            phpt("ext/pgsql", &held(3, 3)),
            phpt("ext/enchant", &held(20, 20)),
            phpt("sapi/cli", &held(99, 99)),
            // A second host's run of tests other suites count: the HTTP-served
            // lane under its host's name, whose row owns no subset.
            phpt("sapi/makala", &held(267, 260)),
            phpt("ext/lexbor", &[("tests", 0), ("run", 0)]),
            phpt(
                "ext/pcre",
                &[("tests", 165), ("run", 0), ("engine_absent", 1)],
            ),
        ])
        .unwrap();
    root
}

/// A pin's path where no tree stands, so a parity test reads as it would
/// on a machine without the pinned php-src.
const NO_TREE: &str = "/nowhere/pinned/php-src";

/// `--session` is the map plus the local facts in one block, for the
/// session-start hook; the local file is laid down from its template when
/// the clone has none, and an unrendered region is said to be one.
#[test]
fn session_prints_the_map_and_the_local_facts_in_one_block() {
    let root = scratch("session");
    std::fs::write(
        root.join("AGENTS.md"),
        "# The map

Read CONTEXT.md first.
",
    )
    .unwrap();
    let local = root.join(".claude/harness.local.md");
    assert!(!local.exists());

    let text = status(&root, &["--session"]).ok();
    assert!(
        text.starts_with(
            "# The map
"
        ),
        "{text}"
    );
    assert!(
        local.exists(),
        "the local file is laid down from its template"
    );
    let template = std::fs::read_to_string(&local).unwrap();
    for name in ["budget", "phpt", "witness", "port-map", "divergence", "git"] {
        assert!(
            template.contains(&format!("--section {name}\" volatile name={name}")),
            "{name} is a volatile region of the local file: {template}"
        );
    }
    for name in ["mutants", "permits"] {
        assert!(
            !template.contains(&format!("name={name}")),
            "{name} is rendered by name, not in the local file: {template}"
        );
    }
    assert!(text.contains("unrendered"), "{text}");
    assert!(text.contains("computed trust"), "{text}");

    std::fs::write(&local, rendered(&template)).unwrap();
    let text = status(&root, &["--session"]).ok();
    assert!(
        text.contains(
            "# The map
"
        ) && text.contains("| crate |"),
        "{text}"
    );
    assert!(!text.contains("unrendered"), "{text}");

    // As JSON, the two blocks the function hook hands the engine:
    // the map as the file is, the facts with the note that they are
    // unrendered; the Markdown is the two joined.
    std::fs::write(&local, &template).unwrap();
    let json = status(&root, &["--session", "--format", "json"]).ok();
    let blocks: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(blocks["map"], "# The map\n\nRead CONTEXT.md first.\n");
    let facts = blocks["facts"].as_str().unwrap();
    assert!(facts.starts_with("# Local facts\n"), "{facts}");
    assert!(facts.contains("unrendered"), "{facts}");
    let text = status(&root, &["--session"]).stdout;
    assert_eq!(
        text,
        format!("{}\n{facts}", blocks["map"].as_str().unwrap())
    );

    let ran = status(&root, &["--session", "--section", "rules"]);
    assert_ne!(ran.code(), 0, "one of --section and --session: {ran}");
    let ran = status(&root, &[]);
    assert_ne!(
        ran.code(),
        0,
        "one of --section and --session is required: {ran}"
    );
}

/// A local file as `computed` renders one: each region's body between its
/// opener and a closer carrying sums.
fn rendered(template: &str) -> String {
    template.replace(
        "<!-- /computed -->\n",
        "\n| crate |\n\n<!-- /computed in=a out=b -->\n",
    )
}

/// A clone keeps its local file across sessions, so a template that moved,
/// a section dropped or added, reached only a clone that had none: the
/// main checkout carried `mutants` and lacked `port-map` long after both
/// changed (harness revision 180). `--session` lays the file down again
/// whenever what was laid differs from the template, and leaves a rendering
/// of the template as it stands.
#[test]
fn session_lays_the_local_file_down_again_when_the_template_moved() {
    let root = scratch("session-relay");
    std::fs::write(root.join("AGENTS.md"), "# The map\n").unwrap();
    let local = root.join(".claude/harness.local.md");
    status(&root, &["--session"]).ok();
    let template = std::fs::read_to_string(&local).unwrap();

    let current = rendered(&template);
    std::fs::write(&local, &current).unwrap();
    let text = status(&root, &["--session"]).ok();
    assert_eq!(std::fs::read_to_string(&local).unwrap(), current);
    assert!(
        text.contains("| crate |") && !text.contains("unrendered"),
        "{text}"
    );

    let older = current.replace(
        "\n## git\n",
        "\n## mutants\n\n<!-- computed exec cmd=\"cargo xtask status --section mutants\" volatile name=mutants | do not edit; run computed -->\n\n| crate |\n\n<!-- /computed in=a out=b -->\n\n## git\n",
    );
    assert_ne!(older, current);
    std::fs::write(&local, &older).unwrap();
    let text = status(&root, &["--session"]).ok();
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        template,
        "a file laid from an older template is laid down again"
    );
    assert!(text.contains("unrendered"), "{text}");
    assert!(!text.contains("name=mutants"), "{text}");
}

/// permit judges by three rules of its own beside the manifest's, the
/// region body, a build apart from the tree and a call no candidate
/// judged, and the section carries a row for each: through the outage of
/// harness revision 11 nothing under `permits` said the judge was down, and
/// a rule with no row is a rule nothing renders.
///
/// The two refusals are counted in two columns. A build that hands
/// the call to the candidate that rebuilds is a skip under `handed on`,
/// since the call was judged; a call that reached the end of the walk
/// unjudged is a failure under `denials`, since it was not. Counting both
/// as denials is what buried the 23 real ones under 5687 walks.
#[test]
fn permits_separates_the_calls_denied_from_the_calls_handed_to_the_next_candidate() {
    let root = scratch("permit-own-rules");
    let store = Store::of_workspace(&root);
    let session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11";

    let mut handed_on = Reading::new("permit", "permit.stale-build");
    handed_on.rule = "permit.stale-build".into();
    handed_on.status = Status::Skip;
    handed_on.message =
        "Edit seams.toml: this build did not judge this call and handed it to the next candidate."
            .into();
    handed_on.session = session.into();
    store.record(std::slice::from_ref(&handed_on)).unwrap();

    // A diff judged at pre-commit is that caller's, never an agent's
    // denial.
    let mut at_pre_commit = Reading::new("permit", "pre-commit/computed.region-body");
    at_pre_commit.rule = "computed.region-body".into();
    at_pre_commit.status = Status::Fail;
    at_pre_commit.session = "pre-commit".into();
    store.record(std::slice::from_ref(&at_pre_commit)).unwrap();

    let mut unjudged = Reading::new("permit", "permit.unjudged-call");
    unjudged.rule = "permit.unjudged-call".into();
    unjudged.status = Status::Fail;
    unjudged.message = "Edit seams.toml: this call was not judged.".into();
    unjudged.session = session.into();
    store.record(std::slice::from_ref(&unjudged)).unwrap();

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = |id: &str| {
        value["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["rule"] == id)
            .unwrap_or_else(|| panic!("no row for {id}:\n{json}"))
            .clone()
    };
    let stale = row("permit.stale-build");
    assert_eq!(stale["denials"], 0, "a walk is not a denial: {stale}");
    assert_eq!(stale["handed_on"], 1, "it is a hand-on: {stale}");
    let walked_past = row("permit.unjudged-call");
    assert_eq!(walked_past["denials"], 1, "{walked_past}");
    assert_eq!(walked_past["handed_on"], 0, "{walked_past}");
    row("computed.region-body");
    let person = row("computed.region-body");
    assert_eq!(person["denials"], 0, "{person}");
    assert_eq!(person["at_pre_commit"], 1, "{person}");

    let md = status(&root, &["--section", "permits"]).ok();
    assert!(
        md.contains("| denials | judged | at pre-commit | handed on |"),
        "the two are two columns:\n{md}"
    );
    let line = md
        .lines()
        .find(|l| l.contains("permit.stale-build"))
        .unwrap_or_else(|| panic!("no row for the refusal:\n{md}"));
    assert!(line.contains("next candidate"), "and says so: {line}");
}

/// The proposed `git.pathspec` row, appended to the scratch manifest
/// verbatim: the file parses with it, and `permits` carries the rule's
/// row, so a denial is visible where the facts are. Once the tracked
/// manifest carries the row this proves the same of it.
#[test]
fn permits_carries_a_row_for_the_proposed_git_pathspec_rule() {
    let root = scratch("permit-pathspec-row");
    Tracked::of(&root)
        .with_rule("git.pathspec", |rule| {
            if !rule.contains_key("tool") {
                rule.extend(
                    r#"
tool = "Bash"
with = ["git add", "git commit"]
require_argv = "--"
text = "An agent stages and commits by pathspec, `git add -- <paths>` and `git commit -F <file> -- <paths>`: the index is shared with every session in the checkout."
remedy = "`git add -- <paths>` and `git commit -F <file> -- <paths>`"
"#
                    .parse::<toml::Table>()
                    .unwrap(),
                );
            }
        })
        .write(&root);
    let store = Store::of_workspace(&root);
    let mut denial = Reading::new("permit", "git add -A && git commit -m x");
    denial.rule = "git.pathspec".into();
    denial.status = Status::Fail;
    denial.message = "An agent stages and commits by pathspec".into();
    denial.session = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11".into();
    store.record(std::slice::from_ref(&denial)).unwrap();

    let md = status(&root, &["--section", "permits"]).ok();
    let row = md
        .lines()
        .find(|l| l.contains("git.pathspec"))
        .unwrap_or_else(|| panic!("no row for the rule:\n{md}"));
    assert!(row.contains("| 1 |"), "the denial is counted: {row}");
}

/// The bench's reading is a measurement whose exit is not its status
/// (harness-design.md §4.4), and the section renders it as before:
/// a `fail` against the bar is one reading and one failure of `permit`
/// in the instruments table, and no rule's row, since it judged none.
#[test]
fn permits_renders_the_bench_reading_as_before() {
    let root = scratch("bench-row");
    let store = Store::of_workspace(&root);
    let mut bench = Reading::new("permit", "xtask");
    bench.status = Status::Fail;
    bench.numbers.insert("p99_ms".into(), 67.0.into());
    bench.numbers.insert("bar_ms".into(), 100.0.into());
    bench.message = "permit at the tool call: p99 67.0 ms against the bar of 100 ms".into();
    store
        .record_measurement(std::slice::from_ref(&bench))
        .unwrap();

    let md = status(&root, &["--section", "permits"]).ok();
    let row = md
        .lines()
        .find(|l| l.starts_with("| permit |"))
        .unwrap_or_else(|| panic!("no line for permit:\n{md}"));
    assert!(row.contains("| 1 | 1 |"), "one reading, one failure: {row}");
    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let permit = value["instruments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["instrument"] == "permit")
        .unwrap()
        .clone();
    assert_eq!(permit["readings"], 1);
    assert_eq!(permit["failures"], 1);
    assert!(
        value["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["denials"] == 0),
        "a missed bar is no denial: {json}"
    );
}

/// The query behind CI's §7 section (harness-design.md): readings,
/// failures, the last reading and the last failure per instrument from the
/// log. An instrument read within fourteen days with no failure in them is
/// a candidate for silent; one whose fixture failed within the window is
/// not, nor one with no reading in it, nor `usage`, whose reading cannot
/// fail.
#[test]
fn permits_lists_the_instruments_and_the_candidates_for_silent() {
    let root = scratch("silent");
    let store = Store::of_workspace(&root);
    // The first rendering is without the window, the second with it.
    let window = |days: Option<i64>| {
        let table = days.map(|n| toml::Table::from_iter([("silent_days".to_owned(), n.into())]));
        Tracked::of(&root)
            .with_table("telemetry", table)
            .write(&root);
    };
    window(None);

    let mut budget = Reading::new("budget", "php_heap");
    budget.status = Status::Pass;
    store.record(std::slice::from_ref(&budget)).unwrap();
    budget.at = xtask::reading::now().unwrap();
    store.record(std::slice::from_ref(&budget)).unwrap();

    let mut vocab = Reading::new("vocab", "php_heap");
    vocab.status = Status::Pass;
    store.record(std::slice::from_ref(&vocab)).unwrap();
    vocab.scope = "fixture".into();
    vocab.status = Status::Fail;
    vocab.at = xtask::reading::now().unwrap();
    store.record(std::slice::from_ref(&vocab)).unwrap();

    let mut old = Reading::new("clippy", "php_heap");
    old.status = Status::Fail;
    store.record(std::slice::from_ref(&old)).unwrap();
    old.status = Status::Pass;
    old.at = xtask::reading::now().unwrap();
    store.record(std::slice::from_ref(&old)).unwrap();

    let mut quiet = Reading::new("test", "php_heap");
    quiet.status = Status::Pass;
    store.record(std::slice::from_ref(&quiet)).unwrap();

    let mut usage = Reading::new("usage", "3619");
    usage.status = Status::Pass;
    usage.at = xtask::reading::now().unwrap();
    store.record(std::slice::from_ref(&usage)).unwrap();

    let md = status(&root, &["--section", "permits"]).ok();
    assert!(
        md.contains("| last reading | last failure |") && md.contains("silent_days"),
        "no window in the manifest: no candidates, and the key is named: {md}"
    );
    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["instruments"][0]["silent"], serde_json::Value::Null);

    window(Some(14));
    let md = status(&root, &["--section", "permits"]).ok();
    let row = |name: &str| {
        md.lines()
            .find(|l| l.starts_with(&format!("| {name} |")))
            .unwrap_or_else(|| panic!("{name}: {md}"))
            .to_owned()
    };
    assert!(row("budget").contains("| 2 | 0 |"), "{}", row("budget"));
    assert!(row("vocab").contains("| 2 | 1 |"), "{}", row("vocab"));
    assert!(
        row("clippy").contains("| 2 | 1 | 20") && row("clippy").contains("| 1970-01-01"),
        "the last reading is today's and the last failure the epoch's: {}",
        row("clippy")
    );
    let named = md
        .lines()
        .find(|l| l.starts_with("The candidates for silent"))
        .unwrap_or_else(|| panic!("{md}"));
    assert!(
        named.contains(": budget, clippy. `usage` is never one"),
        "a failure at the epoch is not one in the window, and an instrument unread in it is not quiet: {named}"
    );

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let instruments = value["instruments"].as_array().unwrap();
    let of = |name: &str| {
        instruments
            .iter()
            .find(|r| r["instrument"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(of("budget")["readings"], 2);
    assert_eq!(of("budget")["failures"], 0);
    assert_eq!(of("budget")["silent"], true);
    assert_eq!(of("vocab")["failures"], 1);
    assert_eq!(of("vocab")["silent"], false);
    assert_eq!(of("test")["silent"], false);
    assert_eq!(of("usage")["silent"], serde_json::Value::Null);
}

/// The weekly bar table is gone: `--section week` and
/// `--week` are refused as unknown, and a manifest's `[[week]]` row as a
/// key outside the grammar.
#[test]
fn the_week_section_and_its_row_are_refused() {
    let root = scratch("week");
    let err = status(&root, &["--section", "week"]).failed();
    assert!(err.contains("invalid value 'week'"), "{err}");
    let err = status(&root, &["--section", "rules", "--week", "1"]).failed();
    assert!(err.contains("unexpected argument '--week'"), "{err}");
    let week = toml::Table::from_iter([("days".to_owned(), "3-7".into())]);
    let text = Tracked::of(&root).with_table("week", Some(week)).text();
    let err = format!("{:#}", Manifest::parse(&text).unwrap_err());
    assert!(err.contains("unknown field `week`"), "{err}");
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red.
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
#[path = "tracked/status/mod.rs"]
mod tracked;
