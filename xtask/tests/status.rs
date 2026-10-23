//! `xtask status`, the one renderer (harness-design.md §4.6): it reads the
//! manifest and the store and computes nothing else.
#![cfg(tracked)]

use std::path::Path;

use xtask::manifest::Manifest;
use xtask::oracle::{PROMISE_VARIABLE, Platform, TREE_VARIABLE};
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::{Tracked, pin};
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
/// `oracle.toml` with that one line rewritten in the copy, the tracked
/// file untouched.
fn scratch_with_pin_path(name: &str, path: &Path) -> Tree {
    let root = scratch(name);
    let pin = root.join("oracle.toml");
    let mut in_source = false;
    let text: Vec<String> = std::fs::read_to_string(&pin)
        .unwrap()
        .lines()
        .map(|line| {
            if line.starts_with('[') {
                in_source = line.trim() == "[source]";
            }
            if in_source && line.starts_with("path = ") {
                format!("path = {:?}", path.display().to_string())
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(&pin, text.join("\n") + "\n").unwrap();
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

/// The `oracle` section (#36): the pin as this machine has it, the
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

/// The pinned php a runner pours (#2253): its version, keg path and md5
/// as the pin states them, and each variant's build for this machine a
/// runner lays (#2298), so the oracle job reads the pin off this section
/// and never parses `oracle.toml` a second time.
#[test]
fn oracle_states_the_pinned_php_a_runner_pours() {
    let root = scratch("oracle-binary");
    let pin = pin();
    let oracle = pin.oracle.clone();
    let path = oracle.path.display().to_string();
    let json = oracle_json(&root, None, None);
    assert_eq!(json["binary"]["version"], oracle.version.as_str());
    assert_eq!(json["binary"]["path"], path.as_str());
    assert_eq!(json["binary"]["md5"], oracle.md5.as_str());
    for name in pin.variant.keys() {
        let built = pin.variant(name).unwrap().oracle;
        let laid = &json["variants"][name.as_str()];
        if built.md5.is_empty() {
            assert!(laid.is_null(), "{name} pins no build here: {laid}");
            continue;
        }
        assert_eq!(laid["md5"], built.md5.as_str(), "{name}");
        assert_eq!(laid["path"], built.path.to_str().unwrap(), "{name}");
        assert_eq!(laid["platform"], Platform::this().unwrap().key(), "{name}");
        let pinned = pin.variant[name].build(Platform::this().unwrap()).unwrap();
        let closure = pinned.provenance.closure.as_ref().map(ToString::to_string);
        assert_eq!(laid["closure"], serde_json::json!(closure), "{name}");
    }
    // The record's digest, SHA-256 over each `name=version\n` in name
    // order (#2935), computed here from the kegs the section names.
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
        .filter(|n| !pin.variant(n).unwrap().oracle.md5.is_empty());
    assert_eq!(
        json["variants"].as_object().unwrap().len(),
        pinned_here.count()
    );
    let md = status_with_pin(&root, &["--section", "oracle"], None, None).ok();
    assert!(
        md.contains(&format!(
            "| binary |  | {path} | php {}, md5 {} |",
            oracle.version, oracle.md5
        )),
        "{md}"
    );
    for (name, laid) in json["variants"].as_object().unwrap() {
        let row = md
            .lines()
            .find(|l| l.starts_with(&format!("| variant {name} |")));
        let md5 = format!("md5 {}", laid["md5"].as_str().unwrap());
        let state = match laid["closure"].as_str() {
            Some(closure) => format!("{md5}, closure {closure} |"),
            None => format!("{md5} |"),
        };
        assert!(row.is_some_and(|row| row.ends_with(&state)), "{md}");
    }
}

/// The pin's tag and commit as `[source]` states them (#66), beside its
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

/// The corpus CI checks out (#1713): the pin's tag, commit and count of
/// `.phpt` files, the sparse patterns of the corpus's roots, and a cache
/// key of the commit and a digest of the patterns, the same on every call,
/// where the targets under the pin are kept (#1733), and the hosts the lane
/// runs a test through, which CI builds (#1908); the Markdown carries the
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
    for pattern in ["/Zend/tests/", "/tests/", "/ext/*/", "/sapi/*/tests/"] {
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
        .map(|name| pin.variant(name).unwrap().oracle.md5);
    let kept: Vec<String> = std::iter::once(pin.oracle.md5.clone())
        .chain(variants.filter(|md5| !md5.is_empty()))
        .map(|md5| format!("target/harness/phpt-runs/pin-{}-{md5}", pin.source.tag))
        .collect();
    assert_eq!(
        corpus["targets"],
        serde_json::json!(kept),
        "the targets are kept under the pin's tag and each oracle's digest"
    );
    // The hosts the lane hands a test to (#1908), the binary under test
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

#[test]
fn a_section_that_has_no_renderer_yet_is_refused() {
    let root = scratch("unknown");
    let err = status(&root, &["--section", "gates"]).failed();
    assert!(err.contains("gates"), "{err}");
}

/// A Markdown row is what a session scans at start, eight tables under
/// the engine's cap on one block (#820): its message is one line cut at
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

/// The week's ticket closes (#1157), from the stores alone: a close is
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
    assert_eq!(
        md,
        "the last 7 days: 3 closes, 1 of them withdrawals, opened 3 tickets, 1.00 per close\n"
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

/// #822: a working tree with more changed paths than the section prints
/// keeps twenty of them, `STATUS_LINES` in `status.rs`, below the
/// branch's line and names how many it left out; the JSON keeps the same
/// lines with the number beside them.
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

/// #787: a track's reading of a subset is the one at its tip or its
/// nearest ancestor on the track. A branch cut behind the tip, whose
/// worktree's log `worktree retire` appends into the track's store after
/// the track's own readings, stands for nothing, and neither does a
/// reading of an older commit written last; the JSON the dashboard's data
/// step reads says the same, with the commit each reading was taken at.
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
/// versioned line each, in the log's order (harness-design.md §4.6,
/// #1745): a whole line with a field this build does not know, a lean row
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
/// a log it is handed as one versioned line each, in the log's order
/// (#1763), as it does a phpt reading: a whole line, a lean row of the
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
            // lane under its host's name, whose row owns no subset (#2334).
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
    for name in [
        "budget",
        "phpt",
        "witness",
        "mutants",
        "divergence",
        "permits",
        "git",
    ] {
        assert!(
            template.contains(&format!("--section {name}\" volatile name={name}")),
            "{name} is a volatile region of the local file: {template}"
        );
    }
    assert!(text.contains("unrendered"), "{text}");
    assert!(text.contains("computed trust"), "{text}");

    std::fs::write(
        &local,
        "# Local\n\n<!-- computed exec cmd=\"x\" volatile name=budget -->\n| crate |\n<!-- /computed in=a out=b -->\n",
    )
    .unwrap();
    let text = status(&root, &["--session"]).ok();
    assert!(
        text.contains(
            "# The map
"
        ) && text.contains("| crate |"),
        "{text}"
    );
    assert!(!text.contains("unrendered"), "{text}");

    // As JSON, the two blocks the function hook hands the engine (#64):
    // the map as the file is, the facts with the note that they are
    // unrendered; the Markdown is the two joined.
    std::fs::write(&local, "# Local\n\n<!-- /computed -->\n").unwrap();
    let json = status(&root, &["--session", "--format", "json"]).ok();
    let blocks: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(blocks["map"], "# The map\n\nRead CONTEXT.md first.\n");
    let facts = blocks["facts"].as_str().unwrap();
    assert!(facts.starts_with("# Local\n"), "{facts}");
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

/// permit judges by three rules of its own beside the manifest's, the
/// region body, a build apart from the tree and a call no candidate
/// judged, and the section carries a row for each: through the outage of
/// #16 nothing under `permits` said the judge was down, and a rule with no
/// row is a rule nothing renders.
///
/// The two refusals are counted in two columns (#118). A build that hands
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

    let md = status(&root, &["--section", "permits"]).ok();
    assert!(
        md.contains("| denials | handed on |"),
        "the two are two columns:\n{md}"
    );
    let line = md
        .lines()
        .find(|l| l.contains("permit.stale-build"))
        .unwrap_or_else(|| panic!("no row for the refusal:\n{md}"));
    assert!(line.contains("next candidate"), "and says so: {line}");
}

/// The row #33 proposes, `git.pathspec`, appended to the scratch
/// manifest verbatim: the file parses with it, and `permits` carries the
/// rule's row, so a denial is visible where the facts are. Once the
/// tracked manifest carries the row this proves the same of it.
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
/// (harness-design.md §4.4, #61), and the section renders it as before:
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

/// The query behind CI's §7 section (harness-design.md): readings and
/// failures per instrument from the log, and an instrument with readings
/// but no failure in fourteen days is a candidate for silent; one whose
/// fixture failed within the window is not.
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

    let md = status(&root, &["--section", "permits"]).ok();
    assert!(
        !md.contains(" silent |") && md.contains("silent_days"),
        "no window in the manifest: no silent column, and the key is named: {md}"
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
    assert!(row("budget").contains("candidate"), "{}", row("budget"));
    assert!(row("vocab").contains("| 2 | 1 |"), "{}", row("vocab"));
    assert!(!row("vocab").contains("candidate"), "{}", row("vocab"));
    assert!(
        row("clippy").contains("candidate"),
        "a failure at the epoch is not one in the window: {}",
        row("clippy")
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
}

/// The weekly bar table is gone (#2934, #2973): `--section week` and
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
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
#[path = "tracked/status/mod.rs"]
mod tracked;
