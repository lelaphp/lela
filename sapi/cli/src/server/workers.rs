//! `PHP_CLI_SERVER_WORKERS` (`Z/sapi/cli/php_cli_server.c:2503`):
//! the server forked into worker processes before it serves,
//! each through the engine's one home for fork ([`php_sapi::fork`]) and
//! each serving the same listening socket with its own event loop, the
//! process that forked them among them; and, once that process's loop
//! stops, every worker it made waited for, as `php_cli_server_dtor` waits.

use std::io::Write;

use php_sapi::c_number::strtol;
use php_sapi::{Forked, Pid};

use super::log::{Log, Type};

/// What one process of the server knows of the workers: the ones it
/// forked, which only the process that forked them waits for, and
/// whether the log names the process each line comes from, as php's
/// does while `php_cli_server_workers_max` is above 1.
pub(super) struct Workers {
    forked: Vec<Pid>,
    many: bool,
}

impl Workers {
    /// `php_cli_server_startup_workers`: nothing where
    /// `PHP_CLI_SERVER_WORKERS` is unset; where it reads as a number
    /// above 1 (`ZEND_ATOL`), that many forks, each child returning at
    /// once to serve and the parent going on to the next, a fork that
    /// fails leaving the workers made so far and counting itself, as
    /// php's count does; any other number php's line on stderr and no
    /// worker. A fork the engine refuses because something already
    /// stands inside it, which nothing does between engine start and the
    /// first request, is a broken invariant of this host, not a fork the
    /// system refused, and says so on `log` before the server serves on
    /// with the workers it has.
    pub(super) fn start(log: &Log) -> Workers {
        let none = Workers {
            forked: Vec::new(),
            many: false,
        };
        let Some(asked) = std::env::var_os("PHP_CLI_SERVER_WORKERS") else {
            return none;
        };
        let max = strtol(asked.as_encoded_bytes());
        if max <= 1 {
            std::io::stderr()
                .lock()
                .write_all(b"number of workers must be larger than 1\n")
                .ok();
            return none;
        }
        let master = std::process::id();
        let mut forked = Vec::new();
        for made in 0..max {
            match php_sapi::fork() {
                Err(error) => {
                    if error.raw_os_error() == Some(libc::EBUSY) {
                        log.write(
                            Type::Error,
                            "Failed to fork a worker: the engine was not idle",
                        );
                    }
                    return Workers {
                        forked,
                        many: made > 0,
                    };
                }
                Ok(Forked::Child) => {
                    crate::ffi::end_with_parent(master);
                    return Workers {
                        forked: Vec::new(),
                        many: true,
                    };
                }
                Ok(Forked::Parent { child }) => forked.push(child),
            }
        }
        Workers { forked, many: true }
    }

    /// Whether the log names the process each line comes from:
    /// `php_cli_server_workers_max` above 1.
    pub(super) fn many(&self) -> bool {
        self.many
    }

    /// The tail of `php_cli_server_dtor`: each worker this process forked
    /// waited for until it exited or a signal ended it.
    pub(super) fn wait(self) {
        for worker in self.forked {
            crate::ffi::wait_for_end(worker.as_raw());
        }
    }
}
