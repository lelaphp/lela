//! Breakpoints as the pinned phpdbg sets, resolves, refuses and lists
//! them (`Z/sapi/phpdbg/phpdbg_bp.c`), over programs of more than one
//! file, each test's bytes the pinned phpdbg's for `phpdbg -qIb -n`, the
//! addresses php prints cut.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A directory of `files`, each a name and its source, for the test
/// `name`, the path the binary names it by.
fn laid(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("sapi-phpdbg-breaks-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, source) in files {
        std::fs::write(dir.join(file), source).unwrap();
    }
    std::fs::canonicalize(&dir).unwrap()
}

/// What the binary wrote for `commands` over the program `main` of `dir`,
/// run from `dir`; `{dir}` in the commands is the directory's path.
fn debugged(dir: &PathBuf, main: &str, commands: &str) -> String {
    let commands = commands.replace("{dir}", &dir.display().to_string());
    let mut child = Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(["-qIb", "-n"])
        .arg(dir.join(main))
        .current_dir(dir)
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `out` with each number php prints for an opline's address, after
/// `#` and ten digits or more, cut to `N`.
fn addresses_cut(out: &str) -> String {
    let mut cut = String::new();
    let mut rest = out;
    while let Some(start) = rest.find('#') {
        let (head, tail) = rest.split_at(start + 1);
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        cut.push_str(head);
        if digits >= 10 {
            cut.push('N');
        } else {
            cut.push_str(&tail[..digits]);
        }
        rest = &tail[digits..];
    }
    cut.push_str(rest);
    cut
}

/// An included file of declarations alone, its code one `RETURN`.
const DECLARATIONS: &str = "<?php\nfunction bar() {\n  $b = 3;\n  return $b;\n}\n";

/// The file that includes it and calls what it declared.
const INCLUDER: &str = "<?php\ninclude __DIR__ . '/inc.php';\nbar();\n";

/// php runs an included file whose code is its one `RETURN` of a
/// constant only while `zend_execute_ex` is php's own
/// (`Z/Zend/zend_vm_def.h:6569-6572`); under phpdbg's it executes it, so
/// the loop steps that `RETURN` and a breakpoint on its line, or on its
/// opline, stops there.
#[test]
fn an_included_file_of_declarations_alone_is_stepped() {
    let dir = laid("decls", &[("inc.php", DECLARATIONS), ("g.php", INCLUDER)]);
    let d = dir.display();
    assert_eq!(
        debugged(&dir, "g.php", "b {dir}/inc.php:6\nr\nc\nq\n"),
        format!(
            "[Successful compilation of {d}/g.php]\n\
             prompt> [Breakpoint #0 added at {d}/inc.php:6]\n\
             prompt> [Breakpoint #0 at {d}/inc.php:6, hits: 1]\n\
             >00006: \n\
             prompt> [Script ended normally]\n\
             prompt> "
        )
    );
    assert_eq!(
        addresses_cut(&debugged(&dir, "g.php", "b {dir}/inc.php:#0\nr\nc\nq\n")),
        format!(
            "[Successful compilation of {d}/g.php]\n\
             prompt> [Pending breakpoint #0 at {d}/inc.php:0]\n\
             prompt> [Breakpoint #0 resolved at {d}/inc.php#0 (opline #N)]\n\
             [Breakpoint #0 in #0 at {d}/inc.php:6, hits: 1]\n\
             >00006: \n\
             prompt> [Script ended normally]\n\
             prompt> "
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}
