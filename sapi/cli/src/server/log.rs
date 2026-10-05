//! The server's log (`sapi_cli_server_log_write`,
//! `Z/sapi/cli/php_cli_server.c:768`): each entry at one of php's three
//! types, written while the level is at or above it, on stderr as
//! `[<asctime of the local time>] <entry>`, after `[<pid>] ` while the
//! server runs workers, one `write` a line so a line is never split by
//! another worker writing to the same stream.

use std::io::Write;

/// `PHP_CLI_SERVER_LOG_*`: what an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Type {
    /// The startup line.
    Process = 1,
    Error = 2,
    Message = 3,
}

/// The log's level, `php_cli_server_log_level`, 3 lowered once for each
/// `-q` to no less than 1, whether `cli_server.color` asks for colour
/// where stdout is a terminal, and whether each line names the process
/// that writes it, as php's do while the server runs workers.
pub(super) struct Log {
    level: u8,
    color: bool,
    pid: bool,
}

impl Log {
    pub(super) fn new(level: u8, color: bool) -> Log {
        Log {
            level,
            color,
            pid: false,
        }
    }

    /// From here each line opens with the id of the process that writes
    /// it, `[<pid>] `, as `sapi_cli_server_log_write` writes them while
    /// `php_cli_server_workers_max` is above 1.
    pub(super) fn name_processes(&mut self) {
        self.pid = true;
    }

    /// Whether a response's line is coloured: `cli_server.color` on and
    /// stdout a terminal (`php_cli_is_output_tty`).
    pub(super) fn colored(&self) -> bool {
        use std::io::IsTerminal;
        self.color && std::io::stdout().is_terminal()
    }

    /// `php_cli_server_logf(type, …)`: the entry, where the level shows
    /// its type.
    pub(super) fn write(&self, kind: Type, entry: &str) {
        if self.level < kind as u8 {
            return;
        }
        let time = crate::ffi::asctime_now()
            .unwrap_or_else(|| String::from("unknown time, can't be fetched"));
        let line = if self.pid {
            format!("[{}] [{time}] {entry}\n", std::process::id())
        } else {
            format!("[{time}] {entry}\n")
        };
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }
}
