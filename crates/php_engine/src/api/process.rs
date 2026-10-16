//! The process group of the native face (design §4.4, revision 41; #1128):
//! the process's real and effective user and group ids and its
//! supplementary groups (the effective pair by #1132's ruling),
//! which php reads from `<unistd.h>` wherever it asks. The engine owns
//! them as it owns the signal record (ADR-0005): they are the process's,
//! one binding in `php_process`'s `unsafe` home (#1935), and every
//! extension that chooses a permission mask, checks a session file's
//! owner or answers `posix_getuid` reads the same one. They are not an extension-to-extension export (§4.10): no
//! extension binds them and none takes them from another.
//!
//! The groups are asked in a child, whose lines the fence prints, as
//! ext/posix's `posix_getgroups` witness asks them.
//!
//! ```php
//! $child = '$g = posix_getgroups(); var_dump(array_is_list($g), $g === array_filter($g, "is_int"));';
//! $p = proc_open([PHP_BINARY, '-n', '-r', $child], [1 => ['pipe', 'w']], $pipes);
//! var_dump(posix_getuid() === getmyuid(), is_int(posix_getgid()));
//! echo stream_get_contents($pipes[1]);
//! var_dump(posix_geteuid() === posix_getuid(), posix_getegid() === posix_getgid());
//! ```
//! ```expect process
//! bool(true)
//! bool(true)
//! bool(true)
//! bool(true)
//! bool(true)
//! bool(true)
//! ```

pub use php_process::process::{egid, euid, gid, groups, uid};
