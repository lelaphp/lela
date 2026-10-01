//! What the tier face reads off a request between steps
//! ([`crate::tier`]): the stack a serving's entry slot stands on, and
//! the properties phpdbg reads off a pending throwable.

use std::sync::Arc;

use php_compile::SourceKind;
use php_heap::{Fatal, FrameRef, VmStack};

use std::fmt::Write as _;

use php_unit::decl::ClassKind;

use super::{Request, Symbol};
use crate::api::call::Function;
use crate::api::raising::{Level, Thrown};
use crate::api::values::{ClassId, PhpObject, PhpString, TypeTag, Value};
use crate::exec::{self, TableId};
use crate::tier::{Called, Class, Code, Evaluated, Named, Shape, Under};

impl Request {
    /// The VM stack the request runs on, read.
    pub(crate) fn stack(&self) -> &VmStack {
        &self.stack
    }

    /// A property of a throwable as its bytes, `zval_get_string` over
    /// `zend_read_property_ex(..., silent)`: the empty string where it
    /// holds no string.
    pub(crate) fn throwable_prop_bytes(&self, object: PhpObject, name: &[u8]) -> Vec<u8> {
        object
            .prop(self, name)
            .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default()
    }

    /// A property of a throwable as a number, `zval_get_long`: zero where
    /// it holds no int.
    pub(crate) fn throwable_prop_int(&self, object: PhpObject, name: &[u8]) -> i64 {
        object
            .prop(self, name)
            .and_then(|value| value.as_int())
            .unwrap_or(0)
    }

    /// phpdbg's `ev` (`Z/sapi/phpdbg/phpdbg_prompt.c:922`) over
    /// `zend_eval_stringl(code, &retval, "eval()'d code")`: `return code;`
    /// compiled as an `eval` under that name and served by the loop bound
    /// to the engine, in a frame's scope or, with nothing executing, at
    /// the top of the request ([`Request::nothing_executing`]); then what
    /// it returned as `print_r` renders it, or what it left uncaught
    /// rendered as `zend_exception_error` renders it at `E_ERROR`, the
    /// request going on either way. A fatal is the `Err`, and ends the
    /// request.
    pub(crate) fn evaluate_in(&mut self, under: Under, code: &[u8]) -> Result<Evaluated, Fatal> {
        match under {
            Under::Frame(frame) => self.evaluate_over(Some(frame), code),
            Under::Nothing => self.nothing_executing(|req| req.evaluate_over(None, code)),
        }
    }

    /// The evaluation itself, in `scope`'s frame as a nested code frame
    /// is, or, with none, on a frame of its own over the global table as
    /// `zend_execute` serves an op array with no frame standing.
    fn evaluate_over(&mut self, scope: Option<FrameRef>, code: &[u8]) -> Result<Evaluated, Fatal> {
        const NAME: &[u8] = b"eval()'d code";
        let source = [&b"return "[..], code, b";"].concat();
        let shared = Arc::clone(&self.shared);
        let unit = match shared.compile_for(self, &source, NAME, SourceKind::Eval) {
            Ok(unit) => unit,
            Err(refused) => {
                let thrown = self.refused_include(&refused, NAME, SourceKind::Eval);
                self.leave_pending(&thrown);
                return Ok(self.evaluation_uncaught());
            }
        };
        if !crate::engine::note_compiled(self, &unit, SourceKind::Eval) {
            return Ok(self.evaluation_uncaught());
        }
        let standing = self.vm.frames().len();
        let callee = match scope {
            Some(frame) => self.push_code_frame(unit, frame, None)?,
            None => self.push_frame(unit)?,
        };
        let reenter = self.shared.reenter;
        let served = reenter(self, callee);
        // `zend_leave_helper`'s arm for a nested code frame: its CVs
        // detached into the table it shares with the frame it was
        // evaluated in, which is attached to it again.
        while self.vm.frames().len() > standing {
            let top = self.stack.current();
            self.leave_frame(top);
        }
        served?;
        if self.is_pending() {
            return Ok(self.evaluation_uncaught());
        }
        let returned = std::mem::replace(&mut self.returned, Value::null());
        let printed = self.print_r(&returned);
        self.release(returned);
        Ok(Evaluated::Printed(printed))
    }

    /// `f` served with the request executing nothing, as php's
    /// `EG(current_execute_data)` is `NULL` before `zend_execute` begins
    /// the program: the frames the request stands on set aside, their
    /// engine half with them, so what `f` serves is the whole chain,
    /// names no active file but its own and traces to `{main}`, and is
    /// pushed above their room on the same page as php pushes it on its
    /// one VM stack. What a frame set aside was lent is its own while
    /// it is off the chain, as a fiber's switch leaves it
    /// ([`Request::switch_execution`]). The global table's names that
    /// stand in a frame's CV are taken back into the table first, as
    /// `zend_detach_symbol_table` takes them, since a name binds a CV by
    /// the frame's number on the stack; those frames are attached again
    /// after, the lowest first, and what `f` left in the table is theirs.
    pub(crate) fn nothing_executing<R>(&mut self, f: impl FnOnce(&mut Request) -> R) -> R {
        let bound = self.unbind_globals();
        self.own_lent_receivers();
        let frames = self.stack.set_aside();
        let vm = std::mem::take(&mut self.vm);
        let served = f(self);
        self.vm = vm;
        self.stack.put_back(&mut self.heap, frames);
        for frame in bound {
            self.attach_symbol_table(frame);
        }
        served
    }

    /// Every name of the global table that stands in a frame's CV, its
    /// value moved into the table where the CV holds one and the name
    /// deleted where it holds none, `zend_detach_symbol_table`'s rule;
    /// the frames they stood in, the lowest first.
    fn unbind_globals(&mut self) -> Vec<FrameRef> {
        let bound: Vec<(Vec<u8>, FrameRef, u32)> = self
            .tables
            .get(TableId::GLOBAL.0 as usize)
            .map(|table| {
                table
                    .entries
                    .iter()
                    .filter_map(|(name, symbol)| match symbol {
                        Symbol::Indirect(frame, slot) => Some((name.clone(), *frame, *slot)),
                        Symbol::Value(_) | Symbol::Held(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut frames: Vec<FrameRef> = Vec::new();
        for (name, frame, slot) in bound {
            let value = self.take_slot(frame, slot);
            if value.tag() == TypeTag::Undef {
                self.symbol_del(TableId::GLOBAL, &name);
            } else {
                self.symbol_update(TableId::GLOBAL, name, value);
            }
            if !frames.contains(&frame) {
                frames.push(frame);
            }
        }
        frames.sort_by_key(|frame| frame.0);
        frames
    }

    /// What an evaluation left pending, rendered uncaught with
    /// `E_DONT_BAIL`, so the request goes on with the status
    /// `php_error_cb` gave it, 255, as php's `ev` leaves it.
    fn evaluation_uncaught(&mut self) -> Evaluated {
        if self.pending().is_none() {
            // An exit, which php's `zend_try` catches.
            self.raised = None;
            return Evaluated::Rendered;
        }
        let _ = self.exception_error(Level::Error);
        self.catch_bailout();
        Evaluated::Rendered
    }

    /// `print_r($value, true)` served by the function of that name, as
    /// `zend_print_zval_r` renders a value.
    fn print_r(&mut self, value: &Value) -> Vec<u8> {
        self.addref(value);
        let rendered = self.call_named(b"print_r", &[*value, Value::bool(true)]);
        match rendered {
            Ok(returned) => {
                let bytes = returned
                    .as_str_bytes()
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                self.release(returned);
                bytes
            }
            Err(thrown) => {
                self.leave_pending(&thrown);
                let _ = self.exception_error(Level::Error);
                self.catch_bailout();
                Vec::new()
            }
        }
    }

    /// The function named `name` called with `arguments`, each given
    /// up after, as `zend_call_function` calls one by name.
    pub(crate) fn call_named(&mut self, name: &[u8], arguments: &[Value]) -> Result<Value, Thrown> {
        let name = PhpString::new(self, name)?;
        let callable = crate::api::coerce::to_callable(self, &Value::string(name));
        self.release(Value::string(name));
        let callable = callable?;
        let called = crate::api::call::call(self, &callable, arguments);
        for argument in arguments {
            self.release(*argument);
        }
        called
    }

    /// A host's call of a function by name ([`crate::tier::call`]): from
    /// the frame standing, or with nothing executing, where
    /// `zend_call_function` renders what the callee left uncaught itself
    /// as a fatal, `zend_throw_exception_internal` with no frame
    /// ([`Request::thrown_without_frame`]), and leaves as a fatal the
    /// callee raised and an `exit` leave. That is caught where the host
    /// asked, as phpdbg's prompt catches it in its `zend_try`, and the
    /// request goes on: no fatal is what it ends in, and its status is
    /// what an `exit` set or, after a fatal, what it was before, as php's
    /// is where the prompt that `php_sapi_phpdbg_log_message` stands at
    /// for the fatal's log is quit: `php_error_cb` writes 255 only once
    /// the log returns.
    pub(crate) fn call_by_name(
        &mut self,
        under: Under,
        name: &[u8],
        arguments: &[crate::tier::Argument],
    ) -> Called {
        match under {
            Under::Frame(_) => self.call_by_name_here(name, arguments),
            Under::Nothing => self.nothing_executing(|req| {
                let status = req.exit_status;
                let ending = req.ending.take();
                let called = req.call_by_name_here(name, arguments);
                let rendered = req.ending.is_some();
                req.ending = ending;
                if !req.bailing_out() {
                    return called;
                }
                req.catch_bailout();
                if rendered {
                    req.exit_status = status;
                    Called::Rendered
                } else {
                    Called::Exited
                }
            }),
        }
    }

    /// The call from wherever the request stands.
    fn call_by_name_here(&mut self, name: &[u8], arguments: &[crate::tier::Argument]) -> Called {
        let mut values = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let value = match argument {
                crate::tier::Argument::Int(n) => Value::int(*n),
                crate::tier::Argument::Str(bytes) => match PhpString::new(self, bytes) {
                    Ok(string) => Value::string(string),
                    Err(_) => return Called::Returned,
                },
            };
            values.push(value);
        }
        match self.call_named(name, &values) {
            Ok(returned) => {
                self.release(returned);
                Called::Returned
            }
            Err(thrown) => {
                if thrown.exception().is_none() {
                    return Called::Returned;
                }
                self.leave_pending(&thrown);
                let _ = self.exception_error(Level::Error);
                self.catch_bailout();
                Called::Rendered
            }
        }
    }

    // ------------------------------------------------ code, by name

    /// `zend_flf_functions` by name, what a dump names a frameless call.
    fn frameless_names(&self) -> Vec<php_unit::decl::Name> {
        self.shared
            .frameless
            .iter()
            .map(|entry| {
                self.shared
                    .functions
                    .get(entry.function as usize)
                    .map(|decl| decl.name.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// A function of the table as the face shows it.
    fn code_view(&self, function: Function) -> Option<Code> {
        let decl = self.function_decl(function)?;
        let name = decl.name.0.to_vec();
        let class = self
            .method_class(function)
            .map(|class| self.class_name(class).into_bytes());
        let Some(user) = self.user_function(function) else {
            return Some(Code {
                name,
                class,
                user: false,
                file: Vec::new(),
                lines: (0, 0),
                ops: Vec::new(),
                params: u32::try_from(decl.parameters.len()).unwrap_or(u32::MAX),
                dump: Vec::new(),
            });
        };
        let body = exec::body_of(&user.unit, user.body)?;
        let args = decl.parameters.iter().filter(|p| !p.variadic).count();
        let shown = match &class {
            Some(class) => format!(
                "{}::{}",
                String::from_utf8_lossy(class),
                String::from_utf8_lossy(&name)
            ),
            None => String::from_utf8_lossy(&name).into_owned(),
        };
        Some(Code {
            dump: self.dump_with_dynamic(&user.unit, body, &shown, args),
            name,
            class,
            user: true,
            file: body.filename.0.to_vec(),
            lines: (body.line_start, body.line_end),
            ops: ops_of(body),
            params: u32::try_from(decl.parameters.len()).unwrap_or(u32::MAX),
        })
    }

    /// `phpdbg_print_function_helper`: the op array, then each function
    /// its body declares, under `dynamic def: n, function name: name`.
    fn dump_with_dynamic(
        &self,
        unit: &php_unit::Unit,
        body: &php_unit::OpArray,
        name: &str,
        args: usize,
    ) -> Vec<u8> {
        let frameless = self.frameless_names();
        let mut dump = body.dump_lines(name, args, &frameless);
        for (i, at) in body.dynamic_func_defs.iter().enumerate() {
            let Some((decl, nested)) = unit.dynamic.get(*at as usize) else {
                continue;
            };
            let nested_name = String::from_utf8_lossy(&decl.name.0).into_owned();
            let _ = write!(dump, "\ndynamic def: {i}, function name: {nested_name}\n");
            let nested_args = decl.parameters.iter().filter(|p| !p.variadic).count();
            dump.push_str(&nested.dump_lines(&nested_name, nested_args, &frameless));
        }
        dump.into_bytes()
    }

    /// [`crate::tier::function_named`] and [`crate::tier::method_named`].
    pub(crate) fn code_named(&self, class: Option<&[u8]>, name: &[u8]) -> Named {
        let function = match class {
            Some(class) => {
                let class = class.strip_prefix(b"\\").unwrap_or(class);
                let Some(class) = self.class_named(&class.to_ascii_lowercase()) else {
                    return Named::NoClass;
                };
                self.method_named(class, &name.to_ascii_lowercase())
            }
            None => self.function_named(name),
        };
        match function.and_then(|function| self.code_view(function)) {
            Some(code) => Named::Code(code),
            None => Named::NoFunction,
        }
    }

    /// A class as the face shows it.
    fn class_face(&self, id: ClassId) -> Option<Class> {
        let decl = self.class_decl(id)?;
        let user = usize::try_from(id.0).is_ok_and(|at| at >= self.shared.classes.len());
        let shape = match decl.kind {
            ClassKind::Interface => Shape::Interface,
            _ if decl.flags.is_abstract => Shape::AbstractClass,
            _ => Shape::Class,
        };
        let methods = decl
            .methods
            .iter()
            .filter_map(|method| {
                let function =
                    self.method_named(id, &method.function.name.0.to_ascii_lowercase())?;
                self.code_view(function)
            })
            .collect();
        let unlinked_parent = decl
            .parent
            .as_ref()
            .filter(|parent| self.class_named(&parent.0.to_ascii_lowercase()).is_none())
            .map(|parent| parent.0.to_vec());
        Some(Class {
            name: self.class_name(id).into_bytes(),
            user,
            shape,
            file: decl.file.0.to_vec(),
            lines: (decl.line, decl.end_line),
            methods,
            unlinked_parent,
        })
    }

    /// [`crate::tier::class_named`].
    pub(crate) fn class_view(&self, name: &[u8]) -> Option<Class> {
        let name = name.strip_prefix(b"\\").unwrap_or(name);
        let id = self.class_named(&name.to_ascii_lowercase())?;
        self.class_face(id)
    }

    /// [`crate::tier::user_classes`].
    pub(crate) fn user_class_views(&self) -> Vec<Class> {
        self.declared_classes()
            .into_iter()
            .filter_map(|id| self.class_face(id))
            .filter(|class| class.user)
            .collect()
    }

    /// [`crate::tier::user_functions`].
    pub(crate) fn user_function_views(&self) -> Vec<Code> {
        (0..self.user_functions.len())
            .filter_map(|at| {
                let user = self.user_functions.get(at)?;
                if user.member.is_some() || !matches!(user.body, exec::Body::Function(_)) {
                    return None;
                }
                self.code_view(self.user_function_id(at)?)
            })
            .collect()
    }

    /// [`crate::tier::frame_code`].
    pub(crate) fn frame_code_view(&self, frame: FrameRef) -> Option<Code> {
        let state = self.vm.at(frame)?;
        if let Some(function) = state.function {
            return self.code_view(function);
        }
        let body = exec::body_of(&state.unit, state.body)?;
        Some(Code {
            dump: self.dump_with_dynamic(&state.unit, body, "$_main", 0),
            name: Vec::new(),
            class: None,
            user: true,
            file: body.filename.0.to_vec(),
            lines: (body.line_start, body.line_end),
            ops: ops_of(body),
            params: 0,
        })
    }

    /// [`crate::tier::table_sizes`].
    pub(crate) fn table_sizes(&self) -> [usize; 4] {
        let functions = self.shared.functions.len()
            + (0..self.user_functions.len())
                .filter(|at| {
                    self.user_functions.get(*at).is_some_and(|user| {
                        user.member.is_none() && matches!(user.body, exec::Body::Function(_))
                    })
                })
                .count();
        [
            self.shared.classes.len() + self.declared.len(),
            functions,
            self.constant_table().len(),
            self.included_files().len(),
        ]
    }

    // ------------------------------------------------ the uncaught

    /// How the program is served.
    pub(crate) fn primary(&self) -> crate::engine::Primary {
        self.description.primary
    }

    /// `zend_try_exception_handler`, then what is left kept for the host
    /// as phpdbg's `phpdbg_handle_exception` reads it
    /// (`Z/sapi/phpdbg/phpdbg_prompt.c:688`): its class, `file` and
    /// `line`, and what `__toString` answered, the status 255.
    pub(crate) fn keep_uncaught(&mut self) {
        self.user_exception_handler();
        let Some(throwable) = self.take_pending() else {
            self.raised = None;
            return;
        };
        let class = self.class_name(throwable.class()).into_bytes();
        let text = match crate::api::call::call_method(self, &throwable, b"__tostring", &[]) {
            Ok(returned) => {
                let text = returned
                    .deref()
                    .as_str_bytes()
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                self.release(returned);
                text
            }
            Err(thrown) => {
                if let Some(inner) = thrown.exception() {
                    self.release(inner.as_value());
                }
                self.raised = None;
                Vec::new()
            }
        };
        self.kept_uncaught = Some(crate::engine::Uncaught {
            file: self.throwable_prop_bytes(throwable, b"file"),
            line: self.throwable_prop_int(throwable, b"line"),
            class,
            text,
        });
        self.release(throwable.as_value());
        self.exit_status = 255;
    }

    /// What [`Request::keep_uncaught`] kept, taken.
    pub(crate) fn take_kept_uncaught(&mut self) -> Option<crate::engine::Uncaught> {
        self.kept_uncaught.take()
    }

    // ------------------------------------------------ values

    /// A symbol's value where it stands, a view.
    fn symbol_read(&mut self, table: super::TableId, symbol: super::Symbol) -> Value {
        match symbol {
            super::Symbol::Value(value) => value,
            super::Symbol::Held(cell) => cell.get(),
            super::Symbol::Indirect(frame, slot) => self.attached(table, frame, slot),
        }
    }

    /// [`crate::tier::variable`]'s read: the name in the frame's table
    /// where it has one, else its compiled variable; a view.
    pub(crate) fn frame_variable(&mut self, frame: FrameRef, name: &[u8]) -> Option<Value> {
        if let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) {
            let symbol = self
                .tables
                .get(table.0 as usize)?
                .entries
                .iter()
                .find(|(held, _)| held.as_slice() == name)
                .map(|(_, symbol)| *symbol)?;
            return Some(self.symbol_read(table, symbol));
        }
        let at = self
            .frame_var_names(frame)
            .iter()
            .position(|held| held.as_slice() == name)?;
        Some(self.slot(frame, u32::try_from(at).ok()?))
    }

    /// The rows of `frame`'s rebuilt table that are no superglobal, each
    /// a view.
    pub(crate) fn frame_variables(&mut self, frame: FrameRef) -> Vec<(Vec<u8>, Value)> {
        let table = self.rebuild_symbol_table(frame);
        let entries = self
            .tables
            .get(table.0 as usize)
            .map(|table| table.entries.clone())
            .unwrap_or_default();
        entries
            .into_iter()
            .filter(|(name, _)| php_unit::AutoGlobal::of(name).is_none())
            .map(|(name, symbol)| {
                let value = self.symbol_read(table, symbol);
                (name, value)
            })
            .collect()
    }

    /// The superglobals of the global table, each armed first, in the
    /// table's order; views.
    pub(crate) fn superglobal_rows(&mut self) -> Vec<(Vec<u8>, Value)> {
        use php_unit::AutoGlobal;
        for global in [
            AutoGlobal::Get,
            AutoGlobal::Post,
            AutoGlobal::Cookie,
            AutoGlobal::Server,
            AutoGlobal::Env,
            AutoGlobal::Request,
            AutoGlobal::Files,
        ] {
            let _ = self.arm(global);
        }
        let entries = self.globals().clone();
        entries
            .into_iter()
            .filter(|(name, _)| AutoGlobal::of(name).is_some())
            .map(|(name, symbol)| {
                let value = self.symbol_read(super::TableId::GLOBAL, symbol);
                (name, value)
            })
            .collect()
    }

    /// The literals of the op array `frame` serves; views.
    pub(crate) fn frame_literals(&self, frame: FrameRef) -> Vec<Value> {
        self.vm
            .serving(frame)
            .and_then(|serving| {
                serving.code().map(|code| {
                    code.literals
                        .iter()
                        .map(|zval| Value::of_zval(*zval))
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// [`crate::tier::backtrace`]: the rows `debug_backtrace()` would
    /// build from the innermost frame, read back into the face's shape.
    pub(crate) fn backtrace_rows(&mut self, limit: i64) -> Vec<crate::tier::TraceRow> {
        let Ok(trace) = self.fetch_debug_backtrace(false, 0, limit) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (_, row) in trace.snapshot() {
            let Some(row) = row.as_array().copied() else {
                continue;
            };
            let field = |name: &[u8]| {
                row.snapshot()
                    .into_iter()
                    .find(|(key, _)| matches!(key, crate::api::values::Key::Str(s) if s.as_bytes() == name))
                    .map(|(_, value)| value)
            };
            let bytes =
                |value: Option<Value>| value.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
            let function = bytes(field(b"function")).unwrap_or_default();
            let class = bytes(field(b"class")).zip(bytes(field(b"type")));
            let file = bytes(field(b"file"));
            let line = field(b"line").and_then(|v| v.as_int());
            let args = field(b"args")
                .and_then(|v| v.as_array().copied())
                .map(|args| {
                    args.snapshot()
                        .into_iter()
                        .map(|(_, v)| v)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let args = args
                .into_iter()
                .map(|value| crate::tier::values_shown(self, value))
                .collect();
            rows.push(crate::tier::TraceRow {
                function,
                class,
                file,
                line,
                args,
            });
        }
        self.release(Value::array(trace));
        rows
    }

    /// `PG(last_error_message)`, `last_error_file` and `last_error_lineno`.
    pub(crate) fn last_error_row(&self) -> Option<(Vec<u8>, Vec<u8>, u32)> {
        self.last_error.map(|last| {
            (
                last.message.as_bytes().to_vec(),
                last.file.as_bytes().to_vec(),
                last.line,
            )
        })
    }
}

/// An op array's oplines as the tier face shows them, each opcode with
/// its line.
fn ops_of(body: &php_unit::OpArray) -> Vec<(php_unit::Opcode, u32)> {
    body.oplines
        .iter()
        .map(|op| (op.opcode, op.lineno))
        .collect()
}
