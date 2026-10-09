use std::path::PathBuf;

use super::{Manifest, RuledOut};
use crate::track::Ticket;

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

/// A manifest without the table rules nothing out, and a ruling is an
/// issue's number: `"#1167"` is refused, as #1739 ruled.
#[test]
fn the_parity_table_is_optional_and_its_ruling_a_number() {
    assert!(Manifest::parse(MINIMAL).unwrap().parity.out.is_empty());
    let text =
        format!("{MINIMAL}[parity]\nout = [{{ suites = [\"ext/ldap\"], ruling = \"#1167\" }}]\n");
    assert!(Manifest::parse(&text).is_err());
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
            "{MINIMAL}[parity]\nout = [{{ suites = [\"ext/ldap\", \"{suite}\"], ruling = 1167 }}]\n{row}"
        );
        let refused = format!("{:#}", Manifest::parse(&text).unwrap_err());
        for named in [suite, "#1167", owner] {
            assert!(refused.contains(named), "{suite}: {named} in {refused}");
        }
    }
}

/// The tracked manifest's `[parity] out` is the seven server-bound suites
/// under the ruling that put them there, read as typed data (#1739, #1759).
#[test]
fn the_tracked_parity_table_is_the_seven_suites_under_1167() {
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
            ruling: Ticket::new(1167),
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
            "{ suites = [\"ext/ldap\"], ruling = 1167 }, { suites = [\"ext/ldap\"], ruling = 1168 }",
            "rules ext/ldap out under #1167, and ext/ldap under #1168",
        ),
        (
            "{ suites = [\"ext/ldap\", \"ext/snmp\", \"ext/ldap\"], ruling = 1167 }",
            "rules ext/ldap out under #1167, and ext/ldap under #1167",
        ),
        (
            "{ suites = [\"ext/x\"], ruling = 1167 }, { suites = [\"ext/x/tests\"], ruling = 1168 }",
            "rules ext/x out under #1167, and ext/x/tests under #1168",
        ),
        (
            "{ suites = [\"ext/x/tests\"], ruling = 1168 }, { suites = [\"ext/x\"], ruling = 1167 }",
            "rules ext/x/tests out under #1168, and ext/x under #1167",
        ),
    ];
    for (out, named) in cases {
        let text = format!("{MINIMAL}[parity]\nout = [{out}]\n");
        let refused = format!("{:#}", Manifest::parse(&text).unwrap_err());
        assert!(refused.contains(named), "{out}: {named} in {refused}");
    }
    let apart = format!(
        "{MINIMAL}[parity]\nout = [{{ suites = [\"ext/x\"], ruling = 1167 }}, {{ suites = [\"ext/xy\"], ruling = 1168 }}]\n"
    );
    assert!(Manifest::parse(&apart).is_ok());
}
