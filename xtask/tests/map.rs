//! The context layer (harness-design.md §5): the map is short, its regions
//! are the ones the design names, the per-directory guidance and the data index have
//! their regions, `scripts/adr-index.sh` lists every ADR, the
//! `SessionStart` hook is wired for the four matchers, and the two blocks
//! the function hook hands the engine are each under its cap with room.

use std::fmt::Write;
use std::path::Path;
use std::process::Command;

use xtask::scratch::Tree;

mod common {
    pub mod repo;
}
use common::repo::scratch_workspace;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(workspace().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A file with regions as the index holds it, `git show :<path>`, never the
/// working copy: another session's `SessionStart` hook or pre-commit
/// rewrites a region in place while these tests run, and a read in the
/// middle of that write saw a bare closer that no commit held (#1322).
/// Under pre-commit it is the index the commit is written from (#2747).
fn staged(root: &Path, path: &str) -> String {
    xtask::test::committed(root, path).unwrap_or_else(|e| panic!("{e:#}"))
}

/// The map as committed, which a render in flight does not tear.
fn map() -> String {
    staged(workspace(), "AGENTS.md")
}

/// The reader is the index and not the disk: a scratch repository stages a
/// rendered map, then its working copy is caught half-way through a render,
/// the closer written and the body not yet, and what the tests read is the
/// staged map (#1322).
#[test]
fn the_map_is_read_from_the_index_and_not_from_a_render_in_flight() {
    let root = Tree::new("map-staged").unwrap();
    scratch_workspace(workspace(), &root);
    let rendered = "# map\n\n<!-- computed exec cmd=\"ls\" inputs=x name=x | do not edit; run computed -->\nx\n<!-- /computed in=1 out=2 -->\n";
    std::fs::write(root.join("AGENTS.md"), rendered).unwrap();
    let add = Command::new("git")
        .args(["add", "--", "AGENTS.md"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(add.status.success(), "{add:?}");
    std::fs::write(
        root.join("AGENTS.md"),
        "# map\n\n<!-- computed exec cmd=\"ls\" inputs=x name=x | do not edit; run computed -->\n<!-- /computed -->\n",
    )
    .unwrap();
    let read = staged(&root, "AGENTS.md");
    assert_eq!(read, rendered);
    assert!(read.lines().all(|l| l != "<!-- /computed -->"), "{read}");
}

/// A test under pre-commit inherits the commit's index and reads scratch
/// repositories as well: that index is read for its own repository alone,
/// and a scratch one is read from its own index (#2747).
#[test]
fn the_commit_s_index_is_read_for_its_own_repository_alone() {
    if let Some(root) = std::env::var_os("MAP_SCRATCH") {
        assert_eq!(staged(Path::new(&root), "only-here"), "x\n");
        return;
    }
    let root = Tree::new("map-foreign-index").unwrap();
    scratch_workspace(workspace(), &root);
    std::fs::write(root.join("only-here"), "x\n").unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    git(&root, &["add", "--", "only-here"]);
    let ours = git(
        workspace(),
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_commit_s_index_is_read_for_its_own_repository_alone",
        ])
        .env("MAP_SCRATCH", &*root)
        .env("XTASK_COMMIT_INDEX", ours.trim())
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stdout)
    );
}

/// The opener of a region by name, if the file has one.
fn opener<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines()
        .find(|l| l.starts_with("<!-- computed ") && l.contains(&format!(" name={name}")))
}

/// The ceiling of harness §5 holds what a person wrote, not what a region
/// rendered: `instruments` grows one line per instrument and `layout` one
/// per crate, and neither is the map being verbose.
fn prose(map: &str) -> usize {
    let mut inside = false;
    map.lines()
        .filter(|line| {
            if line.starts_with("<!-- computed ") {
                inside = true;
                return false;
            }
            if line.starts_with("<!-- /computed") {
                inside = false;
                return false;
            }
            !inside
        })
        .count()
}

#[test]
fn the_map_is_under_100_written_lines_with_the_regions_of_the_design() {
    let map = map();
    let lines = prose(&map);
    assert!(
        lines <= 100,
        "AGENTS.md is {lines} lines outside its computed regions; the map is at most 100 (harness §5)"
    );
    let regions = [
        ("design-of-record", "ls "),
        ("adrs", "scripts/adr-index.sh"),
        ("crates", "status --section crates"),
        ("seams", "status --section locks --format md"),
        ("instruments", "--help"),
        ("layout", "tree src=crates depth=1"),
    ];
    for (name, command) in regions {
        let line = opener(&map, name).unwrap_or_else(|| panic!("no region {name} in AGENTS.md"));
        assert!(line.contains(command), "{name}: {line}");
        assert!(
            line.contains(" inputs=") || line.starts_with("<!-- computed tree"),
            "{name} is checkable, not volatile: {line}"
        );
    }
    assert!(
        opener(&map, "layout-ext").is_some(),
        "the layout has ext beside crates"
    );
    for prose in [
        "CONTEXT.md",
        "third-engine-design.md",
        "docs/adr/",
        "xtask done",
        "seams.toml",
    ] {
        assert!(map.contains(prose), "the map's prose names {prose}");
    }
    assert!(
        map.lines().all(|l| l != "<!-- /computed -->"),
        "every region of the map is rendered"
    );
}

/// The ceiling reads the map's own lines and not a region's: a region that
/// grows by fifty lines is the harness growing, and only a hand-written
/// line past the ceiling fails (harness §5, ledger 12).
#[test]
fn the_ceiling_counts_what_a_person_wrote_and_not_what_a_region_rendered() {
    let map = map();
    let written = prose(&map);
    assert!(written < map.lines().count(), "the map has regions");

    let instruments = opener(&map, "instruments").expect("the map has the instruments region");
    let grown = map.replace(
        instruments,
        &format!("{instruments}\n{}", "one more instrument\n".repeat(50)),
    );
    assert_eq!(prose(&grown), written, "a region's growth is not the map's");

    let padded = format!("a line a person wrote\n{map}");
    assert_eq!(prose(&padded), written + 1);
}

/// The body of a region by name: the lines between its opener and closer.
fn region<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let mut lines = text.lines();
    let opener = format!(" name={name}");
    lines
        .by_ref()
        .find(|l| l.starts_with("<!-- computed ") && l.contains(&opener))
        .unwrap_or_else(|| panic!("no region {name}"));
    lines
        .take_while(|l| !l.starts_with("<!-- /computed"))
        .collect()
}

/// The files whose regions pre-commit renders, as its `regions=` line
/// names them: the one list, which CI's `computed check` repeats.
fn tracked() -> Vec<String> {
    let hook = read("scripts/hooks/pre-commit");
    let line = hook
        .lines()
        .find_map(|l| l.strip_prefix("regions='"))
        .expect("pre-commit names its region files as regions='...'");
    line.trim_end_matches('\'')
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Every tracked file with a region is one pre-commit renders and CI
/// holds with `computed check`, so no region's render waits on a session
/// that happens to run `computed` on its file (#631); a fixture's are the
/// harness's own tests'.
#[test]
fn pre_commit_renders_and_ci_checks_every_tracked_file_with_a_region() {
    let files = tracked();
    let ci = read(".github/workflows/harness.yml");
    let checked = ci
        .lines()
        .find_map(|l| l.trim().strip_prefix("- run: computed check "))
        .expect("CI runs computed check");
    assert_eq!(checked.split_whitespace().collect::<Vec<_>>(), files);
    let listed = Command::new("git")
        .args(["ls-files", "--", "*.md"])
        .current_dir(workspace())
        .output()
        .unwrap();
    for file in String::from_utf8(listed.stdout).unwrap().lines() {
        if file.starts_with("xtask/fixtures/") {
            continue;
        }
        let regions = staged(workspace(), file)
            .lines()
            .any(|l| l.starts_with("<!-- computed "));
        assert!(
            !regions || files.iter().any(|f| f == file),
            "{file} has a region that pre-commit does not render: {files:?}"
        );
    }
}

/// Every `xtask status` region of a tracked file, its file and its opener.
fn status_regions() -> Vec<(String, String)> {
    tracked()
        .into_iter()
        .flat_map(|file| {
            staged(workspace(), &file)
                .lines()
                .filter(|l| l.starts_with("<!-- computed exec cmd=\"cargo xtask status "))
                .map(|l| (file.clone(), l.to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The command of an `exec` opener, `cmd="..."`, as the arguments of xtask.
fn xtask_args(opener: &str) -> Vec<String> {
    let cmd = opener
        .split("cmd=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no cmd in {opener}"));
    cmd.strip_prefix("cargo xtask ")
        .unwrap_or_else(|| panic!("{cmd}"))
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// A scratch workspace holding what the tracked regions read: the tracked
/// locks, and each file a region names among its inputs as the tree holds
/// it, so a section that reads one, `invocation` the type's source,
/// renders it.
fn tracked_tree(root: &Path, regions: &[(String, String)]) {
    scratch_workspace(workspace(), root);
    let locks = Command::new("git")
        .args(["ls-files", "-z", "--", "*.lock"])
        .current_dir(workspace())
        .output()
        .unwrap();
    for lock in String::from_utf8(locks.stdout).unwrap().split('\0') {
        if lock.is_empty() || lock == "Cargo.lock" {
            continue;
        }
        let to = root.join(lock);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(workspace().join(lock), to).unwrap();
    }
    // An input is relative to its region's file.
    for (file, opener) in regions {
        for input in inputs(opener).filter(|i| !i.contains('*')) {
            let mut path = std::path::PathBuf::new();
            for part in Path::new(file).parent().unwrap().join(input).components() {
                match part {
                    std::path::Component::ParentDir => {
                        path.pop();
                    }
                    other => path.push(other),
                }
            }
            let to = root.join(&path);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::copy(workspace().join(&path), to).unwrap();
        }
    }
}

/// A tracked region renders against an empty store, every reading `no
/// reading`, since a store is one machine's and its render would churn
/// the map on every checkout; the live numbers are the local file's
/// (harness-design.md §5, revision 6; #630). Each `xtask status` region
/// is rendered over a scratch workspace with the tracked locks and the
/// files the regions read ([`tracked_tree`]), once with
/// no store and once after a reading for every seam, rule and bundle edge
/// is recorded: the bytes are the same, and no row carries a reading.
#[test]
fn every_status_region_renders_the_same_bytes_whatever_the_store_holds() {
    use xtask::manifest::Manifest;
    use xtask::reading::{Reading, Status, Store};

    let regions = status_regions();
    for name in ["crates", "seams", "bundle-order", "divergence", "edges"] {
        assert!(
            regions
                .iter()
                .any(|(_, l)| l.contains(&format!(" name={name} "))),
            "no xtask status region {name}"
        );
    }
    for (file, opener) in &regions {
        assert!(
            xtask_args(opener).iter().any(|a| a == "--tracked"),
            "{file}: a tracked region renders against the tree alone, with --tracked: {opener}"
        );
    }

    let root = Tree::new("map-tracked").unwrap();
    tracked_tree(&root, &regions);
    let render = || -> Vec<String> {
        regions
            .iter()
            .map(|(_, opener)| {
                let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
                    .arg("--root")
                    .arg(&*root)
                    .args(xtask_args(opener))
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{opener}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8(out.stdout).unwrap()
            })
            .collect()
    };
    let empty = render();

    let manifest = Manifest::load(&root).unwrap();
    let mut readings = Vec::new();
    let mut reading = |instrument: &str, scope: &str, rule: &str| {
        let mut r = Reading::new(instrument, scope);
        r.status = Status::Pass;
        r.rule = rule.to_owned();
        r.message = "a reading of this machine".to_owned();
        r.numbers.insert("items".into(), 9_999.into());
        r.numbers.insert("extensions".into(), 9_999.into());
        readings.push(r);
    };
    for seam in &manifest.seams {
        reading("api-check", &seam.name, "");
    }
    for rule in &manifest.rules {
        reading("permit", &rule.id, &rule.id);
    }
    for edge in &manifest.edges {
        reading("api-check", &edge.from, "");
    }
    Store::of_workspace(&root).record(&readings).unwrap();
    let full = render();

    for (((file, opener), empty), full) in regions.iter().zip(&empty).zip(&full) {
        assert_eq!(empty, full, "{file}: {opener}");
        assert!(
            !full.contains("| pass |") && !full.contains("9999"),
            "{file}: a row carries this machine's reading: {opener}\n{full}"
        );
    }
    let seams = regions
        .iter()
        .position(|(_, l)| l.contains(" name=seams "))
        .unwrap();
    // A tracked table has no `reading` column, which could only say
    // `no reading` there (#2377).
    assert!(
        full[seams].starts_with("| seam | lock | items | ceiling |\n")
            && !full[seams].contains("reading"),
        "{}",
        full[seams]
    );
}

/// The paths an opener's `inputs=` names, comma-separated.
fn inputs(opener: &str) -> impl Iterator<Item = &str> {
    opener
        .split(" inputs=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_default()
        .split(',')
        .filter(|i| !i.is_empty())
}

/// harness-design.md §4.3's fields of an `Invocation` are the type's
/// (#1305, #1319): the region renders `status --section invocation` and
/// names the type's source among its inputs, as `computed affected`
/// says, so a field the type gains, here added to a copy of the source
/// beside the staged design, fails `computed check` until the region is
/// rendered again, and the render reads the field.
#[test]
fn the_invocation_region_is_the_types_and_a_field_it_gains_stales_it() {
    const SOURCE: &str = "xtask/src/oracle/mod.rs";
    let design = staged(workspace(), "harness-design.md");
    let opener = opener(&design, "invocation").expect("§4.3 carries the invocation region");
    assert_eq!(
        xtask_args(opener),
        ["status", "--section", "invocation", "--tracked"]
    );
    assert!(inputs(opener).any(|i| i == SOURCE), "{opener}");

    let root = Tree::new("map-invocation").unwrap();
    let regions: Vec<(String, String)> = design
        .lines()
        .filter(|l| l.starts_with("<!-- computed exec "))
        .map(|l| ("harness-design.md".to_owned(), l.to_owned()))
        .collect();
    tracked_tree(&root, &regions);
    std::fs::write(root.join("harness-design.md"), &design).unwrap();
    let check = || {
        Command::new("computed")
            .args(["check", "harness-design.md"])
            .current_dir(&*root)
            .output()
            .expect("computed is on PATH, as CI installs it")
    };
    let fresh = check();
    assert!(
        fresh.status.success(),
        "the staged region is fresh: {}{}",
        String::from_utf8_lossy(&fresh.stdout),
        String::from_utf8_lossy(&fresh.stderr)
    );
    let out = Command::new("computed")
        .args(["affected", SOURCE])
        .current_dir(&*root)
        .output()
        .expect("computed is on PATH, as CI installs it");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.lines()
            .any(|l| l.starts_with("harness-design.md:") && l.contains(" invocation ")),
        "{SOURCE} is no input of the invocation region: {said}"
    );

    let source = read(SOURCE);
    let gained = source.replacen(
        "    pub raw: bool,\n",
        "    pub raw: bool,\n    /// A field the type gained.\n    ///\n    /// No section gives it.\n    pub gained: u8,\n",
        1,
    );
    assert_ne!(gained, source, "{SOURCE} declares `pub raw: bool`");
    std::fs::write(root.join(SOURCE), gained).unwrap();
    let stale = check();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&stale.stdout),
        String::from_utf8_lossy(&stale.stderr)
    );
    assert!(
        !stale.status.success() && said.contains("invocation") && said.contains("stale"),
        "a field the type gains stales the region until it is rendered: {said}"
    );
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(&*root)
        .args(xtask_args(opener))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rendered = String::from_utf8(out.stdout).unwrap();
    assert!(
        rendered.contains("| `gained` | `u8` | no section gives it | A field the type gained. |"),
        "{rendered}"
    );
    let body = region(&design, "invocation").join("\n");
    assert!(body.contains("| `raw` | `bool` |"), "{body}");
    assert!(!body.contains("gained"), "{body}");
}

/// The `seams` region names every lock its section counts among its
/// inputs, so a commit that changes only one of them, the harness's own
/// `xtask/xtask.lock` included, renders the region again (#631): asked of
/// `computed affected`, which reads the openers' `inputs=`.
#[test]
fn every_lock_the_seams_region_counts_is_one_of_its_inputs() {
    let locks = Command::new("git")
        .args(["ls-files", "--", "crates/*.lock", "xtask/xtask.lock"])
        .current_dir(workspace())
        .output()
        .unwrap();
    let locks = String::from_utf8(locks.stdout).unwrap();
    assert!(locks.lines().any(|l| l == "xtask/xtask.lock"), "{locks}");
    // The staged map alone, so a render in flight here is not what is read.
    let root = Tree::new("map-lock-inputs").unwrap();
    std::fs::write(root.join("AGENTS.md"), map()).unwrap();
    for lock in locks.lines() {
        let out = Command::new("computed")
            .args(["affected", lock])
            .current_dir(&*root)
            .output()
            .expect("computed is on PATH, as CI installs it");
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(
            said.lines()
                .any(|l| l.starts_with("AGENTS.md:") && l.contains(" seams ")),
            "{lock} is no input of the seams region: {said}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A help line says what the instrument does in one sentence and leaves
/// the rest to `xtask <name> --help` (#64): the `instruments` region is
/// the map's widest, and a line of it stays under 200 characters.
#[test]
fn every_help_line_of_the_instruments_region_is_under_200_characters() {
    let map = map();
    let lines = region(&map, "instruments");
    assert!(lines.len() > 10, "the region renders `xtask --help`");
    let widest = lines.iter().max_by_key(|l| l.len()).unwrap();
    assert!(
        widest.len() < 200,
        "an `xtask --help` line is {} characters; the summary is one sentence, the rest is the subcommand's own --help: {widest}",
        widest.len()
    );
}

#[test]
fn ext_and_data_have_their_regions() {
    let ext = staged(workspace(), "ext/AGENTS.md");
    assert!(opener(&ext, "extensions").is_some(), "{ext}");
    let bundle = opener(&ext, "bundle-order").unwrap();
    assert!(bundle.contains("status --section bundle"), "{bundle}");
    for rule in ["extension()", "Capture", "php-src/ext/"] {
        assert!(ext.contains(rule), "the template rules name {rule}");
    }
    let data = staged(workspace(), "data/INDEX.md");
    assert!(opener(&data, "files").is_some(), "{data}");
    for text in [&ext, &data] {
        assert!(text.lines().all(|l| l != "<!-- /computed -->"), "rendered");
    }
}

#[test]
fn the_adr_index_lists_every_adr_with_its_title_status_and_date() {
    let out = Command::new(workspace().join("scripts/adr-index.sh"))
        .current_dir(workspace())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let index = String::from_utf8(out.stdout).unwrap();
    let adrs: Vec<_> = std::fs::read_dir(workspace().join("docs/adr"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .collect();
    assert!(!adrs.is_empty());
    assert_eq!(index.lines().count(), adrs.len(), "{index}");
    assert!(
        index.contains(
            "ADR-0004 (accepted, 2026-09-05): Extensions are one crate each under `ext/`"
        ),
        "{index}"
    );
    let numbers: Vec<&str> = index.lines().map(|l| &l[2..10]).collect();
    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    assert_eq!(numbers, sorted, "in number order");
}

/// The function-hook module, the one place the engine's cap is stated.
const HOOK: &str = "harness-plugins/hooks/harness.ts";

/// The declarations the module is typechecked against: a tracked file
/// Claude Code writes (#62), whose first line names the build that wrote
/// it, so the version the module was held to is a sensed fact and not a
/// pin stated elsewhere; and the tsconfig beside it includes that
/// directory, not a gitignored one.
#[test]
fn the_committed_declarations_carry_the_version_that_wrote_them() {
    let header = read("harness-plugins/types/claude-code.d.ts");
    let first = header.lines().next().unwrap_or_default();
    let version = first
        .strip_prefix("// Written by Claude Code ")
        .and_then(|rest| rest.strip_suffix('.'))
        .unwrap_or_else(|| panic!("the header names no build: {first}"));
    assert!(
        version.split('.').count() == 3 && version.split('.').all(|n| n.parse::<u32>().is_ok()),
        "the version is three numbers: {first}"
    );
    let tsconfig: serde_json::Value =
        serde_json::from_str(&read("harness-plugins/tsconfig.json")).unwrap();
    let include = tsconfig["include"].as_array().unwrap();
    assert!(include.iter().any(|i| i == "types"), "{include:?}");
    assert!(
        !include
            .iter()
            .any(|i| i.as_str().unwrap_or_default().contains(".claude")),
        "{include:?}"
    );
}

/// The room a block keeps under the cap: what a day's growth of the
/// regions came to before the block was measured (#64), so a region that
/// crosses the line fails here and not a session.
const MARGIN: usize = 4_000;

/// The engine's cap on one context block, read from the line of the hook
/// that states it, `const BLOCK_CAP = 32_000;`, so the hook and this test
/// hold the same number and neither restates it.
fn block_cap() -> usize {
    let hook = read(HOOK);
    let line = hook
        .lines()
        .find(|l| l.starts_with("const BLOCK_CAP = "))
        .unwrap_or_else(|| panic!("{HOOK} states the cap as `const BLOCK_CAP = <n>;`"));
    let digits: String = line
        .trim_start_matches("const BLOCK_CAP = ")
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits.parse().unwrap_or_else(|e| panic!("{line}: {e}"))
}

/// `xtask status --session --format json` over a root: the two blocks.
fn session_blocks(root: &Path) -> (String, String) {
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .args(["--root"])
        .arg(root)
        .args(["status", "--session", "--format", "json"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        value["map"].as_str().unwrap().to_owned(),
        value["facts"].as_str().unwrap().to_owned(),
    )
}

/// The section of the local file rendered from this workspace's store,
/// as `computed run` renders it into the local file.
fn section(name: &str) -> String {
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .args(["--root"])
        .arg(workspace())
        .args(["status", "--section", name])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// What is wrong with a block against the cap, if anything.
fn over(name: &str, text: &str, cap: usize) -> Option<String> {
    (text.len() + MARGIN > cap).then(|| {
        format!(
            "the {name} block is {} characters, within {} of the engine's cap of {cap} (a block over it is skipped with the session's map); the room is {MARGIN}",
            text.len(),
            cap - text.len().min(cap)
        )
    })
}

/// The engine caps one context block at what `harness.ts` states, and a
/// session on 2026-09-10 started with no map because the map and the
/// local facts together were over it (#64). The map is one block and the
/// local facts another, and each is held here under the cap with room:
/// the map as the hook hands it, and the facts as the template's regions,
/// every one of them, render from this workspace's store, whatever the
/// local file here holds. A scratch map padded past the cap is what the
/// hold refuses.
#[test]
fn the_map_and_the_local_facts_are_each_under_the_engines_cap_with_room() {
    let cap = block_cap();
    assert!(cap > MARGIN * 2, "a cap of {cap} leaves room for a block");

    // The map as the hook hands it, from a scratch workspace holding the
    // staged map, so a render in flight here is not what is measured.
    let root = Tree::new("map-cap").unwrap();
    scratch_workspace(workspace(), &root);
    std::fs::write(root.join("AGENTS.md"), map()).unwrap();
    let (map, _) = session_blocks(&root);
    assert!(
        map.starts_with("# dax-php\n"),
        "{}",
        &map[..40.min(map.len())]
    );
    if let Some(why) = over("map", &map, cap) {
        panic!("{why}");
    }

    let padding = "a line a person wrote, one more of them\n".repeat(cap / 40 + 1);
    std::fs::write(root.join("AGENTS.md"), format!("{map}{padding}")).unwrap();
    let (padded, facts) = session_blocks(&root);
    assert!(padded.len() > cap, "the scratch map is over the cap");
    let why = over("map", &padded, cap).expect("a map padded past the cap is refused");
    assert!(why.contains("over it is skipped"), "{why}");
    assert!(
        over("facts", &facts, cap).is_none(),
        "the facts are their own block, unrendered here"
    );

    // The scratch workspace had no local file, so `--session` laid the
    // template down there: its head and its regions, in its order, are
    // what the local file renders, whatever the local file here holds.
    let template = std::fs::read_to_string(root.join(".claude/harness.local.md")).unwrap();
    let mut facts = template
        .lines()
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    let names: Vec<&str> = template
        .lines()
        .filter(|l| l.starts_with("<!-- computed "))
        .filter_map(|l| l.split(" name=").nth(1)?.split(' ').next())
        .collect();
    assert!(names.contains(&"port-map"), "{names:?}");
    for name in names {
        write!(
            facts,
            "\n\n## {name}\n\n<!-- computed -->\n{}\n<!-- /computed -->\n",
            section(name)
        )
        .ok();
    }
    // What `session()` appends after the local file: the week's ticket
    // closes in one line, from this workspace's stores (#1157).
    write!(facts, "\n## tickets\n\n{}", section("tickets")).ok();
    // And the rules this session fired, each with its count, and the
    // unreadable-log note, one line. Every rule of the permits section
    // with its count across sessions bounds the block: a session fires
    // no rule the section does not list, and no rule more often than the
    // log does.
    let mut this_session = String::from("| rule | this session |\n|---|---|\n");
    for row in section("permits").lines().skip(2) {
        let cells: Vec<&str> = row.split(" | ").collect();
        let [rule, _, _, _, denials, ..] = cells[..] else {
            break;
        };
        writeln!(this_session, "{rule} | {denials} |").ok();
    }
    assert!(
        this_session.contains("| harness.self | "),
        "the bound lists the permits rows: {this_session}"
    );
    write!(facts, "\n## permits this session\n\n{this_session}\n").ok();
    if let Some(why) = over("facts", &facts, cap) {
        panic!("{why}");
    }
}

/// The hook reads the same line the test does, and hands the engine the
/// map and the facts as two blocks, each held to the cap on its own.
#[test]
fn the_function_hook_states_the_cap_once_and_hands_over_two_blocks() {
    let hook = read(HOOK);
    assert_eq!(
        hook.matches("const BLOCK_CAP = ").count(),
        1,
        "the cap is stated once"
    );
    assert!(
        !hook.contains("32000") && !hook.contains("32 000"),
        "the number lives in BLOCK_CAP alone"
    );
    for needle in [
        "'--format', 'json'",
        "capped(BLOCKS.map, map)",
        "capped(BLOCKS.facts, facts)",
        "text.length <= BLOCK_CAP",
    ] {
        assert!(hook.contains(needle), "the hook has {needle}");
    }
}

#[test]
fn the_session_start_hook_wires_the_clone_and_renders_the_local_file_on_the_four_matchers() {
    let hooks: serde_json::Value =
        serde_json::from_str(&read("harness-plugins/hooks/hooks.json")).unwrap();
    let start = &hooks["hooks"]["SessionStart"][0];
    let matcher = start["matcher"].as_str().unwrap();
    for source in ["startup", "resume", "clear", "compact"] {
        assert!(
            matcher.split('|').any(|m| m == source),
            "{matcher} lacks {source}"
        );
    }
    let command = start["hooks"][0]["command"].as_str().unwrap();
    assert_eq!(start["hooks"][0]["type"], "command");
    // The clone's hooks path is pointed at scripts/hooks once, so pre-push
    // is wired without a person's step; a clone that set its own is kept.
    let hooks_path = command
        .find("git config core.hooksPath scripts/hooks")
        .expect(command);
    let computed = command
        .find("computed run .claude/harness.local.md")
        .expect(command);
    assert!(hooks_path < computed, "hooksPath is set first: {command}");
    // The local file is laid down when missing, before it is rendered; the
    // map itself reaches the model through the plugin's prompt.context
    // (harness-plugins/), so the hook ends with the render and prints no map.
    let laid = command.find("status --session >/dev/null").expect(command);
    assert!(
        laid < computed,
        "laid down before it is rendered: {command}"
    );
    assert!(
        !command.trim_end().ends_with("status --session"),
        "the map is the plugin's block, not the hook's stdout: {command}"
    );
    assert!(
        hooks["hooks"]["PreToolUse"].is_array(),
        "permit's hook stands"
    );
}

/// A scratch workspace holding `scripts/manifest-rules.sh` and the
/// `seams.toml` given, and what it printed for the ids named.
fn manifest_rules(manifest: &str, ids: &[&str]) -> std::process::Output {
    let root = Tree::new("manifest-rules").unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::copy(
        workspace().join("scripts/manifest-rules.sh"),
        root.join("scripts/manifest-rules.sh"),
    )
    .unwrap();
    std::fs::write(root.join("seams.toml"), manifest).unwrap();
    Command::new(root.join("scripts/manifest-rules.sh"))
        .args(ids)
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap()
}

const MANIFEST: &str = "\
[budget]
total = 1

[[rule]]
id = \"a.first\"
scope = \"ext/*\"   # a comment on the row
text = \"The first.\"
# a comment after the row's keys, still the row's

# a comment that heads the next row
[[rule]]
id = \"b.second\"
deny = true
text = \"The second.\"
";

#[test]
fn the_rule_excerpt_prints_each_named_row_verbatim_in_the_order_named() {
    let out = manifest_rules(MANIFEST, &["b.second", "a.first"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "[[rule]]\nid = \"b.second\"\ndeny = true\ntext = \"The second.\"\n\n\
         [[rule]]\nid = \"a.first\"\nscope = \"ext/*\"   # a comment on the row\n\
         text = \"The first.\"\n# a comment after the row's keys, still the row's\n"
    );
}

#[test]
fn the_rule_excerpt_fails_naming_an_id_the_manifest_has_no_row_for() {
    let out = manifest_rules(MANIFEST, &["a.first", "c.deleted", "a.firs"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "a failed render prints no excerpt");
    let said = String::from_utf8(out.stderr).unwrap();
    assert!(said.contains("\"c.deleted\""), "{said}");
    assert!(
        said.contains("\"a.firs\""),
        "a prefix of an id is not the id: {said}"
    );
}

#[test]
fn harness_design_s_rule_excerpt_is_rendered_from_the_manifest() {
    let design = staged(workspace(), "harness-design.md");
    let opener = opener(&design, "manifest-rules").unwrap();
    assert!(opener.contains("scripts/manifest-rules.sh "), "{opener}");
    assert!(
        opener.contains("inputs=seams.toml,scripts/manifest-rules.sh"),
        "{opener}"
    );
}
