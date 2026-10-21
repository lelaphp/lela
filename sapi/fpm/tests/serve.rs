//! The binary tests its configuration as `-t` asks, and serves a request
//! handed to it as `FastCGI` records under php-fpm's name. What the
//! oracle's php-fpm writes for a pool's requests and log is held beside
//! it by `xtask witness`, from `tests/lines.toml` (#1975).

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use php_proto::fcgi::{self, Header, Kind};

/// The binary under test, let run as root with `-R`: as root php-fpm
/// refuses a pool with no `user` directive unless `--allow-to-run-as-root`
/// lets it, as the oracle's does, and a container's tests run as root.
fn php_fpm() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_php-fpm"));
    command.arg("-R");
    command
}

/// A directory of the test's own.
fn tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sapi-fpm-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("the test's directory is made");
    dir
}

/// What a master in `dir` logs to, the slow log its pool may name, and
/// where its stderr goes when its [`Pool`] asks.
const FPM_LOG: &str = "fpm.log";
const SLOW_LOG: &str = "slow.log";
const STDERR: &str = "stderr";

/// A configuration in `dir` of one static child listening on
/// [`socket`], logging to [`FPM_LOG`], with `more` after it.
fn conf(dir: &Path, more: &str) -> PathBuf {
    let path = dir.join("php-fpm.conf");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\n[www]\nlisten = {}\npm = static\npm.max_children = 1\n{more}",
            dir.join(FPM_LOG).display(),
            socket(dir).display()
        ),
    )
    .expect("the configuration is written");
    path
}

/// Where a pool in `dir` listens: a socket in the test's own directory,
/// which no other process names, so nothing else can listen there before
/// the master does (#1474). A loopback port found free and let go was
/// anyone's until the master bound it, and a request sent there reached
/// whatever took it.
fn socket(dir: &Path) -> PathBuf {
    dir.join("fpm.sock")
}

/// What a [`Master`] is started with: the directives its pool takes past
/// [`conf`]'s, with `{dir}` read as the test's directory; the `-d`
/// defines; whether its stderr goes to `stderr` in the directory rather
/// than nowhere; and whether it runs [`readable_php_fpm`], whose child
/// the master can read.
#[derive(Default)]
struct Pool<'a> {
    directives: &'a str,
    defines: &'a [&'a str],
    stderr: bool,
    readable: bool,
}

/// A php-fpm master in the foreground, `-n -F -y`, on a directory of
/// the test's own, which it owns before anything can panic: dropped, as a
/// test that panics drops it, it halts the master and removes the
/// directory, so no master outlives its test to hold a socket another
/// test's descriptors can see (#3093).
struct Master {
    dir: PathBuf,
    child: Option<Child>,
}

/// What a master logged, read once it has exited: [`FPM_LOG`] and
/// [`SLOW_LOG`] of its directory, empty where it wrote none.
struct Log {
    fpm: String,
    slow: String,
}

impl Master {
    /// The master of `pool`, started on the directory `name` gives.
    fn start(name: &str, pool: &Pool) -> Master {
        let mut master = Master {
            dir: tree(name),
            child: None,
        };
        let dir = master.dir.display().to_string();
        let path = conf(&master.dir, &pool.directives.replace("{dir}", &dir));
        let mut command = if pool.readable {
            readable_php_fpm(&master.dir)
        } else {
            php_fpm()
        };
        command.arg("-n");
        for define in pool.defines {
            command.args(["-d", define]);
        }
        let stderr = if pool.stderr {
            std::fs::File::create(master.dir.join(STDERR))
                .expect("the master's stderr is made")
                .into()
        } else {
            Stdio::null()
        };
        let child = command
            .args(["-F", "-y"])
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("php-fpm starts");
        master.child = Some(child);
        master
    }

    /// The test's directory, where the pool's script and logs are.
    fn dir(&self) -> &Path {
        &self.dir
    }

    /// The master's pid, while it runs.
    fn pid(&self) -> Option<Pid> {
        one_process(i64::from(self.child.as_ref()?.id()))
    }

    /// A connection to the pool, tried for ten seconds.
    fn connect(&self) -> UnixStream {
        let socket = socket(&self.dir);
        let started = Instant::now();
        loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => return stream,
                Err(refused) => {
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "nothing listens at {} after ten seconds: {refused}",
                        socket.display()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// The master sent `SIGTERM`, as a master finishing is, and what it
    /// logged once it exited.
    fn stop(self) -> Log {
        self.stop_with(Signal::SIGTERM)
    }

    /// The master sent `signal`, and what it logged once it exited.
    fn stop_with(mut self, signal: Signal) -> Log {
        self.halt(signal);
        let read = |file: &str| std::fs::read_to_string(self.dir.join(file)).unwrap_or_default();
        Log {
            fpm: read(FPM_LOG),
            slow: read(SLOW_LOG),
        }
    }

    /// The master sent `signal` and waited for, ten seconds at most; past
    /// them its process group, which its `setsid` made the pool's, is
    /// killed, and the master with it.
    fn halt(&mut self, signal: Signal) {
        let pid = self.pid();
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(pid) = pid {
            kill(pid, signal).ok();
        }
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if let Some(pid) = pid {
            killpg(pid, Signal::SIGKILL).ok();
        }
        child.kill().ok();
        child.wait().ok();
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.halt(Signal::SIGTERM);
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[test]
fn a_configuration_test_says_the_file_is_sound() {
    let dir = tree("test");
    let path = conf(&dir, "");
    let out = php_fpm()
        .args(["-n", "-t", "-y"])
        .arg(&path)
        .output()
        .expect("php-fpm -t runs");
    std::fs::remove_dir_all(&dir).ok();
    let said = String::from_utf8_lossy(&out.stderr);
    let expected = format!(
        "NOTICE: configuration file {} test is successful",
        path.display()
    );
    assert!(said.contains(&expected), "{out:?}");
    assert!(out.status.success(), "{out:?}");
}

/// With no `-y`, the file tested is `php-fpm.conf` under the engine's
/// `PHP_SYSCONFDIR`, the build's layout (#1404), named whether it opens
/// or not.
#[test]
fn with_no_file_named_the_configuration_is_the_builds_sysconfdirs() {
    let out = php_fpm()
        .args(["-n", "-t"])
        .output()
        .expect("php-fpm -t runs");
    let said = String::from_utf8_lossy(&out.stderr);
    let expected = format!("{}/php-fpm.conf", php_sapi::PHP_SYSCONFDIR);
    assert!(said.contains(&expected), "{out:?}");
}

/// One `FastCGI` record of request 1, padded to a multiple of eight bytes
/// as php's `fcgi_make_header` pads it.
fn record(kind: Kind, content: &[u8]) -> Vec<u8> {
    assert!(
        content.len() <= fcgi::MAX_LENGTH,
        "a record holds {} bytes at most, not {}",
        fcgi::MAX_LENGTH,
        content.len()
    );
    let header = Header::make(kind, 1, content.len());
    let mut out = header.encode().to_vec();
    out.extend_from_slice(content);
    out.resize(out.len() + usize::from(header.padding_length), 0);
    out
}

/// What request 1 writes on `FCGI_STDOUT`, read off `stream` to its end.
fn response(stream: UnixStream, script: &Path) -> Vec<u8> {
    let script = script.display().to_string();
    request(
        stream,
        &[
            ("SCRIPT_FILENAME", script.as_str()),
            ("REQUEST_METHOD", "GET"),
            ("QUERY_STRING", "q=1"),
        ],
    )
}

/// What request 1 with `pairs` as its parameters writes on
/// `FCGI_STDOUT`, read off `stream` to its end.
fn request(stream: UnixStream, pairs: &[(&str, &str)]) -> Vec<u8> {
    request_within(stream, pairs, Duration::from_secs(10))
}

/// [`request`], waiting up to `wait` for each read.
fn request_within(stream: UnixStream, pairs: &[(&str, &str)], wait: Duration) -> Vec<u8> {
    exchange(stream, pairs, wait).0
}

/// What request 1 with `pairs` as its parameters writes on
/// `FCGI_STDOUT` and on `FCGI_STDERR`, read off `stream` to its end,
/// waiting up to `wait` for each read.
fn exchange(mut stream: UnixStream, pairs: &[(&str, &str)], wait: Duration) -> (Vec<u8>, Vec<u8>) {
    let mut params = Vec::new();
    for &(name, value) in pairs {
        fcgi::put_pair(&mut params, name.as_bytes(), value.as_bytes());
    }
    let mut out = record(Kind::BeginRequest, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(Kind::Params, &params));
    out.extend(record(Kind::Params, &[]));
    out.extend(record(Kind::Stdin, &[]));
    stream.write_all(&out).expect("the request is written");
    stream
        .set_read_timeout(Some(wait))
        .expect("the reply's reads are timed");
    let (mut body, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let mut bytes = [0u8; 8];
        if stream.read_exact(&mut bytes).is_err() {
            return (body, stderr);
        }
        let header = Header::decode(bytes);
        let len = usize::from(header.content_length);
        let mut content = vec![0u8; len + usize::from(header.padding_length)];
        stream
            .read_exact(&mut content)
            .expect("the record's content follows its header");
        content.truncate(len);
        match header.kind {
            Kind::Stdout => body.extend_from_slice(&content),
            Kind::Stderr => stderr.extend_from_slice(&content),
            Kind::EndRequest => return (body, stderr),
            _ => {}
        }
    }
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
fn a_request_is_served_by_a_child_under_the_host_s_name() {
    let master = Master::start("serve", &Pool::default());
    let script = master.dir().join("index.php");
    std::fs::write(&script, "<?php echo PHP_SAPI, ' ', $_GET['q'];").expect("the script");
    let body = response(master.connect(), &script);
    master.stop();
    let body = String::from_utf8_lossy(&body);
    assert!(body.ends_with("\r\n\r\nfpm-fcgi 1"), "{body:?}");
}

/// A test that panics while its master serves leaves neither the master,
/// nor a child of its pool, nor its directory (#3093): the panic drops the
/// [`Master`], whose `Drop` halts the master and removes the directory.
/// The panic carries the master's pid and directory out; the pid names
/// the pool's process group too, which the master's `setsid` made, and a
/// `kill` of signal 0 to it finds no process left.
#[test]
fn a_test_that_panics_while_its_master_serves_leaves_no_master_and_no_directory() {
    let panicked = std::panic::catch_unwind(|| {
        let master = Master::start("panics", &Pool::default());
        drop(master.connect());
        let pid = master.pid().expect("the master runs");
        std::panic::panic_any((pid, master.dir().to_path_buf()));
    })
    .expect_err("the closure panics");
    let (pid, dir) = *panicked
        .downcast::<(Pid, PathBuf)>()
        .expect("the panic carries the master's pid and directory");
    assert_eq!(kill(pid, None), Err(nix::errno::Errno::ESRCH), "{pid}");
    assert_eq!(killpg(pid, None), Err(nix::errno::Errno::ESRCH), "{pid}");
    assert!(!dir.exists(), "{}", dir.display());
}

/// What a request answered and the line the master logged for the child
/// that served it, the child sent `SIGQUIT` on its own, as a master
/// finishing sends it, while the request runs (`busy`) or once it is
/// served and the child waits in `accept`.
fn quit_child(name: &str, busy: bool) -> (String, String) {
    let master = Master::start(name, &Pool::default());
    let pid = master.dir().join("pid");
    let script = master.dir().join("index.php");
    std::fs::write(
        &script,
        format!(
            "<?php file_put_contents('{}', getmypid()); {} echo 'served';",
            pid.display(),
            if busy { "usleep(500000);" } else { "" }
        ),
    )
    .expect("the script");
    let stream = master.connect();
    let served = std::thread::spawn(move || response(stream, &script));
    let started = Instant::now();
    let mut child = None;
    while child.is_none() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
        child = std::fs::read_to_string(&pid)
            .ok()
            .and_then(|written| written.trim().parse().ok())
            .and_then(one_process);
    }
    let child = child.expect("the child wrote its pid, one process's, within ten seconds");
    let body = if busy {
        quit(child);
        served.join().expect("the request's thread")
    } else {
        let body = served.join().expect("the request's thread");
        quit(child);
        body
    };
    let log = master.dir().join(FPM_LOG);
    let exited = format!("child {child} exited");
    let started = Instant::now();
    let mut line = None;
    while line.is_none() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
        line = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .find(|l| l.contains(&exited))
            .and_then(|l| l.split_once("] "))
            .map(|(_, rest)| rest.to_owned());
    }
    master.stop();
    let body = String::from_utf8_lossy(&body);
    let body = body
        .rsplit("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_owned();
    (body, line.unwrap_or_default())
}

/// `raw` as the pid of one process, `None` for 0 or below, which
/// `kill(2)` reads as the test runner's own process group, or every
/// process it may signal, or past what a pid holds.
fn one_process(raw: i64) -> Option<Pid> {
    i32::try_from(raw)
        .ok()
        .filter(|&pid| pid > 0)
        .map(Pid::from_raw)
}

/// `SIGQUIT` sent to the child `pid`, as a master finishing sends it.
fn quit(pid: Pid) {
    kill(pid, Signal::SIGQUIT).expect("the child is sent SIGQUIT");
}

/// A child sent `SIGQUIT` as it waits in `accept` ends on its own with
/// code 0, as the oracle's does, and never aborts (#2400): the listening
/// socket its handler closes is closed by nothing else.
#[test]
fn a_child_sent_sigquit_while_it_waits_exits_with_code_0() {
    let (body, line) = quit_child("quit-idle", false);
    assert_eq!(body, "served");
    assert!(
        line.starts_with("NOTICE: [pool www] child ")
            && line.contains(" exited with code 0 after "),
        "{line}"
    );
}

/// A child sent `SIGQUIT` mid-request serves the request, then ends with
/// code 0, as the oracle's does (#2400).
#[test]
fn a_child_sent_sigquit_mid_request_serves_it_and_exits_with_code_0() {
    let (body, line) = quit_child("quit-busy", true);
    assert_eq!(body, "served");
    assert!(
        line.starts_with("NOTICE: [pool www] child ")
            && line.contains(" exited with code 0 after "),
        "{line}"
    );
}

#[test]
fn a_request_body_file_that_does_not_open_is_a_warning_the_pool_logs() {
    let pool = Pool {
        directives: "catch_workers_output = yes\nphp_admin_flag[log_errors] = on\n",
        ..Pool::default()
    };
    let master = Master::start("body-file", &pool);
    let script = master.dir().join("index.php");
    std::fs::write(&script, "<?php echo 'ran';").expect("the script");
    let script = script.display().to_string();
    let missing = master.dir().join("missing").display().to_string();
    let body = request(
        master.connect(),
        &[
            ("SCRIPT_FILENAME", script.as_str()),
            ("REQUEST_METHOD", "POST"),
            ("CONTENT_TYPE", "application/x-www-form-urlencoded"),
            ("CONTENT_LENGTH", "3"),
            ("REQUEST_BODY_FILE", missing.as_str()),
        ],
    );
    let written = master.stop_with(Signal::SIGQUIT).fpm;
    let expected = format!(
        "said into stderr: \"NOTICE: PHP message: PHP Warning:  REQUEST_BODY_FILE: open('{missing}') failed: No such file or directory (2) in Unknown on line 0\""
    );
    assert!(written.contains(&expected), "{written}");
    assert!(String::from_utf8_lossy(&body).ends_with("ran"), "{body:?}");
}

/// #1807: the fork stress of the engine's execution timer
/// (`crates/php_engine/src/interrupt.rs`, which the CLI serves), served
/// by php-fpm's child: eight workers each fork 1875 children straight
/// after a `set_time_limit()`, and each child arms a timer of its own,
/// whose thread a child forked beside a thread that held the standard
/// library's thread table could not start. A forked process of php-fpm's
/// child that ends its request goes back to the child's loop, as php's
/// does, so the children and the workers leave by `exec` rather than by
/// `exit`, so a child's own request end is the CLI's fence to hold, not
/// this one; a child not gone within five seconds is hung, and killed. The
/// request itself is untimed, as the CLI's is, since php-fpm's
/// `max_execution_time` of thirty seconds would count the waits.
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn the_fork_stress_passes_under_a_child() {
    let master = Master::start("fork", &Pool::default());
    let script = master.dir().join("stress.php");
    std::fs::write(&script, STRESS).expect("the stress");
    let script = script.display().to_string();
    let pairs = [
        ("SCRIPT_FILENAME", script.as_str()),
        ("REQUEST_METHOD", "GET"),
    ];
    let body = request_within(master.connect(), &pairs, Duration::from_mins(5));
    master.stop();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.ends_with(
            "\r\n\r\n0 of 15000 children hung, 0 did not fork or end with status 0\n\
             0 of 8 workers did not end with status 0\n"
        ),
        "{body:?}"
    );
}

/// The stress [`the_fork_stress_passes_under_a_child`] serves.
const STRESS: &str = r#"<?php
set_time_limit(0);
$leave = fn () => pcntl_exec('/bin/sh', ['-c', 'exit 0']);
$tally = [];
for ($w = 0; $w < 8; $w++) {
    $tally[$w] = tempnam(sys_get_temp_dir(), 'timer');
    if (($workers[$w] = pcntl_fork()) !== 0) { continue; }
    $hung = $otherwise = 0;
    for ($forked = 0; $forked < 1875; $forked += 25) {
        $children = [];
        for ($i = 0; $i < 25; $i++) {
            set_time_limit(30);
            $child = pcntl_fork();
            if ($child === 0) { set_time_limit(30); $leave(); exit(1); }
            if ($child < 0) { $otherwise++; } else { $children[$child] = true; }
        }
        $deadline = microtime(true) + 5;
        while ($children && microtime(true) < $deadline) {
            $gone = pcntl_waitpid(-1, $status, WNOHANG);
            if ($gone <= 0) { usleep(1000); continue; }
            unset($children[$gone]);
            if (!pcntl_wifexited($status) || pcntl_wexitstatus($status) !== 0) { $otherwise++; }
        }
        foreach ($children as $child => $_) {
            posix_kill($child, SIGKILL);
            pcntl_waitpid($child, $status);
            $hung++;
        }
    }
    file_put_contents($tally[$w], "$hung $otherwise");
    $leave();
    exit(1);
}
$hung = $otherwise = $workers_otherwise = 0;
foreach ($workers as $w => $worker) {
    pcntl_waitpid($worker, $status);
    if (!pcntl_wifexited($status) || pcntl_wexitstatus($status) !== 0) { $workers_otherwise++; }
    [$h, $o] = explode(' ', file_get_contents($tally[$w])) + [0, 0];
    $hung += (int) $h;
    $otherwise += (int) $o;
    unlink($tally[$w]);
}
echo "$hung of 15000 children hung, $otherwise did not fork or end with status 0\n";
echo "$workers_otherwise of 8 workers did not end with status 0\n";
"#;

/// What a process a request execs holds (#2428, #2458), each descriptor
/// named by what it is: the oracle's php-fpm, a static pool of one child
/// with its master's stderr on a file, lists `0` `/dev/null`, `1` the
/// pipe `shell_exec` reads, `2` `/dev/null`, `5` the master's stderr,
/// which `fpm_stdio_save_original_stderr` dup'd without `FD_CLOEXEC` and
/// no child closes, and, for the first request, `7` the request's script,
/// which `php_fopen_primary_script`'s `zend_stream_open` opened with no
/// `O_CLOEXEC` and which stays open through the shutdown functions, since
/// `fpm_main` leaves the handle the scanner listed to `shutdown_executor`.
/// The second request's script is opcache's, held from the first under
/// `opcache.file_update_protection=0`, which caches a file written this
/// second, and `persistent_stream_open_function` answers its open
/// without opening it, so no descriptor of it is listed. The accepted connection and the
/// listening socket are not among them: `fcgi_accept_request` and
/// `fpm_child_cloexec` set `FD_CLOEXEC` on both, as the child here does,
/// and the engine's other descriptors (`/dev/urandom`, opcache's lock
/// file in php) are close-on-exec in both. What this test's own process
/// leaves open past its stdio, a harness's jobserver pipe, reaches the
/// master and every process after it under either, and is left out.
#[test]
fn a_process_a_request_execs_holds_the_master_s_saved_stderr_the_script_and_no_socket() {
    use std::os::unix::fs::MetadataExt;
    let ours = inherited();
    let pool = Pool {
        defines: &["opcache.file_update_protection=0"],
        stderr: true,
        ..Pool::default()
    };
    let master = Master::start("exec-fds", &pool);
    let script = master.dir().join("index.php");
    std::fs::write(
        &script,
        "<?php
function listed() { return shell_exec('for f in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do [ -e /dev/fd/$f ] && /bin/ls -iLd /dev/fd/$f; done'); }
register_shutdown_function(function () { echo \"--\\n\", listed(); });
echo listed();
",
    )
    .expect("the script");
    let bodies: Vec<Vec<u8>> = (0..2)
        .map(|_| response(master.connect(), &script))
        .collect();
    let inode = |path: &Path| std::fs::metadata(path).map_or(0, |m| m.ino());
    let named = [
        (inode(Path::new("/dev/null")), "null"),
        (inode(&master.dir().join(STDERR)), "the master's stderr"),
        (inode(&script), "the script"),
    ];
    master.stop();
    let held = |listed: &str| -> Vec<&'static str> {
        listed
            .lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let ino: u64 = words.next()?.parse().ok()?;
                let fd: u32 = words.next()?.strip_prefix("/dev/fd/")?.parse().ok()?;
                if fd > 2 && ours.contains(&ino) {
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
    };
    let lists: Vec<(Vec<&str>, Vec<&str>)> = bodies
        .iter()
        .map(|body| {
            let body = String::from_utf8_lossy(body);
            let listed = body.split_once("\r\n\r\n").map_or(&*body, |(_, rest)| rest);
            let (running, shutting) = listed.split_once("--\n").unwrap_or((listed, ""));
            (held(running), held(shutting))
        })
        .collect();
    let first = vec!["null", "other", "null", "the master's stderr", "the script"];
    let second = vec!["null", "other", "null", "the master's stderr"];
    assert_eq!(
        lists,
        [(first.clone(), first), (second.clone(), second)],
        "{bodies:?}"
    );
}

/// The binary under test, where the master can read its child's memory:
/// on macOS a copy into `dir` signed ad hoc with
/// `com.apple.security.get-task-allow`, which lets a process of the same
/// user take the task port of a child that runs it (`task_for_pid`, as
/// php-fpm's master asks for it), since the build's own signature carries
/// no entitlement and the kernel refuses the port, as it refuses the
/// pinned php-fpm's; elsewhere the binary itself, whose master attaches
/// to its own child with `ptrace`.
fn readable_php_fpm(dir: &Path) -> Command {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_php-fpm"));
    if !cfg!(target_os = "macos") {
        let mut command = Command::new(binary);
        command.arg("-R");
        return command;
    }
    let copy = dir.join("php-fpm");
    std::fs::copy(&binary, &copy).expect("the binary is copied to be signed");
    let entitlements = dir.join("readable.plist");
    std::fs::write(
        &entitlements,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict><key>com.apple.security.get-task-allow</key><true/></dict></plist>\n",
    )
    .expect("the entitlements are written");
    let signed = Command::new("codesign")
        .args(["-s", "-", "-f", "--entitlements"])
        .arg(&entitlements)
        .arg(&copy)
        .output()
        .expect("codesign runs");
    assert!(signed.status.success(), "{signed:?}");
    let mut command = Command::new(copy);
    command.arg("-R");
    command
}

/// The request [`slow_frames`] serves unless told otherwise: a sleep two
/// calls under a method of a class an included file declares.
const NESTED: &[(&str, &str)] = &[
    (
        "slow.php",
        "<?php\nfunction f($n) { usleep(3000000); }\nfunction g() { f(1); }\ninclude __DIR__ . '/inc.php';\necho 'done';\n",
    ),
    (
        "inc.php",
        "<?php\nclass K { function m() { g(); } }\n(new K)->m();\n",
    ),
];

/// The frame lines of the slow log entry of one request past
/// `request_slowlog_timeout` for `slow.php` of `files`, which ends by
/// echoing `done`, served by a pool of `name` whose configuration ends in
/// `more`, each past its sixteen-digit room address, and the test
/// directory as the engine resolved it; the entry's head is held here.
fn slow_frames(name: &str, more: &str, files: &[(&str, &str)]) -> (Vec<String>, PathBuf) {
    let directives = format!("slowlog = {{dir}}/{SLOW_LOG}\nrequest_slowlog_timeout = 1\n{more}");
    let pool = Pool {
        directives: &directives,
        readable: true,
        ..Pool::default()
    };
    let master = Master::start(name, &pool);
    for (file, text) in files {
        std::fs::write(master.dir().join(file), text).expect("the program's files");
    }
    let script = master.dir().join("slow.php");
    let shown = script.display().to_string();
    let pairs = [
        ("SCRIPT_FILENAME", shown.as_str()),
        ("REQUEST_METHOD", "GET"),
    ];
    let body = request_within(master.connect(), &pairs, Duration::from_secs(20));
    // The op arrays' files are the paths the engine opened, resolved.
    let real = std::fs::canonicalize(master.dir()).expect("the test's directory resolves");
    let Log {
        fpm: log,
        slow: logged,
    } = master.stop();
    assert!(String::from_utf8_lossy(&body).ends_with("done"), "{body:?}");
    let mut lines = logged.lines();
    assert_eq!(lines.next(), Some(""), "{logged}\n{log}");
    let head = lines.next().unwrap_or_default();
    assert!(head.contains("] [pool www] pid "), "{logged}\n{log}");
    assert_eq!(
        lines.next(),
        Some(format!("script_filename = {shown}").as_str()),
        "{logged}"
    );
    let frames = lines
        .map(|line| {
            let address = line.get(3..19).unwrap_or_default();
            assert!(
                line.starts_with("[0x")
                    && line.get(19..21) == Some("] ")
                    && address.chars().all(|c| c.is_ascii_hexdigit()),
                "{logged}"
            );
            line[21..].to_string()
        })
        .collect();
    (frames, real)
}

/// A request past `request_slowlog_timeout` whose child the master can
/// read is logged with its frames after the head, as php's
/// `fpm_php_trace_dump` writes them (#1301): one line per frame from the
/// innermost, its address, the name of what it serves, a method's alone,
/// `[INCLUDE_OR_EVAL]` for an included file, and the file and line it
/// was called from; the main script's frame ends the walk. The engine
/// lays the chain the master reads in its frames' rooms (design §5.6,
/// revision 59), and the child publishes where it starts in its
/// scoreboard slot.
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_slow_request_s_frames_follow_the_head_where_the_child_is_readable() {
    let (frames, real) = slow_frames("slowlog", "", NESTED);
    let file = real.join("slow.php").display().to_string();
    let inc = real.join("inc.php").display().to_string();
    assert_eq!(
        frames,
        [
            format!("usleep() {file}:2"),
            format!("f() {file}:3"),
            format!("g() {inc}:2"),
            format!("m() {inc}:3"),
            format!("[INCLUDE_OR_EVAL]() {file}:4"),
        ]
    );
}

/// A request slow inside a generator resumed through a `yield from`:
/// php links the running generator's frame to the placeholder the
/// generator the loop resumed holds, `execute_fake`, whose `func` is
/// `NULL` (`Z/Zend/zend_generators.c:807`), and the placeholder to the
/// frame that resumed it, so `fpm_php_trace_dump` writes `???()` for the
/// placeholder, with the line of the loop below it, and the running
/// frame's line is `unknown:0`, its walk below stopping at the
/// placeholder (#2897).
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_slow_request_in_a_delegated_generator_writes_php_s_placeholder_line() {
    let program = [(
        "slow.php",
        "<?php\nfunction inner() { usleep(3000000); yield 1; }\nfunction outer() { yield from inner(); }\n\
         foreach (outer() as $v) {}\necho 'done';\n",
    )];
    let (frames, real) = slow_frames("slowlog-fake", "", &program);
    let file = real.join("slow.php").display().to_string();
    assert_eq!(
        frames,
        [
            format!("usleep() {file}:2"),
            "inner() unknown:0".to_string(),
            format!("???() {file}:4"),
        ]
    );
}

/// `request_slowlog_trace_depth` cuts the walk at that many frames, the
/// innermost first, as php's `--callers_limit` does.
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_slow_request_s_frames_stop_at_the_trace_depth() {
    let (frames, real) = slow_frames("slowlog-depth", "request_slowlog_trace_depth = 2\n", NESTED);
    let file = real.join("slow.php").display().to_string();
    assert_eq!(
        frames,
        [format!("usleep() {file}:2"), format!("f() {file}:3")]
    );
}

/// A request asleep in a fiber: the fiber's first frame is linked to the
/// frame of the native that resumed it, as `zend_fiber_resume` writes
/// `fiber->stack_bottom->prev_execute_data`, so the walk runs from the
/// sleep through the fiber's callable into `Fiber::start` and the main
/// script. php's own walk meets the fiber's zeroed bottom frame there,
/// whose `function_name` is NULL and whose call kind is no kind it names
/// (`ZEND_UNREACHABLE`), so this line has no oracle (#2897).
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_slow_request_s_frames_run_through_a_fiber_into_its_resumer() {
    let script = "<?php\n$fiber = new Fiber(function () {\n    usleep(3000000);\n});\n$fiber->start();\necho 'done';\n";
    let (frames, real) = slow_frames("slowlog-fiber", "", &[("slow.php", script)]);
    let file = real.join("slow.php").display().to_string();
    assert_eq!(
        frames,
        [
            format!("usleep() {file}:3"),
            format!("{{closure:{file}:2}}() {file}:5"),
            format!("start() {file}:5"),
        ]
    );
}
