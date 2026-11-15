//! What the debugger reads of a frame, an op array and the linked
//! tables between the steps it drives: read through the native
//! face as an extension reads them, a frame a step handed out turned
//! into a [`FrameView`] by [`tier::frame`], and copied here into the
//! shapes phpdbg's `print`, `list`, `info`, its breakpoints and its seeks
//! read: where a frame stands ([`Position`]), what it serves
//! ([`Serves`]), and a function or a class as its table holds it
//! ([`Code`], [`Class`]). Each is a copy taken when asked, so nothing
//! here holds a borrow of the request across the next step.

use php_ext::call::{self, Function};
use php_ext::decl::ClassKind;

use php_ext::request::{FrameView, MethodRow, OpArrayView, Opcode, Raised, Request};
use php_ext::values::{ClassId, Value};
use php_sapi::tier::{self, FrameRef};

use crate::opcodes;

/// Where a frame stands: the file and line of the opline the loop
/// dispatches next, its index in the op array, its opcode, and the lines
/// the op array spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub file: Vec<u8>,
    pub line: u32,
    pub opline: u32,
    pub opcode: Opcode,
    /// `op_array->line_start` and `line_end`.
    pub lines: (u32, u32),
    /// `op_array->last`, how many oplines the op array holds.
    pub oplines: u32,
    /// The first opline past the `RECV*` oplines, `num_args` and one more
    /// for a variadic: where a function's body begins.
    pub body: u32,
}

/// What a frame serves: a function or a method by the name it was
/// declared under, with the class it was declared in, or the code of a
/// file or an `eval`, which is no function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serves {
    /// A file's or an `eval`'s own code, php's `op_array->function_name`
    /// being `NULL`.
    Code,
    /// A function, a closure or a method: `function_name`, and `scope`'s
    /// name where it has one.
    Function {
        name: Vec<u8>,
        class: Option<Vec<u8>>,
    },
}

/// A function or a method as its table holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    /// `function_name`, as declared.
    pub name: Vec<u8>,
    /// `scope`'s name, for a method.
    pub class: Option<Vec<u8>>,
    /// Whether it is user code, `ZEND_USER_FUNCTION`.
    pub user: bool,
    /// Where it was declared, and the lines its op array spans.
    pub file: Vec<u8>,
    pub lines: (u32, u32),
    /// `op_array->opcodes`, each opcode with its line; `op_array->last`
    /// is its length. Empty for a native.
    pub ops: Vec<(Opcode, u32)>,
    /// `num_args`, and one more for a variadic: how many `RECV*` oplines
    /// open the op array.
    pub params: u32,
    /// `last_var`: how many compiled variables the op array names; zero
    /// for a native.
    pub vars: u32,
    /// The op array as phpdbg's `print` writes it, and each function
    /// its body declares after it (`dynamic_func_defs`), as
    /// `phpdbg_print_function_helper` writes them; empty for a native.
    pub dump: Vec<u8>,
}

/// What a name found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    Code(Code),
    /// The class named is none the request knows.
    NoClass,
    /// The function or the method named is none it knows.
    NoFunction,
}

/// What a class is, as phpdbg's `print class` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Interface,
    AbstractClass,
    Class,
}

/// A class as its table holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub name: Vec<u8>,
    pub user: bool,
    pub shape: Shape,
    pub file: Vec<u8>,
    pub lines: (u32, u32),
    /// Its own methods, in the order it declares them.
    pub methods: Vec<Code>,
    /// The parent it names that no class of that name was declared for
    /// yet, where it names one: phpdbg's "not yet linked".
    pub unlinked_parent: Option<Vec<u8>>,
}

/// The oplines of `code`, each opcode with its line.
pub fn ops_of(code: &OpArrayView<'_>) -> Vec<(Opcode, u32)> {
    code.oplines().map(|op| (op.opcode(), op.line())).collect()
}

/// How many `RECV*` oplines open `code`: `num_args`, and one more for a
/// variadic, since php's compiler emits one per parameter before the
/// body.
fn received(code: &OpArrayView<'_>) -> u32 {
    let count = code
        .oplines()
        .take_while(|op| opcodes::is(op.opcode(), opcodes::RECEIVES))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// `last_var`: how many compiled variables `code` names, the first
/// index [`OpArrayView::variable`] answers none for.
fn compiled_variables(code: &OpArrayView<'_>) -> u32 {
    (0..u32::MAX)
        .find(|cv| code.variable(*cv).is_none())
        .unwrap_or(u32::MAX)
}

/// Where `frame` stands; none for a frame that is not standing.
#[must_use]
pub fn position(req: &Request, frame: FrameRef) -> Option<Position> {
    let view = tier::frame(req, frame)?;
    let code = view.op_array()?;
    let at = view.opline();
    let op = code.opline(at)?;
    Some(Position {
        file: code.filename().to_vec(),
        line: op.line(),
        opline: at.0,
        opcode: op.opcode(),
        lines: code.lines(),
        oplines: u32::try_from(code.len()).unwrap_or(u32::MAX),
        body: received(&code),
    })
}

/// Each opline of the op array `frame` serves, its opcode and its line,
/// in order: what phpdbg's `until`, `next`, `finish` and `leave` read to
/// know where a seek ends.
#[must_use]
pub fn oplines(req: &Request, frame: FrameRef) -> Vec<(Opcode, u32)> {
    tier::frame(req, frame)
        .and_then(|view| view.op_array())
        .map(|code| ops_of(&code))
        .unwrap_or_default()
}

/// `op_array->vars[0..num_args]`: the names of the parameters before a
/// variadic one of the function `frame` serves, as php's
/// `phpdbg_compile_stackframe` reads them off the op array, so no
/// symbol table is built for the read; empty for a file's code.
#[must_use]
pub fn parameter_names(req: &Request, frame: FrameRef) -> Vec<Vec<u8>> {
    let Some(code) = tier::frame(req, frame)
        .filter(|view| view.function().is_some())
        .and_then(|view| view.op_array())
    else {
        return Vec::new();
    };
    let declared = code
        .oplines()
        .take_while(|op| opcodes::is(op.opcode(), opcodes::DECLARED))
        .count();
    (0..declared)
        .map_while(|cv| code.variable(u32::try_from(cv).ok()?))
        .map(<[u8]>::to_vec)
        .collect()
}

/// The name a function was declared under.
fn declared_name(req: &Request, function: Function) -> Vec<u8> {
    req.symbols()
        .function_decl(function)
        .map_or_else(|| b"native".to_vec(), |decl| decl.name.0.to_vec())
}

/// What `view` serves.
fn serves_view(req: &Request, view: &FrameView<'_>) -> Serves {
    let Some(function) = view.function() else {
        return Serves::Code;
    };
    Serves::Function {
        name: declared_name(req, function),
        class: view
            .function_scope()
            .map(|class| req.symbols().class_name(class).to_vec()),
    }
}

/// What `frame` serves; a file's code for a frame that is not standing.
#[must_use]
pub fn serves(req: &Request, frame: FrameRef) -> Serves {
    tier::frame(req, frame).map_or(Serves::Code, |view| serves_view(req, &view))
}

/// Whether `after` was entered from `before`: the user frame
/// `EX(prev_execute_data)` reaches from `after` is `before`, read through
/// the native face.
#[must_use]
pub fn entered_from(req: &Request, after: FrameRef, before: FrameRef) -> bool {
    let caller = tier::frame(req, after)
        .and_then(|view| view.caller())
        .map(|view| view.id());
    caller.is_some() && caller == tier::frame(req, before).map(|view| view.id())
}

/// `function` as its table holds it, under the class `class` names
/// where it is a method.
fn code_of(req: &Request, function: Function, class: Option<ClassId>) -> Option<Code> {
    let decl = req.symbols().function_decl(function)?;
    let name = decl.name.0.to_vec();
    let params = u32::try_from(decl.parameters.len()).unwrap_or(u32::MAX);
    let args = decl.parameters.iter().filter(|p| !p.variadic).count();
    let class = class.map(|class| req.symbols().class_name(class).to_vec());
    let Some(code) = req.frames().op_array(function) else {
        return Some(Code {
            name,
            class,
            user: false,
            file: Vec::new(),
            lines: (0, 0),
            ops: Vec::new(),
            params,
            vars: 0,
            dump: Vec::new(),
        });
    };
    let shown = match &class {
        Some(class) => [&class[..], b"::", &name].concat(),
        None => name.clone(),
    };
    Some(Code {
        dump: dump_with_dynamic(req, code, &shown, args),
        name,
        class,
        user: true,
        file: code.filename().to_vec(),
        lines: code.lines(),
        ops: ops_of(&code),
        params,
        vars: compiled_variables(&code),
    })
}

/// `phpdbg_print_function_helper`: the op array, then each function its
/// body declares, under `dynamic def: n, function name: name`.
fn dump_with_dynamic(req: &Request, code: OpArrayView<'_>, name: &[u8], args: usize) -> Vec<u8> {
    let mut dump = req.frames().dump_op_array(code, name, args);
    for (at, (decl, nested)) in code.dynamic_functions().enumerate() {
        dump.extend_from_slice(format!("\ndynamic def: {at}, function name: ").as_bytes());
        dump.extend_from_slice(&decl.name.0);
        dump.push(b'\n');
        let nested_args = decl.parameters.iter().filter(|p| !p.variadic).count();
        dump.extend(
            req.frames()
                .dump_op_array(nested, &decl.name.0, nested_args),
        );
    }
    dump
}

/// The op array `frame` serves; for a file's code, `name` is empty.
#[must_use]
pub fn frame_code(req: &Request, frame: FrameRef) -> Option<Code> {
    let view = tier::frame(req, frame)?;
    if let Some(function) = view.function() {
        return code_of(req, function, view.function_scope());
    }
    let code = view.op_array()?;
    Some(Code {
        dump: dump_with_dynamic(req, code, b"$_main", 0),
        name: Vec::new(),
        class: None,
        user: true,
        file: code.filename().to_vec(),
        lines: code.lines(),
        ops: ops_of(&code),
        params: 0,
        vars: compiled_variables(&code),
    })
}

/// The class under `name`, case-insensitive, a leading `\` dropped.
fn class_id(req: &mut Request, name: &[u8]) -> Option<ClassId> {
    call::lookup_class(req, &name.to_ascii_lowercase())
        .ok()
        .flatten()
}

/// The method `name` of `class`, under the class that declared it.
fn method_code(req: &Request, class: ClassId, name: &[u8]) -> Option<Code> {
    let name = name.to_ascii_lowercase();
    let function = req.symbols().method_named(class, &name)?;
    let scope = req.symbols().method_row(class, &name).map(MethodRow::scope);
    code_of(req, function, scope)
}

/// `EG(function_table)`'s entry under `name`, case-insensitive.
#[must_use]
pub fn function_named(req: &Request, name: &[u8]) -> Named {
    call::lookup_function(req, name)
        .and_then(|function| code_of(req, function, None))
        .map_or(Named::NoFunction, Named::Code)
}

/// `phpdbg_get_function` (`Z/sapi/phpdbg/phpdbg_utils.c:168-195`) and
/// the names `phpdbg_dump_prototype` reads off what it finds: the
/// function a backtrace row names, `name` in `class`'s function table
/// where the row names a class and in `EG(function_table)` where it does
/// not, both case-insensitive, and the names of its first `num_args`
/// parameters, a native's from its `arginfo`; none where no table holds
/// the name, as none holds a closure's. php's `?=`, for a function with
/// no `arginfo`, and its `[`, for a variadic parameter among the first
/// `num_args`, are never written: every declaration names its
/// parameters, and `num_args` counts no variadic one
/// (`Z/Zend/zend_compile.c:8005`, `Z/Zend/zend_API.c:3045`).
pub fn declared_parameters(req: &mut Request, class: Option<&[u8]>, name: &[u8]) -> Vec<Vec<u8>> {
    let decl = match class {
        Some(class) => class_id(req, class)
            .and_then(|class| req.symbols().method_row(class, name))
            .and_then(|row| req.symbols().row_method(row))
            .map(|method| &method.function),
        None => req.symbols().functions().into_iter().find_map(|function| {
            req.symbols()
                .function_decl(function)
                .filter(|decl| decl.name.0.eq_ignore_ascii_case(name))
        }),
    };
    decl.map(|decl| {
        decl.parameters
            .iter()
            .filter(|parameter| !parameter.variadic)
            .map(|parameter| parameter.name.0.to_vec())
            .collect()
    })
    .unwrap_or_default()
}

/// The method `name` of the class `class`, both case-insensitive, a
/// leading `\` of the class's name dropped.
pub fn method_named(req: &mut Request, class: &[u8], name: &[u8]) -> Named {
    let Some(class) = class_id(req, class) else {
        return Named::NoClass;
    };
    method_code(req, class, name).map_or(Named::NoFunction, Named::Code)
}

/// A class as its table holds it.
fn class_of_id(req: &mut Request, id: ClassId) -> Option<Class> {
    let decl = req.symbols().class_decl(id)?;
    let shape = match decl.kind {
        ClassKind::Interface => Shape::Interface,
        _ if decl.flags.is_abstract => Shape::AbstractClass,
        _ => Shape::Class,
    };
    let file = decl.file.0.to_vec();
    let lines = (decl.line, decl.end_line);
    let parent = decl.parent.as_ref().map(|parent| parent.0.to_vec());
    let declared: Vec<Vec<u8>> = (decl.methods.iter())
        .map(|method| method.function.name.0.to_vec())
        .collect();
    let methods = declared
        .iter()
        .filter_map(|name| method_code(req, id, name))
        .collect();
    let unlinked_parent = parent.filter(|parent| class_id(req, parent).is_none());
    Some(Class {
        name: req.symbols().class_name(id).to_vec(),
        user: req.extensions().class_extension(id).is_none(),
        shape,
        file,
        lines,
        methods,
        unlinked_parent,
    })
}

/// The class under `name`, case-insensitive, a leading `\` dropped.
pub fn class_named(req: &mut Request, name: &[u8]) -> Option<Class> {
    let id = class_id(req, name)?;
    class_of_id(req, id)
}

/// The user functions of the function table in its order, no method and
/// no closure among them.
#[must_use]
pub fn user_functions(req: &Request) -> Vec<Code> {
    req.symbols()
        .functions()
        .into_iter()
        .filter(|function| req.frames().op_array(*function).is_some())
        .filter_map(|function| code_of(req, function, None))
        .collect()
}

/// The user classes the request has declared, in the order it declared
/// them.
pub fn user_classes(req: &mut Request) -> Vec<Class> {
    req.symbols()
        .declared_classes()
        .into_iter()
        .filter(|class| req.extensions().class_extension(*class).is_none())
        .collect::<Vec<_>>()
        .into_iter()
        .filter_map(|class| class_of_id(req, class))
        .collect()
}

/// How many entries each of the tables `clean` counts holds,
/// `zend_hash_num_elements` of each: `EG(class_table)`, every class's
/// and alias's key but the iterator wrapper's, which php does not add;
/// `EG(function_table)`; `EG(zend_constants)`; and the included files.
#[must_use]
pub fn table_sizes(req: &Request) -> [usize; 4] {
    let classes = req
        .symbols()
        .class_keys()
        .into_iter()
        .filter(|(class, alias)| {
            let key = match alias {
                Some(alias) => alias.first().copied(),
                None => req
                    .symbols()
                    .class_decl(*class)
                    .and_then(|d| d.name.0.first().copied()),
            };
            key != Some(0)
        })
        .count();
    [
        classes,
        req.symbols().functions().len(),
        req.symbols().constant_table().len(),
        req.code().included_files().len(),
    ]
}

/// What stands pending on the request where a tier stops for it: a
/// throwable, which phpdbg reads its class, `file`, `line` and
/// `message` off (`phpdbg_prompt.c:1690-1696`), or the graceful exit a
/// destroyed fiber is resumed with, which php makes an object of class
/// `GracefulExit` with none of them and this engine makes no object.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Thrown {
    /// The throwable, a view.
    Object(Value),
    GracefulExit,
}

/// What stands pending on the request; none while nothing is, or an
/// exit is what stands.
#[must_use]
pub fn pending(req: &Request) -> Option<Thrown> {
    match req.diagnostics().raised()? {
        Raised::Throwable(object) => Some(Thrown::Object(object.as_value())),
        Raised::GracefulExit => Some(Thrown::GracefulExit),
        Raised::UnwindExit => None,
    }
}

/// Whether `php_stream_open_for_zend_ex` refuses a file the request
/// opens to compile: while a throwable or an exit stands pending, as
/// what the init files leave (an `exit`, or the unwind exit of an `ev`
/// that met the memory limit) has `phpdbg_compile` answer `Could not
/// open file`.
#[must_use]
pub fn open_refused(req: &Request) -> bool {
    pending(req).is_some() || exiting(req)
}

/// Whether `exit`'s unwind exit is what the request is unwinding, or a
/// fatal is leaving: nothing a tier stops at.
#[must_use]
pub fn exiting(req: &Request) -> bool {
    match req.diagnostics().raised() {
        Some(Raised::UnwindExit) => true,
        Some(Raised::Throwable(_) | Raised::GracefulExit) => false,
        None => req.diagnostics().bailing_out(),
    }
}

/// The name of an object's class as a message prints it, an anonymous
/// class's as `class@anonymous`; none for a value that is no object.
#[must_use]
pub fn class_of(req: &Request, value: Value) -> Option<Vec<u8>> {
    value
        .as_object()
        .map(|object| req.symbols().class_name(object.class()).to_vec())
}

/// `PG(last_error_message)`, `last_error_file` and `last_error_lineno`,
/// read off the row `error_get_last()` builds, and the row given up.
pub fn last_error(req: &mut Request) -> Option<(Vec<u8>, Vec<u8>, u32)> {
    let row = req.diagnostics_mut().last_error().ok()??;
    let array = *row;
    let bytes = |name: &[u8]| {
        array
            .find(name)
            .and_then(|value| value.deref().as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default()
    };
    let read = (
        bytes(b"message"),
        bytes(b"file"),
        array
            .find(b"line")
            .and_then(|value| value.deref().as_int())
            .and_then(|line| u32::try_from(line).ok())
            .unwrap_or(0),
    );
    row.give_up(req);
    Some(read)
}
