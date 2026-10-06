//! The binary under `-n -l`, held to what the oracle at 8.5.9 prints and
//! exits with for a file that parses, one that does not, one the compiler
//! warns of, one that is not there, and the loop over several.

use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("php-lint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join(name);
    std::fs::write(&path, source).ok();
    path
}

/// stdout, stderr and the exit code of `php -n -l <words>`.
fn lint(words: &[&PathBuf]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .arg("-l")
        .args(words)
        .output()
        .unwrap_or_else(|e| panic!("running php -l: {e}"));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn a_file_that_parses_is_phps_sentence_and_exit_0() {
    let ok = scratch("ok.php", "<?php\necho 1;\n");
    assert_eq!(
        lint(&[&ok]),
        (
            format!("No syntax errors detected in {}\n", ok.display()),
            String::new(),
            0
        )
    );
}

#[test]
fn a_file_that_does_not_parse_is_the_parse_error_then_phps_sentence_and_exit_255() {
    let bad = scratch("bad.php", "<?php\necho 1 +;\n");
    assert_eq!(
        lint(&[&bad]),
        (
            format!(
                "\nParse error: syntax error, unexpected token \";\" in {0} on line 2\nErrors parsing {0}\n",
                bad.display()
            ),
            String::new(),
            255
        )
    );
}

#[test]
fn a_deprecation_is_rendered_and_changes_nothing() {
    let deprecated = scratch("dep.php", "<?php\n(integer)1;\n");
    assert_eq!(
        lint(&[&deprecated]),
        (
            format!(
                "\nDeprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in {0} on line 2\nNo syntax errors detected in {0}\n",
                deprecated.display()
            ),
            String::new(),
            0
        )
    );
}

#[test]
fn every_word_left_on_the_line_is_linted_and_the_status_is_the_last_failure_s() {
    let ok = scratch("ok2.php", "<?php\necho 1;\n");
    let bad = scratch("bad2.php", "<?php\necho 1 +;\n");
    let missing = PathBuf::from("nope.php");
    assert_eq!(
        lint(&[&bad, &ok]),
        (
            format!(
                "\nParse error: syntax error, unexpected token \";\" in {0} on line 2\nErrors parsing {0}\nNo syntax errors detected in {1}\n",
                bad.display(),
                ok.display()
            ),
            String::new(),
            255
        )
    );
    assert_eq!(
        lint(&[&ok, &missing, &ok]),
        (
            format!(
                "No syntax errors detected in {0}\nNo syntax errors detected in {0}\n",
                ok.display()
            ),
            String::from("Could not open input file: nope.php\n"),
            1
        )
    );
}

/// stdout, stderr and the exit code of `php -n -d opcache.enable_cli=1 -d
/// opcache.opt_debug_level=0x10000 -l <file>`, the recipe's `dump` regime
/// with `-l` beside it, which is what `xtask compile-identity` runs both
/// binaries under.
fn dump(file: &PathBuf) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .arg("-d")
        .arg("opcache.enable_cli=1")
        .arg("-d")
        .arg("opcache.opt_debug_level=0x10000")
        .arg("-l")
        .arg(file)
        .output()
        .unwrap_or_else(|e| panic!("running php -l under the dump regime: {e}"));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Design §6.1: php prints its op arrays before the optimizer runs, on
/// the second stream, and `-l`'s own sentence on the first. The bytes
/// pinned here are the oracle's for this file at 8.5.9; `xtask
/// compile-identity` is what holds the two dumps to each other file by
/// file, and this test is what keeps the binary printing one at all.
#[test]
fn the_op_arrays_before_the_optimizer_stand_on_the_second_stream_under_the_ini_bit() {
    let source = scratch("dumped.php", "<?php\necho 1;\n");
    assert_eq!(
        dump(&source),
        (
            format!("No syntax errors detected in {}\n", source.display()),
            format!(
                "\n$_main:\n     \
                 ; (lines=2, args=0, vars=0, tmps=0)\n     \
                 ; (before optimizer)\n     \
                 ; {}:1-3\n     \
                 ; return  [] RANGE[0..0]\n\
                 0000 ECHO int(1)\n\
                 0001 RETURN int(1)\n",
                source.display()
            ),
            0
        )
    );
}

/// The same file with no `opcache.opt_debug_level` at all: php dumps
/// nothing, and neither does the engine.
#[test]
fn nothing_is_dumped_where_the_ini_does_not_ask_for_it() {
    let source = scratch("undumped.php", "<?php\necho 1;\n");
    assert_eq!(
        lint(&[&source]),
        (
            format!("No syntax errors detected in {}\n", source.display()),
            String::new(),
            0
        )
    );
}
