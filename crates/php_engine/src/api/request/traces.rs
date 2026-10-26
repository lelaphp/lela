//! The traces of a request: `zend_fetch_debug_backtrace` over the
//! chain of frames and calls, `debug_backtrace()`, the rows a
//! throwable's trace keeps and their rendering as `getTraceAsString()`
//! and a fatal's trace print them, with `#[\SensitiveParameter]`'s
//! placeholders and the `zend.exception_ignore_args` and
//! `zend.exception_string_param_max_len` entries they read. The area
//! holds no state of its own: a trace reads the chain the frames and
//! calls hold.

use std::ops::ControlFlow;

use php_heap::Slot;
use php_unit::Opcode;
use php_unit::decl::{Bare, ClassKind};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::walk::{self, Hold};
use crate::api::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, adopted,
};
use crate::engine::Known;
use crate::exec::{self, IncludeKind};

use super::Frames;
use super::{
    DEBUG_BACKTRACE_IGNORE_ARGS, DEBUG_BACKTRACE_PROVIDE_OBJECT, Expand, Link, Request, Slots,
    append_escaped, links_of,
};

/// The parameters of one callee whose arguments a trace redacts:
/// `declared` by position, the variadic's for the rest.
#[derive(Default)]
struct Sensitive {
    declared: Vec<bool>,
    variadic: bool,
}

impl Sensitive {
    /// Whether the argument at `at`, from zero, is shown redacted.
    fn at(&self, at: usize) -> bool {
        self.declared.get(at).copied().unwrap_or(self.variadic)
    }
}

/// One row of a backtrace before it is an array: the callee, its
/// receiver, the scope it was called in where the callee is a closure,
/// its arguments and the extra named arguments the callee collected,
/// both borrowed from the frame or the call with no owner taken, and
/// the file and line of its call site, which a call a native made has
/// none of. php reads the scope off `func->common.scope` of the
/// closure's own copy of the function; the copy here is the `Closure`
/// object the call stands on, so the scope travels beside the callee.
pub(super) type TraceRow = (
    Traced,
    Option<PhpObject>,
    Option<ClassId>,
    Vec<Value>,
    Vec<(PhpString, Value)>,
    Option<(FileAt, i64)>,
);

/// Where a row's file stands in its [`Rows::files`]: the bytes of the
/// op array's file name, copied once so the rows hold no borrow of the
/// frames they were read off.
pub(super) type FileAt = std::ops::Range<usize>;

/// A backtrace's rows before they are arrays, beside the bytes of the
/// files they name ([`FileAt`]).
#[derive(Default)]
pub(super) struct Rows {
    laid: Vec<TraceRow>,
    files: Vec<u8>,
    /// The file laid last, which a row of the same file names again.
    last: FileAt,
}

impl Rows {
    /// `file` laid at the end of [`Rows::files`], or where it was laid
    /// last where the last file laid is the same one, so the rows of one
    /// file name one place and the rendering copies one string for them.
    fn lay_file(&mut self, file: &[u8]) -> FileAt {
        if self.files.get(self.last.clone()) == Some(file) {
            return self.last.clone();
        }
        let start = self.files.len();
        self.files.extend_from_slice(file);
        self.last = start..self.files.len();
        self.last.clone()
    }

    fn clear(&mut self) {
        self.laid.clear();
        self.files.clear();
        self.last = 0..0;
    }
}

/// The buffers [`Request::fetch_debug_backtrace`] lays its links and
/// rows in, empty between walks and kept with their capacity, so a
/// throwable's backtrace allocates nothing beside the arrays it makes,
/// as `zend_fetch_debug_backtrace` allocates nothing beside its
/// `zend_new_array`s. The links are held at `'static` only while empty
/// ([`relifetimed`]).
#[derive(Default)]
pub(super) struct Scratch {
    links: Vec<Link<'static>>,
    rows: Rows,
}

/// An empty vector of links at another lifetime, its allocation kept:
/// a `collect` of no items back into the vector it came from, which
/// std makes in place for an element of the same layout. Where it is
/// not made in place, the vector is a new empty one and the walk
/// allocates as it would without the buffer.
#[allow(
    clippy::unnecessary_filter_map,
    reason = "the map is what changes the element's lifetime, which a filter keeps"
)]
fn relifetimed<'b>(spare: Vec<Link<'_>>) -> Vec<Link<'b>> {
    debug_assert!(spare.is_empty());
    spare.into_iter().filter_map(|_| None).collect()
}

/// What a trace row names: the function a link serves, or the construct
/// `zend_fetch_debug_backtrace` names a code frame by, `include` and its
/// kin (`Z/Zend/zend_builtin_functions.c:2154`), with the included file
/// as its one argument where a link stands above the frame, since
/// `include_filename` is the file of the link the walk passed last, and
/// none where the frame is the innermost link.
pub(super) enum Traced {
    Function(Function),
    /// A hook trampoline's call, named as php's row copies the
    /// trampoline's `function_name`, `$name::set`, in the class of the
    /// property, its scope:
    ///
    /// ```php
    /// class P { public int $a; }
    /// class C extends P { public int $a { set(int|string $value) { parent::$a::set($value); } } }
    /// $c = new C;
    /// try { $c->a = "x"; } catch (TypeError $e) { echo $e->getTraceAsString(), "\n"; }
    /// ```
    /// ```expect
    /// #0 %s(3): P->$a::set('x')
    /// #1 %s(5): C->$a::set('x')
    /// #2 {main}
    /// ```
    HookTrampoline {
        function: Function,
        name: PhpString,
    },
    /// A frameless call's native (`ZEND_FRAMELESS_ICALL_*`), whose row
    /// php lays down off the user frame at the call's opline before that
    /// frame's own (`Z/Zend/zend_builtin_functions.c:2003`): `function`
    /// first, then the frame's `file` and `line`, then `args`, a table
    /// `zend_new_array(8)` and `zend_hash_real_init_mixed` lay down
    /// whatever the count, where a framed call's are packed.
    Frameless(Function),
    Include {
        kind: IncludeKind,
        file: Option<FileAt>,
    },
    /// A code frame no `INCLUDE_OR_EVAL` pushed that stands over user
    /// code, which php names `unknown` (`ZEND_STR_UNKNOWN`): what a
    /// host's evaluation in a frame is ([`crate::tier::evaluate`]).
    Unknown,
    /// A code frame no `INCLUDE_OR_EVAL` pushed and that stands over no
    /// user code, the main op array's above all: php lays its row down,
    /// `zend_new_array(8)` and `zend_hash_real_init_mixed`, before its
    /// default arm finds nothing to name it by and destroys it, so the
    /// trace has no row for it and the walk's peak has the row's bytes.
    Dropped,
}

impl Request {
    /// `zend_trace_to_string` over the `trace` a throwable holds, with
    /// `include_main`; a slot that holds no array, which is what a
    /// throwable made before this engine recorded one has, renders as
    /// the empty trace does, `#0 {main}`.
    pub(crate) fn trace_string_of(&mut self, throwable: PhpObject) -> Vec<u8> {
        match throwable
            .prop(self, b"trace")
            .and_then(|value| value.as_array())
        {
            Some(trace) => self.trace_to_string(trace, true),
            None => b"#0 {main}".to_vec(),
        }
    }

    /// `zend_fetch_debug_backtrace` (`Z/Zend/zend_builtin_functions.c`)
    /// with the options a throwable is built under, no `include_main`
    /// and no `DEBUG_BACKTRACE_PROVIDE_OBJECT`: one row per link of the
    /// chain below the op array being served, `file`, `line`,
    /// `function`, then `class` and `type` for a method, then `args`, in
    /// the order php adds the keys. The chain is
    /// [`Request::trace_chain`]; a row whose link stands on a native
    /// names no `file` and no `line`, which is the test php makes on
    /// `prev->func` with `ZEND_USER_CODE`.
    /// The arguments are the frame's, each followed through a reference
    /// and one more owner taken, `debug_backtrace_get_args`; under
    /// `zend.exception_ignore_args` the row has no `args`,
    /// `DEBUG_BACKTRACE_IGNORE_ARGS`, a user frame's row and a native's
    /// alike, and the rendering has nothing between the parentheses. The
    /// `Err` is the memory limit refusing the rows.
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// class C { function m($a) { return new Exception('m'); } }
    /// function f($x) { return (new C)->m($x); }
    /// var_dump(f(1)->getTrace());
    /// echo f(1)->getTraceAsString(), "\n";
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(5) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(4)
    ///     ["function"]=>
    ///     string(1) "m"
    ///     ["class"]=>
    ///     string(1) "C"
    ///     ["type"]=>
    ///     string(2) "->"
    ///   }
    ///   [1]=>
    ///   array(3) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(5)
    ///     ["function"]=>
    ///     string(1) "f"
    ///   }
    /// }
    /// #0 %s(4): C->m()
    /// #1 %s(6): f()
    /// #2 {main}
    /// ```
    ///
    /// The row leaves the arguments where they stand: an object or a
    /// counted string the frame holds is still the frame's once the
    /// trace is built without it, and is released as the frame is.
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// function f($o, $s) { throw new Exception('f'); }
    /// try { f(new stdClass, str_repeat('s', 3)); } catch (Exception $e) { var_dump($e->getTrace()); echo $e->getTraceAsString(), "\n"; }
    /// echo "done\n";
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   array(3) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(4)
    ///     ["function"]=>
    ///     string(1) "f"
    ///   }
    /// }
    /// #0 %s(4): f()
    /// #1 {main}
    /// done
    /// ```
    pub(super) fn debug_backtrace(&mut self) -> Result<PhpArray, Thrown> {
        let options = if self.exception_ignore_args() {
            DEBUG_BACKTRACE_IGNORE_ARGS
        } else {
            0
        };
        self.fetch_debug_backtrace(false, options, 0)
    }

    /// `zend_fetch_debug_backtrace` with its three parameters: the link
    /// of the native being served left out where `skip_last` says, as
    /// `debug_backtrace()` leaves itself out; `options`,
    /// `DEBUG_BACKTRACE_PROVIDE_OBJECT` for an `object` row of a
    /// method's receiver between `class` and `type`, where php adds it,
    /// and `DEBUG_BACKTRACE_IGNORE_ARGS` for no `args`; and `limit`
    /// rows at most, none meaning every row.
    ///
    /// The walk lays a row down for every link before it knows whether
    /// to keep it, so the main frame's, which it destroys, is in the
    /// peak, and a walk cut short by `limit` lays down no row past it:
    ///
    /// ```php
    /// function d($a) { return debug_backtrace(); }
    /// function e() { return d(1); }
    /// e(); $s = memory_get_usage(); memory_reset_peak_usage(); $t = e();
    /// echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n";
    /// function d2() { return debug_backtrace(0, 1); }
    /// d2(); unset($t); $s = memory_get_usage(); memory_reset_peak_usage(); $t = d2();
    /// echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n";
    /// ```
    /// ```expect
    /// 1560 1216
    /// 592 592
    /// ```
    pub(crate) fn fetch_debug_backtrace(
        &mut self,
        skip_last: bool,
        options: i64,
        limit: i64,
    ) -> Result<PhpArray, Thrown> {
        let Scratch { links, mut rows } = std::mem::take(&mut self.traces);
        let links = {
            let mut chain = relifetimed(links);
            chain.extend(self.chain());
            // php steps past the native's own link before the walk, so no
            // row of it is one a later row reads back.
            let skipped = usize::from(skip_last).min(chain.len());
            self.push_trace_rows(&chain[skipped..], &mut rows);
            chain.clear();
            relifetimed(chain)
        };
        // `skip_frame: if (ZEND_HAS_OOM()) return;` (8.5.12): once the
        // memory limit was met the walk stops after its first frame.
        if self.has_oom() {
            rows.laid.truncate(1);
        }
        // `frameno < limit`: the walk stops once `limit` rows are in,
        // before it lays down the next, and a dropped row is no row.
        if let Ok(limit) = usize::try_from(limit)
            && limit > 0
            && let Some(cut) = rows
                .laid
                .iter()
                .enumerate()
                .filter(|(_, row)| !matches!(row.0, Traced::Dropped))
                .nth(limit - 1)
                .map(|(at, _)| at + 1)
        {
            rows.laid.truncate(cut);
        }
        let trace = self.render_trace(&mut rows, options);
        rows.clear();
        self.traces = Scratch { links, rows };
        trace
    }

    /// The rows of a trace as `zend_fetch_debug_backtrace` builds each
    /// array: `options`, `DEBUG_BACKTRACE_PROVIDE_OBJECT` for an
    /// `object` row of a method's receiver between `class` and `type`,
    /// and `DEBUG_BACKTRACE_IGNORE_ARGS` for no `args` on a function's
    /// row. A code frame's row keeps the included file as its `args`
    /// under it, between `line` and `function`, where php's
    /// `build_filename_arg` adds it.
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// if (!isset($go)) { function boom($o) { return new Exception('boom'); } $go = 1; $e = include __FILE__; var_dump($e->getTrace()); echo $e->getTraceAsString(), "\n"; return; }
    /// return boom(new stdClass);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(3) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(4)
    ///     ["function"]=>
    ///     string(4) "boom"
    ///   }
    ///   [1]=>
    ///   array(4) {
    ///     ["file"]=>
    ///     string(%d) "%s.php"
    ///     ["line"]=>
    ///     int(3)
    ///     ["args"]=>
    ///     array(1) {
    ///       [0]=>
    ///       string(%d) "%s.php"
    ///     }
    ///     ["function"]=>
    ///     string(7) "include"
    ///   }
    /// }
    /// #0 %s(4): boom()
    /// #1 %s(3): include('%s...')
    /// #2 {main}
    /// ```
    ///
    /// An anonymous class is named whole, `ce->name` with its NUL and
    /// site, where the rendered trace cuts it at the NUL:
    ///
    /// ```php
    /// $o = new class { function b() { return new Exception; } };
    /// $e = $o->b();
    /// var_dump(strlen($e->getTrace()[0]['class']) > 15, strstr($e->getTraceAsString(), 'class@'));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(30) "class@anonymous->b()
    /// #1 {main}"
    /// ```
    ///
    /// Each row's file, function, class and `->` are the interned strings
    /// php's rows share, `ZVAL_STR_COPY` of the op array's file name and
    /// the function's, and its keys `ZSTR_KNOWN`'s, so each frame costs
    /// what php's does whatever the path's length, and only the
    /// `Exception`'s own `file`, which `zend_default_exception_new`
    /// copies, grows with it:
    ///
    /// ```php
    /// function f($a) { return new Exception('x'); }
    /// function h($a) { return f($a); }
    /// class C { function m($a) { return h($a); } static function s($a) { return (new C)->m($a); } }
    /// $w = C::s(1); $v = f(1);
    /// $a = memory_get_usage(); $e = C::s(1); $b = memory_get_usage(); $g = f(1); $c = memory_get_usage();
    /// echo ($b - $a) - ($c - $b), "\n";
    /// ```
    /// ```expect
    /// 1776
    /// ```
    ///
    /// A frame of a function called with no arguments has the process's
    /// empty array as its `args`, `debug_backtrace_get_args`'s
    /// `ZVAL_EMPTY_ARRAY`, so it costs no table:
    ///
    /// ```php
    /// function g() { return new Exception; }
    /// function h() { return g(); }
    /// function k($a) { return new Exception; }
    /// function l() { return k(1); }
    /// $w = h(); $v = l();
    /// $a = memory_get_usage(); $e = h(); $b = memory_get_usage(); $f = l(); $c = memory_get_usage();
    /// var_dump($e->getTrace()[0]['args'], ($c - $b) - ($b - $a));
    /// function z() { debug_zval_dump(debug_backtrace()[0]['args']); }
    /// z();
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// int(216)
    /// array(0) interned {
    /// }
    /// ```
    ///
    /// A frameless call's row names its function before its file and
    /// line and holds its arguments in a hash, as php's does, so a throw
    /// out of one peaks where php's does:
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// pk(function () { $o = new stdClass; try { str_contains($o, 'a'); } catch (TypeError $e) {} });
    /// try { str_contains(new stdClass, 'a'); } catch (TypeError $e) { echo implode(',', array_keys($e->getTrace()[0])), "\n"; }
    /// ```
    /// ```expect
    /// 2992 32
    /// function,file,line,args
    /// ```
    pub(super) fn render_trace(
        &mut self,
        rows: &mut Rows,
        options: i64,
    ) -> Result<PhpArray, Thrown> {
        let ignore_arguments = options & DEBUG_BACKTRACE_IGNORE_ARGS != 0;
        let provide_object = options & DEBUG_BACKTRACE_PROVIDE_OBJECT != 0;
        let mut trace = self.array()?;
        self.constant_expression_row(&mut trace)?;
        let files = std::mem::take(&mut rows.files);
        let bytes_at = |at: &FileAt| files.get(at.clone()).unwrap_or_default();
        // The file string of the row before, which a row of the same file
        // copies: `ZVAL_STR_COPY` of one `op_array->filename`.
        let mut last: Option<(FileAt, PhpString)> = None;
        for (traced, this, scope, arguments, named, site) in rows.laid.drain(..) {
            let site = match site {
                Some((at, line)) => Some((self.row_file(&at, bytes_at(&at), &mut last)?, line)),
                None => None,
            };
            // `zend_new_array(8)` and `zend_hash_real_init_mixed`: every
            // row is a hash laid down before its first key.
            let mut row = PhpArray::sized(self, 8, crate::api::values::Layout::Hash)?.hand_on();
            if let Traced::Frameless(function) = traced {
                let arguments = (!ignore_arguments).then_some(arguments);
                self.frameless_row(&mut row, function, site, arguments)?;
                trace.append(self, Owner::adopt(Value::array(row)))?;
                continue;
            }
            self.set_site(&mut row, site)?;
            // A hook trampoline's row is a function's under the name its
            // call was built under.
            let (traced, built) = match traced {
                Traced::HookTrampoline { function, name } => {
                    (Traced::Function(function), Some(name))
                }
                traced => (traced, None),
            };
            match traced {
                Traced::Function(function) => {
                    let name = self.row_function_name(function, built)?;
                    self.set_string_key(&mut row, Known::Function, Owner::adopt(name))?;
                    // `ce->name` whole: an anonymous class's with its NUL
                    // and site; a receiver's class name for a function of
                    // no scope it is passed to, `zend_ffi_get_func`'s.
                    let class = match (self.method_class(function).or(scope), this) {
                        (Some(class), _) => Some(self.class_name_string(class)?),
                        (None, Some(this)) => {
                            let name = this.handlers(self).get_class_name(self, this);
                            Some(self.declared_string(&name)?.hand_on())
                        }
                        (None, None) => None,
                    };
                    if let Some(class) = class {
                        let class = Value::string(class);
                        self.set_string_key(&mut row, Known::Class, Owner::adopt(class))?;
                        if provide_object && let Some(this) = this {
                            let this = Owner::take(self, this.as_value());
                            self.set_string_key(&mut row, Known::Object, this)?;
                        }
                        let join = if this.is_some() {
                            Known::ObjectOperator
                        } else {
                            Known::PaamayimNekudotayim
                        };
                        let join = self.known(join)?.into_value();
                        self.set_string_key(&mut row, Known::Type, join)?;
                    }
                    if !ignore_arguments {
                        // `debug_backtrace_get_args`'s `false` since
                        // 8.5.12, once the memory limit refused the row's
                        // arguments or was met filling them: the row is
                        // destroyed and the trace so far is the answer.
                        let Some(args) = self.trace_row_arguments(function, arguments, named)?
                        else {
                            self.release(Value::array(row));
                            break;
                        };
                        self.set_string_key(
                            &mut row,
                            Known::Args,
                            Owner::adopt(Value::array(args)),
                        )?;
                    }
                }
                Traced::Include { kind, file } => {
                    // `build_filename_arg`: the included file is the
                    // row's one argument, ahead of `function` and under
                    // `DEBUG_BACKTRACE_IGNORE_ARGS` as without it, made
                    // here with the row as its one owner.
                    if let Some(at) = file {
                        let file = PhpString::new(self, bytes_at(&at))?;
                        let mut args = self.array()?;
                        args.append(self, file.into_value())?;
                        self.set_string_key(
                            &mut row,
                            Known::Args,
                            Owner::adopt(Value::array(args)),
                        )?;
                    }
                    let name = self.known(kind.known())?.into_value();
                    self.set_string_key(&mut row, Known::Function, name)?;
                }
                Traced::Unknown => {
                    let name = Value::string(self.string(b"unknown")?);
                    self.set_string_key(&mut row, Known::Function, Owner::adopt(name))?;
                }
                Traced::Dropped => {
                    // `zend_array_destroy(stack_frame)`.
                    self.release(Value::array(row));
                    continue;
                }
                Traced::Frameless(_) | Traced::HookTrampoline { .. } => {}
            }
            trace.append(self, Owner::adopt(Value::array(row)))?;
            // The rows stop once the memory limit was met, since 8.5.12.
            if self.has_oom() {
                break;
            }
        }
        rows.files = files;
        Ok(trace)
    }

    /// A frameless call's row: its function before its file and line, and
    /// its arguments, where they are asked for, in a hash.
    fn frameless_row(
        &mut self,
        row: &mut PhpArray,
        function: Function,
        site: Option<(PhpString, i64)>,
        arguments: Option<Vec<Value>>,
    ) -> Result<(), Thrown> {
        let name = Value::string(self.function_name_string(function)?);
        self.set_string_key(row, Known::Function, Owner::adopt(name))?;
        self.set_site(row, site)?;
        if let Some(arguments) = arguments {
            let mut args = PhpArray::sized(self, 8, crate::api::values::Layout::Hash)?.hand_on();
            for argument in arguments {
                let argument = Owner::take(self, argument.deref());
                args.append(self, argument)?;
            }
            self.set_string_key(row, Known::Args, Owner::adopt(Value::array(args)))?;
        }
        Ok(())
    }

    /// What a call's row names, and the class it is in: a hook
    /// trampoline's in the class of the property, the trampoline's scope,
    /// its call's; any other call's in its closure's scope, if any.
    fn call_traced(&self, call: &crate::exec::FrameAt<'_>) -> (Traced, Option<ClassId>) {
        let function = call.callee();
        match call.annex().and_then(|annex| annex.trampoline.as_ref()) {
            Some(trampoline) => {
                let name = trampoline.name;
                (
                    Traced::HookTrampoline { function, name },
                    call.called_scope(),
                )
            }
            None if call.info().frameless() => (
                Traced::Frameless(function),
                self.closure_scope(call.closure()),
            ),
            None => (
                Traced::Function(function),
                self.closure_scope(call.closure()),
            ),
        }
    }

    /// A framed row's `function`: the name a hook trampoline's call was
    /// built under, one more owner of it, or the function's own.
    fn row_function_name(
        &mut self,
        function: Function,
        built: Option<PhpString>,
    ) -> Result<Value, Thrown> {
        let Some(built) = built else {
            return Ok(Value::string(self.function_name_string(function)?));
        };
        self.addref(&Value::string(built));
        Ok(Value::string(built))
    }

    /// `func->common.function_name` as a row copies it: the engine's
    /// string for the declared name, read off the declaration with no
    /// copy of its bytes where the engine holds one, and `native` for a
    /// function with no declaration.
    fn function_name_string(&mut self, function: Function) -> Result<PhpString, Thrown> {
        let held = self
            .symbols()
            .function_decl(function)
            .map(|decl| self.shared.existing_interned(&decl.name.0));
        match held {
            Some(Some(held)) => Ok(PhpString::of(held)),
            Some(None) => {
                let name = self.declared_name(function);
                self.declared_string(&name).map(Owner::hand_on)
            }
            None => self.declared_string(b"native").map(Owner::hand_on),
        }
    }

    /// `ce->name` as a row copies it, read as
    /// [`Request::function_name_string`] reads a function's.
    fn class_name_string(&mut self, class: ClassId) -> Result<PhpString, Thrown> {
        let held = self
            .symbols()
            .class_decl(class)
            .and_then(|decl| self.shared.existing_interned(&decl.name.0));
        if let Some(held) = held {
            return Ok(PhpString::of(held));
        }
        let name = self.class_name_bytes(class);
        self.declared_string(&name).map(Owner::hand_on)
    }

    /// The string of a row's file: the one the row before copied where
    /// it names the same file (`last`), with one more owner, and the
    /// engine's string for `bytes` otherwise, which `last` keeps.
    fn row_file(
        &mut self,
        at: &FileAt,
        bytes: &[u8],
        last: &mut Option<(FileAt, PhpString)>,
    ) -> Result<PhpString, Thrown> {
        if let Some((was, file)) = last
            && was == at
        {
            self.addref(&Value::string(*file));
            return Ok(*file);
        }
        let file = self.declared_string(bytes)?.hand_on();
        *last = Some((at.clone(), file));
        Ok(file)
    }

    /// A row's `file` and `line` where it has a site: `ZVAL_STR_COPY` of
    /// the op array's file name, one more owner of its string.
    fn set_site(
        &mut self,
        row: &mut PhpArray,
        site: Option<(PhpString, i64)>,
    ) -> Result<(), Thrown> {
        if let Some((file, line)) = site {
            let file = Value::string(file);
            self.set_string_key(row, Known::File, Owner::adopt(file))?;
            self.set_string_key(row, Known::Line, Owner::adopt(Value::int(line)))?;
        }
        Ok(())
    }

    /// `debug_backtrace_get_args` over a function's row: the positional
    /// arguments, then the extra named ones under their names, each
    /// followed through a reference and given the row's owner, since the
    /// row has them borrowed from the frame, or the
    /// `SensitiveParameterValue` in its place where
    /// [`Request::sensitive_parameters`] marks it. Since 8.5.12 the table
    /// is `array_finit_size_packed`'s, asked for fallibly, a redaction is
    /// followed by a test of the memory limit, and a named one is
    /// `zend_hash_fadd_new`'s: `None`, php's `false`, where the limit
    /// refused one or was met.
    ///
    /// A row's arguments the memory limit refused end the trace before that
    /// row, the limit's fatal raised as the call returns; the limit is set
    /// from what the request holds, which differs between the engines, so its
    /// figure is `%d`:
    ///
    /// ```php
    /// function f(...$a) { ini_set('memory_limit', ((memory_get_usage(true) >> 20) + 4) . 'M'); $t = debug_backtrace(); var_dump("unreached"); }
    /// f(...range(1, 500000));
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of %d bytes exhausted (tried to allocate 8404992 bytes) in %s on line 2
    /// ```
    fn trace_row_arguments(
        &mut self,
        function: Function,
        arguments: Vec<Value>,
        named: Vec<(PhpString, Value)>,
    ) -> Result<Option<PhpArray>, Thrown> {
        let sensitive = self.sensitive_parameters(function);
        // `ZVAL_EMPTY_ARRAY` for a call with no arguments, which a named
        // extra one then separates.
        let mut args = if arguments.is_empty() {
            PhpArray::empty()
        } else {
            let size = u32::try_from(arguments.len()).unwrap_or(u32::MAX);
            let laid = self.memory_mut().fallible(|req| {
                PhpArray::sized(req, size, crate::api::values::Layout::Packed).map(Owner::hand_on)
            })?;
            let Some(laid) = laid else {
                return Ok(None);
            };
            laid
        };
        for (at, argument) in arguments.into_iter().enumerate() {
            let argument = argument.deref();
            let argument = if sensitive.at(at) {
                let redacted = self.sensitive_value(argument)?;
                Owner::adopt(redacted)
            } else {
                Owner::take(self, argument)
            };
            args.append(self, argument)?;
            if sensitive.at(at) && self.has_oom() {
                self.release(Value::array(args));
                return Ok(None);
            }
        }
        for (name, argument) in named {
            let argument = argument.deref();
            let redact = sensitive.variadic;
            let argument = if redact {
                let redacted = self.sensitive_value(argument)?;
                Owner::adopt(redacted)
            } else {
                Owner::take(self, argument)
            };
            if redact && self.has_oom() {
                argument.give_up(self);
                self.release(Value::array(args));
                return Ok(None);
            }
            let name = Owner::take(self, Key::Str(name));
            let added = self
                .memory_mut()
                .fallible(|req| args.set(req, name, argument))?;
            if added.is_none() {
                self.release(Value::array(args));
                return Ok(None);
            }
        }
        Ok(Some(args))
    }

    /// `zend_fetch_debug_backtrace`'s first row while a constant
    /// expression is evaluated (`Z/Zend/zend_builtin_functions.c:1930`):
    /// the site actually executing, `file`, `line` and the function
    /// `[constant expression]`, where it is not the site the override
    /// stands at, so the trace keeps where execution was.
    fn constant_expression_row(&mut self, trace: &mut PhpArray) -> Result<(), Thrown> {
        let Some((file, line)) = self.overridden_site() else {
            return Ok(());
        };
        let executing = match self.vm.innermost_site() {
            Some(site) => Some((site.file.clone(), site.line)),
            None => self.innermost_frame().and_then(|state| {
                state
                    .code()
                    .map(|code| (code.filename.0.clone(), state.line()))
            }),
        };
        let Some((executing, at)) = executing else {
            return Ok(());
        };
        if *executing == *file && at == line {
            return Ok(());
        }
        let mut row = self.array()?;
        let file = Value::string(self.string(&executing)?);
        self.set_string_key(&mut row, Known::File, Owner::adopt(file))?;
        self.set_string_key(
            &mut row,
            Known::Line,
            Owner::adopt(Value::int(i64::from(at))),
        )?;
        let name = Value::string(self.string(b"[constant expression]")?);
        self.set_string_key(&mut row, Known::Function, Owner::adopt(name))?;
        trace.append(self, Owner::adopt(Value::array(row)))
    }

    /// Which arguments of a call to `function` a trace shows redacted,
    /// php's `zend_get_parameter_attribute_str(..., "sensitiveparameter",
    /// i)` over its parameters: each declared one by position, and the
    /// variadic's for every argument past them and every extra named one.
    fn sensitive_parameters(&self, function: Function) -> Sensitive {
        let marked = |parameter: &php_unit::decl::Parameter| {
            parameter
                .attributes
                .iter()
                .any(|attribute| attribute.name.0.eq_ignore_ascii_case(b"SensitiveParameter"))
        };
        let Some(decl) = self.symbols().function_decl(function) else {
            return Sensitive::default();
        };
        Sensitive {
            declared: decl
                .parameters
                .iter()
                .filter(|parameter| !parameter.variadic)
                .map(marked)
                .collect(),
            variadic: decl
                .parameters
                .iter()
                .find(|parameter| parameter.variadic)
                .is_some_and(marked),
        }
    }

    /// `object_init_ex(zend_ce_sensitive_parameter_value)` and its
    /// constructor over `argument`: what a trace shows in place of an
    /// argument `#[\SensitiveParameter]` marks, one owner, the trace's.
    ///
    /// ```php
    /// function f($a, #[\SensitiveParameter] $secret, ...$rest) {
    ///     debug_print_backtrace();
    ///     var_dump(debug_backtrace()[0]['args'][1]->getValue());
    ///     throw new Exception('boom');
    /// }
    /// function g(#[\SensitiveParameter] ...$all) { var_dump(debug_backtrace()[0]['args']); }
    /// g(1, x: 2);
    /// f(1, 'hunter2', 3);
    /// ```
    /// ```expect exit=255
    /// array(2) {
    ///   [0]=>
    ///   object(SensitiveParameterValue)#1 (0) {
    ///   }
    ///   ["x"]=>
    ///   object(SensitiveParameterValue)#2 (0) {
    ///   }
    /// }
    /// #0 %s(9): f(1, Object(SensitiveParameterValue), 3)
    /// string(7) "hunter2"
    ///
    /// Fatal error: Uncaught Exception: boom in %s:5
    /// Stack trace:
    /// #0 %s(9): f(1, Object(SensitiveParameterValue), 3)
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn sensitive_value(&mut self, argument: Value) -> Result<Value, Thrown> {
        let Some(class) = self.class_named(Bare::resolved(b"SensitiveParameterValue")) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: SensitiveParameterValue is not linked",
            ));
        };
        crate::api::call::instantiate_made(self, class, &[argument], &[])
            .map(|object| object.as_value())
    }

    /// php's one `prev_execute_data` chain, innermost first, walked back
    /// out of this engine's two stacks: the user frames, and the calls
    /// being served, each frame's `calls_below` saying where among the
    /// calls it was pushed. A call still being built is not on the
    /// chain, as php's is a chain of activations.
    ///
    /// A fiber's chain goes on into its resumer's, as php's
    /// `stack_bottom->prev_execute_data` names the frame that resumed
    /// it: each stack's links innermost first, the running stack's
    /// before the stacks below it (design §5.11).
    ///
    /// A generator a resumption runs through another stands on the
    /// placeholder php links it to (`execute_fake`), which the walk
    /// expands into the kept frames of the generators between
    /// ([`crate::generator::placeholder_frames`]).
    pub(super) fn chain(&self) -> impl Iterator<Item = Link<'_>> {
        self.executions()
            .flat_map(|vm| links_of(self, vm, 0..vm.depth(), Expand::Generators))
    }

    /// The links a placeholder for `root` expands into, innermost first.
    pub(super) fn placeholder_links(&self, root: PhpObject) -> Vec<Link<'_>> {
        crate::generator::placeholder_frames(self, root)
            .into_iter()
            .map(|state| Link::Frame(Slots::Kept, state))
            .collect()
    }

    /// The rows of `chain`, `EG(current_execute_data)` and its
    /// `prev_execute_data` chain or a stretch of it, innermost first: one
    /// row per link, the function it serves, its
    /// receiver, its arguments, and the site of the link below it. The
    /// main op array's frame is the link that serves no function and
    /// makes no row of its own, since `zend_trace_to_string` closes
    /// every trace with `{main}` whether or not that frame still
    /// stands; every other outermost link is a row with no site, php's
    /// `[internal function]`, which is what a shutdown function or a
    /// destructor served after the main frame popped is.
    ///
    /// php holds one chain; this engine holds two stacks, the user
    /// frames and the calls it is serving, and a frame's `calls_below`
    /// says where among the calls it was pushed, which walks them back
    /// into one. A native that re-entered the loop stands below the
    /// frame it entered, so that frame's site is the native's, which
    /// names no file: `prev->func && ZEND_USER_CODE(...)` is php's
    /// test, and a row with no `file` renders `[internal function]`.
    ///
    /// A user frame's arguments are its parameters' CVs up to the
    /// number passed and then its extra ones
    /// (`debug_backtrace_get_args`), with the extra named ones beside
    /// them; a native's are the call's, as they were placed. A call
    /// still being built is not on the chain, as php's is a chain of
    /// activations.
    ///
    /// A function served from outside the loop over a frame standing at
    /// an include, an error handler the include's warning serves, is
    /// followed by the include's own row, php's `fake_frame`:
    ///
    /// ```php
    /// set_error_handler(function ($no, $msg) { debug_print_backtrace(); return true; });
    /// function f() { include "nope.php"; }
    /// f();
    /// ```
    /// ```expect
    /// #0 %s(3): {closure:%s:2}(2, 'include(nope.ph...', '%s', 3)
    /// #1 %s(3): include()
    /// #2 %s(4): f()
    /// #0 %s(3): {closure:%s:2}(2, 'include(): Fail...', '%s', 3)
    /// #1 %s(3): include()
    /// #2 %s(4): f()
    /// ```
    ///
    /// A frame with a symbol table, one an `eval` or an include in it
    /// attached, has its declared arguments read through the table, where
    /// what the code wrote stands:
    ///
    /// ```php
    /// class A { public function __call($name, $args) { eval('$args = array(); var_dump(debug_backtrace()[1]["args"]);'); } }
    /// (new A)->test("test");
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(4) "test"
    ///   [1]=>
    ///   array(0) {
    ///   }
    /// }
    /// ```
    ///
    /// A native trampoline (`ZEND_ACC_CALL_VIA_TRAMPOLINE`), the C
    /// function `zend_ffi_get_func` makes, has no site of its own to give
    /// what it calls, so the walk goes on to the user code that called
    /// it; and its row, of no scope, names its receiver's class:
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef void (*handler)(int); handler signal(int sig, handler h); int raise(int sig);");
    /// $ffi->signal(SIGUSR1, function ($sig) { debug_print_backtrace(); echo debug_backtrace()[1]['class'], "\n"; });
    /// $ffi->raise(SIGUSR1);
    /// ```
    /// ```expect process
    /// #0 %s(4): {closure:%s:3}(%d)
    /// #1 %s(4): FFI->raise(%d)
    /// FFI
    /// ```
    ///
    /// A destructor a delegated generator's close serves is called from
    /// php's `execute_fake`, the placeholder its delegating generators'
    /// frames are expanded from: its row's site is the fake's
    /// `prev_execute_data`, the resumer's line, and the generators'
    /// rows follow it, each sited at the one below.
    ///
    /// ```php
    /// class Foo { function __destruct() { debug_print_backtrace(); } }
    /// function bar() {
    ///     yield from foo();
    /// }
    /// function foo() { $foo = new Foo(); yield; }
    /// $gen = bar();
    /// foreach ($gen as $dummy);
    /// function outer() { yield from middle(); }
    /// function middle() { yield from foo(); }
    /// foreach (outer() as $dummy);
    /// ```
    /// ```expect
    /// #0 %s(8): Foo->__destruct()
    /// #1 %s(8): bar()
    /// #0 %s(11): Foo->__destruct()
    /// #1 %s(9): middle()
    /// #2 %s(11): outer()
    /// ```
    pub(super) fn trace_rows(&self, chain: &[Link<'_>]) -> Rows {
        let mut rows = Rows::default();
        self.push_trace_rows(chain, &mut rows);
        rows
    }

    /// [`Request::trace_rows`] pushed onto `rows`, which holds none.
    fn push_trace_rows(&self, chain: &[Link<'_>], rows: &mut Rows) {
        // The include the link below `at` stands at, where it is a user
        // frame at an `INCLUDE_OR_EVAL`.
        let including = |at: usize| {
            chain
                .get(at.saturating_add(1))
                .and_then(|below| match below {
                    Link::Frame(_, below) => Some(below),
                    Link::Call(_) | Link::Site(_) | Link::Fake(_) => None,
                })
                .and_then(|below| exec::opline_at(below.code()?, below.opline()))
                .filter(|op| op.opcode == Opcode::IncludeOrEval)
                .and_then(|op| IncludeKind::of(op.extended_value))
        };
        for (at, link) in chain.iter().enumerate() {
            let site = self
                .row_site(chain, at.saturating_add(1))
                .map(|(file, line)| (rows.lay_file(file), line));
            match link {
                Link::Frame(_, state) => {
                    let Some(function) = state.function() else {
                        // A code frame: named by the includer's opline
                        // where that is an `INCLUDE_OR_EVAL`, and the
                        // main op array's frame, which no opline pushed,
                        // makes no row (`zend_trace_to_string` closes
                        // every trace with `{main}`).
                        let Some(kind) = including(at) else {
                            // `zend_fetch_debug_backtrace`'s default arm:
                            // a dummy frame is skipped where it holds no
                            // file and line, and named `unknown` where
                            // it stands over user code.
                            let traced = if site.is_some() {
                                Traced::Unknown
                            } else {
                                Traced::Dropped
                            };
                            rows.laid
                                .push((traced, None, None, Vec::new(), Vec::new(), site));
                            continue;
                        };
                        // `build_filename_arg`: an `eval` names no file.
                        let file = (at > 0 && kind != IncludeKind::Eval)
                            .then(|| state.code())
                            .flatten()
                            .map(|code| rows.lay_file(&code.filename.0));
                        rows.laid.push((
                            Traced::Include { kind, file },
                            None,
                            None,
                            Vec::new(),
                            Vec::new(),
                            site,
                        ));
                        continue;
                    };
                    let arguments = self.traced_arguments(*state, function);
                    rows.laid.push((
                        Traced::Function(function),
                        state.this(),
                        self.closure_scope(state.closure()),
                        arguments,
                        state.extra_named().to_vec(),
                        site.clone(),
                    ));
                    // `fake_frame`: a function served from outside the
                    // loop, an error handler among them, over a frame
                    // that stands at an include is followed by the
                    // include's own row, whose argument is php's
                    // `include_filename`, the site of the row before the
                    // function's, and none where it is the first.
                    if !matches!(state.caller(), exec::Caller::Opline(_))
                        && let Some(kind) = including(at)
                    {
                        let file = (kind != IncludeKind::Eval)
                            .then(|| rows.laid.iter().rev().nth(1))
                            .flatten()
                            .and_then(|row: &TraceRow| {
                                row.5.as_ref().map(|(file, _)| file.clone())
                            });
                        rows.laid.push((
                            Traced::Include { kind, file },
                            None,
                            None,
                            Vec::new(),
                            Vec::new(),
                            site,
                        ));
                    }
                }
                // `ZEND_ACC_CALL_VIA_TRAMPOLINE`: no row of its own, nor
                // has the fake, which the walk meets as the generators'
                // rows after it.
                Link::Site(_) | Link::Fake(_) => {}
                Link::Call(call) => {
                    let (traced, scope) = self.call_traced(call);
                    rows.laid.push((
                        traced,
                        call.this(),
                        scope,
                        call.arguments(),
                        call.extra_named().to_vec(),
                        site,
                    ));
                }
            }
        }
    }

    /// The site a row reads off the link at `below` in `chain`: a user
    /// frame's file and line and a site's; a native's is none, but where
    /// it is a trampoline (`ZEND_ACC_CALL_VIA_TRAMPOLINE`) the walk goes
    /// on to the user code that called it, and where it is php's
    /// `execute_fake`, which has no function, it goes on past the
    /// generators the fake stands for to its `prev_execute_data`, the
    /// resumer, as `zend_fetch_debug_backtrace`'s walk from a `prev`
    /// that is no user code goes on.
    fn row_site<'c>(&self, chain: &[Link<'c>], mut below: usize) -> Option<(&'c [u8], i64)> {
        loop {
            break match chain.get(below) {
                Some(Link::Frame(_, below)) => {
                    below.file().map(|file| (file, i64::from(below.line())))
                }
                Some(Link::Site(below)) => Some((&below.file[..], i64::from(below.line))),
                Some(Link::Call(call)) if self.via_trampoline(call.callee()) => {
                    below = below.saturating_add(1);
                    continue;
                }
                Some(Link::Fake(generators)) => {
                    below = below.saturating_add(1).saturating_add(*generators);
                    continue;
                }
                Some(Link::Call(_)) | None => None,
            };
        }
    }

    /// `zend_trace_to_string` (`Z/Zend/zend_exceptions.c:668`):
    /// `_build_trace_string` over each row that is an array, and php's
    /// warning for one that is not: `#N file(line): ` where the row names
    /// a `file`, `[internal function]: ` where it names none, the
    /// `class`, `type` and `function` where the row has them, and the
    /// `args` between the parentheses; then, under `include_main`, `#N
    /// {main}`, and without it every line closed by its newline, which is
    /// what `debug_print_backtrace()` writes. A field of the wrong type
    /// is php's warning and its `[unknown]` stand-in.
    ///
    /// ```php
    /// $e = new Exception();
    /// $ref = new ReflectionProperty($e, 'trace');
    /// $ref->setValue($e, [null, [], ['file' => null, 'function' => 1, 'args' => 2]]);
    /// var_dump($e->getTraceAsString());
    /// ```
    /// ```expect
    ///
    /// Warning: Expected array for frame 0 in %s on line 5
    ///
    /// Warning: File name is not a string in %s on line 5
    ///
    /// Warning: Value for function is not a string in %s on line 5
    ///
    /// Warning: args element is not an array in %s on line 5
    /// string(67) "#0 [internal function]: ()
    /// #1 [unknown file]: [unknown]()
    /// #2 {main}"
    /// ```
    pub(crate) fn trace_to_string(&mut self, trace: PhpArray, include_main: bool) -> Vec<u8> {
        use std::io::Write as _;
        let mut out: Vec<u8> = Vec::new();
        let mut num = 0usize;
        // `ZEND_HASH_FOREACH_NUM_KEY_VAL(trace, index, frame)`
        // (`Z/Zend/zend_exceptions.c:641`), no owner of the trace taken: the
        // property holds it while a warning reaches the program's handler.
        // Each field is looked up in the frame as `_build_trace_string`
        // looks it up, when it is read; the walk's pin of the frame keeps
        // it for the step.
        let walked = walk::foreach(self, trace, Hold::None, |req, frame| -> walk::Step {
            let Some(row) = frame.value().as_array() else {
                let index = match frame.key() {
                    Key::Int(n) => n.to_string(),
                    Key::Str(_) => String::from("0"),
                };
                crate::api::raising::warning(req, &format!("Expected array for frame {index}"));
                return Ok(ControlFlow::Continue(()));
            };
            let field = |name: &[u8]| row.find(name);
            write!(out, "#{num} ").ok();
            num += 1;
            match field(b"file").map(|file| file.as_str_bytes().map(<[u8]>::to_vec)) {
                None => out.extend_from_slice(b"[internal function]: "),
                Some(None) => {
                    crate::api::raising::warning(req, "File name is not a string");
                    out.extend_from_slice(b"[unknown file]: ");
                }
                Some(Some(file)) => {
                    let line = match field(b"line") {
                        Some(line) => line.as_int().unwrap_or_else(|| {
                            crate::api::raising::warning(req, "Line is not an int");
                            0
                        }),
                        None => 0,
                    };
                    out.extend_from_slice(&file);
                    write!(out, "({line}): ").ok();
                }
            }
            for key in ["class", "type", "function"] {
                let Some(value) = field(key.as_bytes()) else {
                    continue;
                };
                // `smart_str_appends(Z_STRVAL_P)`: up to a NUL, which cuts
                // an anonymous class's name to `class@anonymous`.
                if let Some(text) = value.as_str_bytes() {
                    let text = text.split(|byte| *byte == 0).next().unwrap_or_default();
                    out.extend_from_slice(text);
                } else {
                    crate::api::raising::warning(req, &format!("Value for {key} is not a string"));
                    out.extend_from_slice(b"[unknown]");
                }
            }
            out.push(b'(');
            if let Some(args) = field(b"args") {
                match args.as_array() {
                    Some(args) => out.extend_from_slice(&req.trace_arguments(args)?),
                    None => crate::api::raising::warning(req, "args element is not an array"),
                }
            }
            out.extend_from_slice(b")\n");
            Ok(ControlFlow::Continue(()))
        });
        // The body answers no throwable but what a frame's arguments'
        // walk answered, which none does.
        match walked {
            Ok(_) | Err(_) => {}
        }
        if include_main {
            write!(out, "#{num} {{main}}").ok();
        }
        out
    }

    /// The backtrace a fatal carries, `zend_fetch_debug_backtrace`
    /// beside its rendering by `zend_trace_to_string` at the site; the
    /// memory limit refusing the rows leaves the fatal `{main}` alone
    /// and no rows for `error_get_last()`.
    pub(super) fn trace_as_string(&mut self) -> (Option<PhpArray>, String) {
        let Ok(trace) = self.debug_backtrace() else {
            return (None, String::from("#0 {main}"));
        };
        // `last_fatal_error_backtrace`'s rendering is held as a `String`
        // by the fatal's display and log forms.
        let text = String::from_utf8_lossy(&self.trace_to_string(trace, true)).into_owned();
        (Some(trace), text)
    }

    /// `_build_trace_args` (`Z/Zend/zend_exceptions.c:539`) over one
    /// frame's arguments, `, ` between them and `name: ` before one the
    /// row keeps under a string key, which is an extra named argument
    /// (`_build_trace_string`'s walk of `args`): `smart_str_append_zval`
    /// for a scalar and an enum case, and php's three words for the rest.
    /// `NULL`, `true`, `false` and a long's digits; a double through
    /// `smart_str_append_double` at `precision` with `zero_fraction` set,
    /// which is `zend_gcvt` and then `.0` on a finite text with no `.` in
    /// it, so `1.0`, `-0.0` and `1.0E+20` where `echo` writes `1`, `-0`
    /// and `1.0E+20`; a string in `'`, escaped by `smart_str_append_escaped`
    /// and cut at `zend.exception_string_param_max_len` bytes with `...`
    /// after the cut; `Array`, `Object(ClassName)`, `Resource id #N`, and
    /// an enum case as `Enum::Case`. A reference is followed first,
    /// `ZVAL_DEREF`.
    ///
    /// ```php
    /// memory_get_usage(null, true, false, -7, 1.0, -0.0, 1e20, 0.1, INF, NAN, "a\nb\\c\te\$\e", "exactly fifteen", "sixteen chars!!!", "é");
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: memory_get_usage() expects at most 1 argument, 14 given in %s:2
    /// Stack trace:
    /// #0 %s(2): memory_get_usage(NULL, true, false, -7, 1.0, -0.0, 1.0E+20, 0.1, INF, NAN, 'a\nb\\c\te$\e', 'exactly fifteen', 'sixteen chars!!...', '\xC3\xA9')
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// `zend.exception_ignore_args` leaves the frame without its
    /// arguments, and `zend.exception_string_param_max_len` moves the cut.
    ///
    /// ```php
    /// ini_set('zend.exception_string_param_max_len', '3'); ini_set('zend.exception_ignore_args', '0');
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit('sec...', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    ///
    /// ```php
    /// ini_set('zend.exception_ignore_args', '1');
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit()
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    fn trace_arguments(&mut self, arguments: PhpArray) -> Result<Vec<u8>, Thrown> {
        let mut out: Vec<u8> = Vec::new();
        let mut first = true;
        // `ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(tmp), name, arg)`
        // (`Z/Zend/zend_exceptions.c:616`), no owner of the table taken:
        // the frame holds it.
        walk::foreach(self, arguments, Hold::None, |req, row| -> walk::Step {
            if !first {
                out.extend_from_slice(b", ");
            }
            first = false;
            if let Key::Str(name) = row.key() {
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b": ");
            }
            req.trace_argument(&mut out, &row.value().deref());
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(out)
    }

    /// One argument as [`Request::trace_arguments`] renders it,
    /// `_build_trace_args` (`Z/Zend/zend_exceptions.c`): a reference
    /// followed, `ZVAL_DEREF`, then [`Request::zval_text`], and where
    /// that has no text, `Array`, `Object(ClassName)` or `Resource id #N`.
    fn trace_argument(&mut self, out: &mut Vec<u8>, argument: &Value) {
        let argument = argument.deref();
        if let Some(text) = self.zval_text(&argument, self.exception_string_param_max_len()) {
            out.extend_from_slice(&text);
            return;
        }
        match argument.as_ref() {
            ValueRef::Array(_) => out.extend_from_slice(b"Array"),
            ValueRef::Object(object) => {
                // `Z_OBJ_HANDLER_P(arg, get_class_name)`, then
                // `smart_str_appends(str, ZSTR_VAL(class_name))`: up to
                // the NUL of an anonymous class's name.
                let table = self.object_handlers(object.heap());
                let name = table.get_class_name(self, object);
                out.extend_from_slice(b"Object(");
                out.extend_from_slice(php_unit::decl::shown_bytes(&name));
                out.push(b')');
            }
            ValueRef::Resource(resource) => {
                out.extend_from_slice(b"Resource id #");
                out.extend_from_slice(resource.id().to_string().as_bytes());
            }
            // `zval_text` wrote every scalar and every enum case, and
            // `deref` followed the reference.
            ValueRef::Undef
            | ValueRef::Null
            | ValueRef::Bool(_)
            | ValueRef::Int(_)
            | ValueRef::Float(_)
            | ValueRef::Str(_)
            | ValueRef::Reference(_) => {}
        }
    }

    /// `smart_str_append_zval` (`Z/Zend/zend_smart_str.c:233`): the text
    /// php quotes a value by, in a trace's arguments and in
    /// `UnhandledMatchError`'s message, for a scalar and an enum case,
    /// and none for anything else. A scalar is `smart_str_append_scalar`:
    /// `NULL` for an undefined value as for `null`, `true` or `false`, a
    /// long in decimal, a double by `smart_str_append_double` at
    /// `precision` (`1` for `0`) with `.0` on a finite text that carries
    /// no `.`, so `1.0`, `-0.0` and `1.0E+20` where `echo` writes `1`,
    /// `-0` and `1.0E+20`, and a string in `'`, escaped by
    /// `smart_str_append_escaped` and cut at `truncate` bytes with `...`
    /// after the cut. An enum case is `Enum::Case`. The test is on the
    /// value's own type, before any `ZVAL_DEREF`, so a reference has no
    /// text here and a caller that renders what it refers to derefs first.
    pub(crate) fn zval_text(&self, value: &Value, truncate: usize) -> Option<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        match value.as_ref() {
            ValueRef::Undef | ValueRef::Null => out.extend_from_slice(b"NULL"),
            ValueRef::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
            ValueRef::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
            ValueRef::Float(f) => {
                let precision = match self.ini().precision() {
                    0 => 1,
                    precision => precision,
                };
                let text = php_heap::double_to_string(f, precision, php_heap::Exponent::Upper);
                out.extend_from_slice(text.as_bytes());
                if f.is_finite() && !text.contains('.') {
                    out.extend_from_slice(b".0");
                }
            }
            ValueRef::Str(s) => {
                // `smart_str_append_escaped` writes ASCII alone.
                let mut escaped = String::new();
                append_escaped(&mut escaped, &s.as_bytes()[..truncate.min(s.len())]);
                out.push(b'\'');
                out.extend_from_slice(escaped.as_bytes());
                if s.len() > truncate {
                    out.extend_from_slice(b"...");
                }
                out.push(b'\'');
            }
            ValueRef::Object(object) => {
                let decl = self
                    .symbols()
                    .class_decl(object.class())
                    .filter(|decl| decl.kind == ClassKind::Enum)?;
                // `zend_enum_fetch_case_name`: the case's `name`.
                let case = object.prop(self, b"name")?.as_php_string()?;
                // `smart_str_append(dest, ce->name)`, the name whole.
                out.extend_from_slice(&decl.name.0);
                out.extend_from_slice(b"::");
                out.extend_from_slice(case.as_bytes());
            }
            ValueRef::Array(_) | ValueRef::Resource(_) | ValueRef::Reference(_) => return None,
        }
        Some(out)
    }

    /// `zend.exception_ignore_args`: whether a throwable's trace is
    /// built without the frames' arguments, and whether
    /// `UnhandledMatchError` names its subject by type alone.
    pub(crate) fn exception_ignore_args(&self) -> bool {
        self.typed_ini().exception_ignore_args
    }

    /// `zend.exception_string_param_max_len`: the bytes of a string
    /// argument a trace prints before `...`, `EG(exception_string_param_max_len)`,
    /// which `OnSetExceptionStringParamMaxLen` set from the text with
    /// `ZEND_ATOL` (`Z/Zend/zend.c:177`), so `' +4x'` cuts at four.
    ///
    /// ```php
    /// var_dump(ini_set('zend.exception_string_param_max_len', ' +4x'));
    /// set_time_limit('seconds', 2);
    /// ```
    /// ```expect exit=255
    /// string(2) "15"
    ///
    /// Fatal error: Uncaught ArgumentCountError: set_time_limit() expects exactly 1 argument, 2 given in %s:3
    /// Stack trace:
    /// #0 %s(3): set_time_limit('seco...', 2)
    /// #1 {main}
    ///   thrown in %s on line 3
    /// ```
    pub(crate) fn exception_string_param_max_len(&self) -> usize {
        self.typed_ini().exception_string_param_max_len
    }

    /// A user frame's arguments for its trace row
    /// (`debug_backtrace_get_args`): its parameters' CVs up to the number
    /// passed, read through its symbol table where it has one, whose
    /// values the stack may hold stale (`ZEND_CALL_HAS_SYMBOL_TABLE`,
    /// bug #73156), then its extra ones. Views, no owner taken.
    fn traced_arguments(&self, state: exec::FrameAt<'_>, function: Function) -> Vec<Value> {
        let passed = usize::try_from(state.num_args()).unwrap_or(0);
        let extra = state.extra_args();
        let mut arguments: Vec<Value> = (0..passed.saturating_sub(extra.len()))
            .map(|slot| {
                let slot = Slot(u32::try_from(slot).unwrap_or(u32::MAX));
                Value::of_slot(state.frame.slot(slot))
            })
            .collect();
        if let Some(table) = state.symbol_table() {
            let names = state
                .code()
                .map(|code| code.vars.clone())
                .unwrap_or_default();
            let declared = self
                .symbols()
                .function_decl(function)
                .map_or(0, |decl| decl.parameters.len());
            for (at, argument) in arguments.iter_mut().enumerate().take(declared) {
                if let Some(name) = names.get(at)
                    && let Some(value) = self.symbol_value(table, &name.0)
                {
                    *argument = value;
                }
            }
        }
        arguments.extend(extra.iter().copied());
        arguments
    }

    /// `zend_hash_str_update` on a row: `value` at the string key `name`,
    /// the key a string of the request's that the array takes.
    fn set_string_key(
        &mut self,
        row: &mut PhpArray,
        name: Known,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        // `ZSTR_KNOWN(ZEND_STR_FILE)` and its kin.
        let key = match self.known(name) {
            Ok(name) => name.into_key(),
            Err(thrown) => {
                value.give_up(self);
                return Err(thrown);
            }
        };
        row.set(self, key, value)
    }
}

impl Frames<&mut Request> {
    /// `zend_fetch_debug_backtrace(return_value, 1, options, limit)`,
    /// what `debug_backtrace()` answers from inside the native being
    /// served: the rows below it, under php's options and limit.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn backtrace(self, options: i64, limit: i64) -> Result<Owner<PhpArray>, Thrown> {
        adopted(self.0.fetch_debug_backtrace(true, options, limit))
    }

    /// `zend_fetch_debug_backtrace(return_value, 0, options, limit)`,
    /// what a debugger stopped between two oplines builds, as phpdbg's
    /// `back` and `frame` build it: every row of the chain from the
    /// innermost frame down, none skipped, under php's options and
    /// limit.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn backtrace_here(self, options: i64, limit: i64) -> Result<Owner<PhpArray>, Thrown> {
        adopted(self.0.fetch_debug_backtrace(false, options, limit))
    }
}
