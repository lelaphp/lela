//! A child started with no `$cwd` inherits the process's working
//! directory where the request stands in it, as php's does, since php
//! adds no `chdir` to the spawn then (`Z/ext/standard/proc_open.c:1394`),
//! and `exec(2)` needs no access to the directory it is called in.
//! Where the request stands elsewhere, the child is still
//! changed into the request's directory. A request given no directory,
//! its host's `getcwd(3)` having failed, stands in the process's.
//!
//! This binary holds one test: it moves the process's working directory,
//! which every test of a binary shares, so no other test may stand
//! beside it.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use php_ext::test::{Capture, Engine, Files};

/// Every way a request starts a child with no `$cwd`: `proc_open` of an
/// array and of a string, `exec`, `shell_exec`, `system`, `passthru` and
/// `popen`, each printing its name from the child, or `refused` where
/// the spawn failed.
const EVERY_SPAWN: &str = r#"<?php
foreach ([['/bin/echo', 'proc_open array'], 'echo proc_open string'] as $command) {
    $p = @proc_open($command, [1 => ['pipe', 'w']], $pipes);
    echo $p === false ? "refused\n" : stream_get_contents($pipes[1]);
    if ($p !== false) { proc_close($p); }
}
echo exec('echo exec'), "\n";
echo shell_exec('echo shell_exec');
system('echo system');
passthru('echo passthru');
$h = popen('echo popen', 'r');
echo fread($h, 100);
pclose($h);
"#;

/// Where a child with no `$cwd` stands: the shell's `pwd -P` and
/// `proc_open`'s `/bin/pwd -P`.
const WHERE: &str = r#"<?php
echo exec('pwd -P'), "\n";
$p = proc_open(['/bin/pwd', '-P'], [1 => ['pipe', 'w']], $pipes);
echo stream_get_contents($pipes[1]);
proc_close($p);
"#;

fn served(engine: &Engine, dir: &Path, source: &str) -> (String, Option<String>) {
    let served = engine.serve_capture_in(dir, b"Standard input code", source, Capture::default());
    (served.stdout_text(), served.fatal)
}

#[test]
fn a_child_inherits_the_process_s_directory_it_cannot_enter_and_is_moved_into_another() {
    // Canonical, as `getcwd(3)` and `pwd -P` name it: macOS's is under a
    // link, `/var` to `/private/var`.
    let files = Files::new("standard-process-dir", &[]);
    let base = files.path().canonicalize().expect("the tree resolves");
    let (locked, other) = (base.join("locked"), base.join("other"));
    fs::create_dir(&locked).expect("the tree takes the locked directory");
    fs::create_dir(&other).expect("the tree takes the other directory");
    let engine = support::engine();
    let before = std::env::current_dir();
    // The process stands in `locked`, which its own user may then not
    // enter, read or search; the request stands there too.
    let entered = std::env::set_current_dir(&locked).is_ok()
        && fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).is_ok();
    let in_locked = served(&engine, &locked, EVERY_SPAWN);
    // A request its host could give no directory, `getcwd(3)` having
    // failed under the CLI in a directory like this one, stands in the
    // process's.
    let in_none = served(&engine, Path::new(""), EVERY_SPAWN);
    let in_other = served(&engine, &other, WHERE);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))
        .expect("the locked directory is opened again");
    if let Ok(before) = before {
        std::env::set_current_dir(before).ok();
    }

    assert!(entered, "the process entered the directory and locked it");
    let every = "proc_open array\nproc_open string\nexec\nshell_exec\nsystem\npassthru\npopen\n";
    assert_eq!(in_locked, (every.to_owned(), None));
    assert_eq!(in_none, (every.to_owned(), None));
    let other = other.display();
    assert_eq!(in_other, (format!("{other}\n{other}\n"), None));
}
