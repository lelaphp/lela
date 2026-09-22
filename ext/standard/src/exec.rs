//! `shell_exec` (`Z/ext/standard/exec.c:498`), the one native of
//! `exec.c` the backtick operator reaches: `zend_compile_shell_exec`
//! (`Z/Zend/zend_compile.c:10915`) compiles `` `…` `` as the parts
//! concatenated and then a call of this name, so the operator has no
//! opcode of its own and every semantic of it is here.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Stdio};

use php_ext::decl::{FunctionDecl, Type};
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

use crate::output::Entry;
use crate::{argument_count_between, declared, parameter, string_argument};

/// The one native, as the stub declares it
/// (`basic_functions.stub.php:2705`), so
/// `ReflectionFunction::__toString` renders it as the oracle does.
///
/// ```php
/// echo new ReflectionFunction('shell_exec');
/// ```
/// ```expect
/// Function [ <internal:standard> function shell_exec ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $command ]
///   }
///   - Return [ string|false|null ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        declared(
            b"shell_exec",
            &[parameter(b"command", Type::String)],
            Type::Union(vec![Type::String, Type::False, Type::Null]),
        ),
        shell_exec as Entry,
    )]
}

/// `shell_exec(string $command): string|false|null`,
/// `PHP_FUNCTION(shell_exec)`: the command handed to the shell, what
/// it wrote read whole and handed back, and `null` where it wrote
/// nothing, which is php's `RETVAL_STR` left unwritten. The command's
/// exit status is the shell's and no part of the value, as `pclose`'s
/// is nothing php reads.
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
    let Some(written) = piped(&command) else {
        let text = String::from_utf8_lossy(&command).into_owned();
        docref(req, Level::Warning, &format!("Unable to execute '{text}'"));
        return Ok(Value::bool(false));
    };
    if written.is_empty() {
        return Ok(Value::null());
    }
    Ok(Value::string(PhpString::new(req, &written)?))
}

/// `VCWD_POPEN(command, "r")` and `php_stream_copy_to_mem` over the
/// pipe it opened, then `pclose`: the command is the shell's argument,
/// `/bin/sh -c`, as POSIX specifies `popen`; what it writes on the
/// pipe is read to the end, and its standard input and error stay this
/// process's, which is what the pipe leaves them. `None` is the `popen` that
/// answered null, the caller's warning and `false`; a read that ends
/// early is the bytes it had, as `php_stream_copy_to_mem` keeps what it
/// copied.
fn piped(command: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(OsStr::from_bytes(command))
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut written = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut written);
    }
    let _ = child.wait();
    Some(written)
}
