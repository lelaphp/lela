//! The sections the store renders: locks and budget, the settled rows
//! folded into one line, `--tracked` reading none, and the local
//! sections.

use super::*;

#[test]
fn locks_and_budget_render_the_readings_the_instruments_wrote() {
    let root = tracked_scratch("readings");
    let store = Store::of_workspace(&root);

    let mut api = Reading::new("api-check", "api");
    api.status = Status::Pass;
    api.numbers.insert("items".into(), 301.into());
    store.record(std::slice::from_ref(&api)).unwrap();

    let mut heap = Reading::new("budget", "php_heap");
    heap.status = Status::Fail;
    heap.numbers.insert("lines".into(), 20001.into());
    heap.message = "php_heap: 20001 lines over the ceiling of 20000".into();
    store.record(std::slice::from_ref(&heap)).unwrap();

    // The items column is what the tree's lock lists, `__private`'s lines
    // apart as `api_items` holds them, and not the reading's count; the
    // ceiling column is the tracked manifest's `[budget] api_items`,
    // whatever a person has it at.
    std::fs::create_dir_all(root.join("crates/php_ext")).unwrap();
    std::fs::write(
        root.join("crates/php_ext/api.lock"),
        "# a header\nvalues::Zval\nvalues::Array\n\n__private::Entry\nrequest::Request\n",
    )
    .unwrap();
    let ceiling = Manifest::load(&root)
        .unwrap()
        .budget
        .api_items
        .unwrap()
        .to_string();
    let md = status(&root, &["--section", "locks"]).ok();
    assert!(
        md.contains(&format!("| api | api.lock | 3 | {ceiling} | pass |")),
        "{md}"
    );
    assert!(!md.contains("301"), "{md}");
    assert!(
        md.contains("| heap | heap.lock |  |  | no reading |"),
        "{md}"
    );

    let md = status(&root, &["--section", "budget"]).ok();
    assert!(
        md.contains("php_heap")
            && md.contains("20001")
            && md.contains("20000")
            && md.contains("fail"),
        "{md}"
    );
    assert!(md.contains("php_unit") && md.contains("no reading"), "{md}");
    assert!(
        md.lines().any(|l| l.starts_with("| scripts |")),
        "scripts/** has its row beside the crates (#34): {md}"
    );

    let json = status(&root, &["--section", "budget", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["crate"] == "php_heap")
        .unwrap();
    assert_eq!(row["ceiling"], 20000);
    assert_eq!(row["reading"]["numbers"]["lines"], 20001);
}

/// The sections that grow by a row per crate render the rows a session
/// acts on and count the settled rest in one line (harness revision 88): a
/// failure, a skip, a row with no reading, a budget within a tenth of its
/// ceiling and a phpt subset whose comparison fails are rows; a pass is a
/// count. Fifty more passing crates are the same section, `--uncut` is
/// every row, and the JSON is every reading whatever the flag.
#[test]
fn the_settled_rows_of_a_section_fold_into_one_line_that_does_not_grow() {
    let root = tracked_scratch("settled");
    let store = Store::of_workspace(&root);
    let record = |instrument: &str, scope: &str, status: Status, numbers: &[(&str, i64)]| {
        let mut r = Reading::new(instrument, scope);
        r.status = status;
        for (k, v) in numbers {
            r.numbers.insert((*k).into(), (*v).into());
        }
        r.message = format!("{scope}: {}", status.as_str());
        store.record(std::slice::from_ref(&r)).unwrap();
    };
    let ceiling = |name: &str| {
        let manifest = Manifest::load(&root).unwrap();
        i64::try_from(manifest.crate_named(name).unwrap().budget).unwrap()
    };
    let spare = ceiling("php_syntax") / 2;
    let near = ceiling("php_heap") - 1;
    record("budget", "php_syntax", Status::Pass, &[("lines", spare)]);
    record("budget", "php_heap", Status::Pass, &[("lines", near)]);
    record("budget", "php_unit", Status::Fail, &[("lines", 1)]);
    let md = status(&root, &["--section", "budget"]).ok();
    assert!(
        !md.contains("| php_syntax |"),
        "a pass with room is counted: {md}"
    );
    assert!(
        md.contains("| php_heap |"),
        "within a tenth of its ceiling: {md}"
    );
    assert!(
        md.contains("| php_unit |") && md.contains("| php_compile |"),
        "{md}"
    );
    let line = md.lines().last().unwrap();
    assert!(
        line.starts_with("1 more crates pass")
            && line.ends_with("`cargo xtask status --section budget --uncut`."),
        "{line}"
    );
    let whole = status(&root, &["--section", "budget", "--uncut"]).ok();
    assert!(
        whole.contains("| php_syntax |") && !whole.contains("--uncut"),
        "{whole}"
    );
    let json = status(&root, &["--section", "budget", "--format", "json"]).ok();
    assert!(json.contains("\"php_syntax\""), "{json}");

    record("witness", "ext/broken", Status::Fail, &[("failed", 1)]);
    record("witness", "ext/missing", Status::Skip, &[]);
    record("witness", "ext/a0", Status::Pass, &[("passed", 3)]);
    let before = status(&root, &["--section", "witness"]).ok();
    for n in 1..=50 {
        record(
            "witness",
            &format!("ext/a{n}"),
            Status::Pass,
            &[("passed", 3)],
        );
    }
    let after = status(&root, &["--section", "witness"]).ok();
    assert_eq!(
        before.replace("1 more crates", "51 more crates"),
        after,
        "fifty passing crates are one number of the line"
    );
    assert!(
        after.contains("| ext/broken |") && after.contains("| ext/missing |"),
        "{after}"
    );
    assert!(!after.contains("| ext/a7 |"), "{after}");
    let uncut = status(&root, &["--section", "witness", "--uncut"]).stdout;
    assert_eq!(
        uncut.lines().filter(|l| l.starts_with("| ext/a")).count(),
        51
    );

    record("witness", "ext/broken", Status::Pass, &[("passed", 1)]);
    record("witness", "ext/missing", Status::Pass, &[("passed", 1)]);
    let settled = status(&root, &["--section", "witness"]).ok();
    assert_eq!(
        settled, "53 crates pass; every row: `cargo xtask status --section witness --uncut`.\n",
        "a section with every row settled is the line alone"
    );

    record("phpt", "ext/date", Status::Pass, &[("passed", 9)]);
    record("phpt", "ext/date@against", Status::Fail, &[("lost", 1)]);
    record("phpt", "ext/spl", Status::Pass, &[("passed", 9)]);
    record("phpt", "ext/spl@against", Status::Pass, &[("held", 9)]);
    let md = status(&root, &["--section", "phpt"]).ok();
    assert!(
        md.contains("| ext/date | pass |") && md.contains("| fail lost=1 |"),
        "a pass below its target is a row: {md}"
    );
    assert!(!md.contains("| ext/spl |"), "{md}");
    assert!(md.ends_with("1 more subsets pass, none below the target it is held to; every row: `cargo xtask status --section phpt --uncut`.\n"), "{md}");
}

/// `--tracked` renders a section against the tree alone, as a tracked
/// region carries it (harness-design.md §5, revision 6): a reading
/// the store holds is still the plain section's, the tracked table has no
/// `reading` column, which could only say `no reading` there, and
/// a section whose rows are this checkout's alone is refused.
#[test]
fn tracked_renders_no_reading_whatever_the_store_holds() {
    let root = tracked_scratch("tracked");
    let mut reading = Reading::new("permit", "harness.self");
    reading.status = Status::Fail;
    reading.rule = "harness.self".into();
    Store::of_workspace(&root)
        .record(std::slice::from_ref(&reading))
        .unwrap();
    let row = "| harness.self |  | deny |";

    let live = status(&root, &["--section", "rules"]).ok();
    assert!(live.contains(&format!("{row} fail |")), "{live}");
    assert!(
        live.starts_with("| rule | adr | predicate | reading |\n"),
        "{live}"
    );
    let tracked = status(&root, &["--section", "rules", "--tracked"]).ok();
    assert!(
        tracked.starts_with("| rule | adr | predicate |\n|---|---|---|\n"),
        "{tracked}"
    );
    assert!(tracked.contains(&format!("{row}\n")), "{tracked}");
    assert!(!tracked.contains("reading"), "{tracked}");
    for section in ["locks", "bundle"] {
        let ran = status(&root, &["--section", section, "--tracked"]);
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let out = ran.stdout;
        assert!(!out.contains("reading"), "{section}: {out}");
    }

    for section in ["git", "permits", "phpt"] {
        let ran = status(&root, &["--section", section, "--tracked"]);
        assert_ne!(ran.code(), 0, "{section}: {ran}");
        let err = ran.stderr;
        assert!(
            err.contains(&format!("the {section} section is this checkout's")),
            "{section}: {err}"
        );
    }
}

/// The volatile sections of the local file (harness-design.md §5) render
/// the readings of an instrument that does not exist yet as "no reading",
/// and one that was written as its row.
#[test]
fn the_local_sections_render_from_the_store_alone() {
    let root = tracked_scratch("local");
    for section in ["phpt", "witness", "mutants", "ticket", "permits", "bundle"] {
        let ran = status(&root, &["--section", section]);
        assert_eq!(ran.code(), 0, "{section}: {ran}");
        let md = ran.stdout;
        assert!(md.contains("no reading"), "{section}: {md}");
    }

    let store = Store::of_workspace(&root);
    let mut phpt = Reading::new("phpt", "Zend/tests");
    phpt.status = Status::Fail;
    phpt.numbers.insert("passed".into(), 9.into());
    phpt.numbers.insert("failed".into(), 1.into());
    phpt.message = "Zend/tests: 1 of 10 failed".into();
    store.record(std::slice::from_ref(&phpt)).unwrap();
    let md = status(&root, &["--section", "phpt"]).ok();
    assert!(
        md.contains("Zend/tests") && md.contains("failed=1") && md.contains("fail"),
        "{md}"
    );
    assert!(!md.contains("no reading"), "{md}");

    let mut ticket = Reading::new("ticket", "28");
    ticket.status = Status::Pass;
    ticket.numbers.insert("criteria".into(), 7.into());
    ticket.numbers.insert("ticked".into(), 7.into());
    ticket.numbers.insert("unticked".into(), 0.into());
    ticket.commit = "905018f".into();
    ticket.session = "session-abc".into();
    ticket.message = "#28: 7 of 7 criteria ticked; closed by xtask ticket close at 905018f".into();
    store.record(std::slice::from_ref(&ticket)).unwrap();
    let md = status(&root, &["--section", "ticket"]).ok();
    let row = md
        .lines()
        .find(|l| l.contains("| 28 |"))
        .unwrap_or_else(|| panic!("{md}"));
    assert!(
        row.contains("905018f") && row.contains("session-") && row.contains("ticked=7"),
        "what closed it and the session, from the store alone: {row}"
    );

    let mut denial = Reading::new("permit", "harness.self");
    denial.rule = "harness.self".into();
    denial.status = Status::Fail;
    denial.adr = String::new();
    denial.message = "Edit seams.toml: Harness configuration ... by a person.".into();
    store.record(std::slice::from_ref(&denial)).unwrap();
    store.record(std::slice::from_ref(&denial)).unwrap();
    let md = status(&root, &["--section", "permits"]).ok();
    let row = md.lines().find(|l| l.contains("harness.self")).unwrap();
    assert!(row.contains("| 2 |"), "two denials in the log: {row}");
    assert!(
        md.contains("ext.no-locks") && md.contains("| 0 |"),
        "a rule that never fired says so: {md}"
    );

    let json = status(&root, &["--section", "permits", "--format", "json"]).ok();
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    let row = rows["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap();
    assert_eq!(row["denials"], 2);
    assert_eq!(row["reading"]["status"], "fail");
}
