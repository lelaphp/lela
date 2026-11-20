//! What a request can write into php-fpm's logs, and who may read them:
//! each log the master makes is its owner's alone, `0600`
//! whatever the umask, the slow log among them where the trace finds it
//! gone, as a rotation leaves it; and a `\r` or `\n` in a field the
//! request supplies is written as two bytes, in the access log's `%e`,
//! `%f`, `%m`, `%o`, `%q`, `%r` and `%u`, in the line naming a slow
//! request, in the slow log's `script_filename` and frames, which hold
//! the program's path, and in the lines naming a program the pool refused
//! by its extension or that did not open, so the request writes no line
//! of its own.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use php_proto::fcgi::{self, Header, Kind};

/// The program the request runs, named with a line break, which sleeps
/// past `request_slowlog_timeout` and `request_terminate_timeout` and
/// echoes `done`.
const PROGRAM: &str = "slow\r\nFORGED-F.php";

/// The access log's format: every token whose field the request supplies.
const FORMAT: &str = "R=%r|Q=%q|E=%{X_FIELD}e|O=%{X-Out}o|U=%u|M=%m|F=%f";

/// A master in `dir` whose pool's configuration ends in `directives`,
/// each `{dir}` in them the directory,
/// its stderr nowhere, started by a shell that set the umask to `mask`
/// first, halted and its directory removed when dropped.
struct Master {
    dir: PathBuf,
    child: Option<Child>,
}

impl Master {
    fn start(name: &str, mask: &str, directives: &str) -> Master {
        let dir = std::env::temp_dir().join(format!("sapi-fpm-logs-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the test's directory is made");
        let mut master = Master { dir, child: None };
        let dir = master.dir.clone();
        let conf = dir.join("php-fpm.conf");
        std::fs::write(
            &conf,
            format!(
                "[global]\nerror_log = {d}/fpm.log\n[www]\nlisten = {d}/fpm.sock\n\
                 pm = static\npm.max_children = 1\naccess.log = {d}/access.log\n\
                 access.format = \"{FORMAT}\"\n{}",
                directives.replace("{dir}", &dir.display().to_string()),
                d = dir.display()
            ),
        )
        .expect("the configuration is written");
        std::fs::write(
            dir.join(PROGRAM),
            "<?php header('X-Out: out'); usleep(2500000); echo 'done';\n",
        )
        .expect("the program is written");
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("umask {mask} && exec \"$0\" \"$@\""))
            .arg(readable(&dir))
            .args(["-R", "-n", "-F", "-y"])
            .arg(&conf)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("php-fpm starts");
        master.child = Some(child);
        master
    }

    fn connect(&self) -> UnixStream {
        let socket = self.dir.join("fpm.sock");
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

    /// The master sent `SIGTERM` and waited for, ten seconds at most,
    /// its process group killed past them.
    fn halt(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
        kill(pid, Signal::SIGTERM).ok();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        killpg(pid, Signal::SIGKILL).ok();
        child.kill().ok();
        child.wait().ok();
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.halt();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// The binary under test, where the master can read its child's memory,
/// as `tests/serve.rs`'s `readable_php_fpm` makes it: on macOS a copy in
/// `dir` signed ad hoc with `com.apple.security.get-task-allow`, and
/// elsewhere the binary itself.
fn readable(dir: &Path) -> PathBuf {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_lela-fpm"));
    if !cfg!(target_os = "macos") {
        return binary;
    }
    let copy = dir.join("lela-fpm");
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
    copy
}

/// One `FastCGI` record of request 1.
fn record(kind: Kind, content: &[u8]) -> Vec<u8> {
    let header = Header::make(kind, 1, content.len());
    let mut out = header.encode().to_vec();
    out.extend_from_slice(content);
    out.resize(out.len() + usize::from(header.padding_length), 0);
    out
}

/// What request 1 with `pairs` as its parameters writes on
/// `FCGI_STDOUT`, read off `stream` to its end.
fn request(mut stream: UnixStream, pairs: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut params = Vec::new();
    for &(name, value) in pairs {
        fcgi::put_pair(&mut params, name, value);
    }
    let mut out = record(Kind::BeginRequest, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(Kind::Params, &params));
    out.extend(record(Kind::Params, &[]));
    out.extend(record(Kind::Stdin, &[]));
    stream.write_all(&out).expect("the request is written");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("the reply's reads are timed");
    let mut body = Vec::new();
    loop {
        let mut bytes = [0u8; 8];
        if stream.read_exact(&mut bytes).is_err() {
            return body;
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
            Kind::EndRequest => return body,
            _ => {}
        }
    }
}

/// What a master started under `mask` logged for one slow request whose
/// every field carries a line break, the slow log removed before it was
/// sent: each log's permission bits and bytes, by name, and the program's path.
struct Logged {
    modes: Vec<(&'static str, u32)>,
    access: Vec<u8>,
    fpm: Vec<u8>,
    slow: Vec<u8>,
    program: Vec<u8>,
    /// The program's path as the engine opened it, resolved.
    real: Vec<u8>,
}

/// What request 1 for `program`, every field of it carrying a line
/// break, writes on `FCGI_STDOUT`.
fn broken(stream: UnixStream, program: &[u8]) -> Vec<u8> {
    // `Basic` and the base64 of `us\r\nFORGED-U:pw`.
    let auth = b"Basic dXMNCkZPUkdFRC1VOnB3";
    let pairs: [(&[u8], &[u8]); 7] = [
        (b"SCRIPT_FILENAME", program),
        (b"SCRIPT_NAME", b"/s\r\nFORGED-R"),
        (b"REQUEST_METHOD", b"GET\r\nFORGED-M"),
        (b"QUERY_STRING", b"a=1\r\nFORGED-Q"),
        (b"X_FIELD", b"e1\r\nFORGED-E"),
        (b"HTTP_AUTHORIZATION", auth),
        (b"REQUEST_URI", b"/s"),
    ];
    request(stream, &pairs)
}

/// The escaped request line of [`broken`]'s request, as a warning names it.
const REQUEST: &str = "(request: \"GET\\r\\nFORGED-M /s\\r\\nFORGED-R?a=1\\r\\nFORGED-Q\")";

fn served(name: &str, mask: &str) -> Logged {
    let mut master = Master::start(
        name,
        mask,
        "slowlog = {dir}/slow.log\nrequest_slowlog_timeout = 1\n",
    );
    let stream = master.connect();
    // The configuration made it; a rotation takes it away.
    std::fs::remove_file(master.dir.join("slow.log")).expect("the slow log stood");
    let program = master
        .dir
        .join(PROGRAM)
        .into_os_string()
        .into_encoded_bytes();
    let body = broken(stream, &program);
    assert!(
        body.ends_with(b"done"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    master.halt();
    let read = |file: &str| std::fs::read(master.dir.join(file)).unwrap_or_default();
    let mode = |file: &'static str| {
        let meta = std::fs::metadata(master.dir.join(file))
            .unwrap_or_else(|e| panic!("{file} was made: {e}"));
        (file, meta.permissions().mode() & 0o777)
    };
    Logged {
        modes: vec![mode("fpm.log"), mode("access.log"), mode("slow.log")],
        access: read("access.log"),
        fpm: read("fpm.log"),
        slow: read("slow.log"),
        program,
        real: std::fs::canonicalize(&master.dir)
            .expect("the test's directory resolves")
            .join(PROGRAM)
            .into_os_string()
            .into_encoded_bytes(),
    }
}

/// `bytes` with each `\r` and `\n` the two bytes that name it.
fn escaped(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in bytes {
        match b {
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\n' => out.extend_from_slice(b"\\n"),
            _ => out.push(b),
        }
    }
    out
}

/// Whether a line of `log` begins with what a request made up.
fn forged(log: &[u8]) -> bool {
    log.split(|&b| b == b'\n' || b == b'\r')
        .any(|line| line.starts_with(b"FORGED"))
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_request_s_line_breaks_are_escaped_and_each_log_is_its_owner_s_under_umask_022() {
    let logged = served("022", "022");
    assert_eq!(
        logged.modes,
        [
            ("fpm.log", 0o600),
            ("access.log", 0o600),
            ("slow.log", 0o600)
        ]
    );
    let mut line = b"R=/s\\r\\nFORGED-R|Q=a=1\\r\\nFORGED-Q|E=e1\\r\\nFORGED-E|O=out|\
                     U=us\\r\\nFORGED-U|M=GET\\r\\nFORGED-M|F="
        .to_vec();
    line.extend_from_slice(&escaped(&logged.program));
    line.push(b'\n');
    assert_eq!(
        String::from_utf8_lossy(&logged.access),
        String::from_utf8_lossy(&line)
    );
    let fpm = String::from_utf8_lossy(&logged.fpm);
    assert!(
        fpm.contains(&format!("{REQUEST} executing too slow")),
        "{fpm}"
    );
    let mut head = b"script_filename = ".to_vec();
    head.extend_from_slice(&escaped(&logged.program));
    head.push(b'\n');
    assert!(
        logged.slow.windows(head.len()).any(|w| w == head),
        "{}",
        String::from_utf8_lossy(&logged.slow)
    );
    let mut frame = b"usleep() ".to_vec();
    frame.extend_from_slice(&escaped(&logged.real));
    frame.extend_from_slice(b":1\n");
    assert!(
        logged.slow.windows(frame.len()).any(|w| w == frame),
        "{}",
        String::from_utf8_lossy(&logged.slow)
    );
    for log in [&logged.access, &logged.fpm, &logged.slow] {
        assert!(!forged(log), "{}", String::from_utf8_lossy(log));
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn each_log_is_its_owner_s_under_umask_000() {
    let logged = served("000", "000");
    assert_eq!(
        logged.modes,
        [
            ("fpm.log", 0o600),
            ("access.log", 0o600),
            ("slow.log", 0o600)
        ]
    );
}

/// A request past `request_terminate_timeout` is named in the error log's
/// `execution timed out` line with its method, path and query escaped, as
/// the slow request's line is.
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_timed_out_request_s_line_breaks_are_escaped() {
    let mut master = Master::start("timeout", "022", "request_terminate_timeout = 1\n");
    let stream = master.connect();
    let program = master
        .dir
        .join(PROGRAM)
        .into_os_string()
        .into_encoded_bytes();
    broken(stream, &program);
    master.halt();
    let log = std::fs::read(master.dir.join("fpm.log")).unwrap_or_default();
    let fpm = String::from_utf8_lossy(&log);
    assert!(
        fpm.contains(&format!("{REQUEST} execution timed out")),
        "{fpm}"
    );
    assert!(!forged(&log), "{fpm}");
}

/// A program the pool refuses, by its extension or because it did not
/// open, is named in the error log's line for it with each `\r` and `\n`
/// of its path written as two bytes, one line for each refusal;
/// the second refusal is held only where this user cannot open a file
/// whose permissions are `0o000`.
#[test]
#[cfg_attr(miri, ignore = "Miri does not model fork(2)")]
fn a_refused_program_s_line_breaks_are_escaped() {
    let mut master = Master::start("refused", "022", "catch_workers_output = yes\n");
    let path = |name: &str| master.dir.join(name).into_os_string().into_encoded_bytes();
    let denied = path("x\r\nFORGED-X.txt");
    let unopened = path("closed\r\nFORGED-O.php");
    // Each file stands, as the pool finds a program before it refuses
    // one; the second is no one's to read.
    for file in [&denied, &unopened] {
        std::fs::write(OsStr::from_bytes(file), "<?php echo 'ran';\n")
            .expect("the program is written");
    }
    std::fs::set_permissions(
        OsStr::from_bytes(&unopened),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("the program is closed");
    // A user the file's permissions do not refuse, root, opens it, and the
    // pool serves it: the line for a program that did not open is held
    // only where this user is refused the file.
    let refused_to_us = std::fs::File::open(OsStr::from_bytes(&unopened)).is_err();
    let pairs = |program: &[u8]| -> Vec<u8> {
        request(
            master.connect(),
            &[
                (b"SCRIPT_FILENAME", program),
                (b"REQUEST_METHOD", b"GET"),
                (b"REQUEST_URI", b"/x"),
            ],
        )
    };
    let body = pairs(&denied);
    assert!(
        body.ends_with(b"Access denied.\n"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let body = pairs(&unopened);
    if refused_to_us {
        assert!(
            body.ends_with(b"Access denied.\n"),
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
    master.halt();
    let log = std::fs::read(master.dir.join("fpm.log")).unwrap_or_default();
    let fpm = String::from_utf8_lossy(&log);
    let line = |text: Vec<u8>| {
        let text = String::from_utf8_lossy(&text).into_owned();
        assert_eq!(
            fpm.lines().filter(|line| line.contains(&text)).count(),
            1,
            "{text}\n{fpm}"
        );
    };
    line(
        [
            &b"Access to the script '"[..],
            &escaped(&denied),
            b"' has been denied (see security.limit_extensions)",
        ]
        .concat(),
    );
    if refused_to_us {
        line(
            [
                &b"Unable to open primary script: "[..],
                &escaped(&unopened),
                b" (Permission denied)",
            ]
            .concat(),
        );
    }
    assert!(!forged(&log), "{fpm}");
}
