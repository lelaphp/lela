use super::Manifest;

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
