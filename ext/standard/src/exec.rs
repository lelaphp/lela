//! `Z/ext/standard/exec.c`: `exec`, `system`, `passthru`, `shell_exec`,
//! `escapeshellcmd`, `escapeshellarg` and `proc_nice`. A command is
//! `VCWD_POPEN`'s: `/bin/sh -c` with the request's working directory and
//! environment, as php's ZTS build gives it the virtual directory and
//! `putenv` gives it the environment; its standard error stays the
//! host's. `shell_exec` is also the one native the backtick operator
//! reaches: `zend_compile_shell_exec` (`Z/Zend/zend_compile.c:10915`)
//! compiles `` `…` `` as the parts concatenated and then a call of this
//! name, so the operator has no opcode of its own.
//!
//! The natives, as the stub declares them.
//!
//! ```php
//! foreach (['exec', 'system', 'passthru', 'escapeshellcmd', 'escapeshellarg', 'shell_exec', 'proc_nice'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function exec ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> string $command ]
//!     Parameter #1 [ <optional> &$output = null ]
//!     Parameter #2 [ <optional> &$result_code = null ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function system ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $command ]
//!     Parameter #1 [ <optional> &$result_code = null ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function passthru ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $command ]
//!     Parameter #1 [ <optional> &$result_code = null ]
//!   }
//!   - Return [ ?false ]
//! }
//! Function [ <internal:standard> function escapeshellcmd ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $command ]
//!   }
//!   - Return [ string ]
//! }
//! Function [ <internal:standard> function escapeshellarg ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $arg ]
//!   }
//!   - Return [ string ]
//! }
//! Function [ <internal:standard> function shell_exec ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> string $command ]
//!   }
//!   - Return [ string|false|null ]
//! }
//! Function [ <internal:standard> function proc_nice ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> int $priority ]
//!   }
//!   - Return [ bool ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Stdio};

use php_ext::abi::{False, OrFalse, Path, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::is_space;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, value_error};
use php_ext::request::{Realpaths, Request, Streams as _};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::ffi::locale::Mblen;

/// An untyped by-reference out-parameter, `&$output = null`.
type Out = Ref<Untyped<Value>>;

/// `VCWD_POPEN(cmd, "r")`: the shell over the command, in the request's
/// working directory and environment, its `output` a pipe. The shell is
/// `sh` to itself, as `popen(3)` names it, so its own messages say so:
///
/// ```php
/// var_dump(exec("nope_xyz_cmd 2>&1", $o, $c), $c, shell_exec("nope_xyz_cmd 2>&1"));
/// ```
/// ```expect
/// string(35) "sh: nope_xyz_cmd: command not found"
/// int(127)
/// string(36) "sh: nope_xyz_cmd: command not found
/// "
/// ```
fn popen(req: &Request, command: &[u8]) -> Option<Child> {
    crate::proc_open::shell(req, command)
        .stdout(Stdio::piped())
        .spawn()
        .ok()
}

/// `pclose`'s answer as the plain files kind hands it back: the exit
/// status where the command exited, the raw wait status otherwise. The
/// wait lends the request ([`Limits::blocking`]), as every wait of the
/// command's does.
fn pclose(req: &mut Request, mut child: Child) -> i64 {
    req.limits_mut()
        .blocking(|| child.wait())
        .map_or(-1, |status| {
            i64::from(status.code().unwrap_or_else(|| status.into_raw()))
        })
}

/// `strip_trailing_whitespace`: the line with C's `isspace` bytes taken
/// off its end.
fn strip_trailing_whitespace(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|c| !is_space(*c))
        .map_or(0, |at| at + 1);
    &line[..end]
}

/// How `php_exec` hands the command's `output` on: `0` keeps only the
/// last line, `1` writes each line as it comes, `2` gathers the lines
/// into an array, `3` writes the bytes as they come.
/// What `php_exec` answers: `false` where the command did not start, and
/// otherwise the last line, which a raw pass-through leaves out.
enum Ran {
    Failed,
    Raw,
    Last(Owner<PhpString>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Last,
    Print,
    Gather,
    Raw,
}

/// `php_exec(type, cmd, array, return_value)` (`Z/ext/standard/exec.c:117`):
/// the command's `output` read a line at a time and handled as `mode`
/// says, the last line with its trailing whitespace taken off as the
/// answer (the empty string where there was none) and `pclose`'s status;
/// php's warning and `false` where the shell could not be started. The
/// pipe is php's plain stream, whose reads clear the stat cache. Each
/// read and the wait for the child lend the request
/// ([`Limits::blocking`]), so a callback C makes from another thread
/// meanwhile is served there (#2401).
fn php_exec(
    req: &mut Request,
    mode: Mode,
    command: &[u8],
    mut array: Option<&mut Owner<PhpArray>>,
) -> Result<(Ran, i64), Thrown> {
    let Some(mut child) = popen(req, command) else {
        let text = format!("Unable to fork [{}]", String::from_utf8_lossy(command));
        docref(req, Level::Warning, &text);
        return Ok((Ran::Failed, -1));
    };
    let Some(pipe) = child.stdout.take() else {
        return Ok((Ran::Failed, pclose(req, child)));
    };
    let mut reader = BufReader::new(pipe);
    // `php_stdiop_read`'s `php_clear_stat_cache`, since the command may
    // have changed any file (`Z/main/streams/plain_wrapper.c:500`).
    req.clear_stat_cache(Realpaths::Kept);
    if mode == Mode::Raw {
        let mut chunk = vec![0u8; 4096];
        while let Ok(read) = req.limits_mut().blocking(|| reader.read(&mut chunk)) {
            if read == 0 {
                break;
            }
            req.response_mut().write(&chunk[..read]);
        }
        return Ok((Ran::Raw, pclose(req, child)));
    }
    let mut last: Vec<u8> = Vec::new();
    loop {
        let mut line = Vec::new();
        match req
            .limits_mut()
            .blocking(|| reader.read_until(b'\n', &mut line))
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        match mode {
            Mode::Print => {
                req.response_mut().write(&line);
                last = line;
            }
            Mode::Gather => {
                last = strip_trailing_whitespace(&line).to_vec();
                if let Some(array) = array.as_deref_mut() {
                    let value = PhpString::new(req, &last)?.into_value();
                    array.append(req, value)?;
                }
            }
            Mode::Last | Mode::Raw => last = line,
        }
    }
    let status = pclose(req, child);
    let answer = PhpString::new(req, strip_trailing_whitespace(&last))?;
    Ok((Ran::Last(answer), status))
}

/// `$command` as `Z_PARAM_PATH_STR(cmd)` held it, and the empty
/// command's `ValueError`, as `php_exec_ex` reads them.
fn command_bytes(req: &mut Request, command: Path) -> Result<Vec<u8>, Thrown> {
    if command.0.as_bytes().is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    Ok(command.0.as_bytes().to_vec())
}

/// `string|false`: the last line, or `false` where the command did not
/// start.
fn last_line(ran: Ran) -> OrFalse<Owner<PhpString>> {
    match ran {
        Ran::Last(line) => OrFalse::Value(line),
        Ran::Failed | Ran::Raw => OrFalse::False,
    }
}

/// `exec(string $command, &$output = null, &$result_code = null): string|false`,
/// `php_exec_ex` with `mode` 0 (`Z/ext/standard/exec.c:218`): the last line
/// of the command's `output`, its trailing whitespace off; with `$output`,
/// every line so, appended to the array it holds or to a new one; the
/// exit status in `$result_code`.
///
/// ```php
/// var_dump(exec('printf "a\n b \n\nlast  \n"'), exec('printf "x\ny"', $out, $rc), $out, $rc, exec('exit 3', $o2, $rc2), $o2, $rc2);
/// $keep = ['kept']; exec('echo more', $keep); var_dump($keep, exec('pwd') === getcwd());
/// try { exec(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "last"
/// string(1) "y"
/// array(2) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
/// }
/// int(0)
/// string(0) ""
/// array(0) {
/// }
/// int(3)
/// array(2) {
///   [0]=>
///   string(4) "kept"
///   [1]=>
///   string(4) "more"
/// }
/// bool(true)
/// exec(): Argument #1 ($command) must not be empty
/// ```
///
/// Reading what the command wrote clears the stat cache, as the plain
/// stream php reads it through does, so a file the command touched is
/// stat'd again; `shell_exec` the same.
///
/// ```php
/// $f = __DIR__ . '/exec-stat.txt';
/// touch($f, 1); var_dump(filemtime($f)); exec("touch $f"); var_dump(filemtime($f) > 1);
/// touch($f, 1); var_dump(filemtime($f)); shell_exec("touch $f"); var_dump(filemtime($f) > 1);
/// unlink($f);
/// ```
/// ```expect
/// int(1)
/// bool(true)
/// int(1)
/// bool(true)
/// ```
#[php_function]
fn exec(
    req: &mut Request,
    command: Path,
    #[php(default = null)] output: Out,
    #[php(default = null)] result_code: Out,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let command = command_bytes(req, command)?;
    let (answer, status) = if output.is_given() {
        let held = output.get().map_or_else(Value::null, |held| held.0);
        let mut array = match held.as_ref() {
            ValueRef::Array(array) => {
                let mut array = Owner::take(req, array);
                array.separate(req)?;
                array
            }
            _ => PhpArray::new(req)?,
        };
        let done = php_exec(req, Mode::Gather, &command, Some(&mut array));
        output.set(req, Untyped(array.into_value()))?;
        done?
    } else {
        php_exec(req, Mode::Last, &command, None)?
    };
    set_code(req, &result_code, status, answer).map(last_line)
}

/// The exit status written to `$result_code`, the answer given up where
/// the write throws.
fn set_code(req: &mut Request, result_code: &Out, status: i64, answer: Ran) -> Result<Ran, Thrown> {
    match result_code.set(req, Untyped(Value::int(status))) {
        Ok(()) => Ok(answer),
        Err(thrown) => {
            if let Ran::Last(line) = answer {
                line.give_up(req);
            }
            Err(thrown)
        }
    }
}

/// `system(string $command, &$result_code = null): string|false`,
/// `php_exec_ex` with `mode` 1 (`Z/ext/standard/exec.c:225`): each line of
/// the command's `output` written as it comes, and the last one, its
/// trailing whitespace off, handed back.
///
/// ```php
/// var_dump(system('printf "one\ntwo\n"', $rc), $rc, system('exit 5', $rc), $rc);
/// ```
/// ```expect
/// one
/// two
/// string(3) "two"
/// int(0)
/// string(0) ""
/// int(5)
/// ```
#[php_function]
fn system(
    req: &mut Request,
    command: Path,
    #[php(default = null)] result_code: Out,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let command = command_bytes(req, command)?;
    let (answer, status) = php_exec(req, Mode::Print, &command, None)?;
    set_code(req, &result_code, status, answer).map(last_line)
}

/// `passthru(string $command, &$result_code = null): false|null`,
/// `php_exec_ex` with `mode` 3 (`Z/ext/standard/exec.c:232`): the command's
/// `output` written as its bytes come, and `null`.
///
/// ```php
/// var_dump(passthru('printf "raw-bytes"', $rc), $rc);
/// ```
/// ```expect
/// raw-bytesNULL
/// int(0)
/// ```
#[php_function]
fn passthru(
    req: &mut Request,
    command: Path,
    #[php(default = null)] result_code: Out,
) -> Result<Option<False>, Thrown> {
    let command = command_bytes(req, command)?;
    let (answer, status) = php_exec(req, Mode::Raw, &command, None)?;
    Ok(match set_code(req, &result_code, status, answer)? {
        Ran::Failed => Some(False),
        Ran::Raw => None,
        Ran::Last(line) => {
            line.give_up(req);
            None
        }
    })
}

/// `php_mblen(str + x, l - x)` in the escaping walks: the bytes the
/// character at the start of `rest` takes, `None` for one that is no
/// valid character, which the walk drops without resetting the state,
/// as php's does.
fn character(mblen: &mut Mblen, rest: &[u8]) -> Option<usize> {
    usize::try_from(mblen.len(rest)).ok().map(|len| len.max(1))
}

/// `php_escape_shell_cmd` (`Z/ext/standard/exec.c:274`): each byte that
/// could break out of a command behind a backslash; a quote paired with
/// a later one of its kind left alone; an invalid multibyte sequence
/// dropped; php's `ValueError`s for a command too long for
/// `sysconf(_SC_ARG_MAX)` before or after.
pub fn escape_shell_cmd(req: &mut Request, input: &[u8]) -> Result<Vec<u8>, Thrown> {
    let max = crate::ffi::arg_max();
    if input.len() > max.saturating_sub(3) {
        let text = format!("Command exceeds the allowed length of {max} bytes");
        return Err(value_error(req, &text));
    }
    let mut out = Vec::with_capacity(input.len() * 2);
    let mut pending: Option<usize> = None;
    let mut mblen = Mblen::new();
    let mut x = 0;
    while let Some(rest) = input.get(x..).filter(|rest| !rest.is_empty()) {
        let Some(len) = character(&mut mblen, rest) else {
            x += 1;
            continue;
        };
        if len > 1 {
            out.extend_from_slice(&rest[..len]);
            x += len;
            continue;
        }
        let c = rest[0];
        match c {
            b'"' | b'\'' => {
                if pending.is_none() {
                    pending = rest[1..].iter().position(|&d| d == c).map(|at| x + 1 + at);
                    if pending.is_none() {
                        out.push(b'\\');
                    }
                } else if pending.is_some_and(|p| input.get(p) == Some(&c)) {
                    pending = None;
                } else {
                    out.push(b'\\');
                }
                out.push(c);
            }
            b'#' | b'&' | b';' | b'`' | b'|' | b'*' | b'?' | b'~' | b'<' | b'>' | b'^' | b'('
            | b')' | b'[' | b']' | b'{' | b'}' | b'$' | b'\\' | b'\n' | 0xff => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c),
        }
        x += 1;
    }
    if out.len() > max + 1 {
        let text = format!("Escaped command exceeds the allowed length of {max} bytes");
        return Err(value_error(req, &text));
    }
    Ok(out)
}

/// `php_escape_shell_arg` (`Z/ext/standard/exec.c:381`): the argument in
/// single quotes, each quote inside as `'\''`, an invalid multibyte
/// sequence dropped, with php's `ValueError`s for one too long.
fn escape_shell_arg(req: &mut Request, input: &[u8]) -> Result<Vec<u8>, Thrown> {
    let max = crate::ffi::arg_max();
    if input.len() > max.saturating_sub(3) {
        let text = format!("Argument exceeds the allowed length of {max} bytes");
        return Err(value_error(req, &text));
    }
    let mut out = vec![b'\''];
    let mut mblen = Mblen::new();
    let mut x = 0;
    while let Some(rest) = input.get(x..).filter(|rest| !rest.is_empty()) {
        let Some(len) = character(&mut mblen, rest) else {
            x += 1;
            continue;
        };
        if rest[0] == b'\'' {
            out.extend_from_slice(b"'\\'");
        }
        out.extend_from_slice(&rest[..len]);
        x += len;
    }
    out.push(b'\'');
    if out.len() > max + 1 {
        let text = format!("Escaped argument exceeds the allowed length of {max} bytes");
        return Err(value_error(req, &text));
    }
    Ok(out)
}

/// `escapeshellcmd(string $command): string`,
/// `PHP_FUNCTION(escapeshellcmd)` (`Z/ext/standard/exec.c:475`).
///
/// ```php
/// var_dump(escapeshellcmd("a\"b\" c`d#\xff\xc3\xa9"), escapeshellcmd("'x"), escapeshellcmd("echo 'a' \"b"), escapeshellcmd(''));
/// ```
/// ```expect
/// string(13) "a"b" c\`d\#é"
/// string(3) "\'x"
/// string(12) "echo 'a' \"b"
/// string(0) ""
/// ```
///
/// The walk is by `php_mblen`'s characters under the request's
/// `LC_CTYPE`: a Shift-JIS character whose second byte is a backslash is
/// copied whole, and under `C.UTF-8` a byte that begins no character is
/// dropped.
///
/// ```php
/// foreach (['C', 'ja_JP.SJIS', 'C.UTF-8'] as $l) { setlocale(LC_CTYPE, $l); var_dump(bin2hex(escapeshellcmd("\x95\;"))); }
/// ```
/// ```expect locale=ja_JP.SJIS
/// string(10) "955c5c5c3b"
/// string(8) "955c5c3b"
/// string(8) "5c5c5c3b"
/// ```
#[php_function]
fn escapeshellcmd(req: &mut Request, command: Path) -> Result<Owner<PhpString>, Thrown> {
    let escaped = escape_shell_cmd(req, command.0.as_bytes())?;
    PhpString::new(req, &escaped)
}

/// `escapeshellarg(string $arg): string`, `PHP_FUNCTION(escapeshellarg)`
/// (`Z/ext/standard/exec.c:490`).
///
/// ```php
/// var_dump(escapeshellarg(''), escapeshellarg('$HOME; ls'), escapeshellarg("it's \xc3"));
/// try { escapeshellarg("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "''"
/// string(11) "'$HOME; ls'"
/// string(10) "'it'\''s '"
/// escapeshellarg(): Argument #1 ($arg) must not contain any null bytes
/// ```
///
/// The walk is by `php_mblen`'s characters under the request's
/// `LC_CTYPE`, as `escapeshellcmd`'s.
///
/// ```php
/// foreach (['C', 'ja_JP.SJIS', 'C.UTF-8'] as $l) { setlocale(LC_CTYPE, $l); var_dump(bin2hex(escapeshellarg("\x95'\xe9"))); }
/// ```
/// ```expect locale=ja_JP.SJIS
/// string(16) "2795275c2727e927"
/// string(8) "27952727"
/// string(12) "27275c272727"
/// ```
#[php_function]
fn escapeshellarg(req: &mut Request, arg: Path) -> Result<Owner<PhpString>, Thrown> {
    let escaped = escape_shell_arg(req, arg.0.as_bytes())?;
    PhpString::new(req, &escaped)
}

/// `shell_exec(string $command): string|false|null`,
/// `PHP_FUNCTION(shell_exec)` (`Z/ext/standard/exec.c:498`): the command
/// handed to the shell, what it wrote read whole and handed back, and
/// `null` where it wrote nothing, which is php's `RETVAL_STR` left
/// unwritten. The command's exit status is no part of the value.
///
/// ```php
/// var_dump(shell_exec('echo hi'), shell_exec('true'), shell_exec('exit 3'));
/// ```
/// ```expect
/// string(3) "hi
/// "
/// NULL
/// NULL
/// ```
///
/// `Z_PARAM_PATH` refuses a command with a null byte in it and the body
/// refuses the empty one, both `zend_argument_value_error`'s sentence:
///
/// ```php
/// try { shell_exec(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { shell_exec("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { shell_exec(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// shell_exec(): Argument #1 ($command) must not be empty
/// shell_exec(): Argument #1 ($command) must not contain any null bytes
/// shell_exec() expects exactly 1 argument, 0 given
/// ```
#[php_function]
fn shell_exec(
    req: &mut Request,
    command: PhpString,
) -> Result<Option<OrFalse<Owner<PhpString>>>, Thrown> {
    let command = command.as_bytes().to_vec();
    if command.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if command.contains(&0) {
        return Err(arg_value_error(req, 1, "not contain any null bytes"));
    }
    let Some(mut child) = popen(req, &command) else {
        let text = String::from_utf8_lossy(&command).into_owned();
        docref(req, Level::Warning, &format!("Unable to execute '{text}'"));
        return Ok(Some(OrFalse::False));
    };
    let mut written = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        req.limits_mut()
            .blocking(|| pipe.read_to_end(&mut written))
            .ok();
    }
    pclose(req, child);
    // The pipe is read through php's plain stream, whose reads clear the
    // stat cache (`Z/main/streams/plain_wrapper.c:500`).
    req.clear_stat_cache(Realpaths::Kept);
    if written.is_empty() {
        return Ok(None);
    }
    Ok(Some(OrFalse::Value(PhpString::new(req, &written)?)))
}

/// `proc_nice(int $priority): bool`, `PHP_FUNCTION(proc_nice)`
/// (`Z/ext/standard/exec.c:535`): `nice(2)` with the increment, and
/// php's warning where it set `errno`, which it does where the process
/// may not raise its priority.
///
/// ```php
/// var_dump(proc_nice(0));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn proc_nice(req: &mut Request, priority: i64) -> bool {
    // `nice((int) pri)`: the low 32 bits, as C's conversion keeps them.
    #[allow(clippy::cast_possible_truncation)]
    let priority = priority as i32;
    if crate::ffi::nice(priority) {
        return true;
    }
    docref(
        req,
        Level::Warning,
        "Only a super user may attempt to increase the priority of a process",
    );
    false
}
