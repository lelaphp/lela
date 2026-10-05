//! The binary tests its configuration as `-t` asks, and serves a request
//! handed to it as `FastCGI` records under php-fpm's name.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A configuration of one static child listening at `listen`.
fn conf(dir: &Path, listen: &str) -> PathBuf {
    let path = dir.join("php-fpm.conf");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\n[www]\nlisten = {listen}\npm = static\npm.max_children = 1\n",
            dir.join("fpm.log").display()
        ),
    )
    .unwrap();
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

#[test]
fn a_configuration_test_says_the_file_is_sound() {
    let dir = tree("test");
    let path = conf(&dir, "127.0.0.1:9");
    let out = php_fpm()
        .args(["-n", "-t", "-y"])
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
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
    let out = php_fpm().args(["-n", "-t"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    let expected = format!("{}/php-fpm.conf", php_sapi::PHP_SYSCONFDIR);
    assert!(said.contains(&expected), "{out:?}");
}

/// One `FastCGI` record of request 1.
fn record(kind: u8, content: &[u8]) -> Vec<u8> {
    let len = u16::try_from(content.len()).unwrap().to_be_bytes();
    let mut out = vec![1, kind, 0, 1, len[0], len[1], 0, 0];
    out.extend_from_slice(content);
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
fn request_within(mut stream: UnixStream, pairs: &[(&str, &str)], wait: Duration) -> Vec<u8> {
    let mut params = Vec::new();
    for &(name, value) in pairs {
        params.push(u8::try_from(name.len()).unwrap());
        params.push(u8::try_from(value.len()).unwrap());
        params.extend_from_slice(name.as_bytes());
        params.extend_from_slice(value.as_bytes());
    }
    let mut out = record(1, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(4, &params));
    out.extend(record(4, &[]));
    out.extend(record(5, &[]));
    stream.write_all(&out).unwrap();
    stream.set_read_timeout(Some(wait)).unwrap();
    let mut body = Vec::new();
    loop {
        let mut header = [0u8; 8];
        if stream.read_exact(&mut header).is_err() {
            return body;
        }
        let len = usize::from(u16::from_be_bytes([header[4], header[5]]));
        let mut content = vec![0u8; len + usize::from(header[6])];
        stream.read_exact(&mut content).unwrap();
        content.truncate(len);
        match header[1] {
            6 => body.extend_from_slice(&content),
            3 => return body,
            _ => {}
        }
    }
}

/// A connection to the pool listening at `socket`, tried for ten seconds
/// from `started`.
fn connect(socket: &Path, started: Instant) -> Option<UnixStream> {
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Some(stream),
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[test]
fn a_request_is_served_by_a_child_under_the_host_s_name() {
    let dir = tree("serve");
    let socket = socket(&dir);
    let path = conf(&dir, &socket.display().to_string());
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php echo PHP_SAPI, ' ', $_GET['q'];").unwrap();
    let mut master = php_fpm()
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let body = connect(&socket, started)
        .map(|s| response(s, &script))
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let body = String::from_utf8_lossy(&body);
    assert!(body.ends_with("\r\n\r\nfpm-fcgi 1"), "{body:?}");
}

/// A child that has served `pm.max_requests` ends its engine on the way
/// out, as php's child calls `php_module_shutdown`: a `dba_popen`
/// `cdb_make` file the request left open is finished there, so the next
/// child's request reads the value, the oracle's php-fpm's bytes for the
/// same two requests (#1804).
#[test]
fn a_child_past_its_max_requests_finishes_a_persistent_cdb_make_file() {
    let dir = tree("engine-end");
    let socket = socket(&dir);
    let path = conf(&dir, &socket.display().to_string());
    let mut written = std::fs::read_to_string(&path).unwrap();
    written.push_str("pm.max_requests = 1\n");
    std::fs::write(&path, written).unwrap();
    let file = dir.join("t1804.cdb");
    let writer = dir.join("writer.php");
    let reader = dir.join("reader.php");
    std::fs::write(
        &writer,
        format!(
            "<?php $f = '{}'; $d = dba_popen($f, 'n', 'cdb_make'); dba_insert('key0', 'value0', $d); clearstatcache(); echo filesize($f);",
            file.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &reader,
        format!(
            "<?php $f = '{}'; clearstatcache(); echo filesize($f), ' '; var_dump(dba_fetch('key0', dba_open($f, 'r', 'cdb')));",
            file.display()
        ),
    )
    .unwrap();
    let mut master = php_fpm()
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let first = connect(&socket, started)
        .map(|s| response(s, &writer))
        .unwrap_or_default();
    // The next child is the one that answers once the first has gone.
    let second = connect(&socket, Instant::now())
        .map(|s| response(s, &reader))
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let first = String::from_utf8_lossy(&first);
    let second = String::from_utf8_lossy(&second);
    assert!(first.ends_with("\r\n\r\n2066"), "{first:?}");
    assert!(
        second.ends_with("\r\n\r\n2082 string(6) \"value0\"\n"),
        "{second:?}"
    );
}

/// A request whose start fails, session's `rinit` raising under
/// `session.auto_start` over a session file whose `DateTime` does not
/// unserialize (#1775): the child renders the uncaught fatal and
/// session's warning, finishes the `FastCGI` request with what they wrote
/// and closes the connection (`fcgi_finish_request(request, 1)`), and
/// ends with `FPM_EXIT_SOFTWARE` (`Z/sapi/fpm/fpm/fpm_main.c:1838-1843`),
/// which the master logs as php's does before it starts the next child,
/// whose request is served. The bytes and the log's lines are the oracle's
/// php-fpm's under the same configuration and line (#1879).
#[test]
fn a_child_whose_request_start_fails_finishes_the_request_and_ends() {
    let dir = tree("start-failed");
    let socket = socket(&dir);
    let path = conf(&dir, &socket.display().to_string());
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php echo \"ran\\n\";").unwrap();
    std::fs::write(
        dir.join("sess_t1879"),
        "x|O:8:\"DateTime\":1:{s:4:\"date\";i:5;}",
    )
    .unwrap();
    let mut master = php_fpm()
        .args(["-n", "-F", "-d", "session.auto_start=1", "-d"])
        .arg(format!("session.save_path={}", dir.display()))
        .arg("-y")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let script_name = script.display().to_string();
    let pairs = [
        ("SCRIPT_FILENAME", script_name.as_str()),
        ("REQUEST_METHOD", "GET"),
    ];
    let mut cookie = pairs.to_vec();
    cookie.push(("HTTP_COOKIE", "PHPSESSID=t1879"));
    let failed = connect(&socket, started)
        .map(|s| request(s, &cookie))
        .unwrap_or_default();
    let next = connect(&socket, Instant::now())
        .map(|s| request(s, &pairs))
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let log = std::fs::read_to_string(dir.join("fpm.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        String::from_utf8_lossy(&failed),
        "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n<br />\n<b>Fatal error</b>:  Uncaught Error: Invalid serialization data for DateTime object in [no active file]:0\nStack trace:\n#0 [internal function]: DateTime-&gt;__unserialize(Array)\n#1 {main}\n  thrown in <b>[no active file]</b> on line <b>0</b><br />\n<br />\n<b>Warning</b>:  PHP Request Startup: Failed to decode session object. Session has been destroyed in <b>Unknown</b> on line <b>0</b><br />\n"
    );
    let next = String::from_utf8_lossy(&next);
    assert!(next.ends_with("\r\n\r\nran\n"), "{next:?}");
    let ended: Vec<&str> = log
        .lines()
        .filter_map(|line| line.split_once("] ").map(|(_, rest)| rest))
        .filter(|line| line.starts_with("WARNING: [pool www] child "))
        .collect();
    assert_eq!(ended.len(), 1, "{log}");
    assert!(ended[0].contains(" exited with code 70 after "), "{log}");
}

/// A log line with its time, the pids and a child's lifetime masked: the
/// number after `pid`, `child` and `after` is `N`.
#[cfg(target_os = "macos")]
fn masked(line: &str) -> String {
    let line = line.split_once("] ").map_or(line, |(_, rest)| rest);
    let mut out: Vec<String> = Vec::new();
    for word in line.split(' ') {
        let numbered = out
            .last()
            .is_some_and(|w| matches!(w.as_str(), "pid" | "child" | "after"));
        let rest = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        if numbered && rest.len() < word.len() {
            out.push(format!("N{rest}"));
        } else {
            out.push(word.to_owned());
        }
    }
    out.join(" ")
}

/// The lines the oracle's php-fpm writes for a static pool of one child
/// at `log_level = debug`, started and sent `SIGQUIT`, times and pids
/// masked (#1382): the pin's php-fpm (`oracle.toml`'s `[sapi]`) run under
/// the configuration `quit_at_debug` writes, its log put through
/// [`masked`]. Captured again on #1502, byte for byte these lines.
#[cfg(target_os = "macos")]
const QUIT_AT_DEBUG: &str = "\
DEBUG: pid N, fpm_scoreboard_init_main(), line 38: got clock tick '100'
DEBUG: pid N, fpm_signals_init_main(), line 219: Unblocking all signals
DEBUG: pid N, fpm_socket_af_inet_socket_by_addr(), line 325: Found address for 127.0.0.1, socket opened on 127.0.0.1
DEBUG: pid N, fpm_event_init_main(), line 345: event module is kqueue and 1 fds have been reserved
NOTICE: pid N, fpm_init(), line 83: fpm is running, pid N
DEBUG: pid N, fpm_children_make(), line 449: blocking signals before child birth
DEBUG: pid N, fpm_children_make(), line 473: unblocking signals, child born
DEBUG: pid N, fpm_children_make(), line 479: [pool www] child N started
DEBUG: pid N, fpm_event_loop(), line 373: 3488 bytes have been reserved in SHM
NOTICE: pid N, fpm_event_loop(), line 374: ready to handle connections
DEBUG: pid N, fpm_got_signal(), line 100: received SIGQUIT
NOTICE: pid N, fpm_got_signal(), line 101: Finishing ...
DEBUG: pid N, fpm_pctl(), line 239: switching to 'finishing' state
DEBUG: pid N, fpm_pctl_kill_all(), line 165: [pool www] sending signal 3 SIGQUIT to child N
DEBUG: pid N, fpm_pctl_kill_all(), line 174: 1 child(ren) still alive
DEBUG: pid N, fpm_event_loop(), line 426: event module triggered 1 events
DEBUG: pid N, fpm_pctl_kill_all(), line 165: [pool www] sending signal 15 SIGTERM to child N
DEBUG: pid N, fpm_pctl_kill_all(), line 174: 1 child(ren) still alive
DEBUG: pid N, fpm_got_signal(), line 81: received SIGCHLD
DEBUG: pid N, fpm_event_loop(), line 426: event module triggered 1 events
DEBUG: pid N, fpm_children_bury(), line 300: [pool www] child N exited on signal 15 (SIGTERM) after N seconds from start
NOTICE: pid N, fpm_pctl_exit(), line 69: exiting, bye-bye!
";

/// How many loopback ports [`quit_at_debug`] tries before it gives up.
#[cfg(target_os = "macos")]
const TRIES: usize = 5;

/// A loopback port the system found free, let go at once: free for anyone
/// until the master binds it.
#[cfg(target_os = "macos")]
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The masked log of a static pool of one child at `log_level = debug`
/// listening on the first port of `ports` its master binds, started and
/// sent `SIGQUIT` once ready.
///
/// The pool listens on a loopback address of its own, since the lines
/// held are the oracle's for an inet socket, and a port found free and let
/// go is anyone's until the master binds it (#1502). A master that finds
/// its port taken logs php's `unable to bind listening socket` and exits
/// before it is ready; its log is set aside and the pool starts again on
/// the next port, up to [`TRIES`] times. The oracle's lines are the same
/// for any port, so the port the pool ends on is never held.
#[cfg(target_os = "macos")]
fn quit_at_debug(dir: &Path, ports: impl IntoIterator<Item = u16>) -> String {
    let mut tried = Vec::new();
    for (attempt, port) in ports.into_iter().take(TRIES).enumerate() {
        let path = dir.join(format!("php-fpm-{attempt}.conf"));
        let log = dir.join(format!("fpm-{attempt}.log"));
        std::fs::write(
            &path,
            format!(
                "[global]\nerror_log = {}\nlog_level = debug\n[www]\nlisten = 127.0.0.1:{port}\npm = static\npm.max_children = 1\n",
                log.display()
            ),
        )
        .unwrap();
        let mut master = php_fpm()
            .args(["-n", "-F", "-y"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut ended = None;
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("ready to handle connections")
            && started.elapsed() < Duration::from_secs(10)
        {
            ended = master.try_wait().unwrap();
            if ended.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let written = std::fs::read_to_string(&log).unwrap_or_default();
        if ended.is_some() && written.contains("unable to bind listening socket") {
            tried.push(port);
            continue;
        }
        let _ = Command::new("kill")
            .arg("-QUIT")
            .arg(master.id().to_string())
            .status();
        let _ = master.wait();
        let written = std::fs::read_to_string(&log).unwrap_or_default();
        return written.lines().map(|l| masked(l) + "\n").collect();
    }
    panic!("every port the pool tried was taken before its master bound it: {tried:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_static_pool_at_debug_sent_sigquit_writes_the_oracle_s_lines() {
    let dir = tree("debug");
    let lines = quit_at_debug(&dir, std::iter::repeat_with(free_port));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(lines, QUIT_AT_DEBUG);
}

/// A port another process takes before the master binds it is given up
/// for the next (#1502): the first port offered is held by a listener of
/// the test's own for the master's whole life, as a process that took it
/// in the window would hold it, and the lines are still the oracle's.
#[cfg(target_os = "macos")]
#[test]
fn a_port_taken_before_the_master_binds_it_is_given_up_for_another() {
    let dir = tree("debug-taken");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let held = taken.local_addr().unwrap().port();
    let ports = std::iter::once(held).chain(std::iter::repeat_with(free_port));
    let lines = quit_at_debug(&dir, ports);
    let first = std::fs::read_to_string(dir.join("fpm-0.log")).unwrap_or_default();
    drop(taken);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        first.contains("unable to bind listening socket"),
        "the held port was not the first the master tried: {first}"
    );
    assert_eq!(lines, QUIT_AT_DEBUG);
}

#[test]
fn a_request_body_file_that_does_not_open_is_a_warning_the_pool_logs() {
    let dir = tree("body-file");
    let socket = socket(&dir);
    let path = dir.join("php-fpm.conf");
    let log = dir.join("fpm.log");
    std::fs::write(
        &path,
        format!(
            "[global]\nerror_log = {}\n[www]\nlisten = {}\npm = static\npm.max_children = 1\ncatch_workers_output = yes\nphp_admin_flag[log_errors] = on\n",
            log.display(),
            socket.display()
        ),
    )
    .unwrap();
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php echo 'ran';").unwrap();
    let mut master = php_fpm()
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let script = script.display().to_string();
    let missing = dir.join("missing").display().to_string();
    let body = connect(&socket, Instant::now())
        .map(|s| {
            request(
                s,
                &[
                    ("SCRIPT_FILENAME", script.as_str()),
                    ("REQUEST_METHOD", "POST"),
                    ("CONTENT_TYPE", "application/x-www-form-urlencoded"),
                    ("CONTENT_LENGTH", "3"),
                    ("REQUEST_BODY_FILE", missing.as_str()),
                ],
            )
        })
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-QUIT")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let written = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    let expected = format!(
        "said into stderr: \"NOTICE: PHP message: PHP Warning:  REQUEST_BODY_FILE: open('{missing}') failed: No such file or directory (2) in Unknown on line 0\""
    );
    assert!(written.contains(&expected), "{written}");
    assert!(String::from_utf8_lossy(&body).ends_with("ran"), "{body:?}");
}

/// A php.ini's `[HOST=]` table of the request's `SERVER_NAME`, lowered,
/// and its `[PATH=]` tables along the script's directory, set for that
/// request alone as `PHP_INI_SYSTEM`: the oracle's bytes from php-fpm
/// `-c php.ini` serving the two requests on its one child (#1692), held
/// here since a fence takes no ini of its own (#1634).
#[test]
fn a_request_s_host_and_directory_tables_are_set_for_it_alone() {
    let dir = tree("sections");
    let socket = socket(&dir);
    let path = conf(&dir, &socket.display().to_string());
    std::fs::create_dir_all(dir.join("www/sub")).unwrap();
    std::fs::create_dir_all(dir.join("scan")).unwrap();
    let www = dir.join("www").display().to_string();
    let ini = dir.join("php.ini");
    std::fs::write(
        &ini,
        format!(
            "precision=10\n[PATH={www}]\nprecision=5\n[PATH={www}/sub/]\nprecision=3\n\
             [HOST=Example.COM]\nserialize_precision=7\n"
        ),
    )
    .unwrap();
    let program = "<?php var_dump(ini_get('precision'), ini_get('serialize_precision'), ini_set('precision', '9'));\n";
    std::fs::write(dir.join("www/sub/t.php"), program).unwrap();
    std::fs::write(dir.join("www/t.php"), program).unwrap();
    let mut master = php_fpm()
        .env("PHP_INI_SCAN_DIR", dir.join("scan"))
        .arg("-c")
        .arg(&ini)
        .args(["-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let served = |script: &str, server_name: &str| {
        let script = dir.join(script).display().to_string();
        let pairs = [
            ("SCRIPT_FILENAME", script.as_str()),
            ("REQUEST_METHOD", "GET"),
            ("SERVER_NAME", server_name),
        ];
        let body = connect(&socket, started)
            .map(|s| request(s, &pairs))
            .unwrap_or_default();
        String::from_utf8_lossy(&body).into_owned()
    };
    let sub = served("www/sub/t.php", "Example.com");
    let top = served("www/t.php", "other");
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let head = "X-Powered-By: PHP/8.5.9\r\nContent-type: text/html; charset=UTF-8\r\n\r\n";
    assert_eq!(
        sub,
        format!("{head}string(1) \"3\"\nstring(1) \"7\"\nbool(false)\n")
    );
    assert_eq!(
        top,
        format!("{head}string(1) \"5\"\nstring(2) \"-1\"\nbool(false)\n")
    );
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
    let dir = tree("fork");
    let socket = socket(&dir);
    let path = conf(&dir, &socket.display().to_string());
    let script = dir.join("stress.php");
    std::fs::write(&script, STRESS).unwrap();
    let mut master = php_fpm()
        .args(["-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let script = script.display().to_string();
    let pairs = [
        ("SCRIPT_FILENAME", script.as_str()),
        ("REQUEST_METHOD", "GET"),
    ];
    let body = connect(&socket, started)
        .map(|s| request_within(s, &pairs, Duration::from_mins(5)))
        .unwrap_or_default();
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(master.id().to_string())
        .status();
    let _ = master.wait();
    let _ = std::fs::remove_dir_all(&dir);
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
