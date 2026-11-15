//! The binary is php's debugger: the file its line names compiled and
//! the prompt read off stdin. What the pinned phpdbg writes for a session
//! is held beside it by `xtask witness`, from `tests/lines.toml`;
//! what is held here is a relation within one session, not bytes.

use std::io::Write;
use std::process::{Command, Stdio};

/// What the binary wrote for `commands` over a program of `source`.
fn debugged(name: &str, source: &str, commands: &str) -> String {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let file = dir.join("t.php");
    std::fs::write(&file, source).unwrap();
    let file = std::fs::canonicalize(&file).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(["-qIb", "-n"])
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `clean` counts `EG(class_table)`'s keys: an alias's among them, the
/// engine's and `class_alias()`'s, and the iterator wrapper php never
/// adds not: what `get_declared_classes()`, `get_declared_interfaces()`
/// and `get_declared_traits()` list together, read through the native
/// face's linked tables.
#[test]
fn clean_counts_the_class_tables_keys() {
    let out = debugged(
        "clean-classes",
        "<?php\nclass A {}\ninterface I {}\ntrait T {}\nclass_alias('A', 'B');\nclass_alias('A', 'C');\necho count(get_declared_classes()) + count(get_declared_interfaces()) + count(get_declared_traits()), \"\\n\";\necho 1;\n",
        "b 8\nr\nclean\ny\nq\n",
    );
    let listed = out
        .lines()
        .find_map(|line| {
            line.strip_prefix("prompt> ")
                .filter(|rest| rest.parse::<u32>().is_ok())
        })
        .map(str::to_owned);
    let counted = out
        .lines()
        .find_map(|line| line.strip_prefix("Classes    "))
        .map(str::to_owned);
    assert!(listed.is_some(), "{out}");
    assert_eq!(listed, counted, "{out}");
}
