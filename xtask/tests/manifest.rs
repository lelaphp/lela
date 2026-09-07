//! The seam manifest and the oracle pin through the one reader
//! (harness-design.md §4.1).

use std::path::Path;

use xtask::manifest::Manifest;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

#[test]
fn the_tracked_manifest_loads_with_its_rules_and_the_oracle_pin() {
    let manifest = Manifest::load(workspace()).unwrap();
    let ids: Vec<&str> = manifest.rules.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"ext.no-locks"), "rules: {ids:?}");
    assert!(ids.contains(&"harness.self"), "rules: {ids:?}");
    let no_locks = manifest.rule("ext.no-locks").unwrap();
    assert_eq!(no_locks.adr, ["0005"]);
    assert!(manifest.crate_named("php_engine").is_some());
    assert!(manifest.seams.iter().any(|s| s.lock == "api.lock"));
    assert_eq!(manifest.oracle.pin, "oracle.toml");
}

const MINIMAL: &str = r#"
[oracle]
pin = "oracle.toml"
[budget]
total = 1
new = 1
docs = 1
harness = 1
lock_packages = 1
clean_build_release_s = 1
api_items = 1
bindings = 1
"#;

#[test]
fn a_key_outside_the_grammar_is_refused_by_name() {
    let text = format!("{MINIMAL}\n[[crate]]\nname = \"php_heap\"\nbudget = 1\nceiling = 2\n");
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("ceiling"), "the offending key is named: {err}");
}

#[test]
fn a_table_outside_the_grammar_is_refused_by_name() {
    let text = format!("{MINIMAL}\n[policy]\nstrict = true\n");
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("policy"),
        "the offending table is named: {err}"
    );
}

#[test]
fn a_rule_with_no_predicate_is_refused() {
    let text = format!("{MINIMAL}\n[[rule]]\nid = \"x.empty\"\nscope = \"ext/*\"\ntext = \"t\"\n");
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("x.empty") && err.contains("0 predicates"),
        "{err}"
    );
}

#[test]
fn a_rule_with_two_predicates_is_refused() {
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.double\"\nscope = \"ext/*\"\ndeny_pattern = 'a'\ncheck = \"b\"\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("x.double") && err.contains("2 predicates"),
        "{err}"
    );
    assert!(
        err.contains("deny_pattern") && err.contains("check"),
        "{err}"
    );
}

#[test]
fn the_oracle_version_comes_from_the_pin_and_is_written_in_no_source_file() {
    let manifest = Manifest::load(workspace()).unwrap();
    let version = &manifest.pin.oracle.version;
    assert!(!version.is_empty());
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in walk(&src) {
        let text = std::fs::read_to_string(&entry).unwrap();
        assert!(
            !text.contains(version.as_str()),
            "{} states the oracle version",
            entry.display()
        );
        assert!(
            !text.contains(&manifest.pin.oracle.md5),
            "{} states the oracle digest",
            entry.display()
        );
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}
