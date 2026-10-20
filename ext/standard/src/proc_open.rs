//! `Z/ext/standard/proc_open.c`, a child process with the fds its
//! `$descriptor_spec` names, beside `file.c`'s `popen` and `pclose`, the
//! same child over one pipe. The child is `std::process::Command`, which
//! on this platform is `posix_spawn(3)` where php's is and `fork(2)` then
//! `exec(2)` where fds past the standard three are placed, as
//! `close_parentends_of_pipes` places them; its pid is the `process`
//! resource's, a type this crate registers per request, and `waitpid(2)`,
//! `kill(2)`, `dup(2)` and `openpty(3)` are `src/ffi/`'s. The parent's end
//! of a pipe is a stream of the engine's plain files kind, `STDIO`, over
//! the fd (`php_stream_fopen_from_fd`); a `popen` stream's close waits for
//! its child, which is what `pclose` answers.
//!
//! The child's working directory and environment are the request's own
//! where the call names none, since a request's `chdir` and `putenv` are
//! its own and not the process's (ADR-0005).
//!
//! The five natives, in the order `basic_functions.stub.php` declares
//! them.
//!
//! ```php
//! foreach (['pclose', 'popen', 'proc_open', 'proc_close', 'proc_terminate', 'proc_get_status'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function pclose ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $handle ]
//!   }
//!   - Return [ int ]
//! }
//! Function [ <internal:standard> function popen ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $command ]
//!     Parameter #1 [ <required> string $mode ]
//!   }
//! }
//! Function [ <internal:standard> function proc_open ] {
//!
//!   - Parameters [6] {
//!     Parameter #0 [ <required> array|string $command ]
//!     Parameter #1 [ <required> array $descriptor_spec ]
//!     Parameter #2 [ <required> &$pipes ]
//!     Parameter #3 [ <optional> ?string $cwd = null ]
//!     Parameter #4 [ <optional> ?array $env_vars = null ]
//!     Parameter #5 [ <optional> ?array $options = null ]
//!   }
//! }
//! Function [ <internal:standard> function proc_close ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $process ]
//!   }
//!   - Return [ int ]
//! }
//! Function [ <internal:standard> function proc_terminate ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $process ]
//!     Parameter #1 [ <optional> int $signal = 15 ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function proc_get_status ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $process ]
//!   }
//!   - Return [ array ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsStr;
use std::ops::ControlFlow;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Stdio};

use php_ext::abi::{OrFalse, Path, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{value_name, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::{Cast, FdStream, OwnedStream as _, Request, Stream, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Resource, ResourceType, Value, ValueRef};

use crate::State;

/// What the request keeps for `proc_open`: the `process` resource type,
/// registered on first use; `FG(pclose_wait)`, set while `proc_close`
/// closes a process so its destructor waits for the child; and
/// `FG(pclose_ret)`, what that destructor answered.
#[derive(Default)]
pub(super) struct ProcState {
    kind: Option<ResourceType>,
    pclose_wait: bool,
    pclose_ret: i64,
}

/// `php_process_handle` (`Z/ext/standard/proc_open.h`): the child's pid,
/// the command as `proc_get_status` prints it, the pipe streams the
/// resource holds one owner of each, and the wait status cached once the
/// child is seen to have exited (`has_cached_exit_wait_status`).
struct Process {
    child: i32,
    command: Vec<u8>,
    pipes: Vec<Owner<Stream>>,
    cached: Option<i32>,
}

// -------------------------------------------------------------- popen

/// `popen(string $command, string $mode)`, `PHP_FUNCTION(popen)`
/// (`Z/ext/standard/file.c:773`): `Z_PARAM_PATH` refuses a null byte; the
/// first `b` of `$mode` is dropped and what is left must be `r`, `rb`,
/// `w` or `wb`, php's own check before the C library's; then `popen(3)`,
/// `/bin/sh -c` with one end of a pipe as the child's `stdout` for
/// `r` and its standard input for `w`, whose own `rb` and `wb` the C
/// library refuses with `EINVAL`. The stream is the plain files kind over
/// the pipe with `$mode` as given, never seeking, and `pclose` waits for
/// the child and answers its exit status.
///
/// ```php
/// $f = popen('echo x; exit 2', 'r');
/// var_dump(stream_get_meta_data($f), fgets($f), feof($f), fgets($f), feof($f), pclose($f));
/// $f = popen('cat > ' . __DIR__ . '/popen.txt', 'wb');
/// var_dump(fwrite($f, "piped\n"), pclose($f), file_get_contents(__DIR__ . '/popen.txt'));
/// var_dump(popen('true', 'rbb'));
/// try { popen("a\0b", 'r'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { popen('true', 'rw'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(7) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(false)
///   ["stream_type"]=>
///   string(5) "STDIO"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(false)
/// }
/// string(2) "x
/// "
/// bool(false)
/// bool(false)
/// bool(true)
/// int(2)
/// int(6)
/// int(0)
/// string(6) "piped
/// "
///
/// Warning: popen(true,rb): Invalid argument in %s on line 6
/// bool(false)
/// popen(): Argument #1 ($command) must not contain any null bytes
/// popen(): Argument #2 ($mode) must be one of "r", "rb", "w", or "wb"
/// ```
#[php_function]
fn popen(
    req: &mut Request,
    command: Path,
    mode: PhpString,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let command = command.0.as_bytes().to_vec();
    let mode = mode.as_bytes().to_vec();
    let mut posix_mode = mode.clone();
    if let Some(at) = posix_mode.iter().position(|b| *b == b'b') {
        posix_mode.remove(at);
    }
    if !matches!(posix_mode.as_slice(), b"r" | b"w" | b"rb" | b"wb") {
        return Err(arg_value_error(
            req,
            2,
            "be one of \"r\", \"rb\", \"w\", or \"wb\"",
        ));
    }
    let shown = |req: &mut Request, message: &str| {
        let text = format!(
            "popen({},{}): {message}",
            String::from_utf8_lossy(&command),
            String::from_utf8_lossy(&posix_mode)
        );
        php_ext::raising::warning(req, &text);
    };
    if posix_mode.len() == 2 {
        let text = strerror(req, libc_einval());
        shown(req, &text);
        return Ok(Untyped(OrFalse::False));
    }
    let reading = posix_mode == b"r";
    let mut sh = shell(req, &command);
    if reading {
        sh.stdout(Stdio::piped());
    } else {
        sh.stdin(Stdio::piped());
    }
    let mut child = match sh.spawn() {
        Ok(child) => child,
        Err(error) => {
            let text = strerror(req, error.raw_os_error().unwrap_or(0));
            shown(req, &text);
            return Ok(Untyped(OrFalse::False));
        }
    };
    let pipe: Option<OwnedFd> = if reading {
        child.stdout.take().map(OwnedFd::from)
    } else {
        child.stdin.take().map(OwnedFd::from)
    };
    let Some(pipe) = pipe else {
        return Ok(Untyped(OrFalse::False));
    };
    let stream = req.stream_from_fd(pipe, FdStream::Process { mode: &mode, child })?;
    Ok(Untyped(OrFalse::Value(stream.into_resource())))
}

/// `strerror(errno)`: the host's text for an `errno`, without the
/// ` (os error N)` Rust puts after it.
fn strerror(req: &Request, code: i32) -> String {
    let text = req.environment().os_error_text(code);
    text.split_once(" (os error")
        .map_or(text.clone(), |(text, _)| text.to_owned())
}

/// `EINVAL`, what the C library's `popen(3)` answers a `mode` it does not
/// take.
const fn libc_einval() -> i32 {
    22
}

/// `/bin/sh -c command`, as `popen(3)` and `proc_open`'s string form hand
/// a command, `sh` its `argv[0]`, in the request's working directory and
/// environment ([`in_request`]). What `exec`'s `VCWD_POPEN` and `mail`'s `popen` start
/// too, so a shell's own message names itself `sh` as php's children do.
pub(crate) fn shell(req: &Request, command: &[u8]) -> Command {
    use std::os::unix::process::CommandExt;
    let mut sh = Command::new("/bin/sh");
    sh.arg0("sh").arg("-c").arg(OsStr::from_bytes(command));
    in_request(req, &mut sh);
    sh
}

/// The request's working directory and environment, where a child of
/// the process would have the process's: the environment handed over in
/// its own order, as php hands `posix_spawn` its `envp`. php adds no
/// `chdir` to a spawn with no `$cwd` (`Z/ext/standard/proc_open.c:1394`),
/// so the child inherits the process's directory, which `exec(2)` needs
/// no access to; the engine's requests share one process and each stands
/// in a directory of its own, so the child is changed into the request's
/// only where that is not the process's. A request whose host could not
/// name the process's directory, `getcwd(3)` failing, was given none and
/// stands in it (#2917).
fn in_request(req: &Request, command: &mut Command) {
    let cwd = req.environment().cwd();
    if !cwd.as_os_str().is_empty() && crate::ffi::process_dir().as_ref() != Some(&cwd) {
        command.current_dir(cwd);
    }
    crate::ffi::environment(command, req.environment().env_pairs());
}

/// `pclose($handle): int`, `PHP_FUNCTION(pclose)`
/// (`Z/ext/standard/file.c:829`): the stream's resource closed and what
/// its kind's close answered, the child's exit status for a stream
/// `popen` opened and `0` for any other.
///
/// ```php
/// var_dump(pclose(popen('exit 3', 'r')), pclose(fopen(__FILE__, 'r')));
/// try { pclose(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(0)
/// pclose(): Argument #1 ($handle) must be of type resource, int given
/// ```
#[php_function]
fn pclose(req: &mut Request, handle: Stream) -> i64 {
    handle.pclose(req)
}

// ---------------------------------------------------------- proc_open

/// Which of php's `DESCRIPTOR_TYPE_*` an entry is: the child's end
/// alone (`STD`, a file or a redirect), a pipe whose parent end becomes a
/// stream of `mode`, or a pty's.
enum Parent {
    None,
    Pipe { fd: OwnedFd, mode: &'static [u8] },
    Socket { fd: OwnedFd },
}

/// `descriptorspec_item`: the fd number the child sees, the fd the
/// parent holds for it until the spawn, and the parent's end.
struct Descriptor {
    index: i64,
    childend: OwnedFd,
    parent: Parent,
}

/// The `process` resource type, `le_proc_open`, registered with its
/// destructor on first use in the request.
fn process_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().proc_open.kind {
        return kind;
    }
    let kind = req
        .memory_mut()
        .resources()
        .register_type(b"process", Some(process_dtor));
    req.state::<State>().proc_open.kind = Some(kind);
    kind
}

/// `waitpid_cached` (`Z/ext/standard/proc_open.c:235`): the status the
/// resource cached once the child exited, else `waitpid(2)`, whose answer
/// is cached where it says the child exited. A wait that blocks lends
/// the request ([`Limits::blocking`]), `proc_close`'s (#2401); the
/// resource is read before it and written after it, never during.
fn waitpid_cached(req: &mut Request, res: Resource, options: i32) -> std::io::Result<(i32, i32)> {
    let Some(process) = res.native::<Process>(req) else {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    };
    let child = process.child;
    if let Some(status) = process.cached {
        return Ok((child, status));
    }
    let wait = || loop {
        match crate::ffi::waitpid(child, options) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            answered => break answered,
        }
    };
    let answered = if options & crate::ffi::WNOHANG == 0 {
        req.limits_mut().blocking(wait)
    } else {
        wait()
    };
    if let Ok((pid, status)) = answered
        && pid > 0
        && exited(status)
        && let Some(process) = res.native_mut::<Process>(req)
    {
        process.cached = Some(status);
    }
    answered
}

/// `WIFEXITED`, `WEXITSTATUS`, `WIFSIGNALED`, `WTERMSIG`, `WIFSTOPPED`
/// and `WSTOPSIG` over a wait status, as `<sys/wait.h>` lays it out.
const fn exited(status: i32) -> bool {
    status.trailing_zeros() >= 7
}
const fn exit_status(status: i32) -> i32 {
    (status >> 8) & 0xff
}
const fn signaled(status: i32) -> bool {
    status & 0x7f != 0 && status & 0x7f != 0x7f
}
const fn term_sig(status: i32) -> i32 {
    status & 0x7f
}
const fn stopped(status: i32) -> bool {
    status & 0xff == 0x7f
}
const fn stop_sig(status: i32) -> i32 {
    (status >> 8) & 0xff
}

/// `proc_open_rsrc_dtor` (`Z/ext/standard/proc_open.c:259`): each pipe
/// the resource holds given up and closed, then the child waited for,
/// without blocking unless `proc_close` is closing it, and
/// `FG(pclose_ret)` its exit status, the wait status where a signal ended
/// it, or `-1` where no status was had.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the resource destructor's shape, which a destructor that throws needs"
)]
fn process_dtor(req: &mut Request, res: Resource) -> Result<(), Thrown> {
    let Some(process) = res.native_mut::<Process>(req) else {
        return Ok(());
    };
    let pipes = std::mem::take(&mut process.pipes);
    for pipe in pipes {
        // A pipe the program closed is a closed resource: only the owner
        // is left to give up.
        if Stream::from_value(req, &pipe.as_value()).is_some() {
            pipe.free(req);
        } else {
            pipe.give_up(req);
        }
    }
    let wait = req.state::<State>().proc_open.pclose_wait;
    let options = if wait { 0 } else { crate::ffi::WNOHANG };
    let ret = match waitpid_cached(req, res, options) {
        Ok((pid, status)) if pid > 0 => {
            if exited(status) {
                exit_status(status)
            } else {
                status
            }
        }
        _ => -1,
    };
    req.state::<State>().proc_open.pclose_ret = i64::from(ret);
    Ok(())
}

/// `zend_fetch_resource(res, "process", le_proc_open)` over the
/// resource `Z_PARAM_RESOURCE` held: a live `process` resource, and the
/// fetch's `TypeError` for one of another type or closed.
fn process_of(req: &mut Request, res: Resource, native: &str) -> Result<Resource, Thrown> {
    let kind = process_type(req);
    if res.kind() != Some(kind) {
        let text = format!("{native}(): supplied resource is not a valid process resource");
        return Err(throw_error(req, "TypeError", &text));
    }
    Ok(res)
}

/// `get_valid_arg_string` (`Z/ext/standard/proc_open.c:505`): one element
/// of the command array as a string, the first not empty and none with a
/// null byte, each refusal php's `ValueError`.
fn arg_string(req: &mut Request, value: &Value, elem_num: usize) -> Result<Vec<u8>, Thrown> {
    let text = with_string(req, value, <[u8]>::to_vec)?;
    if elem_num == 1 && text.is_empty() {
        return Err(throw_error(
            req,
            "ValueError",
            "First element must contain a non-empty program name",
        ));
    }
    if text.contains(&0) {
        let message = format!("Command array element {elem_num} contains a null byte");
        return Err(throw_error(req, "ValueError", &message));
    }
    Ok(text)
}

/// `get_string_parameter` (`Z/ext/standard/proc_open.c:815`): the
/// entry's element at `index` as a string, php's `Missing %s`
/// where there is none.
fn string_parameter(
    req: &mut Request,
    ht: PhpArray,
    index: i64,
    name: &str,
) -> Result<Vec<u8>, Thrown> {
    let Some(value) = ht.get(&Key::Int(index)) else {
        return Err(throw_error(req, "ValueError", &format!("Missing {name}")));
    };
    with_string(req, &value, <[u8]>::to_vec)
}

/// `set_proc_descriptor_to_pipe` (`Z/ext/standard/proc_open.c:883`): a
/// new pipe; a `mode` not starting with `w` gives the child the reading
/// end, and the parent writes, php's `O_WRONLY`, so its stream is `w`;
/// a `w` the other way round.
fn pipe_descriptor(req: &mut Request, index: i64, mode: &[u8]) -> Option<Descriptor> {
    let (reader, writer) = match std::io::pipe() {
        Ok(pair) => pair,
        Err(error) => {
            let text = format!(
                "Unable to create pipe {}",
                strerror(req, error.raw_os_error().unwrap_or(0))
            );
            docref(req, Level::Warning, &text);
            return None;
        }
    };
    let (reader, writer) = (OwnedFd::from(reader), OwnedFd::from(writer));
    Some(if mode.starts_with(b"w") {
        Descriptor {
            index,
            childend: writer,
            parent: Parent::Pipe {
                fd: reader,
                mode: b"r",
            },
        }
    } else {
        Descriptor {
            index,
            childend: reader,
            parent: Parent::Pipe {
                fd: writer,
                mode: b"w",
            },
        }
    })
}

/// A `dup(2)` of an fd for an entry, php's warning
/// `Failed to dup() for descriptor %d: %s` where it fails; `nindex` is the entry's
/// position in the spec, as php prints it.
fn dup_descriptor(req: &mut Request, from: RawFd, nindex: usize) -> Option<OwnedFd> {
    match crate::ffi::dup(from) {
        Ok(fd) => Some(fd),
        Err(error) => {
            let text = format!(
                "Failed to dup() for descriptor {nindex}: {}",
                strerror(req, error.raw_os_error().unwrap_or(0))
            );
            docref(req, Level::Warning, &text);
            None
        }
    }
}

/// What the walk over `$descriptor_spec` keeps besides its entries:
/// the pty's two ends, opened by the first `pty` entry and shared by
/// every other.
#[derive(Default)]
struct Pty {
    master: Option<OwnedFd>,
    slave: Option<OwnedFd>,
}

/// `set_proc_descriptor_from_array` (`Z/ext/standard/proc_open.c:1029`):
/// one array of the spec, by its first element. `Ok(None)` is php's
/// `FAILURE` after its warning; the `Err` is a `ValueError`.
fn array_descriptor(
    req: &mut Request,
    ht: PhpArray,
    index: i64,
    done: &[Descriptor],
    pty: &mut Pty,
) -> Result<Option<Descriptor>, Thrown> {
    let ztype = string_parameter(req, ht, 0, "handle qualifier")?;
    let child_only = |childend| Descriptor {
        index,
        childend,
        parent: Parent::None,
    };
    match ztype.as_slice() {
        b"pipe" => {
            let mode = string_parameter(req, ht, 1, "mode parameter for 'pipe'")?;
            Ok(pipe_descriptor(req, index, &mode))
        }
        b"file" => {
            let file = string_parameter(req, ht, 1, "file name parameter for 'file'")?;
            let mode = string_parameter(req, ht, 2, "mode parameter for 'file'")?;
            Ok(file_descriptor(req, &file, &mode, done.len())?.map(child_only))
        }
        b"redirect" => Ok(redirect_descriptor(req, ht, done)?.map(child_only)),
        b"null" => match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            Ok(file) => Ok(Some(child_only(OwnedFd::from(file)))),
            Err(error) => {
                let text = format!(
                    "Failed to open /dev/null: {}",
                    strerror(req, error.raw_os_error().unwrap_or(0))
                );
                docref(req, Level::Warning, &text);
                Ok(None)
            }
        },
        b"pty" => Ok(pty_descriptor(req, index, done.len(), pty)),
        b"socket" => Ok(socket_descriptor(req, index)),
        _ => {
            let text = format!(
                "{} is not a valid descriptor spec/mode",
                String::from_utf8_lossy(&ztype)
            );
            docref(req, Level::Warning, &text);
            Ok(None)
        }
    }
}

/// `set_proc_descriptor_to_socket` (`Z/ext/standard/proc_open.c:920`): a
/// pair of connected unix sockets, the first the parent's, a
/// `generic_socket` stream, and the second the child's.
fn socket_descriptor(req: &mut Request, index: i64) -> Option<Descriptor> {
    const AF_UNIX: i32 = 1;
    const SOCK_STREAM: i32 = 1;
    match crate::ffi::socketpair(AF_UNIX, SOCK_STREAM, 0) {
        Ok((parentend, childend)) => Some(Descriptor {
            index,
            childend,
            parent: Parent::Socket { fd: parentend },
        }),
        Err(error) => {
            let text = format!(
                "Unable to create socket pair: {}",
                strerror(req, error.raw_os_error().unwrap_or(0))
            );
            docref(req, Level::Warning, &text);
            None
        }
    }
}

/// `set_proc_descriptor_to_file` (`Z/ext/standard/proc_open.c:940`): the
/// path opened through its wrapper, the stream forced into an fd and
/// given up, and the child's copy of the fd.
fn file_descriptor(
    req: &mut Request,
    file: &[u8],
    mode: &[u8],
    nindex: usize,
) -> Result<Option<OwnedFd>, Thrown> {
    let Some(stream) = req.open_stream(file, mode, false)? else {
        return Ok(None);
    };
    let fd = stream.cast(req, Cast::Fd, true);
    let childend = fd.and_then(|fd| dup_descriptor(req, fd, nindex));
    stream.free(req);
    Ok(childend)
}

/// `redirect_proc_descriptor` (`Z/ext/standard/proc_open.c:993`) behind
/// its checks: the target an int, and the child end of the entry for it,
/// or the parent's own standard fd for `0`, `1` or `2`, duplicated.
fn redirect_descriptor(
    req: &mut Request,
    ht: PhpArray,
    done: &[Descriptor],
) -> Result<Option<OwnedFd>, Thrown> {
    let Some(target) = ht.get(&Key::Int(1)).map(|value| value.deref()) else {
        return Err(throw_error(req, "ValueError", "Missing redirection target"));
    };
    let Some(target) = target.as_int() else {
        let name = value_name(req, &target);
        let text = format!("Redirection target must be of type int, {name} given");
        return Err(throw_error(req, "ValueError", &text));
    };
    let found = done
        .iter()
        .find(|d| d.index == target)
        .map(|d| d.childend.as_raw_fd());
    let redirect_to = match found {
        Some(fd) => fd,
        None if (0..=2).contains(&target) => RawFd::try_from(target).unwrap_or(0),
        None => {
            let text = format!("Redirection target {target} not found");
            docref(req, Level::Warning, &text);
            return Ok(None);
        }
    };
    Ok(dup_descriptor(req, redirect_to, done.len()))
}

/// `set_proc_descriptor_to_pty` (`Z/ext/standard/proc_open.c:844`): the
/// pty opened by the first entry that names one; the child's end a copy
/// of its slave and the parent's a copy of its master, a stream `r+`.
fn pty_descriptor(
    req: &mut Request,
    index: i64,
    nindex: usize,
    pty: &mut Pty,
) -> Option<Descriptor> {
    if pty.master.is_none() {
        match crate::ffi::openpty() {
            Ok((master, slave)) => {
                pty.master = Some(master);
                pty.slave = Some(slave);
            }
            Err(error) => {
                let text = format!(
                    "Could not open PTY (pseudoterminal): {}",
                    strerror(req, error.raw_os_error().unwrap_or(0))
                );
                docref(req, Level::Warning, &text);
                return None;
            }
        }
    }
    let master = pty.master.as_ref()?.as_raw_fd();
    let slave = pty.slave.as_ref()?.as_raw_fd();
    let childend = dup_descriptor(req, slave, nindex)?;
    let parentend = dup_descriptor(req, master, nindex)?;
    Some(Descriptor {
        index,
        childend,
        parent: Parent::Pipe {
            fd: parentend,
            mode: b"r+",
        },
    })
}

/// `set_proc_descriptor_from_resource` (`Z/ext/standard/proc_open.c:1090`):
/// a stream of the spec, cast to its fd as `PHP_STREAM_AS_FD` casts it,
/// and the fd duplicated for the child.
fn resource_descriptor(
    req: &mut Request,
    value: &Value,
    index: i64,
    nindex: usize,
) -> Result<Option<Descriptor>, Thrown> {
    let Some(stream) = Stream::from_value(req, value) else {
        return Err(throw_error(
            req,
            "TypeError",
            "proc_open(): supplied resource is not a valid stream resource",
        ));
    };
    let Some(fd) = stream.cast(req, Cast::Fd, true) else {
        return Ok(None);
    };
    Ok(dup_descriptor(req, fd, nindex).map(|childend| Descriptor {
        index,
        childend,
        parent: Parent::None,
    }))
}

/// The child's environment, name and value.
type Env = Vec<(Vec<u8>, Vec<u8>)>;

/// `php_array_to_envp` (`Z/ext/standard/proc_open.c:142`): each element
/// as a string, an empty one left out, under its key as `key=value`; an
/// element with no string key is taken as the `name=value` it spells.
fn environment(req: &mut Request, env: PhpArray) -> Result<Env, Thrown> {
    let mut pairs = Vec::new();
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(environment, key, element)`
    // (`Z/ext/standard/proc_open.c:171`), no owner of the table taken: the
    // argument holds it while `zval_get_string` runs `__toString`.
    walk::foreach(req, env, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let text = with_string(req, &value, <[u8]>::to_vec)?;
        if text.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        match key {
            Key::Str(name) if !name.is_empty() => pairs.push((name.as_bytes().to_vec(), text)),
            _ => {
                if let Some(at) = text.iter().position(|b| *b == b'=') {
                    pairs.push((text[..at].to_vec(), text[at + 1..].to_vec()));
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(pairs)
}

/// `proc_open(array|string $command, array $descriptor_spec, &$pipes,
/// ?string $cwd = null, ?array $env_vars = null, ?array $options = null)`,
/// `PHP_FUNCTION(proc_open)` (`Z/ext/standard/proc_open.c:1193`): a command
/// array's elements as the child's `argv`, searched for on the path, and
/// a string handed to `/bin/sh -c`; each entry of the spec by its
/// integer key, a stream or an array naming a `pipe`, a `socket`, a
/// `file`, a `redirect` to another, `null` or a `pty`; the child spawned with each
/// fd placed where its key says, in `$cwd` and with `$env_vars` where
/// they are given; then `$pipes` written with a stream for each pipe's
/// parent end, under its key, and the `process` resource. A spawn that
/// fails is php's `posix_spawn() failed: %s`.
///
/// ```php
/// $p = proc_open('cat', [0 => ['pipe', 'r'], 1 => ['pipe', 'w'], 2 => ['redirect', 1]], $pipes);
/// var_dump($p, array_keys($pipes));
/// fwrite($pipes[0], "hello\n"); fclose($pipes[0]);
/// var_dump(fread($pipes[1], 100), fread($pipes[1], 100), stream_get_meta_data($pipes[1]), proc_close($p), $pipes[1]);
/// $p = proc_open(['sh', '-c', 'echo $FOO; pwd; echo err >&2; echo three >&3; exit 4'], [1 => ['pipe', 'w'], 2 => ['file', __DIR__ . '/proc_open.txt', 'w'], 3 => ['pipe', 'w']], $pipes, '/', ['FOO' => 'bar', 'EMPTY' => '']);
/// function all($pipe) { $out = ''; while (!feof($pipe)) { $out .= fread($pipe, 100); } return $out; }
/// var_dump(all($pipes[1]), all($pipes[3]), proc_close($p), file_get_contents(__DIR__ . '/proc_open.txt'));
/// var_dump(proc_open(['nonexistent_command_of_this_witness'], [], $pipes));
/// $p = proc_open('true', [0 => ['null'], 1 => ['pty']], $pipes);
/// var_dump(stream_get_meta_data($pipes[1])['mode'], proc_close($p));
/// $p = proc_open('cat', [0 => ['socket'], 1 => ['socket']], $pipes);
/// fwrite($pipes[0], "sock\n"); stream_socket_shutdown($pipes[0], STREAM_SHUT_WR);
/// var_dump(stream_get_meta_data($pipes[0])['stream_type'], fgets($pipes[1]), proc_close($p));
/// ```
/// ```expect
/// resource(%d) of type (process)
/// array(2) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
/// }
/// string(6) "hello
/// "
/// string(0) ""
/// array(7) {
///   ["timed_out"]=>
///   bool(false)
///   ["blocked"]=>
///   bool(true)
///   ["eof"]=>
///   bool(true)
///   ["stream_type"]=>
///   string(5) "STDIO"
///   ["mode"]=>
///   string(1) "r"
///   ["unread_bytes"]=>
///   int(0)
///   ["seekable"]=>
///   bool(false)
/// }
/// int(0)
/// resource(%d) of type (Unknown)
/// string(6) "bar
/// /
/// "
/// string(6) "three
/// "
/// int(4)
/// string(4) "err
/// "
///
/// Warning: proc_open(): posix_spawn() failed: No such file or directory in %s on line 9
/// bool(false)
/// string(2) "r+"
/// int(0)
/// string(14) "generic_socket"
/// string(5) "sock
/// "
/// int(0)
/// ```
///
/// The spec is held element by element, the refusals `ValueError`s and
/// an entry that cannot be made php's warning and `false`:
///
/// ```php
/// foreach ([
///   fn() => proc_open([], [], $p),
///   fn() => proc_open([''], [], $p),
///   fn() => proc_open(["a\0b"], [], $p),
///   fn() => proc_open('true', ['x' => ['pipe', 'r']], $p),
///   fn() => proc_open('true', [1], $p),
///   fn() => proc_open('true', [[]], $p),
///   fn() => proc_open('true', [['pipe']], $p),
///   fn() => proc_open('true', [['file']], $p),
///   fn() => proc_open('true', [['file', 'x']], $p),
///   fn() => proc_open('true', [['redirect']], $p),
///   fn() => proc_open('true', [['redirect', '1']], $p),
///   fn() => proc_open('true', [['redirect', 5]], $p),
///   fn() => proc_open('true', [['bogus']], $p),
///   fn() => proc_open('true', [3 => ['file', '/nonexistent/x', 'r']], $p),
///   fn() => proc_open('true', [fopen('php://memory', 'r')], $p),
/// ] as $i => $f) {
///   try { var_dump($f()); } catch (ValueError $e) { echo $i, ': ', $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// 0: proc_open(): Argument #1 ($command) must not be empty
/// 1: First element must contain a non-empty program name
/// 2: Command array element 1 contains a null byte
/// 3: proc_open(): Argument #2 ($descriptor_spec) must be an integer indexed array
/// 4: proc_open(): Argument #2 ($descriptor_spec) must only contain arrays and streams
/// 5: Missing handle qualifier
/// 6: Missing mode parameter for 'pipe'
/// 7: Missing file name parameter for 'file'
/// 8: Missing mode parameter for 'file'
/// 9: Missing redirection target
/// 10: Redirection target must be of type int, string given
///
/// Warning: proc_open(): Redirection target 5 not found in %s on line 14
/// bool(false)
///
/// Warning: proc_open(): bogus is not a valid descriptor spec/mode in %s on line 15
/// bool(false)
///
/// Warning: proc_open(/nonexistent/x): Failed to open stream: No such file or directory in %s on line 16
/// bool(false)
///
/// Warning: proc_open(): Cannot represent a stream of type MEMORY as a File Descriptor in %s on line 17
/// bool(false)
/// ```
#[php_function]
fn proc_open(
    req: &mut Request,
    command: Union<PhpArray, PhpString>,
    descriptor_spec: PhpArray,
    pipes: Ref<Untyped<Value>>,
    #[php(default = null)] cwd: Option<PhpString>,
    #[php(default = null)] env_vars: Option<PhpArray>,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let _ = options;
    let cwd = cwd.map(|cwd| cwd.as_bytes().to_vec());
    let (command_str, argv) = command_of(req, command)?;
    let env = match env_vars {
        Some(env) => Some(environment(req, env)?),
        None => None,
    };
    let mut descriptors: Vec<Descriptor> = Vec::new();
    let mut pty = Pty::default();
    // `ZEND_HASH_FOREACH_KEY_VAL(descriptorspec, nindex, str_index, descitem)`
    // (`Z/ext/standard/proc_open.c:1288`), no owner of the table taken: the
    // argument holds it while a stream's cast runs its wrapper.
    let walked = walk::foreach(
        req,
        descriptor_spec,
        Hold::None,
        |req, row| -> walk::Step<bool> {
            let key = row.key().clone();
            let item = row.value().deref();
            let Key::Int(index) = key else {
                return Err(arg_value_error(req, 2, "be an integer indexed array"));
            };
            let nindex = descriptors.len();
            let made = match item.as_ref() {
                ValueRef::Resource(_) => resource_descriptor(req, &item, index, nindex)?,
                ValueRef::Array(ht) => array_descriptor(req, *ht, index, &descriptors, &mut pty)?,
                _ => {
                    return Err(arg_value_error(req, 2, "only contain arrays and streams"));
                }
            };
            match made {
                Some(descriptor) => descriptors.push(descriptor),
                None => return Ok(ControlFlow::Break(false)),
            }
            Ok(ControlFlow::Continue(()))
        },
    )?;
    if walked.is_some() {
        return Ok(Untyped(OrFalse::False));
    }
    let child = child_command(
        req,
        &command_str,
        argv.as_deref(),
        cwd.as_deref(),
        env.as_ref(),
    );
    let Some(pid) = spawn(req, child, &descriptors) else {
        return Ok(Untyped(OrFalse::False));
    };
    drop(pty);
    let mut opened = PhpArray::new(req)?;
    let mut held = Vec::new();
    let filled = match parent_ends(req, descriptors, &mut opened, &mut held) {
        Ok(()) => pipes.set(req, Untyped(opened.into_value())),
        Err(thrown) => {
            opened.give_up(req);
            Err(thrown)
        }
    };
    if let Err(thrown) = filled {
        for stream in held {
            stream.free(req);
        }
        return Err(thrown);
    }
    let kind = process_type(req);
    let process = Process {
        child: pid,
        command: command_str,
        pipes: held,
        cached: None,
    };
    let res = Resource::new(req, kind, process)?;
    Ok(Untyped(OrFalse::Value(res)))
}

/// The parent's end of each pipe made a stream, `$pipes[index]` written
/// with it and the resource's own owner of it kept in `held`
/// (`Z/ext/standard/proc_open.c:1478`).
fn parent_ends(
    req: &mut Request,
    descriptors: Vec<Descriptor>,
    opened: &mut Owner<PhpArray>,
    held: &mut Vec<Owner<Stream>>,
) -> Result<(), Thrown> {
    for descriptor in descriptors {
        let Descriptor {
            index,
            childend,
            parent,
        } = descriptor;
        drop(childend);
        let stream = match parent {
            Parent::Pipe { fd, mode } => req.stream_from_fd(fd, FdStream::File { mode })?,
            Parent::Socket { fd } => req.stream_from_fd(fd, FdStream::Socket)?,
            Parent::None => continue,
        };
        let value = Owner::take(req, stream.as_value());
        held.push(stream);
        opened.set(req, Owner::index(index), value)?;
    }
    Ok(())
}

/// The command as `proc_get_status` prints it, and the `argv` of an
/// array command.
type Commanded = (Vec<u8>, Option<Vec<Vec<u8>>>);

/// `get_command_from_array` (`Z/ext/standard/proc_open.c:777`) and the
/// string form: the command as `proc_get_status` prints it, the first
/// element of an array, and the array's elements as `argv`.
fn command_of(req: &mut Request, command: Union<PhpArray, PhpString>) -> Result<Commanded, Thrown> {
    match command {
        Union::A(array) => {
            if array.is_empty() {
                return Err(arg_value_error(req, 1, "not be empty"));
            }
            let mut argv = Vec::new();
            // `ZEND_HASH_FOREACH_VAL(array, arg_zv)`
            // (`Z/ext/standard/proc_open.c:785`), no owner of the table
            // taken: the argument holds it while `__toString` runs.
            walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
                argv.push(arg_string(req, &row.value().deref(), argv.len() + 1)?);
                Ok(ControlFlow::Continue(()))
            })?;
            Ok((argv[0].clone(), Some(argv)))
        }
        Union::B(command) => Ok((command.as_bytes().to_vec(), None)),
    }
}

/// The child as `posix_spawnp` of `argv` or `/bin/sh -c` of the string
/// would start it: in `$cwd` or the request's working directory, with
/// `$env_vars` or the request's environment.
fn child_command(
    req: &Request,
    command_str: &[u8],
    argv: Option<&[Vec<u8>]>,
    cwd: Option<&[u8]>,
    env: Option<&Env>,
) -> Command {
    let mut child = match argv {
        Some(argv) => {
            let mut child = Command::new(OsStr::from_bytes(&argv[0]));
            child.args(argv[1..].iter().map(|arg| OsStr::from_bytes(arg)));
            in_request(req, &mut child);
            child
        }
        None => shell(req, command_str),
    };
    if let Some(cwd) = cwd {
        child.current_dir(OsStr::from_bytes(cwd));
    }
    if let Some(env) = env {
        crate::ffi::environment(&mut child, env);
    }
    child
}

/// The spawn, each entry's child end placed at its key: the child's pid,
/// or `None` after php's `posix_spawn() failed: %s`.
fn spawn(req: &mut Request, mut child: Command, descriptors: &[Descriptor]) -> Option<i32> {
    let moves = descriptors
        .iter()
        .filter_map(|d| {
            RawFd::try_from(d.index)
                .ok()
                .map(|to| (d.childend.as_raw_fd(), to))
        })
        .collect();
    crate::ffi::place_fds(&mut child, moves);
    match child.spawn() {
        Ok(spawned) => Some(i32::try_from(spawned.id()).unwrap_or(-1)),
        Err(error) => {
            let text = format!(
                "posix_spawn() failed: {}",
                strerror(req, error.raw_os_error().unwrap_or(0))
            );
            docref(req, Level::Warning, &text);
            None
        }
    }
}

/// `proc_close($process): int`, `PHP_FUNCTION(proc_close)`
/// (`Z/ext/standard/proc_open.c:359`): the resource closed with
/// `FG(pclose_wait)` set, so its destructor closes the pipes and waits
/// for the child, and what it answered: the exit status, the wait status
/// where a signal ended the child, `-1` where it was waited for already.
///
/// ```php
/// $p = proc_open('exit 5', [], $pipes);
/// var_dump(proc_close($p), $p);
/// try { proc_close($p); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { proc_close(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { proc_close(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// resource(%d) of type (Unknown)
/// proc_close(): supplied resource is not a valid process resource
/// proc_close(): supplied resource is not a valid process resource
/// proc_close(): Argument #1 ($process) must be of type resource, int given
/// ```
#[php_function]
fn proc_close(req: &mut Request, process: Resource) -> Result<i64, Thrown> {
    let res = process_of(req, process, "proc_close")?;
    req.state::<State>().proc_open.pclose_wait = true;
    req.state::<State>().proc_open.pclose_ret = -1;
    res.close(req);
    let state = &mut req.state::<State>().proc_open;
    state.pclose_wait = false;
    Ok(state.pclose_ret)
}

/// `proc_terminate($process, int $signal = 15): bool`,
/// `PHP_FUNCTION(proc_terminate)` (`Z/ext/standard/proc_open.c:333`):
/// `kill(2)` of the child with the signal, and whether it was sent.
///
/// ```php
/// $p = proc_open('sleep 5', [], $pipes);
/// var_dump(proc_terminate($p), proc_terminate($p, 0));
/// time_nanosleep(0, 200000000);
/// $s = proc_get_status($p);
/// var_dump($s['running'], $s['signaled'], $s['termsig'], proc_terminate($p), proc_close($p));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// int(15)
/// bool(false)
/// int(-1)
/// ```
#[php_function]
fn proc_terminate(
    req: &mut Request,
    process: Resource,
    #[php(default = 15)] signal: i64,
) -> Result<bool, Thrown> {
    let res = process_of(req, process, "proc_terminate")?;
    let pid = res.native::<Process>(req).map_or(-1, |p| p.child);
    let signal = i32::try_from(signal).unwrap_or(-1);
    Ok(crate::ffi::kill(pid, signal))
}

/// `proc_get_status($process): array`, `PHP_FUNCTION(proc_get_status)`
/// (`Z/ext/standard/proc_open.c:381`): the command, the pid, whether the
/// status was cached, and what `waitpid(2)` under `WNOHANG | WUNTRACED`
/// says of the child; a status is cached once the child is seen to have
/// exited, so a second call answers the same, where a child a signal
/// ended is reaped and a second call finds it gone, neither running nor
/// signalled.
///
/// ```php
/// $p = proc_open('exit 7', [], $pipes); time_nanosleep(0, 200000000);
/// $s = proc_get_status($p); var_dump(is_int($s['pid'])); unset($s['pid']); var_dump($s);
/// $s = proc_get_status($p); var_dump($s['cached'], $s['exitcode'], proc_close($p));
/// $p = proc_open('sleep 1', [], $pipes);
/// $s = proc_get_status($p); var_dump($s['command'], $s['running'], $s['exitcode']); proc_terminate($p); proc_close($p);
/// ```
/// ```expect
/// bool(true)
/// array(8) {
///   ["command"]=>
///   string(6) "exit 7"
///   ["cached"]=>
///   bool(true)
///   ["running"]=>
///   bool(false)
///   ["signaled"]=>
///   bool(false)
///   ["stopped"]=>
///   bool(false)
///   ["exitcode"]=>
///   int(7)
///   ["termsig"]=>
///   int(0)
///   ["stopsig"]=>
///   int(0)
/// }
/// bool(true)
/// int(7)
/// int(7)
/// string(7) "sleep 1"
/// bool(true)
/// int(-1)
/// ```
#[php_function]
fn proc_get_status(req: &mut Request, process: Resource) -> Result<Owner<PhpArray>, Thrown> {
    let res = process_of(req, process, "proc_get_status")?;
    let (mut running, mut is_signaled, mut is_stopped) = (true, false, false);
    let (mut exitcode, mut termsig, mut stopsig) = (-1, 0, 0);
    let waited = waitpid_cached(req, res, crate::ffi::WNOHANG | crate::ffi::WUNTRACED);
    let Some(process) = res.native::<Process>(req) else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: a process resource without its process",
        ));
    };
    match waited {
        Ok((pid, status)) if pid == process.child => {
            if exited(status) {
                running = false;
                exitcode = exit_status(status);
            }
            if signaled(status) {
                running = false;
                is_signaled = true;
                termsig = term_sig(status);
            }
            if stopped(status) {
                is_stopped = true;
                stopsig = stop_sig(status);
            }
        }
        Ok(_) => {}
        Err(_) => running = false,
    }
    let cached = process.cached.is_some();
    let command = process.command.clone();
    let pid = process.child;
    let mut out = PhpArray::new(req)?;
    let command = PhpString::new(req, &command)?;
    let rows: [(&[u8], Value); 9] = [
        (b"command", Value::string(*command)),
        (b"pid", Value::int(i64::from(pid))),
        (b"cached", Value::bool(cached)),
        (b"running", Value::bool(running)),
        (b"signaled", Value::bool(is_signaled)),
        (b"stopped", Value::bool(is_stopped)),
        (b"exitcode", Value::int(i64::from(exitcode))),
        (b"termsig", Value::int(i64::from(termsig))),
        (b"stopsig", Value::int(i64::from(stopsig))),
    ];
    for (name, value) in rows {
        out.set_str(req, name, Owner::take(req, value))?;
    }
    command.give_up(req);
    Ok(out)
}
