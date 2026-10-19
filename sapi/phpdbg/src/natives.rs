//! The extension phpdbg registers beside the bundle,
//! `sapi_phpdbg_module_entry` (`Z/sapi/phpdbg/phpdbg.c:682`): the eleven
//! natives of `phpdbg.stub.php` and its four constants, declared here
//! beside the debugger whose state they read and write (#1348), and handed
//! to the engine as the host's own extension. A native reaches the session
//! where it stands in the request's sink; inside what the prompt serves, an `ev`,
//! the prompt holds it and the native finds none, as it finds no
//! debugger to talk to.
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::os::unix::ffi::OsStrExt as _;

use php_ext::abi::{Extension, ExtensionBuilder, Union};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::{truthy, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, raise};
use php_ext::request::Opcode;
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::bp::At;
use crate::opcodes;
use crate::out::Element;
use crate::read::{self, Code};
use crate::session::{self, Logged};

/// The constants `register_phpdbg_symbols` registers: the version, and
/// the three elements `phpdbg_color` colours.
///
/// ```php
/// var_dump(PHPDBG_VERSION === PHP_VERSION, PHPDBG_COLOR_PROMPT, PHPDBG_COLOR_ERROR, PHPDBG_COLOR_NOTICE);
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> bool(true)
/// int(0)
/// int(1)
/// int(2)
/// [Script ended normally]
/// prompt>
/// ```
#[php_const]
mod constants {
    pub(crate) const PHPDBG_VERSION: &str = php_ext::request::PHP_VERSION;
    pub(crate) const PHPDBG_COLOR_PROMPT: i64 = 0;
    pub(crate) const PHPDBG_COLOR_ERROR: i64 = 1;
    pub(crate) const PHPDBG_COLOR_NOTICE: i64 = 2;
}

/// The extension, named `phpdbg`, its natives in the stub's order.
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            phpdbg_break_next::declared(),
            phpdbg_break_file::declared(),
            phpdbg_break_method::declared(),
            phpdbg_break_function::declared(),
            phpdbg_color::declared(),
            phpdbg_prompt::declared(),
            phpdbg_exec::declared(),
            phpdbg_clear::declared(),
            phpdbg_start_oplog::declared(),
            phpdbg_end_oplog::declared(),
            phpdbg_get_executable::declared(),
        ])
        .constants(constants::declared())
        .fatal_logged(crate::debugger::fatal_logged)
        .compile_file(crate::list::compile_file)
        .build(crate::NAME)
}

/// `phpdbg_break_next(): void`: a breakpoint on the opline after the one
/// the nearest user frame stands at, the call's own, and nothing where no
/// user frame stands, as at shutdown. php numbers the breakpoint by the
/// opline's address, which no fence can pin, so the fence is the call
/// with no user frame; `sapi/phpdbg/tests/phpdbg_break_next.phpt` is the
/// one with.
///
/// ```php
/// register_shutdown_function('phpdbg_break_next');
/// echo "A\n";
/// ```
/// ```request phpdbg
/// r
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> A
/// [Script ended normally]
/// prompt> [Not running]
/// prompt>
/// ```
#[php_function]
fn phpdbg_break_next(req: &mut Request) {
    let Some(frame) = session::with(req, |s| s.standing).flatten() else {
        return;
    };
    let Some(position) = read::position(req, frame) else {
        return;
    };
    let serves = read::serves(req, frame);
    let (function, class) = match &serves {
        read::Serves::Code => (None, None),
        read::Serves::Function { name, class } => (Some(name.as_slice()), class.as_deref()),
    };
    let at = At {
        file: &position.file,
        line: position.line,
        opline: position.opline,
        body: position.body,
        opcode: position.opcode.name(),
        function,
        class,
    };
    session::with(req, |s| {
        s.breaks.set_opline(&mut s.out, &at, position.opline + 1);
    });
}

/// `phpdbg_break_file(string $file, int $line): void`, as `break
/// file:line` sets it.
///
/// ```php
/// phpdbg_break_file(__FILE__, 3);
/// echo "at 3\n";
/// ```
/// ```request phpdbg
/// r
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: echo "at 3\n";
/// prompt> at 3
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_break_file(req: &mut Request, file: PhpString, line: i64) {
    session::with(req, |s| crate::host::file_break(s, file.as_bytes(), line));
}

/// `phpdbg_break_method(string $class, string $method): void`.
///
/// ```php
/// class C { function m() { echo "in m\n"; } }
/// phpdbg_break_method("C", "m");
/// (new C)->m();
/// ```
/// ```request phpdbg
/// r
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at C::m]
/// [Breakpoint #0 in C::m() at %SCRATCH%/witness.php:2, hits: 1]
/// >00002: class C { function m() { echo "in m\n"; } }
///  00003: phpdbg_break_method("C", "m");
///  00004: (new C)->m();
/// prompt> in m
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_break_method(req: &mut Request, class: PhpString, method: PhpString) {
    session::with(req, |s| {
        s.breaks
            .set_method(&mut s.out, class.as_bytes(), method.as_bytes());
    });
}

/// `phpdbg_break_function(string $function): void`.
///
/// ```php
/// function f() { echo "in f\n"; }
/// phpdbg_break_function("f");
/// f();
/// ```
/// ```request phpdbg
/// r
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at f]
/// [Breakpoint #0 in f() at %SCRATCH%/witness.php:2, hits: 1]
/// >00002: function f() { echo "in f\n"; }
///  00003: phpdbg_break_function("f");
///  00004: f();
/// prompt> in f
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_break_function(req: &mut Request, function: PhpString) {
    session::with(req, |s| {
        s.breaks.set_symbol(&mut s.out, function.as_bytes());
    });
}

/// `phpdbg_color(int $element, string $color): void`: the element's
/// colour from here, `none` for a name php's table does not hold
/// (`phpdbg_set_color_ex`), shown while colours are on; an element that
/// is none of the three is php's `ValueError`.
///
/// ```php
/// phpdbg_color(PHPDBG_COLOR_NOTICE, "red");
/// phpdbg_color(PHPDBG_COLOR_ERROR, "nope");
/// phpdbg_color(PHPDBG_COLOR_PROMPT, "blue");
/// try { phpdbg_color(7, "red"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// echo "done\n";
/// ```
/// ```request phpdbg
/// set colors on
/// b 6
/// r
/// foo
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> prompt> %s0;32m[Breakpoint #0 added at %SCRATCH%/witness.php:6]%s
/// prompt> phpdbg_color(): Argument #1 ($element) must be one of PHPDBG_COLOR_PROMPT, PHPDBG_COLOR_NOTICE, or PHPDBG_COLOR_ERROR
/// %s0;31m[Breakpoint #0 at %SCRATCH%/witness.php:6, hits: 1]%s
/// >00006: echo "done\n";
/// prompt> %s0;0m[The command "foo" could not be found]%s
/// prompt> done
/// %s0;31m[Script ended normally]%s
/// prompt>
/// ```
#[php_function]
fn phpdbg_color(req: &mut Request, element: i64, color: PhpString) -> Result<(), Thrown> {
    let element = match element {
        constants::PHPDBG_COLOR_PROMPT => Element::Prompt,
        constants::PHPDBG_COLOR_ERROR => Element::Error,
        constants::PHPDBG_COLOR_NOTICE => Element::Notice,
        _ => {
            return Err(arg_value_error(
                req,
                1,
                "be one of PHPDBG_COLOR_PROMPT, PHPDBG_COLOR_NOTICE, or PHPDBG_COLOR_ERROR",
            ));
        }
    };
    session::with(req, |s| s.out.set_colour_named(element, color.as_bytes()));
    Ok(())
}

/// `phpdbg_prompt(string $string): void`: the prompt's text from here,
/// `phpdbg_set_prompt`, a byte past ASCII shown as `?` as
/// `phpdbg_get_prompt` shows it.
///
/// ```php
/// phpdbg_prompt("dbg>");
/// echo "set\n";
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> set
/// [Script ended normally]
/// dbg>
/// ```
#[php_function]
fn phpdbg_prompt(req: &mut Request, string: PhpString) {
    let text: Vec<u8> = string
        .as_bytes()
        .iter()
        .map(|b| if *b & 0x80 == 0 { *b } else { b'?' })
        .collect();
    session::with(req, |s| s.prompt = text);
}

/// `phpdbg_exec(string $context): string|bool`: the execution context
/// from the next run on, and the one it replaces, or `true` where there
/// was none; `false` and php's warning for a path that is no file.
///
/// ```php
/// var_dump(phpdbg_exec(__FILE__) === __FILE__);
/// var_dump(phpdbg_exec("/nonexistent/x.php"));
/// var_dump(phpdbg_exec("/"));
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> bool(true)
///
/// Warning: Failed to set execution context (/nonexistent/x.php) the file does not exist in %SCRATCH%/witness.php on line 3
/// bool(false)
///
/// Warning: Failed to set execution context (/), not a regular file or symlink in %SCRATCH%/witness.php on line 4
/// bool(false)
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_exec(
    req: &mut Request,
    context: PhpString,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(context.as_bytes()));
    let shown = String::from_utf8_lossy(context.as_bytes()).into_owned();
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
            let previous =
                session::with(req, |s| s.exec.replace(context.as_bytes().to_vec())).flatten();
            match previous {
                Some(previous) => Ok(Union::A(PhpString::new(req, &previous)?)),
                None => Ok(Union::B(true)),
            }
        }
        Ok(_) => {
            let message =
                format!("Failed to set execution context ({shown}), not a regular file or symlink");
            raise(req, Level::Warning, &message);
            Ok(Union::B(false))
        }
        Err(_) => {
            let message =
                format!("Failed to set execution context ({shown}) the file does not exist");
            raise(req, Level::Warning, &message);
            Ok(Union::B(false))
        }
    }
}

/// `phpdbg_clear(): void`: every breakpoint but the opcodes' let go, as
/// `zend_hash_clean` empties the other tables, the count standing.
///
/// ```php
/// phpdbg_break_function("f");
/// phpdbg_clear();
/// function f() { echo "in f\n"; }
/// f();
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at f]
/// in f
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_clear(req: &mut Request) {
    session::with(req, |s| s.breaks.clean());
}

/// `phpdbg_start_oplog(): void`: an oplog opened, the one each opline
/// the loop reaches is written to until `phpdbg_end_oplog` closes it.
///
/// ```php
/// phpdbg_start_oplog();
/// echo "logged\n";
/// var_dump(phpdbg_end_oplog());
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> logged
/// array(1) {
///   ["%SCRATCH%/witness.php"]=>
///   array(2) {
///     [3]=>
///     int(1)
///     [4]=>
///     int(3)
///   }
/// }
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_start_oplog(req: &mut Request) {
    session::with(req, |s| s.oplogs.push(Vec::new()));
}

/// `phpdbg_end_oplog(array $options = []): ?array`: the oplog opened
/// last, closed, as each file's lines and how many oplines each ran, or
/// with `functions` each function's under its name, and with `opcodes`
/// too by opline; `null` and php's warning where none is open.
///
/// ```php
/// echo "none open\n";
/// var_dump(phpdbg_end_oplog());
/// function f($a) { return $a + 1; }
/// phpdbg_start_oplog();
/// f(1);
/// f(2);
/// $log = phpdbg_end_oplog(["functions" => true]);
/// var_dump($log["f"]);
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> none open
///
/// Warning: Cannot end an oplog without starting it in %SCRATCH%/witness.php on line 3
/// NULL
/// array(1) {
///   [4]=>
///   int(4)
/// }
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_end_oplog(
    req: &mut Request,
    #[php(default = [])] options: PhpArray,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let Some(log) = session::with(req, |s| s.oplogs.pop()).flatten() else {
        raise(
            req,
            Level::Warning,
            "Cannot end an oplog without starting it",
        );
        return Ok(None);
    };
    let (by_function, by_opcode) = shape(req, options)?;
    Ok(Some(to_array(
        req,
        &oplog_rows(&log, by_function, by_opcode),
    )?))
}

/// `phpdbg_get_executable(array $options = []): array`: each line of
/// the files compiled, or of the files `files` names, that runs an
/// opline, a zero beside it; with `functions` under each function's name
/// in its file, and with `opcodes` too by opline.
///
/// ```php
/// function f($a) {
///     return $a + 1;
/// }
/// var_dump(phpdbg_get_executable()[__FILE__]);
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> array(2) {
///   [3]=>
///   int(0)
///   [5]=>
///   int(0)
/// }
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn phpdbg_get_executable(
    req: &mut Request,
    #[php(default = [])] options: PhpArray,
) -> Result<Owner<PhpArray>, Thrown> {
    let (by_function, by_opcode) = shape(req, options)?;
    let sources = session::with(req, |s| s.sources.clone()).unwrap_or_default();
    let named = options.find(b"files").map(|value| value.deref());
    let files: Vec<Vec<u8>> = if let Some(named) = named
        .as_ref()
        .and_then(Value::as_array)
        .filter(|files| !files.is_empty())
    {
        let mut files = Vec::new();
        // Holding nothing, the option holds the array: a conversion may
        // serve `__toString`.
        walk::foreach(req, *named, Hold::None, |req, row| -> walk::Step {
            files.push(with_string(req, row.value(), <[u8]>::to_vec)?);
            Ok(std::ops::ControlFlow::Continue(()))
        })?;
        files
    } else {
        // `file_sources`: each file compiled, which the loop records as it
        // reaches a file's code, and the included files beside them, one
        // of which may hold no code the loop stops at.
        let mut files: Vec<Vec<u8>> = sources.iter().map(|source| source.file.clone()).collect();
        for file in req.included_files() {
            if !files.contains(&file) {
                files.push(file);
            }
        }
        files
    };
    let mut rows = Rows::default();
    let code = |rows: &mut Rows, code: &Code, name: Option<Vec<u8>>| {
        if !files.contains(&code.file) {
            return;
        }
        let at = rows.at(&code.file, name.filter(|_| by_function));
        executable(rows, at, &code.ops, code.params, by_opcode);
    };
    for function in read::user_functions(req) {
        let name = function.name.to_ascii_lowercase();
        code(&mut rows, &function, Some(name));
    }
    for class in read::user_classes(req) {
        if !files.contains(&class.file) {
            continue;
        }
        let key = class.name.to_ascii_lowercase();
        for method in class.methods.iter().filter(|m| m.user) {
            let name = [key.as_slice(), b"::", &method.name].concat();
            code(&mut rows, method, Some(name));
        }
    }
    for file in &files {
        if let Some(source) = sources.iter().find(|source| source.file == *file) {
            let at = rows.at(&source.file, None);
            executable(&mut rows, at, &source.ops, 0, by_opcode);
        }
    }
    to_array(req, &rows)
}

/// `functions` and `opcodes` of an options array: `opcodes` counts only
/// with `functions`.
fn shape(req: &mut Request, options: PhpArray) -> Result<(bool, bool), Thrown> {
    let flag = |req: &mut Request, name: &[u8]| match options.find(name) {
        Some(value) => truthy(req, &value),
        None => Ok(false),
    };
    let by_function = flag(req, b"functions")?;
    let by_opcode = by_function && flag(req, b"opcodes")?;
    Ok((by_function, by_opcode))
}

/// The rows of the array a native answers: each key in the order it was
/// first added, a file's or a function's, and under it the lines or
/// oplines with their counts.
#[derive(Debug, Default)]
struct Rows(Vec<(Vec<u8>, Vec<Sub>)>);

/// Under a top key: counts by line or opline, or a function's own rows.
#[derive(Debug)]
enum Sub {
    Count(i64, i64),
    Named(Vec<u8>, Vec<(i64, i64)>),
}

/// Where a count goes: a top key, and under it a function's name.
#[derive(Debug, Clone, Copy)]
struct Place {
    top: usize,
    named: Option<usize>,
}

impl Rows {
    /// `phpdbg_add_empty_array` under `top`, and under that under `named`
    /// where one is given.
    fn at(&mut self, top: &[u8], named: Option<Vec<u8>>) -> Place {
        let index = self.top(top);
        let named = named.map(|name| {
            let subs = &mut self.0[index].1;
            subs.iter()
                .position(|sub| matches!(sub, Sub::Named(n, _) if *n == name))
                .unwrap_or_else(|| {
                    subs.push(Sub::Named(name, Vec::new()));
                    subs.len() - 1
                })
        });
        Place { top: index, named }
    }

    fn top(&mut self, key: &[u8]) -> usize {
        self.0
            .iter()
            .position(|(k, _)| k == key)
            .unwrap_or_else(|| {
                self.0.push((key.to_vec(), Vec::new()));
                self.0.len() - 1
            })
    }

    /// The count at `index` under `place` set to zero where it is none
    /// yet, or raised by `by` where it is.
    fn count(&mut self, place: Place, index: i64, by: i64) {
        let subs = &mut self.0[place.top].1;
        if let Some(named) = place.named {
            if let Some(Sub::Named(_, counts)) = subs.get_mut(named) {
                match counts.iter_mut().find(|(i, _)| *i == index) {
                    Some((_, n)) => *n += by,
                    None => counts.push((index, by)),
                }
            }
            return;
        }
        let found = subs.iter_mut().find_map(|sub| match sub {
            Sub::Count(i, n) if *i == index => Some(n),
            _ => None,
        });
        match found {
            Some(n) => *n += by,
            None => subs.push(Sub::Count(index, by)),
        }
    }
}

/// `phpdbg_oplog_fill_executable`: past the `RECV*` oplines, each opline
/// that is not one of the ignored, a zero at its line or its number; the
/// return the compiler put last left out, and a `NEW`'s constructor call
/// counted with it.
fn executable(rows: &mut Rows, at: Place, ops: &[(Opcode, u32)], params: u32, by_opcode: bool) {
    let mut end = ops.len();
    // Every op array ends on the compiler's own return, `extended_value`
    // -1 in php, and a file's code on its `RETURN 1`: php leaves both out.
    if end >= 1 && opcodes::is(ops[end - 1].0, opcodes::RETURNS) {
        end -= 1;
    }
    let mut index = params as usize;
    while index < end {
        let (opcode, line) = ops[index];
        if !opcodes::is(opcode, opcodes::IGNORED) {
            let key = if by_opcode {
                i64::try_from(index).unwrap_or(i64::MAX)
            } else {
                i64::from(line)
            };
            if opcodes::is(opcode, opcodes::NEW)
                && ops
                    .get(index + 1)
                    .is_some_and(|(o, _)| opcodes::is(*o, opcodes::DO_FCALL))
            {
                index += 1;
            }
            rows.count(at, key, 0);
        }
        index += 1;
    }
}

/// `phpdbg_end_oplog`'s walk: each logged opline counted under its file,
/// or with `by_function` under its function's name at the top, `Class::f`
/// for a method, a file's code under the file.
fn oplog_rows(log: &[Logged], by_function: bool, by_opcode: bool) -> Rows {
    let mut rows = Rows::default();
    let mut file_at: Option<(Vec<u8>, Place)> = None;
    let mut place: Option<Place> = None;
    let mut last = Last::Unset;
    for entry in log {
        if file_at.as_ref().is_none_or(|(file, _)| *file != entry.file) {
            let at = rows.at(&entry.file, None);
            file_at = Some((entry.file.clone(), at));
            place = Some(at);
        }
        if by_function {
            let this = match &entry.function {
                None => Last::Code,
                Some(name) => Last::Function(name.clone(), entry.class.clone()),
            };
            match &this {
                Last::Code => {
                    if last != Last::Code {
                        place = file_at.as_ref().map(|(_, at)| *at);
                    }
                }
                Last::Function(name, class) => {
                    if last != this {
                        let key = match class {
                            Some(class) => [class.as_slice(), b"::", name].concat(),
                            None => name.clone(),
                        };
                        place = Some(rows.at(&key, None));
                    }
                }
                Last::Unset => {}
            }
            last = this;
        }
        let key = if by_opcode {
            i64::from(entry.opline)
        } else {
            if opcodes::is(entry.opcode, opcodes::IGNORED) {
                continue;
            }
            i64::from(entry.line)
        };
        if let Some(at) = place {
            rows.count(at, key, 1);
        }
    }
    rows
}

/// `last_function` and `last_scope` of `phpdbg_end_oplog`'s walk: none
/// read yet, a file's code, or a function and its class.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Last {
    Unset,
    Code,
    Function(Vec<u8>, Option<Vec<u8>>),
}

/// The rows as the array php answers.
fn to_array(req: &mut Request, rows: &Rows) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, subs) in &rows.0 {
        let mut inner = PhpArray::new(req)?;
        for sub in subs {
            match sub {
                Sub::Count(index, n) => {
                    inner.set(req, Owner::index(*index), Owner::int(*n))?;
                }
                Sub::Named(name, counts) => {
                    let mut named = PhpArray::new(req)?;
                    for (index, n) in counts {
                        named.set(req, Owner::index(*index), Owner::int(*n))?;
                    }
                    let key = PhpString::new(req, name)?.into_key();
                    inner.set(req, key, named.into_value())?;
                }
            }
        }
        let key = PhpString::new(req, key)?.into_key();
        out.set(req, key, inner.into_value())?;
    }
    Ok(out)
}
