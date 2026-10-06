//! A process ends its engine, and a `pcntl_fork` child that returns ends
//! the copy the fork made, as php's child calls `php_module_shutdown` over
//! the persistent list it inherited: a `dba_popen` `cdb_make` file is
//! finished there, and again by the parent where the parent ends too.
//! Held to what the oracle at 8.5.9 prints under `php -n` for the same
//! two processes, the writer and a reader started after it (#1804).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The writer: one record inserted through a persistent link, then a
/// fork; the child ends as `child` says, the parent waits for it, prints
/// the file's size and ends as `parent` says.
const WRITER: &str = r#"<?php
[, $f, $child, $parent] = $argv;
$d = dba_popen($f, 'n', 'cdb_make');
dba_insert('key0', 'value0', $d);
$pid = pcntl_fork();
if ($pid === 0) {
    if ($child === 'kill') posix_kill(getmypid(), SIGKILL);
    if ($child === 'exit') exit(0);
    echo "child end\n";
    return;
}
pcntl_waitpid($pid, $status);
clearstatcache();
echo "after the child: ", filesize($f), "\n";
if ($parent === 'kill') posix_kill(getmypid(), SIGKILL);
"#;

/// The reader, the next process: the size and the value.
const READER: &str = r#"<?php
clearstatcache();
echo filesize($argv[1]), "\n";
var_dump(dba_fetch('key0', dba_open($argv[1], 'r', 'cdb')));
"#;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("php-engine-end-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir.join(name)
}

/// stdout of `php -n <script> <words>`.
fn php(script: &Path, words: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .arg(script)
        .args(words)
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What the writer prints with its child and parent ending as named,
/// then what the reader prints.
fn forked(name: &str, child: &str, parent: &str) -> String {
    let writer = scratch(&format!("{name}-writer.php"));
    let reader = scratch(&format!("{name}-reader.php"));
    let file = scratch(&format!("{name}.cdb"));
    std::fs::write(&writer, WRITER).ok();
    std::fs::write(&reader, READER).ok();
    std::fs::remove_file(&file).ok();
    let path = file.display().to_string();
    let mut out = php(&writer, &[&path, child, parent]);
    out.push_str(&php(&reader, &[&path]));
    for made in [&writer, &reader, &file] {
        std::fs::remove_file(made).ok();
    }
    out
}

/// The child's end finishes the file, which the parent sees at 2,082
/// bytes; the parent killed before its own end, the next process reads
/// the value.
#[test]
fn a_forked_child_s_end_finishes_a_persistent_cdb_make_file() {
    assert_eq!(
        forked("child", "return", "kill"),
        "child end\nafter the child: 2082\n2082\nstring(6) \"value0\"\n"
    );
    assert_eq!(
        forked("child-exit", "exit", "kill"),
        "after the child: 2082\n2082\nstring(6) \"value0\"\n"
    );
}

/// Both ends finish the file, as both of php's processes destroy the
/// persistent list: the parent's write of its hash table lands where the
/// child's left the shared offset, over the record, and the next process
/// reads nothing, as php's does.
#[test]
fn a_child_and_its_parent_each_finish_the_file_as_php_s_do() {
    assert_eq!(
        forked("both", "return", "end"),
        "child end\nafter the child: 2082\n2082\nbool(false)\n"
    );
}

/// A child killed before its end leaves the file to the parent's end.
#[test]
fn a_killed_child_leaves_the_file_to_the_parent_s_end() {
    assert_eq!(
        forked("killed", "kill", "end"),
        "after the child: 2066\n2082\nstring(6) \"value0\"\n"
    );
}
