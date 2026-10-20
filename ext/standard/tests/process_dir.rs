//! A child started with no `$cwd` inherits the process's working
//! directory where the request stands in it, as php's does, since php
//! adds no `chdir` to the spawn then (`Z/ext/standard/proc_open.c:1394`),
//! and `exec(2)` needs no access to the directory it is called in
//! (#2917). Where the request stands elsewhere, the child is still
//! changed into the request's directory.
//!
//! This binary holds one test: it moves the process's working directory,
//! which every test of a binary shares, so no other test may stand
//! beside it.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use php_ext::test::{Capture, Engine};

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
    (
        String::from_utf8_lossy(&served.stdout).into_owned(),
        served.fatal,
    )
}

#[test]
fn a_child_inherits_the_process_s_directory_it_cannot_enter_and_is_moved_into_another() {
    // Canonical, as `getcwd(3)` and `pwd -P` name it: macOS's is under a
    // link, `/var` to `/private/var`.
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    let base = temp.join(format!("php-process-dir-{}", std::process::id()));
    let (locked, other) = (base.join("locked"), base.join("other"));
    assert!(fs::create_dir_all(&locked).is_ok() && fs::create_dir_all(&other).is_ok());
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
        ])
        .build();
    let before = std::env::current_dir();
    // The process stands in `locked`, which its own user may then not
    // enter, read or search; the request stands there too.
    let entered = std::env::set_current_dir(&locked).is_ok()
        && fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).is_ok();
    let in_locked = served(&engine, &locked, EVERY_SPAWN);
    let in_other = served(&engine, &other, WHERE);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).ok();
    if let Ok(before) = before {
        std::env::set_current_dir(before).ok();
    }
    fs::remove_dir_all(&base).ok();

    assert!(entered, "the process entered the directory and locked it");
    assert_eq!(
        in_locked,
        (
            "proc_open array\nproc_open string\nexec\nshell_exec\nsystem\npassthru\npopen\n"
                .to_owned(),
            None
        )
    );
    let other = other.display();
    assert_eq!(in_other, (format!("{other}\n{other}\n"), None));
}
