//! `php -S` is php's event loop (`Z/sapi/cli/php_cli_server.c:2770`;
//! design §4.8, revision 122): one thread waits on every connection at once,
//! reads each request as its bytes arrive, and serves the requests one
//! after another on that thread. What these hold is the order and the
//! time the loop serves in, which no comparison of bytes carries; what
//! the oracle's `php -S` writes for a request is held beside it by
//! `xtask witness`, from `tests/lines.toml`.

mod server;

use std::io::Write;
use std::time::{Duration, Instant};

use server::{Server, body};

const SLOW: &str = "<?php usleep(800000); echo 'slow';";
const FAST: &str = "<?php echo 'fast';";

/// A request that sleeps holds back the one sent while it sleeps, which
/// is served once it ends, as php's one loop serves them.
#[test]
fn a_request_that_sleeps_holds_back_the_next() {
    let server = Server::start("held", &[("slow.php", SLOW), ("fast.php", FAST)], &[]);
    let began = Instant::now();
    let mut slow = server.send("/slow.php");
    std::thread::sleep(Duration::from_millis(200));
    let mut fast = server.send("/fast.php");
    assert_eq!(body(&mut fast), "fast");
    let fast_at = began.elapsed();
    assert_eq!(body(&mut slow), "slow");
    assert!(
        fast_at >= Duration::from_millis(700),
        "the second request was served after {fast_at:?}, beside the first"
    );
    assert_eq!(server.stop(), Some(0));
}

/// A client that sends half a request and waits holds no one back: the
/// loop reads what has arrived and serves the next client, then reads
/// the rest of the first when it comes.
#[test]
fn a_slow_client_holds_back_no_other() {
    let server = Server::start("slow-client", &[("fast.php", FAST)], &[]);
    let mut slow = server.connect();
    slow.write_all(b"GET /fast.php HTTP/1.0\r\nHo").ok();
    std::thread::sleep(Duration::from_millis(100));
    let began = Instant::now();
    assert_eq!(server.get("/fast.php"), "fast");
    assert!(began.elapsed() < Duration::from_secs(5));
    slow.write_all(b"st: x\r\n\r\n").ok();
    assert_eq!(body(&mut slow), "fast");
    assert_eq!(server.stop(), Some(0));
}

/// `pcntl_fork` while another client's request waits to be served forks,
/// as php's does: that request is not in flight beside the one forking,
/// so the engine's one home for fork has nothing to refuse. The
/// child ends where it starts.
#[test]
fn a_fork_with_another_request_waiting_goes_ahead() {
    let fork = "<?php usleep(400000); $pid = pcntl_fork();\
        if ($pid === 0) posix_kill(getmypid(), SIGKILL);\
        echo $pid > 0 ? 'forked' : 'refused: ' . pcntl_strerror(pcntl_get_last_error());";
    let server = Server::start(
        "fork-waiting",
        &[("fork.php", fork), ("slow.php", SLOW)],
        &[],
    );
    let mut forking = server.send("/fork.php");
    std::thread::sleep(Duration::from_millis(100));
    let mut waiting = server.send("/slow.php");
    assert_eq!(body(&mut forking), "forked");
    assert_eq!(body(&mut waiting), "slow");
    assert_eq!(server.stop(), Some(0));
}
