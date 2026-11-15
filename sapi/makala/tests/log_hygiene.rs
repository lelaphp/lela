//! Who may read the server's logs, and what a request can write into
//! them: a log file the server makes is its owner's alone, `0600`
//! whatever the umask, and one that stands already keeps its permissions; and no
//! request writes a line of its own, the line breaks a request's decoded
//! path can carry written as two bytes where a refusal names the path and
//! where php's entry names the program, in the error log the line names
//! and in the file `error_log` names, and a request line with a raw one
//! refused before it is logged.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{Request, eventually, read_until_closed};

/// `makala -n` over `root` with `args`, php's diagnostics logged and not
/// shown, started by a shell that set the umask to `mask` first, killed
/// when dropped.
struct Umasked {
    child: Child,
    addr: SocketAddr,
}

impl Umasked {
    fn start(root: &Path, mask: &str, args: &[&str]) -> Umasked {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("umask {mask} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_makala"))
            .args(["-n", "--listen", "127.0.0.1:0", "--threads", "2"])
            .args(["-d", "log_errors=1", "-d", "display_errors=0"])
            .args(args)
            .arg("--root")
            .arg(root)
            .current_dir(common::manifest_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("starting makala: {e}"));
        let stderr = child.stderr.take().unwrap_or_else(|| panic!("no stderr"));
        let mut lines = BufReader::new(stderr).lines();
        let line = lines
            .next()
            .and_then(Result::ok)
            .unwrap_or_else(|| panic!("makala wrote no started line"));
        let addr = line
            .split_once("listening on http://")
            .and_then(|(_, rest)| rest.split([',', ' ']).next()?.parse().ok())
            .unwrap_or_else(|| panic!("not a started line: {line}"));
        std::thread::spawn(move || for _ in lines {});
        Umasked { child, addr }
    }

    /// `bytes` on a connection of their own, and what came back.
    fn exchange(&self, bytes: &[u8]) -> String {
        let mut stream = TcpStream::connect(self.addr).unwrap_or_else(|e| panic!("{e}"));
        stream.write_all(bytes).unwrap_or_else(|e| panic!("{e}"));
        read_until_closed(&mut stream, Duration::from_secs(30)).text()
    }
}

impl Drop for Umasked {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// The permission bits of `path`, once it stands.
fn mode(path: &Path) -> u32 {
    assert!(
        eventually(Duration::from_secs(10), || path.exists()),
        "{} was not made",
        path.display()
    );
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// Under umask `022` and `000` the access log and the error log are each
/// made `0600`, so a local user other than the server's reads neither; an
/// access log that stood at `0644` is appended to and keeps it.
#[test]
fn a_log_file_the_server_makes_is_its_owner_s_alone() {
    for mask in ["022", "000"] {
        let root = common::root(
            &format!("log-mode-{mask}"),
            &[("warn.php", "<?php echo $undefined;")],
        );
        let (access, errors) = (root.join("access.log"), root.join("error.log"));
        let server = Umasked::start(
            &root,
            mask,
            &[
                "--access-log",
                &access.to_string_lossy(),
                "--error-log",
                &errors.to_string_lossy(),
            ],
        );
        server.exchange(&Request::get("/warn.php").close().bytes());
        assert_eq!(mode(&access), 0o600, "the access log under umask {mask}");
        assert_eq!(mode(&errors), 0o600, "the error log under umask {mask}");
    }
    let root = common::root("log-mode-stood", &[("a.php", "<?php echo 'a';")]);
    let access = root.join("access.log");
    std::fs::write(&access, "before\n").unwrap_or_else(|e| panic!("{e}"));
    std::fs::set_permissions(&access, std::fs::Permissions::from_mode(0o644))
        .unwrap_or_else(|e| panic!("{e}"));
    let server = Umasked::start(&root, "022", &["--access-log", &access.to_string_lossy()]);
    server.exchange(&Request::get("/a.php").close().bytes());
    let read = || std::fs::read_to_string(&access).unwrap_or_default();
    assert!(eventually(Duration::from_secs(10), || read()
        .lines()
        .count()
        == 2));
    assert!(read().starts_with("before\n"), "{}", read());
    assert_eq!(mode(&access), 0o644);
}

/// A request whose decoded path names a program with `\r\n` in its name,
/// one whose path past such a file is refused by
/// `security.limit_extensions`, and one naming a directory so named,
/// which does not open as a program, write the name with the two escaped:
/// php's warning in the file `error_log` names and in the error log, and
/// each refusal in the error log. A `%0D%0A` in a query is written as the
/// client sent it, and a request line with a raw `\r` is refused before
/// the access log has a line for it. No line of any log begins with what
/// the request made up.
#[test]
fn a_request_s_line_breaks_write_no_line_of_their_own() {
    let root = common::root(
        "log-breaks",
        &[
            ("a\r\nFORGED-A.php", "<?php echo $undefined;"),
            ("x\r\nFORGED-X.jpg", "<?php echo 'ran';"),
            ("q.php", "<?php echo 'q';"),
        ],
    );
    std::fs::create_dir(root.join("d\r\nFORGED-D.php")).unwrap_or_else(|e| panic!("{e}"));
    let root = root.canonicalize().unwrap_or_else(|e| panic!("{e}"));
    let (access, errors, php) = (
        root.join("access.log"),
        root.join("error.log"),
        root.join("php.log"),
    );
    let server = Umasked::start(
        &root,
        "022",
        &[
            "--access-log",
            &access.to_string_lossy(),
            "--error-log",
            &errors.to_string_lossy(),
        ],
    );
    let define = format!("error_log={}", php.display());
    let to_file = Umasked::start(
        &root,
        "022",
        &["-d", &define, "--access-log", &access.to_string_lossy()],
    );
    let get = |server: &Umasked, target: &str| {
        server.exchange(&Request::get(target).host("localhost").close().bytes())
    };
    get(&server, "/a%0D%0AFORGED-A.php");
    get(&server, "/x%0D%0AFORGED-X.jpg/y.php");
    get(&server, "/q.php?q=%0D%0AFORGED-Q");
    get(&server, "/d%0D%0AFORGED-D.php/");
    get(&to_file, "/a%0D%0AFORGED-A.php");
    let refused = server.exchange(b"GET /q.php?q=\rFORGED-R HTTP/1.1\r\nHost: a\r\n\r\n");
    assert!(refused.starts_with("HTTP/1.1 400"), "{refused}");
    let read = |path: &Path| std::fs::read(path).unwrap_or_default();
    let lines = |path: &Path| read(path).split(|&b| b == b'\n').count() - 1;
    assert!(eventually(Duration::from_secs(10), || {
        lines(&access) == 5 && lines(&errors) >= 3 && lines(&php) == 1
    }));
    let shown = root.display();
    let warning = format!(
        "PHP Warning:  Undefined variable $undefined in {shown}/a\\r\\nFORGED-A.php on line 1\n"
    );
    let refusal = format!(
        "Access to the script '{shown}/x\\r\\nFORGED-X.jpg' has been denied (see security.limit_extensions)\n"
    );
    let unopened = format!(
        "Unable to open primary script: {shown}/d\\r\\nFORGED-D.php (No such file or directory)\n"
    );
    let text = |path: &Path| String::from_utf8_lossy(&read(path)).into_owned();
    assert!(text(&php).ends_with(&warning), "{}", text(&php));
    assert!(text(&errors).contains(&warning), "{}", text(&errors));
    assert!(text(&errors).contains(&refusal), "{}", text(&errors));
    assert!(text(&errors).contains(&unopened), "{}", text(&errors));
    assert!(
        text(&access).contains("\"GET /q.php?q=%0D%0AFORGED-Q HTTP/1.1\" 200"),
        "{}",
        text(&access)
    );
    for log in [&access, &errors, &php] {
        let bytes = read(log);
        assert!(
            bytes
                .split(|&b| b == b'\n' || b == b'\r')
                .all(|line| !line.starts_with(b"FORGED")),
            "{}",
            text(log)
        );
    }
}
