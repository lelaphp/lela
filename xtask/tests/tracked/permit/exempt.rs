//! What a rule's `exempt` and `unless_pattern` let through under the
//! scope grammar.

use super::*;

/// The `exempt` key: a rule `permit` judges does not judge a call
/// whose file `exempt` covers, under the path grammar `scope` uses. The
/// fixture is the `witness.exit-authored` edit under the fence grammar's
/// own parser, `xtask/src/witness/fence.rs`, which the tracked row denies
/// until a person carries the exempt onto it; the row is built here
/// through the constructors so the predicate is proven without it, and
/// once the manifest carries it the first half holds it as tracked.
#[test]
fn a_call_the_rules_exempt_covers_is_not_judged_by_that_rule() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/witness.exit-authored");
    let exempt = call(&dir.join("exempt.json"));
    let rule = manifest.rule("witness.exit-authored").unwrap();
    let judged = judge(&manifest, workspace(), &exempt).unwrap();
    if rule.exempts(Path::new("xtask/src/witness/fence.rs")) {
        assert!(
            judged.findings.iter().all(|f| f.rule != rule.id),
            "the tracked row exempts the parser: {judged:?}"
        );
    } else {
        assert_eq!(
            judged.denial().map(|f| f.rule.as_str()),
            Some(rule.id.as_str()),
            "the negative control: denied until the row lands: {judged:?}"
        );
    }

    let mut manifest = manifest;
    let rule = manifest
        .rules
        .iter_mut()
        .find(|r| r.id == "witness.exit-authored")
        .unwrap();
    if rule.exempt.is_empty() {
        rule.exempt = vec![
            "xtask/src/witness/**".to_owned(),
            "xtask/tests/witness.rs".to_owned(),
        ];
    }
    let judged = judge(&manifest, workspace(), &exempt).unwrap();
    assert!(
        judged
            .findings
            .iter()
            .all(|f| f.rule != "witness.exit-authored"),
        "the parser's own fence line is exempt: {judged:?}"
    );
    let judged = judge(&manifest, workspace(), &call(&dir.join("deny.json"))).unwrap();
    assert_eq!(
        judged.denial().map(|f| f.rule.as_str()),
        Some("witness.exit-authored"),
        "an extension stays denied: {judged:?}"
    );
}

/// Every predicate `permit` judges by a file reads `exempt` the same way:
/// the rule over `**` fires on a write at `crates/x.rs` and not on the
/// same write at `xtask/tests/x.rs`, which the exempt covers. `deny_argv`
/// judges a `Bash` call, which has no file, and an exempt names files, so
/// it is the one judged predicate an exempt cannot reach.
#[test]
fn every_judged_predicate_honours_exempt_under_the_scope_grammar() {
    let manifest = Manifest::load(workspace()).unwrap();
    let written = "pub fn read(p: *const u8) -> u8 {\n    unsafe { *p }\n}\nstatic M: std::sync::Mutex<u8> = todo!();\n";
    let probe = |shape: fn(&mut Rule)| {
        let mut rule = Rule::named("exempt.probe", "**");
        rule.exempt = vec!["xtask/tests/**".to_owned()];
        rule.predicate.deny = None;
        shape(&mut rule);
        rule
    };
    let rules = [
        probe(|r| r.predicate.deny = Some(true)),
        probe(|r| r.predicate.deny_pattern = Some("Mutex".to_owned())),
        probe(|r| r.predicate.deny_types = Some(vec!["std::sync::Mutex".to_owned()])),
        probe(|r| r.predicate.allow = Some(vec!["crates/php_heap/**".to_owned()])),
        probe(|r| r.predicate.on = Some("pub".to_owned())),
    ];
    for rule in rules {
        let name = rule.predicate.named().join(",");
        let mut manifest = manifest.clone();
        manifest.rules = vec![rule];
        let write = |path: &str| {
            let text = serde_json::json!({
                "tool_name": "Write",
                "tool_input": { "file_path": path, "content": written }
            })
            .to_string();
            judge(&manifest, workspace(), &Call::parse(&text).unwrap()).unwrap()
        };
        let fired = write("crates/x.rs");
        assert!(
            fired.findings.iter().any(|f| f.rule == "exempt.probe"),
            "{name}: the rule fires outside the exempt: {fired:?}"
        );
        let spared = write("xtask/tests/x.rs");
        assert!(
            spared.findings.is_empty(),
            "{name}: the rule fires inside the exempt: {spared:?}"
        );
    }
}

/// `unless_pattern`: a hit of the `deny_pattern` whose own command
/// also matches it is let through, and a hit in another command of the
/// same line is not.
#[test]
fn unless_pattern_allows_a_call_its_deny_pattern_matches() {
    let manifest = probe(|rule| rule.predicate.unless_pattern = Some("--subset".to_owned()));
    let root = workspace();
    assert!(denied(&manifest, root, "cargo xtask phpt --census"));
    assert!(!denied(
        &manifest,
        root,
        "cargo xtask phpt --subset ext/zlib"
    ));
    assert!(!denied(
        &manifest,
        root,
        "cargo xtask phpt --census --subset Zend/tests; echo ok"
    ));
    assert!(denied(
        &manifest,
        root,
        "cargo xtask phpt --subset ext/zlib && cargo xtask phpt"
    ));
    // The row that carries one without a `deny_pattern` is refused.
    let text = Tracked::workspace()
        .with_rule("orphan", |rule| {
            rule.extend(
                "tool = \"Bash\"\ndeny = true\nunless_pattern = \"x\"\ntext = \"t\"\n"
                    .parse::<toml::Table>()
                    .unwrap(),
            );
        })
        .text();
    let refused = Manifest::parse(&text).unwrap_err().to_string();
    assert!(
        refused.contains("`unless_pattern` beside `deny`"),
        "{refused}"
    );
}
