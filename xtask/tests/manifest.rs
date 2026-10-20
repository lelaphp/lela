//! The seam manifest and the oracle pin through the one reader
//! (harness-design.md §4.1).

use std::path::Path;

use xtask::manifest::{AvoidFrom, Crate, Edge, Manifest, Rule, Scripts, Seam};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
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

/// `[budget] scripts` is one row, the ceiling and the generators it leaves
/// out, read as the file writes it and absent when the file has none; the
/// constructor a test builds it by is the row the file would parse (#34).
#[test]
fn the_scripts_row_is_the_ceiling_and_its_generators_and_is_absent_when_unwritten() {
    let parsed = Manifest::parse(MINIMAL).unwrap();
    assert_eq!(parsed.budget.scripts, None);
    let text = format!(
        "{MINIMAL}scripts = {{ ceiling = 150, generators = [\"scripts/census.sh\", \"scripts/extract_ops.py\"] }}\n"
    );
    let parsed = Manifest::parse(&text).unwrap();
    assert_eq!(
        parsed.budget.scripts,
        Some(Scripts::new(
            150,
            &["scripts/census.sh", "scripts/extract_ops.py"]
        ))
    );
    let text = format!("{MINIMAL}scripts = {{ ceiling = 150 }}\n");
    let parsed = Manifest::parse(&text).unwrap();
    assert_eq!(parsed.budget.scripts, Some(Scripts::new(150, &[])));
    let text = format!("{MINIMAL}scripts = {{ ceiling = 150, exclude = [] }}\n");
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("exclude"),
        "a key outside the row is refused by name: {err}"
    );
}

/// A row a test builds by name (`Crate::named` and the constructors beside
/// it) is the row the parser reads from the file with that name and
/// nothing else on it, so a constructor cannot drift from the grammar.
#[test]
fn a_row_built_by_name_is_the_row_the_file_would_parse() {
    let text = format!(
        "{MINIMAL}\n[[crate]]\nname = \"php_heap\"\nbudget = 1\n[[seam]]\nname = \"api\"\nproducer = \"php_engine\"\nlock = \"api.lock\"\n[[edge]]\nfrom = \"php_bundle\"\nto = \"php_ext\"\n[[rule]]\nid = \"x.deny\"\nscope = \"ext/*\"\ndeny = true\ntext = \"x.deny\"\n"
    );
    let parsed = Manifest::parse(&text).unwrap();
    let built = Manifest::named("php_heap");
    assert_eq!(built.budget, parsed.budget);
    assert_eq!(built.crates, parsed.crates);
    assert_eq!(built.oracle.pin, parsed.oracle.pin);
    assert_eq!(Seam::named("api", "php_engine"), parsed.seams[0]);
    assert_eq!(Edge::between("php_bundle", "php_ext"), parsed.edges[0]);
    assert_eq!(Rule::named("x.deny", "ext/*"), parsed.rules[0]);
    assert_eq!(Rule::named("x.deny", "ext/*").predicate.named(), ["deny"]);

    let row = Crate::named("php_engine")
        .with_phpt(&["Zend/tests", "tests"])
        .with_feature("jit");
    let text = format!(
        "{MINIMAL}\n[[crate]]\nname = \"php_engine\"\nbudget = 1\nfeature = \"jit\"\nphpt = [\"Zend/tests\", \"tests\"]\n"
    );
    assert_eq!(row, Manifest::parse(&text).unwrap().crates[0]);
}

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
fn an_empty_with_phrase_is_refused() {
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.blank\"\ntool = \"Bash\"\nrequire_argv = \"--\"\nwith = [\"git add\", \"\"]\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("x.blank") && err.contains("empty `with`"),
        "{err}"
    );
}

/// A `deny_argv` element is an option, an option and the name its value
/// sets, or a subcommand (#1001); any other shape would match nothing and
/// deny nothing, so the manifest refuses it.
#[test]
fn a_deny_argv_element_of_no_known_shape_is_refused() {
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.shape\"\ntool = \"Bash\"\ndeny_argv = [\"-n\", \"-c core.hooksPath\", \"git commit\"]\nwith = \"git commit\"\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("x.shape") && err.contains("\"git commit\""),
        "{err}"
    );
}

/// `unless_argv` excuses a `deny_argv` element by one `with` phrase and
/// the option that bounds it (#3068): an element the row does not deny,
/// a phrase it does not name, or an excuse with no option is refused,
/// naming the row and the excuse.
#[test]
fn an_unless_argv_excuse_names_a_denied_element_and_one_with_phrase() {
    let row = |excuse: &str| {
        format!(
            "{MINIMAL}\n[[rule]]\nid = \"x.lane\"\ntool = \"Bash\"\ndeny_argv = [\"--full\", \"--jit\"]\nunless_argv = {{ {excuse} }}\nwith = [\"xtask phpt\", \"xtask bench\"]\ntext = \"t\"\n"
        )
    };
    let parsed = Manifest::parse(&row(r#""--jit" = "xtask phpt --subset""#)).unwrap();
    assert_eq!(
        parsed.rule("x.lane").unwrap().predicate.named(),
        ["deny_argv"]
    );
    for excuse in [
        r#""--census" = "xtask phpt --subset""#,
        r#""--jit" = "xtask soak --subset""#,
        r#""--jit" = "xtask phpt""#,
    ] {
        let err = Manifest::parse(&row(excuse)).unwrap_err().to_string();
        assert!(
            err.contains("x.lane") && err.contains("unless_argv"),
            "{excuse}: {err}"
        );
    }
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.orphan\"\ntool = \"Bash\"\nrequire_argv = \"--subset\"\nunless_argv = {{ \"--jit\" = \"xtask phpt --subset\" }}\nwith = \"xtask phpt\"\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("`unless_argv` beside `require_argv`"), "{err}");
}

#[test]
fn a_qualifier_without_its_predicate_is_refused() {
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.orphan\"\ntool = \"Bash\"\ncheck = \"c\"\nunless = \"u\"\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(err.contains("x.orphan") && err.contains("unless"), "{err}");

    // `deny_macros` qualifies `deny_types` (#1888): the macros a type ban
    // cannot see, rendered beside the types, and nothing on their own or
    // beside another predicate.
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.macros\"\nscope = \"ext/*\"\ndeny_macros = [\"std::thread_local\"]\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("x.macros") && err.contains("0 predicates"),
        "{err}"
    );
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.macros\"\nscope = \"ext/*\"\ndeny_pattern = \"thread_local\"\ndeny_macros = [\"std::thread_local\"]\ntext = \"t\"\n"
    );
    let err = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        err.contains("x.macros") && err.contains("`deny_macros` qualifies deny_types"),
        "{err}"
    );
    let text = format!(
        "{MINIMAL}\n[[rule]]\nid = \"x.macros\"\nscope = \"ext/*\"\ndeny_types = [\"std::sync::OnceLock\"]\ndeny_macros = [\"std::thread_local\"]\ntext = \"t\"\n"
    );
    let parsed = Manifest::parse(&text).unwrap();
    let rule = parsed.rule("x.macros").unwrap();
    assert_eq!(rule.predicate.named(), ["deny_types"]);
    assert_eq!(
        rule.predicate.deny_macros.as_deref(),
        Some(&["std::thread_local".to_owned()][..])
    );
}

/// A macro or a method is denied by its full path: clippy matches paths,
/// and no table of a module's macros or functions expands a wildcard, so
/// the manifest refuses `deny_macros = ["std::*"]` and
/// `deny_methods = ["std::thread::*"]` rather than hand `generate` a row
/// that denies nothing and `permit` a pattern that matches nothing (#1888,
/// #1950).
#[test]
fn a_wildcard_of_macros_or_methods_is_refused() {
    for (id, row) in [
        (
            "x.macros",
            "deny_types = [\"std::sync::OnceLock\"]\ndeny_macros = [\"std::*\"]",
        ),
        ("x.methods", "deny_methods = [\"std::thread::*\"]"),
    ] {
        let text =
            format!("{MINIMAL}\n[[rule]]\nid = \"{id}\"\nscope = \"ext/*\"\n{row}\ntext = \"t\"\n");
        let err = Manifest::parse(&text).unwrap_err().to_string();
        assert!(
            err.contains(id) && err.contains("::*") && err.contains("full path"),
            "{err}"
        );
    }
}

#[test]
fn the_oracle_version_comes_from_the_pin_and_is_written_in_no_source_file() {
    let manifest = Manifest::load(workspace()).unwrap();
    let version = &manifest.pin.oracle.version;
    assert!(!version.is_empty());
    let src = xtask::package_dir().join("src");
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
    // An extension's subset is its own by the design (§12, ext/AGENTS.md):
    // every `.phpt` under its directory (#1590), so its row says nothing;
    // a row that names subsets is what it owns.
    assert_eq!(subsets("ext/pcre"), [Path::new("ext/pcre")]);
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
    // beside a tier lane, a control of the runner or a comparison alike,
    // so `done` finds the engine's reading under the crate and neither
    // displaces it; a suffix the grammar has not is no scope (#1744).
    assert_eq!(path("ext/pcre@control").unwrap(), Path::new("ext/pcre"));
    assert_eq!(
        path("php_engine@force").unwrap(),
        Path::new("crates/php_engine")
    );
    assert_eq!(
        path("ext/pcre@control@against").unwrap(),
        Path::new("ext/pcre")
    );
    assert_eq!(path("php_engine@jit"), None);

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

/// The lock-line rule is one lock's, keyed by its name, `<lock>-line`:
/// `php_engine`'s row lists three locks and only `api.lock` has the rule.
#[test]
fn the_lock_line_rule_is_the_locks_own_and_not_its_rows() {
    let manifest = Manifest::load(workspace()).unwrap();
    assert_eq!(
        manifest.lock_line_rule("api.lock").map(|r| r.id.as_str()),
        Some("api.lock-line")
    );
    assert_eq!(manifest.lock_line_rule("sapi.lock").map(|r| &r.id), None);
    assert_eq!(manifest.lock_line_rule("tier.lock").map(|r| &r.id), None);
    assert_eq!(manifest.lock_line_rule("heap.lock").map(|r| &r.id), None);
}

/// `avoid_from` is the glossary file, or a table naming the file and one
/// heading of it (#63); a table without the file is refused.
#[test]
fn avoid_from_names_the_glossary_whole_or_one_section_of_it() {
    let head = "[oracle]\npin = \"oracle.toml\"\n[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\nclean_build_release_s = 1\napi_items = 1\nbindings = 1\n";
    let whole = format!(
        "{head}[[rule]]\nid = \"vocab.avoid\"\nscope = \"crates/*\"\navoid_from = \"CONTEXT.md\"\ntext = \"t\"\n"
    );
    let rule = Manifest::parse(&whole).unwrap().rules.remove(0);
    let from = rule.predicate.avoid_from.as_ref().unwrap();
    assert_eq!(from, &AvoidFrom::File("CONTEXT.md".to_owned()));
    assert_eq!((from.file(), from.section()), ("CONTEXT.md", None));

    let section = format!(
        "{head}[[rule]]\nid = \"vocab.harness\"\nscope = \"xtask\"\navoid_from = {{ file = \"CONTEXT.md\", section = \"Harness\" }}\ntext = \"t\"\n"
    );
    let rule = Manifest::parse(&section).unwrap().rules.remove(0);
    let from = rule.predicate.avoid_from.as_ref().unwrap();
    assert_eq!(
        (from.file(), from.section()),
        ("CONTEXT.md", Some("Harness"))
    );
    assert_eq!(rule.predicate.named(), ["avoid_from"]);

    let bare = format!(
        "{head}[[rule]]\nid = \"vocab.harness\"\nscope = \"xtask\"\navoid_from = {{ section = \"Harness\" }}\ntext = \"t\"\n"
    );
    assert!(Manifest::parse(&bare).is_err());
}

/// A `[lock_home]` table names the crate a lock lives with when no
/// `[[crate]]` row carries it (the `ops` seam's producer is a file). A
/// manifest that carries the table parses, the home is read from it, and
/// a row's own lock still wins over the table.
#[test]
fn a_lock_home_table_names_the_home_of_a_lock_no_row_carries() {
    let head = "[oracle]\npin = \"oracle.toml\"\n[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\nclean_build_release_s = 1\napi_items = 1\nbindings = 1\n";
    let whole = format!(
        "{head}[[crate]]\nname = \"php_heap\"\nbudget = 1\nlock = \"heap.lock\"\n[lock_home]\n\"ops.lock\" = \"php_unit\"\n"
    );
    let manifest = Manifest::parse(&whole).unwrap();
    assert_eq!(manifest.lock_home("ops.lock").as_deref(), Some("php_unit"));
    assert_eq!(manifest.lock_home("heap.lock").as_deref(), Some("php_heap"));
    assert_eq!(manifest.lock_home("api.lock"), None);
    let bare = Manifest::parse(head).unwrap();
    assert!(bare.lock_homes.is_empty());
    assert_eq!(bare.lock_home("ops.lock"), None);
}

/// `generated` is the list of files the manifest renders (#2473); any
/// other shape is refused naming the row and what it takes, the map of
/// each file to one rule that seams.toml wrote until #2474 among them
/// (#2513).
#[test]
fn generated_decodes_as_a_list_of_files_and_any_other_shape_is_refused() {
    let row = |generated: &str| {
        format!(
            "{MINIMAL}[[rule]]\nid = \"generated.committed\"\ngenerated = {generated}\ntext = \"t\"\n"
        )
    };
    let list: Vec<String> = Manifest::parse(&row(r#"["crates/clippy.toml", "ext/clippy.toml"]"#))
        .unwrap()
        .generated_files()
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(list, ["crates/clippy.toml", "ext/clippy.toml"]);
    for refused in [
        r#"{ "ext/clippy.toml" = "ext.no-locks", "crates/clippy.toml" = "fork.threads" }"#,
        "\"ext/clippy.toml\"",
    ] {
        let err = format!("{:#}", Manifest::parse(&row(refused)).unwrap_err());
        assert!(
            err.contains("rule \"generated.committed\" spells `generated` as a")
                && err.contains("the row takes a list of the files"),
            "{refused}: {err}"
        );
    }
}
