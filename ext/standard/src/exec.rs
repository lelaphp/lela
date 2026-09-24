//! `Z/ext/standard/exec.c`: `exec`, `system`, `passthru`, `shell_exec`,
//! `escapeshellcmd`, `escapeshellarg` and `proc_nice`. A command is
//! `VCWD_POPEN`'s: `/bin/sh -c` with the request's working directory and
//! environment, as php's ZTS build gives it the virtual directory and
//! `putenv` gives it the environment; its standard error stays the
//! host's. `shell_exec` is also the one native the backtick operator
//! reaches: `zend_compile_shell_exec` (`Z/Zend/zend_compile.c:10915`)
//! compiles `` `…` `` as the parts concatenated and then a call of this
//! name, so the operator has no opcode of its own.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};

use php_ext::decl::{FunctionDecl, Type};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value, ValueRef};

use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::{assign_ref, given};
use crate::{argument_count_between, declared, long_argument, parameter, string_argument};

/// The natives, as the stub declares them.
///
/// ```php
/// foreach (['exec', 'system', 'passthru', 'escapeshellcmd', 'escapeshellarg', 'shell_exec', 'proc_nice'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function exec ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $command ]
///     Parameter #1 [ <optional> &$output = null ]
///     Parameter #2 [ <optional> &$result_code = null ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function system ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $command ]
///     Parameter #1 [ <optional> &$result_code = null ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function passthru ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $command ]
///     Parameter #1 [ <optional> &$result_code = null ]
///   }
///   - Return [ ?false ]
/// }
/// Function [ <internal:standard> function escapeshellcmd ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $command ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function escapeshellarg ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $arg ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function shell_exec ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $command ]
///   }
///   - Return [ string|false|null ]
/// }
/// Function [ <internal:standard> function proc_nice ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> int $priority ]
///   }
///   - Return [ bool ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub("exec(string $command, &$output = null, &$result_code = null): string|false"),
            exec as Entry,
        ),
        (
            stub("system(string $command, &$result_code = null): string|false"),
            system,
        ),
        (
            stub("passthru(string $command, &$result_code = null): false|null"),
            passthru,
        ),
        (
            stub("escapeshellcmd(string $command): string"),
            escapeshellcmd,
        ),
        (stub("escapeshellarg(string $arg): string"), escapeshellarg),
        (
            declared(
                b"shell_exec",
                &[parameter(b"command", Type::String)],
                Type::Union(vec![Type::String, Type::False, Type::Null]),
            ),
            shell_exec,
        ),
        (stub("proc_nice(int $priority): bool"), proc_nice),
    ]
}

/// `VCWD_POPEN(cmd, "r")`: the shell over the command, in the request's
/// working directory and environment, its `output` a pipe.
fn popen(req: &Request, command: &[u8]) -> Option<Child> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(OsStr::from_bytes(command))
        .current_dir(req.cwd().0)
        .env_clear()
        .envs(
            req.env_pairs()
                .iter()
                .map(|(name, value)| (OsStr::from_bytes(name), OsStr::from_bytes(value))),
        )
        .stdout(Stdio::piped())
        .spawn()
        .ok()
}

/// `pclose`'s answer as the plain files kind hands it back: the exit
/// status where the command exited, the raw wait status otherwise.
fn pclose(mut child: Child) -> i64 {
    child.wait().map_or(-1, |status| {
        i64::from(status.code().unwrap_or_else(|| status.into_raw()))
    })
}

/// `strip_trailing_whitespace`: the line with C's `isspace` bytes taken
/// off its end.
fn strip_trailing_whitespace(line: &[u8]) -> &[u8] {
    let end = line
        .iter()
        .rposition(|c| !matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .map_or(0, |at| at + 1);
    &line[..end]
}

/// How `php_exec` hands the command's `output` on: `0` keeps only the
/// last line, `1` writes each line as it comes, `2` gathers the lines
/// into an array, `3` writes the bytes as they come.
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
/// php's warning and `false` where the shell could not be started.
fn php_exec(
    req: &mut Request,
    mode: Mode,
    command: &[u8],
    mut array: Option<&mut PhpArray>,
) -> Result<(Value, i64), Thrown> {
    let Some(mut child) = popen(req, command) else {
        let text = format!("Unable to fork [{}]", String::from_utf8_lossy(command));
        docref(req, Level::Warning, &text);
        return Ok((Value::bool(false), -1));
    };
    let Some(pipe) = child.stdout.take() else {
        return Ok((Value::bool(false), pclose(child)));
    };
    let mut reader = BufReader::new(pipe);
    if mode == Mode::Raw {
        let mut chunk = vec![0u8; 4096];
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                break;
            }
            req.write(&chunk[..read]);
        }
        return Ok((Value::null(), pclose(child)));
    }
    let mut last: Vec<u8> = Vec::new();
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        match mode {
            Mode::Print => {
                req.write(&line);
                last = line;
            }
            Mode::Gather => {
                last = strip_trailing_whitespace(&line).to_vec();
                if let Some(array) = array.as_deref_mut() {
                    let value = Value::string(PhpString::new(req, &last)?);
                    array.append(req, value)?;
                }
            }
            Mode::Last | Mode::Raw => last = line,
        }
    }
    let status = pclose(child);
    let answer = PhpString::new(req, strip_trailing_whitespace(&last))?;
    Ok((Value::string(answer), status))
}

/// `Z_PARAM_PATH_STR(cmd)` and the empty command's `ValueError`, as
/// `php_exec_ex` reads them.
fn command_argument(req: &mut Request) -> Result<Vec<u8>, Thrown> {
    let command = crate::md5::path_argument(req, 1)?;
    if command.as_bytes().is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    Ok(command.as_bytes().to_vec())
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
fn exec(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let command = command_argument(req)?;
    let (answer, status) = if given(req, 2).is_some() {
        let held = req.argument(2).unwrap_or_default();
        let mut array = match held.deref().as_ref() {
            ValueRef::Array(array) => {
                let mut array = *array;
                req.addref(&Value::array(array));
                array.separate(req)?;
                array
            }
            _ => PhpArray::new(req)?,
        };
        let done = php_exec(req, Mode::Gather, &command, Some(&mut array));
        assign_ref(req, 2, Value::array(array));
        done?
    } else {
        php_exec(req, Mode::Last, &command, None)?
    };
    if given(req, 3).is_some() {
        assign_ref(req, 3, Value::int(status));
    }
    Ok(answer)
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
fn system(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let command = command_argument(req)?;
    let (answer, status) = php_exec(req, Mode::Print, &command, None)?;
    if given(req, 2).is_some() {
        assign_ref(req, 2, Value::int(status));
    }
    Ok(answer)
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
fn passthru(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let command = command_argument(req)?;
    let (answer, status) = php_exec(req, Mode::Raw, &command, None)?;
    if given(req, 2).is_some() {
        assign_ref(req, 2, Value::int(status));
    }
    Ok(answer)
}

/// `php_mblen` under the `C.UTF-8` `LC_CTYPE` php sets at startup: the
/// length of the UTF-8 sequence at the start of `bytes`, `None` for one
/// that is no valid sequence. php's `mbrlen` keeps `BG(mblen_state)`
/// from one call to the next, so a string that ends inside a sequence
/// leaves the next string's first bytes read as its end; that state is
/// not kept here.
fn mblen(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.first()?;
    let (len, range) = match first {
        0x00..=0x7f => return Some(1),
        0xc2..=0xdf => (2, 0x80..=0xbf),
        0xe0 => (3, 0xa0..=0xbf),
        0xed => (3, 0x80..=0x9f),
        0xe1..=0xef => (3, 0x80..=0xbf),
        0xf0 => (4, 0x90..=0xbf),
        0xf4 => (4, 0x80..=0x8f),
        0xf1..=0xf3 => (4, 0x80..=0xbf),
        _ => return None,
    };
    let second = *bytes.get(1)?;
    if !range.contains(&second) {
        return None;
    }
    let rest = bytes.get(2..len)?;
    rest.iter()
        .all(|c| (0x80..=0xbf).contains(c))
        .then_some(len)
}

/// `php_escape_shell_cmd` (`Z/ext/standard/exec.c:274`): each byte that
/// could break out of a command behind a backslash; a quote paired with
/// a later one of its kind left alone; an invalid multibyte sequence
/// dropped; php's `ValueError`s for a command too long for
/// `sysconf(_SC_ARG_MAX)` before or after.
pub(super) fn escape_shell_cmd(req: &mut Request, input: &[u8]) -> Result<Vec<u8>, Thrown> {
    let max = crate::ffi::arg_max();
    if input.len() > max.saturating_sub(3) {
        let text = format!("Command exceeds the allowed length of {max} bytes");
        return Err(throw(req, "ValueError", &text));
    }
    let mut out = Vec::with_capacity(input.len() * 2);
    let mut pending: Option<usize> = None;
    let mut x = 0;
    while let Some(rest) = input.get(x..).filter(|rest| !rest.is_empty()) {
        let Some(len) = mblen(rest) else {
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
        return Err(throw(req, "ValueError", &text));
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
        return Err(throw(req, "ValueError", &text));
    }
    let mut out = vec![b'\''];
    let mut x = 0;
    while let Some(rest) = input.get(x..).filter(|rest| !rest.is_empty()) {
        let Some(len) = mblen(rest) else {
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
        return Err(throw(req, "ValueError", &text));
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
fn escapeshellcmd(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let command = crate::md5::path_argument(req, 1)?;
    let escaped = escape_shell_cmd(req, command.as_bytes())?;
    Ok(Value::string(PhpString::new(req, &escaped)?))
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
fn escapeshellarg(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let argument = crate::md5::path_argument(req, 1)?;
    let escaped = escape_shell_arg(req, argument.as_bytes())?;
    Ok(Value::string(PhpString::new(req, &escaped)?))
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
fn shell_exec(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let command = string_argument(req, 1)?.as_bytes().to_vec();
    if command.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if command.contains(&0) {
        return Err(arg_value_error(req, 1, "not contain any null bytes"));
    }
    let Some(mut child) = popen(req, &command) else {
        let text = String::from_utf8_lossy(&command).into_owned();
        docref(req, Level::Warning, &format!("Unable to execute '{text}'"));
        return Ok(Value::bool(false));
    };
    let mut written = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut written);
    }
    pclose(child);
    if written.is_empty() {
        return Ok(Value::null());
    }
    Ok(Value::string(PhpString::new(req, &written)?))
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
fn proc_nice(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let priority = long_argument(req, 1)?;
    // `nice((int) pri)`: the low 32 bits, as C's conversion keeps them.
    #[allow(clippy::cast_possible_truncation)]
    let priority = priority as i32;
    if crate::ffi::nice(priority) {
        return Ok(Value::bool(true));
    }
    docref(
        req,
        Level::Warning,
        "Only a super user may attempt to increase the priority of a process",
    );
    Ok(Value::bool(false))
}
