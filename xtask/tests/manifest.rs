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
fn a_qualifier_without_its_predicate_is_refused() {
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.orphan\"\ntool = \"Bash\"\ncheck = \"c\"\nunless = \"u\"\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("x.orphan") && err.contains("unless"), "{err}");
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

/// `[[week]]` is a bar of the thirty days (design §11): the days, the text,
/// and the readings it is measured by, each a status or a number against
/// a bound. A bound without a number, or a number without a bound, is
/// refused by name.
#[test]
fn a_week_row_names_the_readings_that_measure_its_bar() {
    let text = format!(
        "{MINIMAL}\n[[week]]\ndays = \"3-7\"\ntext = \"t\"\nreads = [{{ instrument = \"budget\", scope = \"xtask\", number = \"lines\", max = 14000 }}, {{ instrument = \"clippy\", scope = \"xtask\" }}]\n"
    );
    let manifest = Manifest::parse(&text).unwrap();
    assert_eq!(manifest.weeks.len(), 1);
    assert_eq!(manifest.weeks[0].reads.len(), 2);

    let text = format!(
        "{MINIMAL}\n[[week]]\ndays = \"3-7\"\ntext = \"t\"\nreads = [{{ instrument = \"budget\", scope = \"xtask\", min = 100 }}]\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("3-7") && err.contains("number"), "{err}");

    let text = format!(
        "{MINIMAL}\n[[week]]\ndays = \"3-7\"\ntext = \"t\"\nreads = [{{ instrument = \"budget\", scope = \"xtask\", number = \"lines\" }}]\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("3-7") && err.contains("min"), "{err}");
}

/// A bar's read names an instrument `xtask` has, so a row cannot arrive
/// before the instrument whose reading it reads; the instrument ships its
/// row (AGENTS.md, "the instrument ships before the code it measures").
#[test]
fn a_week_read_names_an_instrument_xtask_has() {
    let text = format!(
        "{MINIMAL}\n[[week]]\ndays = \"3-7\"\ntext = \"t\"\nreads = [{{ instrument = \"phpt-nightly\", scope = \"tests/basic\" }}]\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("3-7") && err.contains("phpt-nightly") && err.contains("instrument"),
        "{err}"
    );

    // The same row naming the instrument that now exists parses.
    let text = format!(
        "{MINIMAL}\n[[week]]\ndays = \"3-7\"\ntext = \"t\"\nreads = [{{ instrument = \"phpt\", scope = \"ext/pcre\" }}]\n"
    );
    assert!(Manifest::parse(&text).is_ok());
}

/// `[telemetry] silent_days` is the window of the silent query (harness
/// §7); optional, and nothing in code stands in for it when it is absent.
#[test]
fn telemetry_carries_the_silent_window_and_is_optional() {
    let manifest = Manifest::parse(MINIMAL).unwrap();
    assert!(manifest.telemetry.is_none());
    let text = format!("{MINIMAL}\n[telemetry]\nsilent_days = 14\n");
    let manifest = Manifest::parse(&text).unwrap();
    assert_eq!(manifest.telemetry.unwrap().silent_days, 14);
    let text = format!("{MINIMAL}\n[telemetry]\nsilent = 14\n");
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("silent"), "the offending key is named: {err}");
}

#[test]
fn a_crate_row_names_the_phpt_subsets_the_crate_owns() {
    let text = format!(
        "{MINIMAL}\n[[crate]]\nname = \"php_engine\"\nbudget = 1\nphpt = [\"Zend/tests\", \"tests\"]\n[[crate]]\nname = \"php_heap\"\nbudget = 1\n[[crate]]\nname = \"ext/pcre\"\nbudget = 1\n[[crate]]\nname = \"ext/spl\"\nbudget = 1\nphpt = [\"ext/spl/tests\", \"Zend/tests/generators\"]\n"
    );
    let manifest = Manifest::parse(&text).unwrap();
    let subsets = |name: &str| manifest.crate_named(name).unwrap().subsets();
    assert_eq!(
        subsets("php_engine"),
        [Path::new("Zend/tests"), Path::new("tests")]
    );
    // An extension's subset is its own by the design (§12, ext/AGENTS.md),
    // so its row says nothing; a row that names subsets is what it owns.
    assert_eq!(subsets("ext/pcre"), [Path::new("ext/pcre/tests")]);
    assert_eq!(
        subsets("ext/spl"),
        [
            Path::new("ext/spl/tests"),
            Path::new("Zend/tests/generators")
        ]
    );
    // A crate the corpus has no directory for owns none, which is the case
    // `done` reads: such a crate is not asked for a phpt reading.
    assert!(subsets("php_heap").is_empty());
}

/// The path a reading's `tree` is hashed over, one function for every
/// scope kind a reading carries (harness-design.md §4.2, §4.5 condition
/// 1): a crate's sources, a subset's crate, a lock's home, the workspace.
#[test]
fn a_scope_maps_to_the_path_its_reading_is_stamped_over() {
    let text = format!(
        "{MINIMAL}\n[[crate]]\nname = \"php_engine\"\nbudget = 1\nlock = [\"api.lock\", \"tier.lock\"]\nlock_from = {{ \"api.lock\" = \"php_ext\" }}\nphpt = [\"Zend/tests\"]\n[[crate]]\nname = \"php_ext\"\nbudget = 1\n[[crate]]\nname = \"ext/pcre\"\nbudget = 1\n[[crate]]\nname = \"xtask\"\nbudget = 1\n[[seam]]\nname = \"api\"\nproducer = \"php_engine\"\nconsumers = [\"ext/*\"]\nlock = \"api.lock\"\n[[seam]]\nname = \"tier\"\nproducer = \"php_engine\"\nconsumers = [\"php_jit\"]\nlock = \"tier.lock\"\n[[seam]]\nname = \"ops\"\nproducer = \"ops.toml\"\nconsumers = [\"php_engine\"]\nlock = \"ops.lock\"\ngenerated = true\n"
    );
    let manifest = Manifest::parse(&text).unwrap();
    let path = |scope: &str| manifest.scope_path(scope);

    // A crate: its sources, wherever the layout puts them.
    assert_eq!(path("php_engine").unwrap(), Path::new("crates/php_engine"));
    assert_eq!(path("ext/pcre").unwrap(), Path::new("ext/pcre"));
    assert_eq!(path("xtask").unwrap(), Path::new("xtask"));

    // A subset: the crate whose subset it is, under the crate's name and
    // beside a tier lane or a control of the runner alike, so `done` finds
    // the engine's reading under the crate and neither displaces it.
    assert_eq!(path("ext/pcre@control").unwrap(), Path::new("ext/pcre"));
    assert_eq!(
        path("php_engine@jit").unwrap(),
        Path::new("crates/php_engine")
    );

    // A lock: the seam's name maps to the directory the lock lives in, the
    // crate `lock_from` names when the row says so, and the workspace for a
    // lock no row names, which is what api-check's readings hash over.
    assert_eq!(path("tier").unwrap(), Path::new("crates/php_engine"));
    assert_eq!(path("api").unwrap(), Path::new("crates/php_ext"));
    assert_eq!(path("ops").unwrap(), Path::new("."));

    // The workspace.
    assert_eq!(path("workspace").unwrap(), Path::new("."));

    // A name the manifest has no row for maps to nothing: a reading over it
    // would be one `done` never finds fresh, and the constructor refuses it.
    // A parsed manifest has no pin, so no directory of the pinned corpus
    // is known here either; that arm is phpt's test, beside the tree.
    assert_eq!(path("php_nowhere"), None);
    assert_eq!(path("ext/pcre/tests@control"), None);
}
