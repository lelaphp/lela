//! The licence sensor where a test fixes what the instrument reads off the
//! machine: the day, a file's history and the pinned tree. The negative
//! controls that need none of the three are the fixtures of
//! `xtask/tests/fixtures.rs`.

use anyhow::bail;

use super::*;
use crate::scratch::{Tree, UNHOOKED};

/// A manifest of one crate and the rule, over `crates/`.
const MANIFEST: &str = r#"
[budget]
total = 1
new = 1
docs = 1
harness = 1
lock_packages = 1

[[crate]]
name = "php_unit"
budget = 1

[[rule]]
id = "licence.header"
scope = ["crates/**"]
check = "licence-header"
text = "t"
"#;

const FOUR: &str = "// Copyright (c) Them.\n// Copyright (c) Others.\n// Derived in part from php-src, used under BSD-3-Clause: see NOTICE.\n";

/// The table of ports with one row: `num.rs`, a component under `licence`
/// whose notice stands at lines 2 to 3 of the pinned tree's `ext/x.c`.
fn ports(licence: &str) -> String {
    format!(
        "## 3. Ports\n\n| component | where | origin | licence | notice text |\n|---|---|---|---|---|\n| bc, its authors | `crates/php_unit/src/num.rs` | `ext/x.c` | {licence} | `LICENSES/n.txt` (`ext/x.c:2-3`) |\n"
    )
}

/// `num.rs` under `expression`, with `changed` between the line that says
/// whose it is and its notice.
fn num(expression: &str, changed: &str) -> String {
    format!(
        "{SPDX}{expression}\n{FOUR}// A port of bc as php-src carries it, an altered version: see THIRD-PARTY.md.\n{changed}//\n// bc's notice, as php-src carries it:\n//   /* One line,\n//      and another. */\n// It stands, byte for byte, in LICENSES/n.txt.\n\n//! The numbers.\n"
    )
}

fn git_in(tree: &Tree, args: &[&str]) {
    git::git(tree.path(), &[&UNHOOKED[..], args].concat()).unwrap();
}

/// A repository of one crate whose `num.rs` is the row's, under `licence`.
fn workspace(name: &str, licence: &str) -> Tree {
    let tree = Tree::new(name).unwrap();
    git_in(&tree, &["init", "-q", "-b", "main"]);
    for dir in ["LICENSES", "crates/php_unit/src", "php-src/ext"] {
        std::fs::create_dir_all(tree.path().join(dir)).unwrap();
    }
    let ports = ports(licence);
    let package = format!("[package]\nname = \"php_unit\"\nlicense = \"{BASE} AND {licence}\"\n");
    let files = [
        (
            COPYRIGHT,
            "Copyright (c) Them.\nCopyright (c) Others.\nThe text.\n",
        ),
        ("LICENSES/n.txt", "/* One line,\n   and another. */\n"),
        (
            "php-src/ext/x.c",
            "#include <x.h>\n/* One line,\n   and another. */\nint x;\n",
        ),
        (THIRD_PARTY, ports.as_str()),
        ("crates/php_unit/Cargo.toml", package.as_str()),
    ];
    for (file, text) in files {
        tree.write(file, text).unwrap();
    }
    tree
}

fn surveyed(tree: &Tree, pinned: Option<&Path>, today: Option<&str>) -> Survey {
    let manifest = Manifest::parse(MANIFEST).unwrap();
    let rule = manifest.rule_checked_by(CHECK).unwrap();
    let today = || match today {
        Some(day) => Ok(day.to_owned()),
        None => bail!("no change is in hand, so no day is asked"),
    };
    survey(tree.path(), &manifest, rule, pinned, &today).unwrap()
}

/// Every clause of a survey: each file's, then what is wider than one.
fn clauses(survey: &Survey) -> String {
    joined(survey.unmet.iter().chain(&survey.wider), "\n")
}

/// A file under an LGPL licence names each day its history has, read off
/// the author dates, and the day a commit made now would carry where it
/// differs from `HEAD`'s. A commit that does not touch the file is held
/// to nothing of today: the day is not so much as asked, so a commit made
/// elsewhere on a later day passes as the one before it did.
#[test]
fn an_lgpl_file_names_each_day_it_changed_and_today_where_it_differs_from_head() {
    let lgpl = "LGPL-2.1-or-later";
    let tree = workspace("licence-days", lgpl);
    let expression = format!("{BASE} AND {lgpl}");
    let on = |days: &str| {
        let changed = format!(
            "{CHANGED}bc's C sources on 2026-09-28 by this project's contributors,\n// by translation into Rust{days}.\n"
        );
        tree.write("crates/php_unit/src/num.rs", num(&expression, &changed))
            .unwrap();
    };
    let commit = |day: &str| {
        git_in(&tree, &["add", "-A"]);
        let date = format!("--date={day}T12:00:00");
        git_in(&tree, &["commit", "-q", &date, "-m", day]);
    };
    on("");
    commit("2026-09-28");
    let first = surveyed(&tree, None, None);
    assert_eq!(clauses(&first), "");
    assert_eq!((first.files, first.components), (1, 1));

    // A second day in history, which the lines do not name.
    tree.write(
        "crates/php_unit/src/other.rs",
        format!("{SPDX}{BASE}\n{FOUR}"),
    )
    .unwrap();
    on(" ");
    commit("2026-10-01");
    let unnamed = clauses(&surveyed(&tree, None, None));
    assert_eq!(
        unnamed,
        "crates/php_unit/src/num.rs is under an LGPL licence and changed on 2026-09-28, 2026-10-01; `// Changed from ` lines under its fifth line name each day it changed, and none names 2026-10-01"
    );

    // The day named, in hand: the file differs from HEAD's, so today is
    // asked and is not named either.
    on("; changed again on 2026-10-01");
    let today = clauses(&surveyed(&tree, None, Some("2026-11-16")));
    assert!(
        today.ends_with("and none names 2026-11-16"),
        "a change in hand is held to the day it is made: {today}"
    );
    on("; changed again on 2026-10-01 and\n// 2026-11-16");
    let named = surveyed(&tree, None, Some("2026-11-16"));
    assert_eq!(clauses(&named), "");

    // Committed on that day, and read on a later one with nothing in hand.
    commit("2026-11-16");
    let later = surveyed(&tree, None, None);
    assert_eq!(clauses(&later), "");
}

/// `LICENSES/` is the record: a file's notice is held to it whether a
/// pinned tree is laid or not, and each notice there to the lines its row
/// names in the pinned tree where one is. Without the tree the survey
/// names the notices it did not compare; with it, a notice that is not
/// those lines fails, as does a row naming lines the file does not have.
#[test]
fn a_notice_under_licenses_is_held_to_the_pinned_tree_where_it_is_laid() {
    let tree = workspace("licence-record", "Zlib");
    let expression = format!("{BASE} AND Zlib");
    tree.write("crates/php_unit/src/num.rs", num(&expression, ""))
        .unwrap();
    let pinned = tree.path().join("php-src");

    let unlaid = surveyed(&tree, None, None);
    assert_eq!(clauses(&unlaid), "");
    assert_eq!(unlaid.unrecorded, ["LICENSES/n.txt"]);
    assert!(unlaid.recorded.is_empty());

    let laid = surveyed(&tree, Some(&pinned), None);
    assert_eq!(clauses(&laid), "");
    assert_eq!(laid.recorded, ["LICENSES/n.txt"]);
    assert!(laid.unrecorded.is_empty());

    // The pinned tree moves under the record: one line more above it.
    tree.write(
        "php-src/ext/x.c",
        "#include <x.h>\n\n/* One line,\n   and another. */\nint x;\n",
    )
    .unwrap();
    assert_eq!(
        clauses(&surveyed(&tree, Some(&pinned), None)),
        "LICENSES/n.txt is not lines 2-3 of ext/x.c in the pinned php-src"
    );
    assert_eq!(clauses(&surveyed(&tree, None, None)), "");

    // The record moves under the file: the file's notice is held to it
    // with no tree laid.
    tree.write("LICENSES/n.txt", "/* One line,\n   and one more. */\n")
        .unwrap();
    assert_eq!(
        clauses(&surveyed(&tree, None, None)),
        "crates/php_unit/src/num.rs's line 9 is \"//      and another. */\" and the header's is \"//      and one more. */\""
    );

    // Lines the pinned file does not have.
    tree.write(THIRD_PARTY, ports("Zlib").replace("2-3", "9-12"))
        .unwrap();
    let past = clauses(&surveyed(&tree, Some(&pinned), None));
    assert!(
        past.ends_with("LICENSES/n.txt is not lines 9-12 of ext/x.c in the pinned php-src"),
        "{past}"
    );
}

/// The day is the author's own: the seconds an identity ends with, or the
/// ones git hands a hook after an `@`, moved by the zone after them.
#[test]
fn the_day_of_a_commit_is_read_in_its_authors_zone() {
    // 2026-11-15T23:30:00Z, then 2026-11-16T04:00:00Z.
    for (ident, day) in [
        ("A U Thor <a@b.c> 1794785400 +0000", "2026-11-15"),
        ("A U Thor <a@b.c> 1794785400 +0100\n", "2026-11-16"),
        ("A U Thor <a@b.c> 1794801600 -0530", "2026-11-15"),
        ("A U Thor <a@b.c> 1794801600 +0000", "2026-11-16"),
        ("@1794785400 +0100", "2026-11-16"),
        ("@1794801600 -0530", "2026-11-15"),
    ] {
        assert_eq!(day_of(ident).unwrap(), day, "{ident}");
    }
    let err = day_of("A U Thor <a@b.c>").unwrap_err();
    assert!(format!("{err:#}").contains("does not end with a time and its zone"));
}

/// The row as it is proposed for the manifest, for a workspace whose
/// manifest does not carry it yet: the three trees, less the vendored
/// copies `THIRD-PARTY.md` lists and the files php-src generates, and a
/// list that starts empty.
#[cfg(tracked)]
fn proposed() -> Rule {
    let owned = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect();
    let mut rule = Rule::named("licence.header", "");
    rule.scope = crate::manifest::OneOrMany::Many(owned(&["crates/**", "ext/**", "sapi/**"]));
    rule.exempt = owned(&[
        "ext/lexbor/lexbor/**",
        "crates/timelib_sys/lib/**",
        "ext/mbstring/libmbfl/**",
        "ext/uri/uriparser/**",
        "ext/fileinfo/libmagic/**",
        "ext/fileinfo/data_file.c",
        "ext/mbstring/rare_cp_bitvec.h",
        "ext/mbstring/unicode_data.h",
    ]);
    rule.predicate = crate::manifest::Predicates {
        check: Some(CHECK.to_owned()),
        allow: Some(Vec::new()),
        ..Default::default()
    };
    rule
}

/// The workspace itself, the positive control: every file the rule covers
/// begins with its header, each component's file carries its notices as
/// `LICENSES/` holds them, each of those is the pinned php-src's lines
/// where that tree is laid, and each crate's `license` covers what its
/// files state. The rule is the manifest's, or the proposed row where the
/// manifest does not carry one yet. Every failure is printed, none
/// counted.
#[cfg(tracked)]
#[test]
fn the_workspace_begins_every_file_with_its_header() {
    let root = crate::package_dir().parent().unwrap();
    let manifest = Manifest::load(root).unwrap();
    let proposed = proposed();
    let rule = manifest.rule_checked_by(CHECK).unwrap_or(&proposed);
    let pinned = manifest.pin.as_ref().and_then(|pin| pin.source.tree().ok());
    let survey = survey(root, &manifest, rule, pinned.as_deref(), &|| today(root)).unwrap();
    assert!(
        survey.unmet.is_empty() && survey.wider.is_empty(),
        "{} unmet:\n{}",
        survey.unmet.len() + survey.wider.len(),
        clauses(&survey)
    );
    assert!(survey.files > 0 && survey.components > 0, "{survey:?}");
    if pinned.is_some() {
        assert!(survey.unrecorded.is_empty(), "{survey:?}");
    }
}
