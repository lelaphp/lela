use std::path::Path;
use std::process::Command;

use super::coverage::{Range, changed_unsafe_lines, judge};
use super::finding::Finding;
use super::{Act, Claimed, Class, Coverage, Findings, INSTRUMENT, Outcome, Scope};
use crate::manifest::{Manifest, Review};
use crate::reading::{Status, Store};
use crate::scratch::Tree;

/// A fixture of `xtask/fixtures/review/`.
fn fixture(name: &str) -> String {
    let path = crate::package_dir().join("fixtures/review").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// git in `dir` under a fixed identity, committing at `at` seconds past
/// the epoch where one is given; what it printed, trimmed.
fn git_at(dir: &Path, at: Option<i64>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    crate::batch::without_git(&mut command);
    if let Some(at) = at {
        let date = format!("@{at} +0000");
        command
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, None, args)
}

/// `path` written with `text` and committed as `message`; the commit.
fn commit(dir: &Path, path: &str, text: &str, message: &str) -> String {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, text).unwrap();
    git(dir, &["add", "--", path]);
    git(dir, &["commit", "-qm", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// A repository on `main` with one commit, `m0`.
fn repository(name: &str) -> (Tree, String) {
    let dir = Tree::new(name).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    let m0 = commit(&dir, "a.txt", "a\n", "m0");
    (dir, m0)
}

fn range(from: &str, head: &str, at: i64) -> Range {
    Range {
        from: from.to_owned(),
        head: head.to_owned(),
        at,
    }
}

/// Before every merge, and after it.
const EARLY: i64 = 0;
const LATE: i64 = i64::MAX;

#[test]
fn the_table_parses_as_ruled_and_a_row_that_narrows_it_unseen_is_refused() {
    let parsed = Manifest::parse(&fixture("table.toml")).unwrap();
    assert_eq!(
        parsed.review,
        Some(Review {
            crates: vec!["php_heap".to_owned(), "php_process".to_owned()],
            unsafe_lines: true
        })
    );
    assert_eq!(
        Manifest::parse(&fixture("no-table.toml")).unwrap().review,
        None
    );
    for (name, said) in [
        (
            "unknown-crate.toml",
            "\"php_hep\", which is no [[crate]] row",
        ),
        ("classes-nothing.toml", "so no diff is class A"),
        ("unknown-key.toml", "exempt"),
    ] {
        let refused = format!("{:#}", Manifest::parse(&fixture(name)).unwrap_err());
        assert!(refused.contains(said), "{name}: {refused}");
    }
}

#[test]
fn a_scope_reads_back_as_it_was_written() {
    let head = "fc0cb20d3230dfe2fd6a8ea3b67804cc32a6eafb";
    for (scope, text) in [
        (
            Scope::Range {
                ticket: 4268,
                head: head.to_owned(),
            },
            "4268/fc0cb20",
        ),
        (
            Scope::Finding {
                ticket: 4268,
                head: head.to_owned(),
                hash: "0123456789abcdef".to_owned(),
            },
            "4268/fc0cb20-0123456789ab",
        ),
        (
            Scope::Merge {
                commit: head.to_owned(),
            },
            "merge/fc0cb20",
        ),
    ] {
        assert_eq!(scope.to_string(), text);
        assert_eq!(Scope::of(text).unwrap().to_string(), text);
    }
    for none in [
        "4268",
        "0/fc0cb20",
        "x/fc0cb20",
        "4268/fc0cb2",
        "4268/fc0cb20-0123",
        "merge/x",
    ] {
        assert_eq!(Scope::of(none), None, "{none}");
    }
}

/// A changed line names `unsafe` only in its code: not in a comment, a
/// string or an identifier such as `unsafe_code`.
#[test]
fn an_unsafe_line_is_one_whose_code_names_the_keyword() {
    let diff = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1 +1,8 @@
-    let n = unsafe { read(p) };
+    // SAFETY: unsafe is fine here
+#![forbid(unsafe_code)]
+    bail!(\"unsafe is permitted only in the homes\");
+    let y = 1; // unsafe
+ * unsafe in a block comment
+pub unsafe fn read(p: *const u8) -> u8 {
+    let s = \"a \\\" quote\"; unsafe { s }
 unsafe context line
";
    assert_eq!(changed_unsafe_lines(diff), 3);
}

#[test]
fn a_diff_is_class_a_by_a_crate_the_table_names_or_a_changed_unsafe_line() {
    let (dir, m0) = repository("review-class");
    let table = Review {
        crates: vec!["php_heap".to_owned()],
        unsafe_lines: true,
    };
    let heap = commit(&dir, "crates/php_heap/src/a.rs", "fn a() {}\n", "heap");
    let quoted = commit(
        &dir,
        "xtask/src/a.rs",
        "const S: &str = \"unsafe\";\n",
        "quoted",
    );
    let bare = commit(
        &dir,
        "xtask/src/b.rs",
        "fn b() { unsafe { c() } }\n",
        "bare",
    );
    let heap_class = Class::A {
        crates: vec!["php_heap".to_owned()],
        unsafe_lines: 0,
    };
    assert_eq!(
        Class::of(&dir, Some(&table), &m0, &heap).unwrap(),
        heap_class
    );
    assert_eq!(
        Class::of(&dir, Some(&table), &heap, &quoted).unwrap(),
        Class::B
    );
    assert_eq!(
        Class::of(&dir, Some(&table), &quoted, &bare).unwrap(),
        Class::A {
            crates: vec![],
            unsafe_lines: 1
        }
    );
    assert_eq!(Class::of(&dir, None, &m0, &bare).unwrap(), Class::B);
    assert_eq!(heap_class.to_string(), "class A (php_heap)");
}

/// Round 3, Q26 (c), each way: a range from `main` to the tip covers, and
/// so do two that chain; a lone delta, a commit past the head, a reading
/// taken after the merge and a review of another branch do not.
#[test]
fn a_branch_is_covered_by_ranges_that_chain_from_its_base_to_its_tip() {
    let (dir, m0) = repository("review-chain");
    git(&dir, &["checkout", "-qb", "t1"]);
    let b1 = commit(&dir, "b.txt", "1\n", "b1");
    let b2 = commit(&dir, "b.txt", "2\n", "b2");
    git(&dir, &["checkout", "-q", "main"]);
    git(&dir, &["checkout", "-qb", "t2"]);
    let other = commit(&dir, "c.txt", "1\n", "other");
    let judged = |ranges: &[Range]| judge(&dir, &m0, &b2, 100, ranges).unwrap();
    assert_eq!(judged(&[range(&m0, &b2, EARLY)]), Coverage::Covered);
    assert_eq!(
        judged(&[range(&m0, &b1, EARLY), range(&b1, &b2, EARLY)]),
        Coverage::Covered
    );
    assert_eq!(
        judged(&[range(&b1, &b2, EARLY)]),
        Coverage::Partial(1),
        "a lone delta"
    );
    assert_eq!(
        judged(&[range(&m0, &b1, EARLY)]),
        Coverage::Partial(1),
        "past the head"
    );
    assert_eq!(
        judged(&[range(&m0, &b2, 100)]),
        Coverage::Unreviewed,
        "after the merge"
    );
    assert_eq!(judged(&[range(&m0, &b2, LATE)]), Coverage::Unreviewed);
    assert_eq!(
        judged(&[range(&m0, &other, EARLY)]),
        Coverage::Unreviewed,
        "another branch"
    );
    assert_eq!(judged(&[]), Coverage::Unreviewed);
}

/// A merge of `main` past the head leaves the branch covered where git
/// made its tree unaided, and partial where the merge carries a
/// resolution: a change made inside it, or a conflict resolved by hand.
#[test]
fn a_merge_of_main_past_the_head_covers_only_where_it_carries_no_resolution() {
    let (dir, m0) = repository("review-merges");
    git(&dir, &["checkout", "-qb", "t1"]);
    let head = commit(&dir, "a.txt", "branch\n", "b1");
    git(&dir, &["checkout", "-q", "main"]);
    let m1 = commit(&dir, "m.txt", "main\n", "m1");
    let m2 = commit(&dir, "a.txt", "main\n", "m2");
    let reviewed = [range(&m0, &head, EARLY)];
    let judged = |main: &str, tip: &str| judge(&dir, main, tip, 100, &reviewed).unwrap();

    git(&dir, &["checkout", "-q", "t1"]);
    git(&dir, &["merge", "-q", "--no-edit", &m1]);
    let clean = git(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(judged(&m1, &clean), Coverage::Covered);

    git(&dir, &["checkout", "-q", "-b", "t2", &head]);
    git(&dir, &["merge", "-q", "--no-commit", "--no-ff", &m1]);
    std::fs::write(dir.join("m.txt"), "changed inside the merge\n").unwrap();
    git(&dir, &["add", "m.txt"]);
    git(&dir, &["commit", "-qm", "merge main with a change"]);
    let changed = git(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(judged(&m1, &changed), Coverage::Partial(1));

    git(&dir, &["checkout", "-q", "-b", "t3", &head]);
    let conflict = Command::new("git")
        .arg("-C")
        .arg(&*dir)
        .args(["merge", "-q", "--no-edit", &m2])
        .output()
        .unwrap();
    assert!(!conflict.status.success(), "a.txt conflicts");
    std::fs::write(dir.join("a.txt"), "resolved\n").unwrap();
    git(&dir, &["add", "a.txt"]);
    git(&dir, &["commit", "-qm", "merge main by hand"]);
    let resolved = git(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(judged(&m2, &resolved), Coverage::Partial(1));
}

fn findings(head: &str) -> Findings {
    Findings {
        ticket: 4268,
        from: None,
        head: head.to_owned(),
        role: "standards".to_owned(),
        reviewer: "fable".to_owned(),
        builder: "builder-4268".to_owned(),
        findings: vec![],
    }
}

fn claimed(claim: &str) -> Claimed {
    Claimed {
        file: "xtask/src/review.rs".to_owned(),
        claim: claim.to_owned(),
        scenario: "a merge with no parents".to_owned(),
        reproducer: None,
    }
}

/// Round 5, Q8 (a): two skeptics confirm, a split stays a split, a
/// refute is refused to the builder, a reproduction confirms, and a
/// disposition stands beside whether the finding was confirmed.
#[test]
fn a_finding_is_confirmed_by_two_skeptics_or_a_reproducer_and_refuted_by_no_builder() {
    let (dir, head) = repository("review-finding");
    let mut finding = Finding::claimed(&findings(&head), &claimed("the merge is misread"), &head);
    let act = |f: &mut Finding, act: Act| f.apply(&dir, &act);
    assert_eq!(finding.outcome(), Outcome::Open);
    act(&mut finding, Act::Holds { by: "opus".into() }).unwrap();
    assert_eq!(finding.outcome(), Outcome::Open, "one skeptic");
    act(
        &mut finding,
        Act::Holds {
            by: "sonnet".into(),
        },
    )
    .unwrap();
    assert_eq!(finding.outcome(), Outcome::Confirmed);
    let refused = act(
        &mut finding,
        Act::Refutes {
            by: "builder-4268".into(),
        },
    )
    .unwrap_err();
    assert!(format!("{refused}").contains("only someone other than the branch's builder"));
    assert_eq!(
        finding.outcome(),
        Outcome::Confirmed,
        "the refusal changed nothing"
    );
    act(&mut finding, Act::Refutes { by: "haiku".into() }).unwrap();
    assert_eq!(finding.outcome(), Outcome::Split);
    let reading = finding.reading().unwrap();
    assert_eq!(reading.status, Status::Skip);
    assert_eq!(
        Finding::of(&reading).unwrap(),
        finding,
        "the reading is the finding"
    );
    for group in [
        "holds:opus",
        "holds:sonnet",
        "refutes:haiku",
        "role:standards",
        "file:xtask/src/review.rs",
    ] {
        assert!(
            reading.groups.contains_key(group),
            "{group}: {:?}",
            reading.groups
        );
    }
    assert!(
        reading
            .message
            .starts_with("the merge is misread; scenario: ")
    );

    act(
        &mut finding,
        Act::Reproduced {
            by: "opus".into(),
            reproducer: None,
        },
    )
    .unwrap_err();
    act(
        &mut finding,
        Act::Reproduced {
            by: "opus".into(),
            reproducer: Some("probe.php".into()),
        },
    )
    .unwrap();
    assert_eq!(finding.outcome(), Outcome::Confirmed);
    act(&mut finding, Act::Ticketed { number: 4300 }).unwrap();
    assert_eq!(finding.outcome(), Outcome::Ticketed(4300));
    assert!(finding.confirmed());
    let reading = finding.reading().unwrap();
    assert_eq!(reading.int("ticketed"), Some(4300));
    assert_eq!(reading.int("confirmed"), Some(1));
    assert_eq!(Finding::of(&reading).unwrap(), finding);
    act(
        &mut finding,
        Act::Fixed {
            commit: "HEAD".into(),
        },
    )
    .unwrap();
    assert_eq!(finding.outcome(), Outcome::Fixed(head.clone()));

    let mut lone = Finding::claimed(&findings(&head), &claimed("another"), &head);
    act(&mut lone, Act::Refutes { by: "haiku".into() }).unwrap();
    assert_eq!(lone.outcome(), Outcome::Refuted);
    assert_eq!(lone.reading().unwrap().status, Status::Pass);
}

/// The whole path: a findings file recorded, the same claim from a second
/// reviewer joining the one finding, a resolve, and the merge into `main`
/// judged covered by the land's call, with its one reading in the store.
#[test]
fn a_recorded_review_covers_the_merge_it_predates() {
    let (dir, _) = repository("review-whole");
    std::fs::write(dir.join("seams.toml"), fixture("table.toml")).unwrap();
    git(&dir, &["add", "seams.toml"]);
    git(&dir, &["commit", "-qm", "manifest"]);
    git(&dir, &["checkout", "-qb", "t4268"]);
    let head = commit(&dir, "crates/php_heap/src/a.rs", "fn a() {}\n", "heap");
    let mut file = findings(&head);
    file.findings = vec![claimed("the merge is misread")];
    let write = |name: &str, file: &Findings| {
        let path = dir.join(name);
        let json = serde_json::json!({
            "ticket": file.ticket, "head": file.head, "role": file.role,
            "reviewer": file.reviewer, "builder": file.builder,
            "findings": file.findings.iter().map(|c| serde_json::json!({
                "file": c.file, "claim": c.claim, "scenario": c.scenario
            })).collect::<Vec<_>>(),
        });
        std::fs::write(&path, json.to_string()).unwrap();
        path
    };
    super::record(&dir, &write("first.json", &file)).unwrap();
    file.role = "spec".to_owned();
    file.reviewer = "opus".to_owned();
    super::record(&dir, &write("second.json", &file)).unwrap();

    let store = Store::of_workspace(&dir);
    let scope = Finding::claimed(&file, &file.findings[0], &head)
        .scope
        .to_string();
    let recorded = store.last_of(INSTRUMENT, &scope).unwrap().unwrap();
    for group in [
        "role:standards",
        "role:spec",
        "reviewer:fable",
        "reviewer:opus",
    ] {
        assert!(
            recorded.groups.contains_key(group),
            "{group}: {:?}",
            recorded.groups
        );
    }
    assert_eq!(recorded.int("confirmed"), Some(0));
    super::resolve(
        &dir,
        &scope,
        &Act::Holds {
            by: "sonnet".into(),
        },
    )
    .unwrap();
    super::resolve(&dir, &scope, &Act::Holds { by: "haiku".into() }).unwrap();
    let confirmed = store.last_of(INSTRUMENT, &scope).unwrap().unwrap();
    assert_eq!(confirmed.status, Status::Fail);
    assert!(super::resolve(&dir, "4268/0000000", &Act::Holds { by: "x".into() }).is_err());

    git(&dir, &["checkout", "-q", "main"]);
    let later = crate::reading::epoch_seconds(&crate::reading::now().unwrap()).unwrap() + 60;
    git_at(
        &dir,
        Some(later),
        &["merge", "-q", "--no-ff", "-m", "Merge t4268", "t4268"],
    );
    let merge = git(&dir, &["rev-parse", "HEAD"]);
    super::after_land(&dir);
    let scope = Scope::Merge { commit: merge }.to_string();
    let judged = store.last_of(INSTRUMENT, &scope).unwrap().unwrap();
    assert_eq!(judged.status, Status::Pass, "{}", judged.message);
    assert_eq!(judged.int("covered"), Some(1));
    assert_eq!(judged.int("class_a"), Some(1));
    assert!(
        judged.groups.contains_key("crate:php_heap"),
        "{:?}",
        judged.groups
    );
    assert!(
        judged.message.contains("class A (php_heap), covered"),
        "{}",
        judged.message
    );
    let before = store.history(INSTRUMENT).unwrap().readings.len();
    super::cover(&dir, None).unwrap();
    assert_eq!(
        store.history(INSTRUMENT).unwrap().readings.len(),
        before,
        "a merge is judged once"
    );
}
