//! The nightly and builder-subset lanes: what each rule denies, by the
//! flag, the runner and the checkout, and what it lets through.

use super::*;

/// `lanes.nightly-flags`' fixtures under the row as proposed, in the
/// tracked manifest in place of the tracked row: every `deny*.json` denied
/// and every `allow*.json` let through. Under the row as tracked the same,
/// save that until the paste the `allow-excused*.json` lines, the tier's
/// one-subset reading, are denied with the rest (#3068).
#[test]
fn the_nightly_flags_let_the_tier_s_one_subset_reading_through() {
    let tracked = Manifest::load(workspace()).unwrap();
    let proposed = Tracked::workspace()
        .with_rule("lanes.nightly-flags", |rule| {
            *rule = PROPOSED_NIGHTLY_FLAGS.parse().unwrap();
        })
        .text();
    let proposed = Manifest::parse(&proposed).unwrap();
    let pasted = tracked
        .rule("lanes.nightly-flags")
        .is_some_and(|r| r.predicate.unless_argv.is_some());
    let quiet = checkout("nightly-flags-quiet", None);
    let dir = fixtures().join("rules/lanes.nightly-flags");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in [
        "allow-excused.json",
        "deny-jit-full.json",
        "deny-jit.json",
        "deny-bench.json",
    ] {
        assert!(names.iter().any(|n| n == name), "{name}: {names:?}");
    }
    let denied = |manifest: &Manifest, name: &str| {
        judge(manifest, quiet.path(), &call(&dir.join(name)))
            .unwrap()
            .findings
            .iter()
            .any(|f| f.rule == "lanes.nightly-flags")
    };
    for name in &names {
        let expect = name.starts_with("deny");
        assert_eq!(denied(&proposed, name), expect, "proposed row: {name}");
        let excused = name.starts_with("allow-excused");
        assert_eq!(
            denied(&tracked, name),
            expect || (excused && !pasted),
            "tracked row: {name}"
        );
    }
}

/// The two halves of the nightly lane the phpt runner asked for (#15):
/// the flag is denied wherever it stands in the call, not only beside the
/// subcommand, by `lanes.nightly-flags` (#1860), save the tier's `--jit`
/// lane on one subset `--subset` names (#3052), and upstream's runner is
/// denied where it is run and not where it is cited, by `lanes.nightly`.
/// The rule denied `--subset x --full` on neither count before, and denied
/// reading the file on the second.
#[test]
fn the_nightly_lane_is_denied_by_the_flag_and_not_by_a_citation() {
    let manifest = Manifest::load(workspace()).unwrap();
    let bash = |command: &str| {
        Call::parse(
            &serde_json::json!({
                "tool_name": "Bash",
                "tool_input": { "command": command, "description": "one call" }
            })
            .to_string(),
        )
        .unwrap()
    };
    let denied = |command: &str| {
        judge(&manifest, workspace(), &bash(command))
            .unwrap()
            .denial()
            .map(|f| f.rule.clone())
    };

    for (command, rule) in [
        (
            "cargo run -q -p xtask -- phpt --full",
            "lanes.nightly-flags",
        ),
        (
            "cargo run -p xtask -- phpt --subset Zend/tests --full",
            "lanes.nightly-flags",
        ),
        ("xtask phpt --jit force", "lanes.nightly-flags"),
        (
            "cargo run -q -p xtask -- phpt --jit exit-fuzz --full",
            "lanes.nightly-flags",
        ),
        ("php run-tests.php -p target/release/php", "lanes.nightly"),
        ("cd php-src && ./run-tests.php ext/pcre", "lanes.nightly"),
    ] {
        assert_eq!(
            denied(command).as_deref(),
            Some(rule),
            "the lane must be denied: {command}"
        );
    }

    for command in [
        "cargo run -q -p xtask -- phpt --subset pcre",
        "xtask phpt --subset Zend/tests --jit force",
        "cargo run -q -p xtask -- phpt --jit exit-fuzz --subset tests",
        "cargo test -p xtask --test phpt",
        "grep -n 'EXPECTF' /Users/x/php-src-8.5.9/run-tests.php",
        "echo 'upstream (run-tests.php:1885-1889) merges the streams'",
    ] {
        assert_eq!(
            denied(command),
            None,
            "the subset and a citation are not the lane: {command}"
        );
    }
}

/// `lanes.builder-subset`'s fixture (#1145, #1149), as `require_argv =
/// "--subset"` under `with = "xtask phpt"` reads it (#1860): in a
/// checkout whose branch records a ticket the lane without `--subset` is
/// denied, however xtask is run, and the subsets, an extension's and
/// Zend/tests, as a word or after `=`, pass; the merge check's call
/// passes in a checkout that records none, and is denied in one that
/// does, so it is `where` that lets it through.
#[test]
fn the_builder_subset_rule_denies_the_lane_in_a_ticketed_checkout_alone() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/lanes.builder-subset");
    let ticketed = checkout("builder-subset-ticketed", Some(1145));
    let main = checkout("builder-subset-main", None);
    let rule = |root: &Path, file: &str| {
        let judged = judge(&manifest, root, &call(&dir.join(file))).unwrap();
        judged.denial().map(|f| f.rule.clone())
    };
    let denied = Some("lanes.builder-subset".to_owned());
    assert_eq!(rule(ticketed.path(), "deny.json"), denied);
    assert_eq!(rule(ticketed.path(), "allow.json"), None);
    assert_eq!(rule(ticketed.path(), "allow-zend.json"), None);
    assert_eq!(rule(main.path(), "allow-main.json"), None);
    assert_eq!(rule(ticketed.path(), "allow-main.json"), denied);
    // Through `cargo run` on the package or a variable the line assigns,
    // the lane is still run; the subset after `=` is still named.
    assert_eq!(rule(ticketed.path(), "deny-run.json"), denied);
    assert_eq!(rule(ticketed.path(), "deny-variable.json"), denied);
    assert_eq!(rule(ticketed.path(), "allow-equals.json"), None);
    // A message whose subject or body cites the instrument, under cargo
    // or not, runs nothing (#790, #1771).
    assert_eq!(rule(ticketed.path(), "allow-heredoc.json"), None);
    assert_eq!(rule(ticketed.path(), "allow-message-cargo.json"), None);
    // Reading the instrument's options runs no lane, and the exception is
    // the instrument's own flag, not a later `-h` (#1424).
    assert_eq!(rule(ticketed.path(), "allow-help.json"), None);
    assert_eq!(rule(ticketed.path(), "deny-help-elsewhere.json"), denied);
}

/// `lanes.nightly`'s heredoc fixtures (#790): a commit whose message has a
/// line opening `run-tests.php:2950)` cites upstream's runner and runs
/// nothing, so it passes; the runner first in the call, or after a
/// separator, is still denied, and so is its run under php.
#[test]
fn the_nightly_rule_denies_the_runner_where_it_is_run_and_not_a_message_line() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/lanes.nightly");
    let quiet = checkout("nightly-quiet", None);
    let rule = |file: &str| {
        let judged = judge(&manifest, quiet.path(), &call(&dir.join(file))).unwrap();
        judged.denial().map(|f| f.rule.clone())
    };
    let denied = Some("lanes.nightly".to_owned());
    assert_eq!(rule("allow-heredoc.json"), None);
    assert_eq!(rule("deny-first.json"), denied);
    assert_eq!(rule("deny-separator.json"), denied);
    assert_eq!(rule("deny.json"), denied);
    // Flags before the runner, or a binary named by a variable, still run
    // it; reading its source does not (#1446).
    assert_eq!(rule("deny-flags.json"), denied);
    assert_eq!(rule("deny-variable.json"), denied);
    assert_eq!(rule("allow-read.json"), None);
}

/// A `Bash` rule judges what a command runs, not text quoted inside it
/// (#1771, #1832): a heredoc that cites the nightly lane's command, as a
/// ticket's draft or a program on python's stdin, passes
/// `lanes.nightly-flags`; the same command run after the heredoc closes,
/// or in a body a shell reads as the commands it runs, is denied.
#[test]
fn the_nightly_rule_judges_what_the_call_runs_and_not_a_heredoc_that_cites_it() {
    let manifest = Manifest::load(workspace()).unwrap();
    let dir = fixtures().join("rules/lanes.nightly-flags");
    let rule = |file: &str| {
        let judged = judge(&manifest, workspace(), &call(&dir.join(file))).unwrap();
        judged.denial().map(|f| f.rule.clone())
    };
    let denied = Some("lanes.nightly-flags".to_owned());
    assert_eq!(rule("allow-heredoc-lane.json"), None);
    assert_eq!(rule("allow-heredoc-python.json"), None);
    assert_eq!(rule("deny-after-heredoc.json"), denied);
    assert_eq!(rule("deny-shell-heredoc.json"), denied);
    assert_eq!(rule("deny-heredoc-piped.json"), denied);
}
