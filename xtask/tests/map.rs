//! The context layer (harness-design.md §5): the map is short, its regions
//! are the ones the design names, the local file's template is the seven
//! volatile sections, the per-directory guidance and the data index have
//! their regions, the ADR index script lists every ADR, and the
//! `SessionStart` hook is wired for the four matchers.

use std::path::Path;
use std::process::Command;

use xtask::status::{LOCAL_SECTIONS, local_template};

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

#[test]
fn the_map_is_under_150_lines_with_the_six_regions_of_the_design() {
    let map = read("AGENTS.md");
    let lines = map.lines().count();
    assert!(
        lines <= 150,
        "AGENTS.md is {lines} lines; the map is at most 150 (harness §5)"
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

#[test]
fn the_local_template_is_the_seven_volatile_sections() {
    let template = local_template();
    for name in LOCAL_SECTIONS {
        let line = opener(&template, name).unwrap();
        assert!(line.contains(" volatile "), "{line}");
        assert!(line.contains(&format!("status --section {name}")), "{line}");
    }
    assert_eq!(
        template.matches("<!-- /computed -->").count(),
        LOCAL_SECTIONS.len()
    );
    let ignored = read(".gitignore");
    assert!(
        ignored.contains("/.claude/harness.local.md"),
        "the local file is gitignored"
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
