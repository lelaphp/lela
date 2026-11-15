//! `PHP_CLI_SERVER_WORKERS=N`: `php -S` forks N workers before it serves
//! (`php_cli_server_startup_workers`, `Z/sapi/cli/php_cli_server.c:2503`),
//! each through the engine's one home for fork, and every one of
//! the N + 1 processes serves the same socket in its own event loop and
//! writes its log lines after its process id. Which process serves a
//! request is the kernel's to say, so this is held by what the processes
//! do and not by bytes; a number that is not above 1, which forks
//! nothing, is held to the oracle by `xtask witness`, from
//! `tests/lines.toml`.

mod server;

use std::collections::BTreeSet;
use std::time::Duration;

use server::Server;

/// The process ids the log's "started" lines carry, `[<pid>] [<time>] …`.
fn started_pids(lines: &[String]) -> BTreeSet<u32> {
    lines
        .iter()
        .filter(|line| line.ends_with("started"))
        .filter_map(|line| line.strip_prefix('[')?.split_once(']')?.0.parse().ok())
        .collect()
}

/// Two workers and the process that forked them each say they started,
/// under their own ids; a request is served by one of them; a `SIGINT`
/// to each stops all three, the first once it has waited for the others.
#[test]
fn two_workers_and_their_parent_serve_and_stop() {
    let mut server = Server::start(
        "workers",
        &[("index.php", "<?php echo getmypid();")],
        &[("PHP_CLI_SERVER_WORKERS", "2")],
    );
    assert!(
        server.wait_for(Duration::from_secs(30), |lines| started_pids(lines).len()
            == 3),
        "three processes never said they started: {:?}",
        server.log()
    );
    let pids = started_pids(&server.log());
    assert!(pids.contains(&server.pid()));
    for _ in 0..6 {
        let answered: u32 = server.get("/index.php").parse().unwrap_or(0);
        assert!(
            pids.contains(&answered),
            "{answered} is not one of {pids:?}"
        );
    }
    for pid in &pids {
        Server::interrupt(*pid);
    }
    assert_eq!(server.exit_status(Duration::from_secs(10)), Some(0));
    for pid in pids {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success());
        assert!(!alive, "worker {pid} outlived the server");
    }
}
