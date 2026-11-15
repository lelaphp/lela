//! What a process a request execs holds, held to what the oracle
//! at 8.5.9 lists under `php -n` with stdin on `/dev/null`: past fds 0,
//! 1 and 2, the program's own file, which `cli_seek_file_begin` opens
//! with `fopen` and no `O_CLOEXEC` and holds while the program runs, and
//! a file `fopen()` opened without `e` and has not closed. A file opened
//! with `e`, one closed, and an included one, which php closes once it
//! has compiled it, are not held; nor is the program's file once the
//! program has ended, since `do_cli` destroys its `zend_file_handle` before
//! `php_request_shutdown` calls the shutdown functions.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// The names of the descriptors past the three a `shell_exec` lists, by
/// the inode each opens; what this test's own process leaves open past
/// its stdio, a harness's jobserver pipe, reaches `php` and every process
/// after it under either binary, and is left out.
fn held(listed: &str, ours: &[u64], named: &[(u64, &'static str)]) -> Vec<&'static str> {
    listed
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let ino: u64 = words.next()?.parse().ok()?;
            let fd: u32 = words.next()?.strip_prefix("/dev/fd/")?.parse().ok()?;
            if fd < 3 || ours.contains(&ino) {
                return None;
            }
            Some(
                named
                    .iter()
                    .find(|(at, _)| *at == ino)
                    .map_or("other", |(_, name)| name),
            )
        })
        .collect()
}

/// The inodes of what this test's process leaves open past its stdio,
/// as a process it execs inherits them: listed by a child shell, since
/// stat-ing `/dev/fd` in this process while another test thread spawns
/// races the kernel's fd table, and macOS 15.6 panicked on it.
fn inherited() -> Vec<u64> {
    Command::new("/bin/sh")
        .args([
            "-c",
            "for f in $(/usr/bin/seq 3 63); do [ -e /dev/fd/$f ] && /bin/ls -iLd /dev/fd/$f; done",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_process_a_request_execs_holds_the_program_and_what_fopen_left_open() {
    let dir = std::env::temp_dir().join(format!("php-exec-fds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("t.php");
    let data = dir.join("data.txt");
    let closed = dir.join("closed.txt");
    let flagged = dir.join("flagged.txt");
    let included = dir.join("inc.php");
    for path in [&data, &closed, &flagged] {
        std::fs::write(path, "x").unwrap();
    }
    std::fs::write(&included, "<?php $x = 1;").unwrap();
    std::fs::write(
        &script,
        "<?php
function listed() { return shell_exec('for f in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do [ -e /dev/fd/$f ] && /bin/ls -iLd /dev/fd/$f; done'); }
register_shutdown_function(function () { echo \"--\\n\", listed(); });
$f = fopen(__DIR__ . '/data.txt', 'r');
$e = fopen(__DIR__ . '/flagged.txt', 're');
$c = fopen(__DIR__ . '/closed.txt', 'r'); fclose($c);
include __DIR__ . '/inc.php';
echo listed();
",
    )
    .unwrap();
    let inode = |path: &Path| std::fs::metadata(path).map_or(0, |m| m.ino());
    let ours = inherited();
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let named = [
        (inode(&script), "the program"),
        (inode(&data), "data.txt"),
        (inode(&closed), "closed.txt"),
        (inode(&flagged), "flagged.txt"),
        (inode(&included), "inc.php"),
    ];
    std::fs::remove_dir_all(&dir).ok();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (running, shutting) = stdout.split_once("--\n").unwrap_or((&stdout, ""));
    assert_eq!(
        held(running, &ours, &named),
        ["the program", "data.txt"],
        "{stdout}"
    );
    assert_eq!(held(shutting, &ours, &named), ["data.txt"], "{stdout}");
}
