//! The `readline` extension: `php-src/ext/readline` as one crate under
//! `ext/` (ADR-0004), over the platform's libedit, the library the pinned
//! php links (design §7.2, revision 72; #1540), bound by hand in
//! [`ffi`]. `readline.c`'s natives are here; `readline_cli.c`, the CLI's
//! interactive shell, is [`shell`], provided to a host that carries php's
//! shell callbacks through [`ExtensionBuilder::shell`], so `php -a` lives
//! where php puts it. The phpt subset is `php-src/ext/readline/tests`,
//! through `xtask phpt --subset`.
//!
//! The library's line, history and callbacks are one per process, as in
//! php; what the request holds is php's two callables, the completion
//! function and the callback handler, kept past the call that named them
//! and given up at `RSHUTDOWN`, and the shell's own state. A witness that
//! reaches the library is a `process` fence: the library's state is the
//! process's, which the linked engine's lane shares between threads.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;
mod shell;

use php_ext::abi::{Callable, Extension, ExtensionBuilder, OrFalse, Path, Shell, True, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::call::call;
use php_ext::coerce::{to_long, with_string};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Thrown, leave_pending};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::ffi::{Completion, Matches};

/// `READLINE_LIB`, `libedit` for the library this crate binds.
#[php_const]
mod constants {
    pub(crate) const READLINE_LIB: &str = "libedit";
}

/// A callable kept past the call that named it, as php copies the
/// callable's zval into a static of its own: the callee and one more owner of
/// the value it came from.
struct Kept {
    callable: Callable,
    value: Owner<Value>,
}

impl Kept {
    /// The callable argument at `position` of the running call.
    fn of(req: &mut Request, callable: Callable, position: u32) -> Kept {
        let value = req.frames().argument(position).unwrap_or_default().deref();
        Kept {
            callable,
            value: Owner::take(req, value),
        }
    }

    fn release(self, req: &mut Request) {
        self.value.give_up(req);
    }
}

/// The request's state: `_readline_completion` and `_prepped_callback`,
/// and the shell's (`zend_cli_readline_globals` with `php_last_char` and
/// `pager_pipe`).
#[derive(Default)]
struct State {
    completion: Option<Kept>,
    prepped: Option<Kept>,
    shell: shell::State,
}

/// The extension value (design §7.1): `readline_module_entry` with
/// `cli_readline`'s ini entries and shell, the stub's functions in its
/// order and `READLINE_LIB`.
///
/// ```php
/// var_dump(extension_loaded('readline'), READLINE_LIB, count(get_extension_funcs('readline')));
/// ```
/// ```expect
/// bool(true)
/// string(7) "libedit"
/// int(12)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::DECLARED)
        .functions(&[
            readline::DECLARED,
            readline_info::DECLARED,
            readline_add_history::DECLARED,
            readline_clear_history::DECLARED,
            readline_read_history::DECLARED,
            readline_write_history::DECLARED,
            readline_completion_function::DECLARED,
            readline_callback_handler_install::DECLARED,
            readline_callback_read_char::DECLARED,
            readline_callback_handler_remove::DECLARED,
            readline_redisplay::DECLARED,
            readline_on_new_line::DECLARED,
        ])
        .request_state::<State>()
        .ini(vec![
            ini(shell::PAGER, b""),
            ini(shell::PROMPT, shell::DEFAULT_PROMPT),
        ])
        .rshutdown(rshutdown)
        .shell(Shell {
            serve: shell::run,
            write: shell::write,
        })
        .info(readline_info_table)
        .build("readline")
}

/// `STD_PHP_INI_ENTRY(name, default, PHP_INI_ALL, OnUpdateString, ..)`.
fn ini(name: &str, default: &[u8]) -> IniDecl {
    IniDecl {
        name: Name(name.as_bytes().into()),
        default: Some(default.into()),
        user: true,
        per_dir: true,
        system: true,
    }
}

/// `PHP_RSHUTDOWN_FUNCTION(readline)`: the completion callable given up,
/// and the callback handler removed and given up where one is installed.
fn rshutdown(req: &mut Request) {
    let state = req.state::<State>();
    let completion = state.completion.take();
    let prepped = state.prepped.take();
    if let Some(kept) = completion {
        kept.release(req);
    }
    if let Some(kept) = prepped {
        ffi::callback_handler_remove();
        kept.release(req);
    }
    shell::close_pager(req);
}

/// `PHP_MINFO_FUNCTION(cli_readline)`: the support row, the library's
/// version string, and the two entries.
///
/// ```php
/// (new ReflectionExtension('readline'))->info();
/// ```
/// ```expect
///
/// readline
///
/// Readline Support => enabled
/// Readline library => EditLine wrapper
///
/// Directive => Local Value => Master Value
/// cli.pager => no value => no value
/// cli.prompt => \b \>  => \b \>
/// ```
fn readline_info_table(req: &mut Request) {
    let version = ffi::library_version();
    let version = if version.is_empty() {
        String::from("Unknown")
    } else {
        String::from_utf8_lossy(&version).into_owned()
    };
    req.extensions_mut().info_table(&[
        &["Readline Support", "enabled"],
        &["Readline library", &version],
    ]);
    req.ini_mut()
        .info_ini_entries(&[shell::PAGER.as_bytes(), shell::PROMPT.as_bytes()]);
}

/// A string of the request's heap.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `readline(?string $prompt = null): string|false`: a line read with
/// the prompt, false at the end of input.
///
/// ```php
/// var_dump(readline());
/// ```
/// ```expect process
/// bool(false)
/// ```
#[php_function]
fn readline(
    req: &mut Request,
    #[php(default = null)] prompt: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let prompt = prompt.map(|p| p.as_bytes().to_vec());
    match ffi::readline(req, prompt.as_deref()) {
        Some(line) => Ok(OrFalse::Value(PhpString::new(req, &line)?)),
        None => Ok(OrFalse::False),
    }
}

/// `readline_info(?string $var_name = null, $value = null): mixed`:
/// libedit's variables, every one php reads for it as an array, or the
/// one named, case aside, set to `$value` where one is given; a name it
/// does not read is `null`.
///
/// ```php
/// var_dump(readline_info());
/// var_dump(readline_info('readline_name', 'x'), readline_info('READLINE_NAME'), readline_info('point'), readline_info('nope'));
/// var_dump(readline_info('attempted_completion_over', 1), readline_info('attempted_completion_over'));
/// ```
/// ```expect process
/// array(6) {
///   ["line_buffer"]=>
///   string(0) ""
///   ["point"]=>
///   int(0)
///   ["end"]=>
///   int(0)
///   ["library_version"]=>
///   string(16) "EditLine wrapper"
///   ["readline_name"]=>
///   string(0) ""
///   ["attempted_completion_over"]=>
///   int(0)
/// }
/// string(0) ""
/// string(1) "x"
/// int(0)
/// NULL
/// int(0)
/// int(1)
/// ```
#[php_function]
fn readline_info(
    req: &mut Request,
    #[php(default = null)] var_name: Option<PhpString>,
    #[php(default = null)] value: Untyped<Value>,
) -> Result<Owner<Value>, Thrown> {
    let value = (!matches!(value.0.deref().as_ref(), ValueRef::Null)).then_some(value.0);
    let Some(name) = var_name else {
        let mut array = PhpArray::new(req)?;
        let rows = [
            (b"line_buffer".as_slice(), string(req, &ffi::line_buffer())?),
            (b"point", Owner::int(ffi::point())),
            (b"end", Owner::int(ffi::end())),
            (b"library_version", string(req, &ffi::library_version())?),
            (b"readline_name", string(req, &ffi::readline_name())?),
            (
                b"attempted_completion_over",
                Owner::int(ffi::attempted_completion_over()),
            ),
        ];
        for (key, row) in rows {
            array.set_str(req, key, row)?;
        }
        return Ok(array.into_value());
    };
    let name = name.as_bytes().to_ascii_lowercase();
    match name.as_slice() {
        b"line_buffer" => {
            let old = string(req, &ffi::line_buffer())?;
            if let Some(value) = value
                && let Err(e) = with_string(req, &value, ffi::set_line_buffer)
            {
                old.give_up(req);
                return Err(e);
            }
            Ok(old)
        }
        b"point" => Ok(Owner::int(ffi::point())),
        b"end" => Ok(Owner::int(ffi::end())),
        b"library_version" => string(req, &ffi::library_version()),
        b"readline_name" => {
            let old = string(req, &ffi::readline_name())?;
            if let Some(value) = value
                && let Err(e) = with_string(req, &value, ffi::set_readline_name)
            {
                old.give_up(req);
                return Err(e);
            }
            Ok(old)
        }
        b"attempted_completion_over" => {
            let old = ffi::attempted_completion_over();
            if let Some(value) = value {
                let value = to_long(req, &value)?;
                ffi::set_attempted_completion_over(value);
            }
            Ok(Owner::int(old))
        }
        _ => Ok(Owner::null()),
    }
}

/// `readline_add_history(string $prompt): true`.
///
/// ```php
/// var_dump(readline_add_history('one'), readline_add_history(''));
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn readline_add_history(prompt: PhpString) -> True {
    ffi::add_history(prompt.as_bytes());
    True
}

/// `readline_clear_history(): true`.
///
/// ```php
/// readline_add_history('one');
/// var_dump(readline_clear_history());
/// ```
/// ```expect process
/// bool(true)
/// ```
#[php_function]
fn readline_clear_history() -> True {
    ffi::clear_history();
    True
}

/// `readline_read_history(?string $filename = null): bool`: the file
/// read into the history, `open_basedir` asked first.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'rl');
/// readline_clear_history();
/// readline_add_history('one');
/// var_dump(readline_write_history($f), readline_clear_history(), readline_read_history($f), readline_read_history($f . '.none'));
/// unlink($f);
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn readline_read_history(req: &mut Request, #[php(default = null)] filename: Option<Path>) -> bool {
    let file = filename.map(|f| f.0.as_bytes().to_vec());
    if let Some(file) = &file
        && req
            .environment_mut()
            .check_open_basedir(file, true)
            .is_err()
    {
        return false;
    }
    ffi::read_history(file.as_deref())
}

/// `readline_write_history(?string $filename = null): bool`: the
/// history written to the file, `open_basedir` asked first.
///
/// ```php
/// ini_set('open_basedir', sys_get_temp_dir());
/// var_dump(readline_write_history('/nowhere/history'));
/// ```
/// ```expect process
/// Warning: readline_write_history(): open_basedir restriction in effect. File(/nowhere/history) is not within the allowed path(s): (%s) in %s on line 3
/// bool(false)
/// ```
#[php_function]
fn readline_write_history(
    req: &mut Request,
    #[php(default = null)] filename: Option<Path>,
) -> bool {
    let file = filename.map(|f| f.0.as_bytes().to_vec());
    if let Some(file) = &file
        && req
            .environment_mut()
            .check_open_basedir(file, true)
            .is_err()
    {
        return false;
    }
    ffi::write_history(file.as_deref())
}

/// `readline_completion_function(callable $callback): bool`: the
/// callable kept as the completion function, the one before given up.
///
/// ```php
/// var_dump(readline_completion_function(fn ($text) => [$text]), readline_completion_function('strtolower'));
/// try { readline_completion_function(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// readline_completion_function(): Argument #1 ($callback) must be a valid callback, no array or string given
/// ```
#[php_function]
fn readline_completion_function(req: &mut Request, callback: Callable) -> bool {
    let kept = Kept::of(req, callback, 1);
    if let Some(old) = req.state::<State>().completion.replace(kept) {
        old.release(req);
    }
    ffi::complete_with(Completion::User);
    true
}

/// `php_readline_completion_cb` past the library: the program's
/// callable called with the word and its bounds; an array answered is
/// its members as strings that start with the word, which
/// `_readline_command_generator` walks, or the empty list for an empty
/// array; anything else, or a call that failed, is nothing, what the
/// call threw left in `EG(exception)` for `readline()` to raise as it
/// returns, as php's `call_user_function` leaves it.
pub(crate) fn user_completion(req: &mut Request, text: &[u8], start: i64, end: i64) -> Matches {
    let Some(callable) = req
        .state::<State>()
        .completion
        .as_ref()
        .map(|k| k.callable.clone())
    else {
        return Matches::None;
    };
    let Ok(word) = string(req, text) else {
        return Matches::None;
    };
    let called = call(req, &callable, &[*word, Value::int(start), Value::int(end)]);
    word.give_up(req);
    let answer = match called {
        Ok(answer) => answer,
        Err(thrown) => {
            leave_pending(req, thrown);
            return Matches::None;
        }
    };
    let held = answer.deref();
    let Some(array) = held.as_array() else {
        answer.give_up(req);
        return Matches::None;
    };
    let mut candidates = Vec::new();
    // `ZEND_HASH_FOREACH_VAL` and `zval_get_string`, which may serve
    // `__toString`; the callback's answer holds the array.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let found = with_string(req, row.value(), |member| {
            member.starts_with(text).then(|| member.to_vec())
        });
        if let Ok(Some(member)) = found {
            candidates.push(member);
        }
        Ok(std::ops::ControlFlow::Continue(()))
    });
    // The body answers no throwable: a conversion's is left pending, as
    // the completion's other conversions leave theirs.
    match walked {
        Ok(_) | Err(_) => {}
    }
    let empty = array.is_empty();
    answer.give_up(req);
    if empty {
        Matches::Empty
    } else {
        Matches::Candidates(candidates)
    }
}

/// `readline_callback_handler_install(string $prompt, callable
/// $callback): true`: the callable kept as the line handler, any handler
/// before it removed and given up, and the library's handler installed
/// with the prompt.
///
/// ```php
/// var_dump(readline_callback_handler_install('> ', fn ($line) => null));
/// try { readline_callback_handler_install('> ', 'nope!'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(readline_callback_handler_remove(), readline_callback_handler_remove());
/// ```
/// ```expect process
/// bool(true)
/// readline_callback_handler_install(): Argument #2 ($callback) must be a valid callback, function "nope!" not found or invalid function name
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn readline_callback_handler_install(
    req: &mut Request,
    prompt: PhpString,
    callback: Callable,
) -> True {
    if let Some(old) = req.state::<State>().prepped.take() {
        ffi::callback_handler_remove();
        old.release(req);
    }
    let kept = Kept::of(req, callback, 2);
    req.state::<State>().prepped = Some(kept);
    ffi::callback_handler_install(prompt.as_bytes());
    True
}

/// `readline_callback_read_char(): void`: one character read for the
/// installed handler, which is called with the line it completes.
///
/// ```php
/// readline_callback_read_char();
/// echo "nothing installed\n";
/// ```
/// ```expect process
/// nothing installed
/// ```
#[php_function]
fn readline_callback_read_char(req: &mut Request) {
    if req.state::<State>().prepped.is_some() {
        ffi::callback_read_char(req);
    }
}

/// `php_rl_callback_handler`: the handler called with the line, `null`
/// at the end of input, what it answers dropped.
pub(crate) fn callback_line(req: &mut Request, line: Option<Vec<u8>>) {
    let Some(callable) = req
        .state::<State>()
        .prepped
        .as_ref()
        .map(|k| k.callable.clone())
    else {
        return;
    };
    let argument = match line {
        Some(line) => match string(req, &line) {
            Ok(value) => value,
            Err(_) => return,
        },
        None => Owner::null(),
    };
    let called = call(req, &callable, &[*argument]);
    argument.give_up(req);
    if let Ok(answer) = called {
        answer.give_up(req);
    }
}

/// `readline_callback_handler_remove(): bool`: the handler removed and
/// given up, false where none is installed.
///
/// ```php
/// var_dump(readline_callback_handler_remove());
/// ```
/// ```expect process
/// bool(false)
/// ```
#[php_function]
fn readline_callback_handler_remove(req: &mut Request) -> bool {
    let Some(kept) = req.state::<State>().prepped.take() else {
        return false;
    };
    ffi::callback_handler_remove();
    kept.release(req);
    true
}

/// `readline_redisplay(): void`.
///
/// ```php
/// readline_redisplay();
/// echo "redrawn\n";
/// ```
/// ```expect process
/// redrawn
/// ```
#[php_function]
fn readline_redisplay() {
    ffi::redisplay();
}

/// `readline_on_new_line(): void`.
///
/// ```php
/// readline_on_new_line();
/// echo "told\n";
/// ```
/// ```expect process
/// told
/// ```
#[php_function]
fn readline_on_new_line() {
    ffi::on_new_line();
}
