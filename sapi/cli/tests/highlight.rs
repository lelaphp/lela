//! The binary under `-n -s` and `-n -w`, `PHP_CLI_MODE_HIGHLIGHT` and
//! `PHP_CLI_MODE_STRIP`, held to the bytes the oracle at 8.5.9 prints for
//! a file with inline HTML, a doc comment, a line comment, an
//! interpolated string and a heredoc; and to its two sentences for `-s`
//! and `-w` after `-r`, and its conflict for `-r` after them.

use std::path::PathBuf;
use std::process::Command;

/// The file: every colour `zend_highlight` has, and the heredoc close
/// `zend_strip` writes with the character after it.
const SOURCE: &str = "<html>\n<?php\n/** doc */\n$name = \"world\"; // greet\necho \"Hello {$name}\\t!\\n\";\n$t = <<<EOT\n  Dear $name,\n  EOT;\n\techo $t;\n?>\n</html>\n";

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("php-highlight-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("hl.php");
    std::fs::write(&path, SOURCE).ok();
    path
}

/// stdout, stderr and the exit code of `php -n <words>`.
fn php(words: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .args(words)
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn s_prints_the_file_as_zend_highlight_writes_it() {
    let file = scratch();
    let expected = concat!(
        "<pre><code style=\"color: #000000\">&lt;html&gt;\n",
        "<span style=\"color: #0000BB\">&lt;?php\n",
        "</span><span style=\"color: #FF8000\">/** doc */\n",
        "</span><span style=\"color: #0000BB\">$name </span><span style=\"color: #007700\">= </span><span style=\"color: #DD0000\">\"world\"</span><span style=\"color: #007700\">; </span><span style=\"color: #FF8000\">// greet\n",
        "</span><span style=\"color: #007700\">echo </span><span style=\"color: #DD0000\">\"Hello </span><span style=\"color: #007700\">{</span><span style=\"color: #0000BB\">$name</span><span style=\"color: #007700\">}</span><span style=\"color: #DD0000\">\\t!\\n\"</span><span style=\"color: #007700\">;\n",
        "</span><span style=\"color: #0000BB\">$t </span><span style=\"color: #007700\">= &lt;&lt;&lt;EOT\n",
        "</span><span style=\"color: #DD0000\">  Dear </span><span style=\"color: #0000BB\">$name</span><span style=\"color: #DD0000\">,\n",
        "</span><span style=\"color: #007700\">  EOT;\n",
        "    echo </span><span style=\"color: #0000BB\">$t</span><span style=\"color: #007700\">;\n",
        "</span><span style=\"color: #0000BB\">?&gt;\n",
        "</span>&lt;/html&gt;\n",
        "</code></pre>",
    );
    let path = file.to_string_lossy().into_owned();
    assert_eq!(php(&["-s", &path]), (expected.to_owned(), String::new(), 0));
}

#[test]
fn w_prints_the_file_as_zend_strip_writes_it_and_the_later_switch_wins() {
    let file = scratch();
    let expected = "<html>\n<?php\n $name = \"world\"; echo \"Hello {$name}\\t!\\n\"; $t = <<<EOT\n  Dear $name,\n  EOT;\necho $t; ?>\n</html>\n";
    let path = file.to_string_lossy().into_owned();
    assert_eq!(php(&["-w", &path]), (expected.to_owned(), String::new(), 0));
    assert_eq!(
        php(&["-s", "-w", &path]),
        (expected.to_owned(), String::new(), 0)
    );
}

#[test]
fn s_and_w_after_r_are_phps_sentences_and_r_after_them_the_conflict() {
    let refused = |stdout: &str| (stdout.to_owned(), String::new(), 1);
    assert_eq!(
        php(&["-r", "echo 1;", "-s"]),
        refused("Source highlighting only works for files.\n")
    );
    assert_eq!(
        php(&["-r", "echo 1;", "-w"]),
        refused("Source stripping only works for files.\n")
    );
    let conflict = "Either execute direct code, process stdin or use a file.\n";
    assert_eq!(php(&["-s", "-r", "echo 1;"]), refused(conflict));
    assert_eq!(php(&["-w", "-r", "echo 1;"]), refused(conflict));
}
