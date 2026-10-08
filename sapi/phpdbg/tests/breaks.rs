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

/// A function, a method and a file's own code, each with oplines.
const OPLINES: &str = "<?php\nfunction foo() {\n  $a = 1;\n  return $a;\n}\nclass A { function b() { return 2; } }\nfoo();\n(new A)->b();\n";

/// `b f.php:#N` keeps the file as typed (`phpdbg_set_breakpoint_file_opline`,
/// `Z/sapi/phpdbg/phpdbg_bp.c:738`): a relative name is pending under
/// that name and never resolves against the compiled `/abs/f.php`. An
/// absolute one at a prompt with a frame of that file's code executing
/// is resolved at once (`phpdbg_resolve_opline_break`, `:591-603`), and
/// resolved again before each opline of that code.
#[test]
fn a_file_opline_breakpoint_keeps_its_name_and_resolves_in_an_executing_frame() {
    let dir = laid("file-opline", &[("f.php", OPLINES)]);
    let d = dir.display();
    assert_eq!(
        debugged(&dir, "f.php", "b f.php:#0\nr\nq\n"),
        format!(
            "[Successful compilation of {d}/f.php]\n\
             prompt> [Pending breakpoint #0 at f.php:0]\n\
             prompt> [Script ended normally]\n\
             prompt> "
        )
    );
    assert_eq!(
        addresses_cut(&debugged(
            &dir,
            "f.php",
            "b foo\nr\nb {dir}/f.php:#0\nc\nq\n"
        )),
        format!(
            "[Successful compilation of {d}/f.php]\n\
             prompt> [Breakpoint #0 added at foo]\n\
             prompt> [Breakpoint #0 in foo() at {d}/f.php:3, hits: 1]\n\
             >00003:   $a = 1;\n \
             00004:   return $a;\n \
             00005: }}\n\
             prompt> [Breakpoint #1 added at {d}/f.php:0]\n\
             prompt> [Breakpoint #1 resolved at {d}/f.php#0 (opline #N)]\n\
             [Breakpoint #1 resolved at {d}/f.php#0 (opline #N)]\n\
             [Breakpoint #1 resolved at {d}/f.php#0 (opline #N)]\n\
             [Breakpoint #1 resolved at {d}/f.php#0 (opline #N)]\n\
             [Script ended normally]\n\
             prompt> "
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// php keeps a function, method or file opline breakpoint, once
/// resolved, in `PHPDBG_BREAK_OPLINE` too, and `info break` lists it
/// under `Opline Breakpoints` with its address and the kind it was
/// resolved from (`phpdbg_print_breakpoints`), above the named tables.
#[test]
fn info_break_lists_resolved_opline_breakpoints_under_the_opline_table() {
    let dir = laid("info-oplines", &[("f.php", OPLINES)]);
    let d = dir.display();
    assert_eq!(
        addresses_cut(&debugged(
            &dir,
            "f.php",
            "b foo\nr\nb foo#0\nb A::b#0\nb {dir}/f.php:#0\ni b\nq\n"
        )),
        format!(
            "[Successful compilation of {d}/f.php]\n\
             prompt> [Breakpoint #0 added at foo]\n\
             prompt> [Breakpoint #0 in foo() at {d}/f.php:3, hits: 1]\n\
             >00003:   $a = 1;\n \
             00004:   return $a;\n \
             00005: }}\n\
             prompt> [Breakpoint #1 added at foo#0]\n\
             prompt> [Breakpoint #2 added at A::b#0]\n\
             prompt> [Breakpoint #3 added at {d}/f.php:0]\n\
             prompt> ------------------------------------------------\n\
             Function Breakpoints:\n\
             #0\t\tfoo\n\
             ------------------------------------------------\n\
             Opline Breakpoints:\n\
             #1\t\t#N\t\t(function breakpoint)\n\
             #2\t\t#N\t\t(method breakpoint)\n\
             #3\t\t#N\t\t(file breakpoint)\n\
             ------------------------------------------------\n\
             File opline Breakpoints:\n\
             #3\t\t{d}/f.php opline 0\n\
             ------------------------------------------------\n\
             Function opline Breakpoints:\n\
             #1\t\tfoo opline 0\n\
             ------------------------------------------------\n\
             Method opline Breakpoints:\n\
             #2\t\tA::b opline 0\n\
             prompt> "
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A second opline breakpoint on one opline of one function or method
/// is refused after its notice and its number given back
/// (`Z/sapi/phpdbg/phpdbg_bp.c:676-682`, `:719-725`): nothing is added
/// to the named tables, while the opline table, which php updated before
/// it refused, keeps the refused number under a resolved opline.
#[test]
fn a_second_opline_breakpoint_on_one_opline_is_refused() {
    let dir = laid("dup-oplines", &[("f.php", OPLINES)]);
    let d = dir.display();
    assert_eq!(
        addresses_cut(&debugged(
            &dir,
            "f.php",
            "b foo#0\nb foo#0\nb baz#0\nb baz#0\nb A::b#0\nb A::b#0\ni b\nq\n"
        )),
        format!(
            "[Successful compilation of {d}/f.php]\n\
             prompt> [Breakpoint #0 added at foo#0]\n\
             prompt> [Breakpoint #1 added at foo#0]\n\
             [Breakpoint already exists for foo#0]\n\
             prompt> [Pending breakpoint #1 at baz#0]\n\
             prompt> [Pending breakpoint #2 at baz#0]\n\
             [Breakpoint already exists for baz#0]\n\
             prompt> [Breakpoint #2 added at A::b#0]\n\
             prompt> [Breakpoint #3 added at A::b#0]\n\
             [Breakpoint already exists for A::b#0]\n\
             prompt> ------------------------------------------------\n\
             Opline Breakpoints:\n\
             #1\t\t#N\t\t(function breakpoint)\n\
             #3\t\t#N\t\t(method breakpoint)\n\
             ------------------------------------------------\n\
             Function opline Breakpoints:\n\
             #0\t\tfoo opline 0\n\
             #1\t\tbaz opline 0\n\
             ------------------------------------------------\n\
             Method opline Breakpoints:\n\
             #2\t\tA::b opline 0\n\
             prompt> "
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}
