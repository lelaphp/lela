//! The parity section: every unbuilt suite of the pinned tree, met of
//! target per suite, and its markdown for a session.

use super::*;

/// With the pinned tree present (#1847), the section names as not built
/// every `ext/*` and `sapi/*` suite of the tree that holds a `.phpt` and
/// that no row owns or `[parity] out` rules out, whether or not a reading
/// measured it; a suite a row owns, `sapi/embed`'s among them though its
/// row owns no subset yet, one ruled out, a directory holding no test, and
/// the core's suites are not.
#[test]
fn parity_names_every_unbuilt_suite_of_the_pinned_tree() {
    let tree = fixture_tree("parity-tree");
    for (under, file) in [
        ("Zend/tests", "a.phpt"),
        ("tests/lang", "a.phpt"),
        ("ext/ctype/tests", "a.phpt"),
        ("ext/pgsql/tests", "a.phpt"),
        ("ext/enchant/tests", "a.phpt"),
        ("ext/oci8/tests/nested", "a.phpt"),
        ("ext/hollow/src", "hollow.c"),
        ("sapi/cli/tests", "a.phpt"),
        ("sapi/pi3web/tests", "a.phpt"),
        ("sapi/embed/tests", "a.phpt"),
        ("sapi/bare", "README"),
    ] {
        std::fs::create_dir_all(tree.join(under)).unwrap();
        std::fs::write(tree.join(under).join(file), "").unwrap();
    }
    let root = parity_store("parity-tree", &tree);
    let json = status_with_pin(
        &root,
        &["--section", "parity", "--format", "json"],
        None,
        None,
    )
    .ok();
    let parity: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parity["unbuilt_from"], "tree", "{json}");
    let unbuilt: Vec<&str> = parity["outside"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["why"] == "unbuilt")
        .map(|o| o["suite"].as_str().unwrap())
        .collect();
    assert_eq!(
        unbuilt,
        ["ext/enchant", "ext/oci8", "sapi/pi3web"],
        "ext/oci8 and sapi/pi3web no reading measured: {json}"
    );
    assert!(
        parity["outside"].as_array().unwrap().contains(
            &serde_json::json!({ "suite": "ext/pgsql", "why": "ruling", "ruling": 1167 })
        ),
        "a suite ruled out stays under its ruling: {json}"
    );

    let md = status_with_pin(&root, &["--section", "parity"], None, None).ok();
    assert!(
        md.contains(
            "Outside parity, not built by the pinned php: ext/enchant, ext/oci8, sapi/pi3web."
        ),
        "{md}"
    );
}

/// The same section as Markdown: a row per suite, the total, and the
/// suites outside parity by ruling and as not built.
#[test]
fn parity_renders_as_markdown_for_a_session() {
    let root = parity_store("parity-md", Path::new(NO_TREE));
    let md = status_with_pin(&root, &["--section", "parity"], None, None).ok();
    assert!(
        md.contains("| ext/ctype | held | 47 | 48 | 10 | 2 | pass |"),
        "{md}"
    );
    assert!(
        md.contains("| ext/json | untargeted |  |  |  |  | pass |"),
        "{md}"
    );
    assert!(
        md.contains("| sapi/cgi | missing |  |  |  |  | no reading |"),
        "{md}"
    );
    assert!(md.contains("236 of 247 target tests met"), "{md}");
    assert!(
        md.contains("#1167") && md.contains("not built by the pinned php: ext/enchant"),
        "{md}"
    );
    assert!(
        md.contains("named only where a reading measured them"),
        "without the pinned tree the section says its unbuilt list is the measured ones: {md}"
    );
}

/// The `parity` section (design §1, #1761): each suite parity counts with
/// met-of-target off the phpt reading that stands for it, the total over
/// the suites held to a target, and the suites outside parity with the
/// ruling that put them there or as not built; a control, a lane, a
/// comparison and a narrower run are no suite's own, and a reading held to
/// no target is in no total. A host's reading whose row owns no subset,
/// `sapi/makala`'s run of the CGI-shaped tests over HTTP, is no suite and
/// none outside, so a test met through two hosts counts once (#2334).
#[test]
fn parity_states_met_of_target_per_suite_and_in_total_and_names_the_suites_outside() {
    let root = parity_store("parity", Path::new(NO_TREE));
    let json = status_with_pin(
        &root,
        &["--section", "parity", "--format", "json"],
        None,
        None,
    )
    .ok();
    let parity: serde_json::Value = serde_json::from_str(&json).unwrap();
    let suite = |name: &str| {
        parity["suites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["suite"] == name)
            .unwrap_or_else(|| panic!("no suite {name}: {json}"))
            .clone()
    };
    assert_eq!(
        suite("Zend/tests")["target"],
        serde_json::json!({ "n": 100, "met": 90, "outside": 10, "predated": 0 })
    );
    assert_eq!(
        suite("ext/ctype")["target"],
        serde_json::json!({ "n": 48, "met": 47, "outside": 10, "predated": 2 })
    );
    assert_eq!(suite("ext/ctype")["state"], "held");
    assert_eq!(
        suite("sapi/cli")["target"],
        serde_json::json!({ "n": 99, "met": 99, "outside": 10, "predated": 0 }),
        "a host's suite is its row's name, the scope its reading is taken under"
    );
    for (name, state) in [
        ("ext/json", "untargeted"),
        ("ext/pcre", "unmeasured"),
        ("ext/lexbor", "empty"),
        ("sapi/cgi", "missing"),
    ] {
        assert_eq!(suite(name)["state"], state, "{name}: {json}");
        assert_eq!(suite(name)["target"], serde_json::Value::Null, "{name}");
    }
    assert_eq!(suite("ext/json")["reading"]["status"], "pass");
    assert_eq!(suite("sapi/cgi")["reading"], serde_json::Value::Null);
    let suites = parity["suites"].as_array().unwrap();
    assert!(
        suites
            .iter()
            .all(|s| !s["suite"].as_str().unwrap().contains('@')
                && s["suite"] != "ext/standard/tests/serialize"
                && s["suite"] != "ext/pgsql"
                && s["suite"] != "sapi/makala"),
        "{json}"
    );
    assert_eq!(
        parity["total"],
        serde_json::json!({ "suites": suites.len(), "held": 3, "met": 236, "target": 247 })
    );
    let outside = parity["outside"].as_array().unwrap();
    assert!(
        outside.contains(
            &serde_json::json!({ "suite": "ext/pgsql", "why": "ruling", "ruling": 1167 })
        ),
        "{json}"
    );
    assert!(
        outside
            .contains(&serde_json::json!({ "suite": "ext/ldap", "why": "ruling", "ruling": 1167 })),
        "a suite ruled out is named whether or not a reading measured it: {json}"
    );
    assert!(
        outside.contains(&serde_json::json!({ "suite": "ext/enchant", "why": "unbuilt" })),
        "{json}"
    );
    assert_eq!(
        outside.len(),
        8,
        "the seven under #1167 and ext/enchant: {json}"
    );
    assert_eq!(
        parity["unbuilt_from"], "readings",
        "without the pinned tree, the unbuilt are the measured ones alone: {json}"
    );
}
