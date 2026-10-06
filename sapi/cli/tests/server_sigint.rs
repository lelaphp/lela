//! `php -S` stopped by `SIGINT`: php's handler only clears
//! `server.is_running` (`Z/sapi/cli/php_cli_server.c:2800-2804`), the
//! event loop ends after its wait, `do_cli_server` returns 0 and the CLI
//! ends the engine with `php_module_shutdown`. So the server ends with
//! status 0, not killed by the signal, with a connection that never sent
//! a request and one that sent half of one still open, after one that
//! was served; and a request in flight when the signal comes is on the
//! loop's one thread and finishes first, its response whole.

mod server;

use std::io::Write;
use std::time::Duration;

use server::{Server, body};

#[test]
fn a_sigint_stops_the_built_in_server_with_status_zero() {
    let server = Server::start("sigint", &[("index.php", "<?php echo 'served';")], &[]);
    assert_eq!(server.get("/index.php"), "served");
    let _idle = server.connect();
    let mut half = server.connect();
    half.write_all(b"GET /index.php HTTP/1.0\r\n").ok();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(server.stop(), Some(0));
}

#[test]
fn a_request_in_flight_finishes_before_a_sigint_stops_the_server() {
    let slow = "<?php usleep(500000); echo 'finished';";
    let mut server = Server::start("sigint-in-flight", &[("slow.php", slow)], &[]);
    let mut in_flight = server.send("/slow.php");
    std::thread::sleep(Duration::from_millis(200));
    Server::interrupt(server.pid());
    assert_eq!(body(&mut in_flight), "finished");
    assert_eq!(server.exit_status(Duration::from_secs(10)), Some(0));
}
