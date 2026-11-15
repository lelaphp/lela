use std::path::PathBuf;

use super::{Manifest, RuledOut};

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

/// A manifest without the table rules nothing out, and a ruling is the
/// citation as the table types it. An issue's bare number is read as the
/// issue is cited, and no other value is a ruling.
#[test]
fn the_parity_table_is_optional_and_its_ruling_a_citation() {
    assert!(Manifest::parse(MINIMAL).unwrap().parity.out.is_empty());
    let ruled = |ruling: &str| {
        let text = format!(
            "{MINIMAL}[parity]\nout = [{{ suites = [\"ext/ldap\"], ruling = {ruling} }}]\n"
        );
        Manifest::parse(&text).map(|manifest| manifest.parity.out[0].ruling.clone())
    };
    assert_eq!(ruled("\"design §12\"").unwrap(), "design §12");
    assert_eq!(ruled("1167").unwrap(), "#1167");
    for refused in ["true", "-1", "1.5", "[\"design §12\"]"] {
        assert!(ruled(refused).is_err(), "{refused}");
    }
}

/// A suite ruled out of parity and owned by a `[[crate]]` row's subsets is
/// refused, naming both: the suite with its ruling, and the row. A suite
/// inside a row's subset is that row's too, and so is a row's subset inside
/// a suite.
#[test]
fn a_ruled_out_suite_a_crate_owns_is_refused_naming_both() {
    let cases = [
        (
            "ext/pgsql",
            "[[crate]]\nname = \"ext/pgsql\"\nbudget = 1\n",
            "ext/pgsql",
        ),
        (
            "Zend/tests/lang",
            "[[crate]]\nname = \"php_engine\"\nbudget = 1\nphpt = [\"Zend/tests\"]\n",
            "php_engine",
        ),
        (
            "sapi",
            "[[crate]]\nname = \"sapi/cgi\"\nbudget = 1\nphpt = [\"sapi/cgi/tests\"]\n",
            "sapi/cgi",
        ),
    ];
    for (suite, row, owner) in cases {
        let text = format!(
            "{MINIMAL}[parity]\nout = [{{ suites = [\"ext/ldap\", \"{suite}\"], ruling = \"design §12\" }}]\n{row}"
        );
        let refused = format!("{:#}", Manifest::parse(&text).unwrap_err());
        for named in [suite, "design §12", owner] {
            assert!(refused.contains(named), "{suite}: {named} in {refused}");
        }
    }
}

/// The tracked manifest's `[parity] out` is the seven server-bound suites
/// under the ruling that put them there, read as typed data (#1739, #1759).
#[test]
fn the_tracked_parity_table_is_the_seven_suites_under_design_12() {
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let suites = [
        "ext/pgsql",
        "ext/pdo_pgsql",
        "ext/ldap",
        "ext/snmp",
        "ext/odbc",
        "ext/pdo_odbc",
        "ext/pdo_dblib",
    ];
    assert_eq!(
        manifest.parity.out,
        [RuledOut {
            suites: suites.iter().map(PathBuf::from).collect(),
            ruling: "design §12".to_owned(),
        }]
    );
}

/// A suite is ruled out once, under one ruling: one listed twice, under
/// two rulings or one, or one inside another ruled suite, is refused,
/// naming both entries and their rulings (#1930).
#[test]
fn a_suite_ruled_out_twice_or_inside_another_is_refused_naming_both() {
    let cases = [
        (
            "{ suites = [\"ext/ldap\"], ruling = \"design §12\" }, { suites = [\"ext/ldap\"], ruling = \"design §3\" }",
            "rules ext/ldap out under design §12, and ext/ldap under design §3",
        ),
        (
            "{ suites = [\"ext/ldap\", \"ext/snmp\", \"ext/ldap\"], ruling = \"design §12\" }",
            "rules ext/ldap out under design §12, and ext/ldap under design §12",
        ),
        (
            "{ suites = [\"ext/x\"], ruling = \"design §12\" }, { suites = [\"ext/x/tests\"], ruling = \"design §3\" }",
            "rules ext/x out under design §12, and ext/x/tests under design §3",
        ),
        (
            "{ suites = [\"ext/x/tests\"], ruling = \"design §3\" }, { suites = [\"ext/x\"], ruling = \"design §12\" }",
            "rules ext/x/tests out under design §3, and ext/x under design §12",
        ),
    ];
    for (out, named) in cases {
        let text = format!("{MINIMAL}[parity]\nout = [{out}]\n");
        let refused = format!("{:#}", Manifest::parse(&text).unwrap_err());
        assert!(refused.contains(named), "{out}: {named} in {refused}");
    }
    let apart = format!(
        "{MINIMAL}[parity]\nout = [{{ suites = [\"ext/x\"], ruling = \"design §12\" }}, {{ suites = [\"ext/xy\"], ruling = \"design §3\" }}]\n"
    );
    assert!(Manifest::parse(&apart).is_ok());
}

/// A pin the `[oracle]` row names and the workspace lacks is a defect and
/// not a refusal to load (#3618 ruling 4, #3642): the manifest loads,
/// `defects` names the file as absent, and `pin` refuses with it; the
/// stale fixture, whose row names a pin it does not carry, loads so, as
/// `permit --diff` and `done` load it to judge a change under it.
#[test]
fn a_pin_the_workspace_lacks_is_a_defect_and_only_pin_refuses() {
    let root = crate::scratch::Tree::new("manifest-pin-absent").unwrap();
    std::fs::write(root.join(super::FILE), MINIMAL).unwrap();
    let manifest = Manifest::load(&root).expect("the manifest loads");
    let defects = manifest.defects();
    assert_eq!(defects.len(), 1, "{defects:?}");
    let said = defects[0].to_string();
    assert!(
        said.starts_with("oracle.toml is absent: no oracle pin stands at"),
        "{said}"
    );
    let refused = format!("{:#}", manifest.pin().unwrap_err());
    assert_eq!(refused, said);
    assert!(manifest.pin.is_none() && manifest.pin_digest().is_empty());

    let stale = crate::built_root().join("xtask/fixtures/stale");
    assert!(!stale.join("oracle.toml").exists());
    let manifest = Manifest::load(&stale).expect("the stale fixture loads");
    assert!(manifest.rule("lanes.nightly").is_some());
    assert!(manifest.pin().is_err());
}

/// `adding` qualifies `deny = true` (#4363): beside it the row loads, and
/// beside another predicate, `deny = false` or none at all, it is refused
/// as an orphaned qualifier, naming the row and the key.
#[test]
fn adding_stands_beside_deny_and_nowhere_else() {
    let row = |predicate: &str| {
        format!(
            "{MINIMAL}[[rule]]\nid = \"x.pipe\"\ntool = \"Write\"\nscope = \"ci/**\"\n{predicate}\nadding = 'x'\ntext = \"t\"\nremedy = \"r\"\n"
        )
    };
    let manifest = Manifest::parse(&row("deny = true")).expect("beside deny = true");
    let rule = manifest.rule("x.pipe").unwrap();
    assert_eq!(rule.predicate.adding.as_deref(), Some("x"));
    assert_eq!(rule.predicate.named(), ["deny"]);
    for predicate in ["deny = false", "deny_pattern = 'y'", "writes = true"] {
        let refused = format!("{:#}", Manifest::parse(&row(predicate)).unwrap_err());
        assert!(
            refused.contains("x.pipe") && refused.contains("`adding` beside"),
            "{predicate}: {refused}"
        );
    }
}
