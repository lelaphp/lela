//! What a write to a source is judged by: a computed region's body, an
//! `unsafe` site and what `witness --accept` wrote.

use super::*;

#[test]
fn an_edit_inside_a_computed_region_body_is_denied_and_one_beside_it_allowed() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("regions");
    let inside = judge(&manifest, workspace(), &call(&dir.join("edit-body.json"))).unwrap();
    let denial = inside.denial().expect("an edit into the body is denied");
    assert_eq!(denial.rule, "computed.region-body");
    assert!(
        denial.reason().contains("computed run"),
        "{}",
        denial.reason()
    );

    let beside = judge(&manifest, workspace(), &call(&dir.join("edit-prose.json"))).unwrap();
    assert!(beside.denial().is_none(), "{beside:?}");

    let fenced = judge(&manifest, workspace(), &call(&dir.join("edit-fenced.json"))).unwrap();
    assert!(
        fenced.denial().is_none(),
        "markers inside a fence are prose: {fenced:?}"
    );

    let rewrite = judge(&manifest, workspace(), &call(&dir.join("write-body.json"))).unwrap();
    assert_eq!(
        rewrite.denial().map(|f| f.rule.as_str()),
        Some("computed.region-body"),
        "a Write that changes a body is the same edit"
    );
    let same = judge(&manifest, workspace(), &call(&dir.join("write-same.json"))).unwrap();
    assert!(same.denial().is_none(), "{same:?}");
    let opener = judge(
        &manifest,
        workspace(),
        &call(&dir.join("write-opener.json")),
    )
    .unwrap();
    assert!(
        opener.denial().is_none(),
        "a changed opener is a template change: {opener:?}"
    );
}

/// `unsafe.homes` judges code, and the harness's own sensors carry the
/// keyword in their negative controls, which are strings and comments: the
/// audit's tests pin `unsafe_sites("unsafe { a(); }")`, and while that read
/// as a site the file could not be written to at all (#18).
#[test]
fn an_unsafe_inside_a_string_or_a_comment_is_not_a_site() {
    let manifest = Manifest::load(workspace()).unwrap();
    let denied = |content: &str| {
        let text = serde_json::json!({
            "tool_name": "Write",
            "tool_input": { "file_path": "crates/php_engine/src/executor.rs", "content": content },
        })
        .to_string();
        let call = Call::parse(&text).unwrap();
        judge(&manifest, workspace(), &call)
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "unsafe.homes")
    };

    assert!(
        denied("let x = unsafe { *p };"),
        "a site outside the homes unsafe.homes names is still denied"
    );
    assert!(denied("pub unsafe fn go() {}"));
    for allowed in [
        r#"let s = "unsafe { a(); }";"#,
        r##"let s = r#"unsafe fn b()"#;"##,
        "// unsafe { a(); }\nlet x = 1;",
        "/* unsafe fn b() */\nlet x = 1;",
        "let c = \'x\'; // a char literal is not a lifetime\nlet s = \"unsafe {\";",
    ] {
        assert!(!denied(allowed), "{allowed:?} carries no unsafe code");
    }

    // The file the ticket found: the audit's own source, unchanged.
    let audit = std::fs::read_to_string(workspace().join("xtask/src/unsafe_audit.rs")).unwrap();
    assert!(
        !denied(&audit),
        "the audit's own negative controls are strings, not sites"
    );
}

/// An unsafe attribute, `#[unsafe(no_mangle)]`, asserts an unsafe contract
/// as a block does, and is a site for both sensors of `unsafe.homes`: the
/// rule's fixture carries it outside a home and inside one (#46).
#[test]
fn an_unsafe_attribute_is_a_site_outside_a_home_and_not_a_finding_inside_one() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/unsafe.homes");
    let fired = |name: &str| {
        judge(&manifest, workspace(), &call(&dir.join(name)))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "unsafe.homes")
    };
    assert!(
        fired("deny-attribute.json"),
        "the attribute outside a home is denied at the edit"
    );
    assert!(
        !fired("allow-attribute.json"),
        "the attribute inside a home is allowed at the edit"
    );
}

/// End to end (#105): `xtask witness --accept` captures the oracle's
/// status onto the fence line and writes its reading and its record of
/// the verdict, and `xtask permit --diff` over that tree permits the line
/// it wrote. Take the reading away and the record still says the line is
/// the instrument's; take the record away too and the same tree is denied
/// with the rule id.
#[test]
fn what_accept_wrote_permit_diff_permits_over_the_same_tree() {
    if !oracle_is_here() {
        return;
    }
    let root = scratch("unless-accept");
    lay_down(&root, &block("exit"));
    let readings = witness::run(
        &root,
        &Manifest::load(&root).unwrap(),
        &Registry::load(&root).unwrap(),
        &Ask {
            crates: vec![CRATE.to_owned()],
            accept: Accept::Status,
            ..Ask::default()
        },
    )
    .unwrap();
    Store::of_workspace(&root).record(&readings).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(FILE)).unwrap(),
        block("exit=3"),
        "the oracle's status is on the fence line and nothing else moved"
    );

    Repo::at(&root).git(&["add", "--", CRATE]);
    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "{out}");

    let store = Store::of_workspace(&root);
    std::fs::remove_file(store.path("witness", CRATE).unwrap()).unwrap();
    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(!out.contains("deny "), "the record stands: {out}");

    let record = store.path("witness", &format!("{CRATE}@accepted")).unwrap();
    std::fs::remove_file(&record).unwrap();
    std::fs::remove_dir_all(record.with_extension("")).unwrap();
    std::fs::remove_file(store.root().join("log/witness.jsonl")).unwrap();
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(
        out.contains("deny ") && out.contains(EXIT_AUTHORED) && out.contains(FILE),
        "{out}"
    );
}

/// A verdict pinned before any record existed is recorded by the next
/// `--accept` that finds the oracle giving it, though the run writes
/// nothing (CW): `--accept` takes the oracle's verdicts, and one the
/// oracle gives again is the oracle's. Before that run the pinned status
/// stands in the diff with no reading and no record and is denied; after
/// it `permit --diff` permits it; and the status changed by hand, which
/// the oracle does not give, is denied again, the next `--accept` leaving
/// it without a record.
#[test]
fn a_verdict_pinned_before_any_record_is_recorded_by_an_accept_that_writes_nothing() {
    if !oracle_is_here() {
        return;
    }
    let root = scratch("unless-reproduced");
    let repo = Repo::at(&root);
    lay_down(&root, &block("exit"));
    repo.git(&["add", "--", CRATE]);
    repo.git(&["commit", "-q", "-m", "the opt-in"]);
    std::fs::write(root.join(FILE), block("exit=3")).unwrap();
    repo.git(&["add", "--", CRATE]);
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "pinned before any record: {ran}");

    let accept = || {
        witness::run(
            &root,
            &Manifest::load(&root).unwrap(),
            &Registry::load(&root).unwrap(),
            &Ask {
                crates: vec![CRATE.to_owned()],
                accept: Accept::Status,
                scope: witness::Scope::Item("native".to_owned()),
                ..Ask::default()
            },
        )
        .unwrap()
    };
    accept();
    assert_eq!(
        std::fs::read_to_string(root.join(FILE)).unwrap(),
        block("exit=3"),
        "the run wrote nothing"
    );
    let out = xtask(&root, &["permit", "--diff"]).ok();
    assert!(
        !out.contains("deny "),
        "the oracle's verdict, recorded: {out}"
    );

    std::fs::write(root.join(FILE), block("exit=4")).unwrap();
    repo.git(&["add", "--", CRATE]);
    accept();
    let ran = xtask(&root, &["permit", "--diff"]);
    assert_eq!(ran.code(), 1, "{ran}");
    assert!(ran.stdout.contains(EXIT_AUTHORED), "{ran}");
}
