//! The process group of the native face (design §4.4, revision 41; #1128):
//! the process's real user and group ids and its supplementary groups,
//! which php reads from `<unistd.h>` wherever it asks. The engine owns
//! them as it owns the signal record (ADR-0005): they are the process's,
//! one binding in `php_process`'s `unsafe` home (#1935), and every
//! extension that chooses a permission mask or answers `posix_getuid`
//! reads the same one. They are not an extension-to-extension export (§4.10): no
//! extension binds them and none takes them from another.
//!
//! ```php
//! $g = posix_getgroups();
//! var_dump(posix_getuid() === getmyuid(), is_int(posix_getgid()), array_is_list($g), $g === array_filter($g, 'is_int'));
//! ```
//! ```expect
//! bool(true)
//! bool(true)
//! bool(true)
//! bool(true)
//! ```

pub use php_process::process::{gid, groups, uid};
