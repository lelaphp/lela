//! The context layer (harness-design.md §5): the map is short, its regions
//! are the ones the design names, the per-directory guidance and the data index have
//! their regions, `scripts/adr-index.sh` lists every ADR, the
//! `SessionStart` hook is wired for the four matchers, and the two blocks
//! the function hook hands the engine are each under its cap with room.

use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(workspace().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
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
fn the_map_is_under_100_written_lines_with_the_six_regions_of_the_design() {
    let map = read("AGENTS.md");
    let lines = prose(&map);
    assert!(
        lines <= 100,
        "AGENTS.md is {lines} lines outside its computed regions; the map is at most 100 (harness §5)"
    );
    let regions = [
        ("design-of-record", "ls "),
        ("adrs", "scripts/adr-index.sh"),
        ("seams", "status --section locks --format md"),
        ("rules", "status --section rules --format md"),
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
    let map = read("AGENTS.md");
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

/// A help line says what the instrument does in one sentence and leaves
/// the rest to `xtask <name> --help` (#64): the `instruments` region is
/// the map's widest, and a line of it stays under 200 characters.
#[test]
fn every_help_line_of_the_instruments_region_is_under_200_characters() {
    let map = read("AGENTS.md");
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
    let ext = read("ext/AGENTS.md");
    assert!(opener(&ext, "extensions").is_some(), "{ext}");
    let bundle = opener(&ext, "bundle-order").unwrap();
    assert!(bundle.contains("status --section bundle"), "{bundle}");
    for rule in ["extension()", "Capture", "php-src/ext/"] {
        assert!(ext.contains(rule), "the template rules name {rule}");
    }
    let data = read("data/INDEX.md");
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
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
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
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
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
/// the map as the hook hands it, and the facts as the local file renders
/// from this workspace's store, whatever the local file here holds. A
/// scratch map padded past the cap is what the hold refuses.
#[test]
fn the_map_and_the_local_facts_are_each_under_the_engines_cap_with_room() {
    let cap = block_cap();
    assert!(cap > MARGIN * 2, "a cap of {cap} leaves room for a block");

    let (map, _) = session_blocks(workspace());
    assert!(
        map.starts_with("# dax-php\n"),
        "{}",
        &map[..40.min(map.len())]
    );
    if let Some(why) = over("map", &map, cap) {
        panic!("{why}");
    }

    let mut facts = read(".claude/harness.local.md")
        .lines()
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    for name in [
        "budget",
        "phpt",
        "witness",
        "mutants",
        "divergence",
        "permits",
        "git",
    ] {
        let _ = write!(
            facts,
            "\n\n## {name}\n\n<!-- computed -->\n{}\n<!-- /computed -->\n",
            section(name)
        );
    }
    // What `session()` appends after the local file: the rules this
    // session fired, a subset of the permits rows, so the whole section
    // once more bounds it; and the unreadable-log note, one line.
    let _ = write!(
        facts,
        "\n## permits this session\n\n{}\n",
        section("permits")
    );
    if let Some(why) = over("facts", &facts, cap) {
        panic!("{why}");
    }

    let root: PathBuf = std::env::temp_dir().join(format!("xtask-map-cap-{}", std::process::id()));
    scratch_workspace(workspace(), &root).unwrap();
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
