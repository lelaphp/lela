//! The compiler (design §4.1, §6.1): one pass from the typed tree to a
//! [`Unit`], after `zend_compile.c`, with the constant-expression tree,
//! early binding, compile-time fatals at the site that owns each
//! construct, and safepoint numbering with its invariant asserted. What is
//! here is the first slice of Lane B (design §11): the statements the
//! engine's loop serves, `echo` and a call of a named function, emitted as
//! php emits them, with the entry safepoint and the numbered return. The
//! tree it walks is `php_syntax`'s, [`Parsed::tree`] (design §4.9):
//! `emit` reads every statement, every expression and every
//! declaration from its own node, and no second parser stands between
//! the source and the unit (#595). What it cannot read is refused as a
//! [`CompileError`].
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod emit;
mod live;
pub mod multibyte;
mod special;
mod stack;

pub use stack::StackLimit;

/// The token stream and the tree (design §4.9), handed up for
/// `php_engine::api::syntax` and nothing of the compiler's own: the
/// lexer, its stream and php's names for its kinds, which
/// `token_get_all` spells, and the parser with the tree's shape and its
/// diagnostics, which `token_get_all` under `TOKEN_PARSE` reads as php's
/// `tokenize_parse` reads the parser's events. It passes through here
/// for the same reason [`compile_source`] does: the engine's edges reach
/// this crate and not the syntax crate (design §4.2).
pub mod syntax {
    pub use php_syntax::tree::{Element, TokenId, Tree};
    pub use php_syntax::{
        Arena, Colours, Diagnostic, FileId, Level, LexOptions, Lexed, Options, Parsed, Position,
        ScannedToken, SyntaxKind, Token, highlight, html_puts, lex, parse, scanned, strip,
    };
}

use std::borrow::Cow;

use php_heap::{Fatal, FatalReason, Intern};
use php_link::{Exact, Folded, Indexed};
use php_syntax::{Arena, Parsed};
use php_unit::decl::{ClassDecl, ConstDecl, FunctionDecl, Visibility};
use php_unit::{Diagnostic, Level, Unit};

/// What the compiler is told beside the source (design §4.1).
#[allow(clippy::struct_excessive_bools)] // php's compile-time ini entries and compiler options, each one fact
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// The file name the unit and its errors carry, borrowed: the unit
    /// takes its own copy where an op array or a declaration records it.
    pub filename: &'a [u8],
    /// Whether the source is a file or the string an `eval` hands over.
    pub kind: SourceKind,
    /// The working directory of the request the unit compiles for,
    /// php's virtual one under ZTS (`VCWD_GETCWD`), which is what
    /// `__DIR__` folds to where `zend_dirname` over `filename` answers
    /// `.` (`zend_try_ct_eval_magic_const`). The host that described the
    /// request is what knows it; the compiler reads no directory of the
    /// process's own.
    pub cwd: &'a [u8],
    /// Whether the caller's `declare(strict_types=1)` is in force.
    pub strict_types: bool,
    /// php's `precision` ini entry as it stands while the file compiles,
    /// which `_convert_to_string` renders a double constant at when the
    /// compiler folds a `.` over it (`zend_double_to_str`, which reads
    /// `EG(precision)`); php's default is 14.
    pub precision: i32,
    /// php's `short_open_tag` ini entry as it stands while the file
    /// compiles, `CG(short_tags)`: whether `<?` opens code. The entry is
    /// an `OnUpdateBool` onto php's compiler state (`Z/main/main.c:820`)
    /// and php's scanner reads it at each `<?` it meets
    /// (`Z/Zend/zend_language_scanner.l:2331`): while it is off the tag
    /// falls to `inline_char_handler`, which takes it and what follows
    /// into the `T_INLINE_HTML` around it, so the file echoes its own
    /// text where the code inside the tag would have been. `<?php` and `<?=` open code
    /// whatever the entry says. php's built-in default is on
    /// (`DEFAULT_SHORT_OPEN_TAG`), which is this one's; the entry is
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so the value the request started
    /// under is the value every file it compiles is read under.
    pub short_open_tag: bool,
    /// Whether php's `zend.assertions` stands at or above zero while the
    /// file compiles, `EG(assertions) >= 0`, which is all
    /// `zend_compile_assert` reads: a call of `assert()` is compiled
    /// behind an `ASSERT_CHECK` where it does, and is the constant `true`
    /// with nothing of its arguments compiled where the entry is `-1`.
    /// The entry moves between `-1` and the rest only at startup
    /// (`OnUpdateAssertions`), so what the request started under is what
    /// every file it compiles reads; php's default is `1`.
    pub assertions: bool,
    /// `CG(skip_shebang)`, which php's CLI, CGI and built-in server set
    /// once a request has started. An `eval`'s string begins after an
    /// open tag whatever it says.
    pub shebang: Shebang,
    /// The persistent constants the engine's extensions declared
    /// (`CONST_PERSISTENT`), which `zend_try_ct_eval_const` substitutes
    /// where they are written; a name not here is fetched as the program serves.
    pub persistent: Indexed<'a, ConstDecl, Exact>,
    /// The natives the engine's bundle declared, php's `CG(function_table)`
    /// as `zend_compile_call` reads it: what `zend_try_compile_special_func`
    /// requires a name to be one of before it serves the call itself
    /// (`fbc->type == ZEND_INTERNAL_FUNCTION`), and what a name must be
    /// in for `INIT_FCALL`; a name the bundle has not is the call by name,
    /// `INIT_FCALL_BY_NAME`, resolved as the program serves. Every
    /// function here is a native; a user function is declared by nothing
    /// this compiler reads yet.
    pub functions: Indexed<'a, FunctionDecl, Folded>,
    /// `zend_flf_functions` over [`Options::functions`], what a
    /// `FRAMELESS_ICALL_*`'s `extended_value` indexes
    /// (`find_frameless_function_offset`): the engine computes it once,
    /// with [`php_unit::frameless_table`], as it registers the natives.
    pub frameless: &'a [php_unit::FramelessEntry],
    /// The classes the engine's bundle declared, php's `CG(class_table)`
    /// as `zend_compile_static_call` reads it: a class the table has
    /// with a public method of the name written is a callee the
    /// compiler knows (`zend_get_compatible_func_or_null`), whose
    /// arguments are sent as a known callee's are; any other class or
    /// method is resolved as the program serves. Every class here is a
    /// native one; a class the file itself declares stands beside them.
    pub classes: Indexed<'a, ClassDecl, Folded>,
    /// The classes the request the source compiles for has declared by
    /// then, [`RequestClasses`].
    pub request_classes: RequestClasses<'a>,
    /// The stack limit of the thread the compiler runs on, php's
    /// `EG(stack_limit)`, which the engine works out once per request
    /// (`zend_call_stack_init`) and the compiler's recursion is held to
    /// (`zend_check_stack_limit`); unchecked where a caller says nothing.
    pub stack: StackLimit,
    /// `CG(rtd_key_counter)` as the source starts compiling: how many
    /// runtime definition keys and anonymous class names were made before
    /// it, which the `$` of each one this source makes counts on from. A
    /// file compiled for the cache starts from none; an `eval`'s string
    /// continues the request's count, so two `eval`s of one string
    /// declare two anonymous classes, as php's one `CG(rtd_key_counter)`
    /// makes them.
    pub keys_before: u32,
    /// `CG(multibyte)`, `zend.multibyte` as it stands while the file
    /// compiles, with the encoding functions the engine bound and what
    /// the scanner made of the file as it opened it
    /// ([`multibyte::Multibyte`]); `None` where the entry is off.
    pub multibyte: Option<multibyte::Multibyte<'a>>,
    /// Which of php's execution hooks the host replaced, [`Hooked`],
    /// which `zend_get_call_op` reads; none for an engine with no tier.
    pub hooked: Hooked,
    /// `CG(compiler_options) & ZEND_COMPILE_EXTENDED_INFO`, which a
    /// host's `-e` sets for every file and `eval` the engine compiles
    /// (#1780): an `EXT_STMT` before each statement, and an
    /// `EXT_FCALL_BEGIN` and `EXT_FCALL_END` around each call
    /// `zend_compile_call_common` makes and each `include` or `eval`,
    /// where `zend_do_extended_stmt`, `zend_do_extended_fcall_begin` and
    /// `zend_do_extended_fcall_end` emit them. What a host evaluates as
    /// a string of its own, `-r`'s code, is compiled without it, as
    /// `zend_eval_stringl` compiles under `ZEND_COMPILE_DEFAULT_FOR_EVAL`.
    pub extended_info: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Options {
            filename: &[],
            kind: SourceKind::File,
            cwd: &[],
            strict_types: false,
            precision: 14,
            short_open_tag: true,
            assertions: true,
            shebang: Shebang::Read,
            persistent: Indexed::default(),
            functions: Indexed::default(),
            frameless: &[],
            classes: Indexed::default(),
            request_classes: RequestClasses::default(),
            stack: StackLimit::UNCHECKED,
            keys_before: 0,
            multibyte: None,
            hooked: Hooked::NONE,
            extended_info: false,
        }
    }
}

/// Which of php's two execution hooks the host replaced, as
/// `zend_get_call_op` (`Z/Zend/zend_compile.c:3919-3949`) reads them
/// while a call compiles: whether `zend_execute_ex` is no longer
/// `execute_ex`, the hook phpdbg and xdebug replace, and whether
/// `zend_execute_internal` is set. A call the hook would not see is
/// never given the opcode that passes it by: under the first no user
/// call is `DO_UCALL` and no call by name `DO_FCALL_BY_NAME`; under the
/// second no native call is `DO_ICALL` or frameless, and no call by
/// name `DO_FCALL_BY_NAME`. Every such call is `DO_FCALL`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hooked {
    /// `zend_execute_ex != execute_ex`: the hook every user op array
    /// runs through.
    pub user: bool,
    /// `zend_execute_internal != NULL`: the hook every native call runs
    /// through.
    pub natives: bool,
}

impl Hooked {
    /// php's own loop with neither hook replaced.
    pub const NONE: Hooked = Hooked {
        user: false,
        natives: false,
    };
}

/// The names of the classes a request has declared, as the compiler
/// asks after one while a source compiles for it: php's
/// `CG(class_table)` is `EG(class_table)` then, so a top-level class
/// whose name the request already holds fails the `zend_hash_add_ptr`
/// that would bind it early (`Z/Zend/zend_compile.c:9381`), and is left
/// to its `DECLARE_CLASS_DELAYED`, which binds it as the unit loads where
/// the name is free by then and raises the redeclaration where it stands
/// where it is not (GH-8846). A parent is never looked for here: opcache
/// compiles with `ZEND_COMPILE_IGNORE_OTHER_FILES`, and a class of
/// another file is no parent an early bind takes. Asked by the lowered
/// name; a source compiled for no request holds none.
#[derive(Clone, Copy)]
pub struct RequestClasses<'a>(&'a dyn Fn(&[u8]) -> bool);

impl<'a> RequestClasses<'a> {
    /// The request's classes as `holds` answers for a lowered name.
    #[must_use]
    pub fn new(holds: &'a dyn Fn(&[u8]) -> bool) -> RequestClasses<'a> {
        RequestClasses(holds)
    }

    /// Whether the request has declared a class by the lowered `name`.
    #[must_use]
    pub fn holds(&self, name: &[u8]) -> bool {
        (self.0)(name)
    }
}

impl Default for RequestClasses<'_> {
    fn default() -> Self {
        RequestClasses(&|_| false)
    }
}

impl std::fmt::Debug for RequestClasses<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestClasses")
    }
}

/// What a file's `#!` first line is to the scanner, `CG(skip_shebang)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shebang {
    /// Off: the line is inline HTML like any other.
    #[default]
    Read,
    /// On: a file begins at `ZEND_COMPILE_POSITION_AT_SHEBANG`, its `#!`
    /// first line passed over (`Z/Zend/zend_language_scanner.l:570`).
    Skipped,
}

/// What the source is, the `type` php's `zend_compile` is called with
/// (`Z/Zend/zend_language_scanner.l:593`): what the scanner begins in
/// and what the main op array's implicit final return answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceKind {
    /// `compile_file`'s `ZEND_USER_FUNCTION`: a file, which begins in
    /// inline HTML and whose main op array ends in `RETURN int(1)`, the
    /// value an `include` of it answers where it returns nothing.
    #[default]
    File,
    /// `compile_string`'s `ZEND_EVAL_CODE`, the scanner begun where the
    /// position says, and its main op array ending in `RETURN null`, the
    /// value an `eval` of it answers where it returns nothing. An `eval`
    /// hands its string over at `ZEND_COMPILE_POSITION_AFTER_OPEN_TAG`
    /// (`Z/Zend/zend_execute.c:5354`), [`SourceKind::EVAL`]: code from
    /// the first byte, as though an open tag stood before it.
    Eval(php_syntax::Position),
}

impl SourceKind {
    /// The string an `eval` hands over.
    pub const EVAL: SourceKind = SourceKind::Eval(php_syntax::Position::AfterOpenTag);
}

/// A compile-time fatal: php's message, at php's line, at the level
/// php raises it, which is how the request ends. `E_PARSE` for the
/// parser's first `syntax error, …` where php's parser refused the file,
/// and for the compiler's own sentence where php's would have compiled it
/// and this compiler cannot read it yet; `E_COMPILE_ERROR` for what
/// php's compiler refuses after parsing, the `(unset)` cast and the
/// stack limit ([`StackLimit::check`]) among them.
///
/// `raised` is what `zend_error` raised before the fatal and php had
/// already rendered when it bailed out: the scanner's warning and
/// deprecation (design §6.1) up to the token the parser refused, and
/// for a fatal after the parse every one of them, then the compiler's
/// own; each in the order raised, for the engine to render before the
/// fatal as php's request shows them.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileError {
    pub level: Level,
    /// php's text, a constant one borrowed where it is one, so a
    /// refusal the compiler makes and drops, a fold that finds no
    /// constant among them, allocates nothing.
    pub message: Cow<'static, str>,
    pub line: u32,
    pub raised: Vec<Diagnostic>,
    /// Whether php throws the refusal as a `CompileError`,
    /// `zend_throw_exception(zend_ce_compile_error)` from the parser's
    /// modifier lists and two statements, rather than raising it as a
    /// fatal: a `try` around the `include` or the `eval` catches it, and
    /// uncaught it renders as the fatal at [`CompileError::level`].
    pub thrown: bool,
    /// Where php stood when it refused, which is where `zend_error`
    /// places it.
    pub stage: Stage,
    /// The functions and classes a source's compile had declared at its
    /// top level before it refused, none where it declared none.
    pub declared: Option<Box<Declared>>,
    /// How many runtime definition keys and anonymous class names the
    /// compile had made when it refused, whatever it declared: php's
    /// `zend_bailout` out of `zend_compile` leaves them counted in
    /// `CG(rtd_key_counter)`, so the request's next compile counts its
    /// own on from them ([`Options::keys_before`]). None for a refusal
    /// php makes before `zend_compile` runs, the parser's and one it
    /// throws.
    pub keys_made: u32,
}

/// What php's compile of a source had added to `CG(function_table)` and
/// `CG(class_table)` when it refused: `zend_compile_func_decl` adds each
/// function at the top level once its body has compiled
/// (`Z/Zend/zend_compile.c:8601-8606`) and `zend_compile_class_decl`
/// each class it binds early (`:9381`), and the fatal's `zend_bailout`
/// out of `zend_compile` takes none of them back, so they stand in the
/// request after the refusal, and a second compile of the file in the
/// request meets the first of them as `Cannot redeclare`.
///
/// `U` is the unit as the compiler made it, and as the engine publishes
/// it for the request that declares what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Declared<U = Unit> {
    /// The functions and the early-bound classes, each with its bodies,
    /// as a unit of their own: no main op array, and each in
    /// [`Unit::top_level`] in the order it landed.
    pub unit: U,
    /// For each of the unit's [`Unit::top_level`], how many of the
    /// refusal's [`CompileError::raised`] php had raised when it landed.
    pub raised_before: Vec<usize>,
}

/// Where php stood when it refused a source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stage {
    /// Compiling it, `zend_is_compiling()`: the refusal stands in the
    /// file at [`CompileError::line`].
    #[default]
    Compiling,
    /// Opening it for the scanner, before anything compiled
    /// (`open_file_for_scanning`, `zend_prepare_string_for_scanning`):
    /// the refusal stands where the request is executing, the `include`
    /// or the `eval` that asked, and at `Unknown` on line 0 for the
    /// program itself.
    Opening,
}

/// Parse and compile one source file: what the engine calls for the
/// file it serves, an `include` or an `eval`, since its edges reach the compiler
/// and not the syntax crate (design §4.2).
///
/// `strings` is where the unit's constant strings are interned (design
/// §5.2): the engine's table for a unit bound for the unit cache, whose
/// strings every request serving it then shares, and the request's own
/// heap for a unit that is one request's alone. php decides the same
/// thing by which handler is installed when `zend_compile_file` runs
/// (`Z/Zend/zend_string.c:363-373`).
///
/// The lexer is told what php's scanner reads off `CG(short_tags)`,
/// [`Options::short_open_tag`], and nothing here reads an ini of its
/// own: the caller that described the request is what knows the entry.
/// With it off, which is what the pin's recipe passes, a `<?` is text
/// the file echoes and the `<?=` beside it still opens code:
///
/// ```php
/// ?>
/// <? echo "code"; ?>
/// <?= "echoed" ?>
/// ```
/// ```expect
/// <? echo "code"; ?>
/// echoed
/// ```
///
/// With it on, the same file is the code the tag opened, and the text
/// around it is gone. The entry is `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so no
/// snippet turns it on from inside: the fence names `[regime.short-tags]`
/// of the pin, whose ini stands after the recipe's for the oracle and for
/// the engine alike (harness-design.md §8, ledger 55; #610):
///
/// ```php
/// ?>
/// <? echo "code"; ?>
/// <?= "echoed" ?>
/// ```
/// ```expect regime=short-tags exit=0
/// codeechoed
/// ```
///
/// `arena` is the compile's ([`Arena`]): the parse and the compiler's
/// temporaries are bumped into it, and it is reset once the unit is
/// made, so the caller that keeps one arena for its compiles hands each
/// the room the last one left.
pub fn compile_source(
    arena: &mut Arena,
    source: &[u8],
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    compile_source_into(arena, source, options, strings, Room::default())
}

/// [`compile_source`] into `room`, the room of a unit the caller gave
/// up, which the unit's main op array is laid down in ([`Room`]).
pub fn compile_source_into(
    arena: &mut Arena,
    source: &[u8],
    options: &Options<'_>,
    strings: &mut dyn Intern,
    room: Room,
) -> Result<Unit, CompileError> {
    let reset = Reset(arena);
    compile_in(reset.0, source, options, strings, room)
}

/// The arena of a compile, reset as the compile hands its unit back, so
/// the unit is made where the caller takes it rather than moved past
/// the reset.
struct Reset<'a>(&'a mut Arena);

impl Drop for Reset<'_> {
    fn drop(&mut self) {
        self.0.reset();
    }
}

/// The room a unit's main op array is laid down in, taken from a unit
/// the caller gave up rather than asked of the allocator (#3028): its
/// oplines' and its constants' vectors, emptied, and its file's name,
/// which a unit of a name as long writes over. php `emalloc`s an
/// `eval`'s op array and its tables from the request's heap, which the
/// last one's `destroy_op_array` gave back, so an `eval` in a loop asks
/// the system for nothing; a request that keeps the room of the unit it
/// gave up last does the same.
#[derive(Debug, Default)]
pub struct Room {
    oplines: Vec<php_unit::Opline>,
    literals: Vec<php_heap::Zval>,
    filename: Option<php_unit::decl::Name>,
}

impl Room {
    /// The most bytes of one table a room keeps: a unit of a large file
    /// gives its own up.
    const KEPT: usize = 64 * 1024;

    /// The room of `main`, the main op array of a unit given up: its
    /// tables taken out emptied, each within [`Room::KEPT`], and its
    /// file's name.
    #[must_use]
    pub fn of(main: &mut php_unit::OpArray) -> Room {
        let mut oplines = std::mem::take(&mut main.oplines);
        let mut literals = std::mem::take(&mut main.literals);
        oplines.clear();
        literals.clear();
        Room {
            oplines: kept(oplines),
            literals: kept(literals),
            filename: Some(std::mem::take(&mut main.filename)),
        }
    }
}

/// `table`, where its room is within [`Room::KEPT`].
fn kept<T>(table: Vec<T>) -> Vec<T> {
    if table.capacity() * size_of::<T>() <= Room::KEPT {
        table
    } else {
        Vec::new()
    }
}

/// [`compile_source`] in `arena`, where the parse and the compiler's
/// temporaries live until the unit is made.
fn compile_in(
    arena: &Arena,
    source: &[u8],
    options: &Options<'_>,
    strings: &mut dyn Intern,
    room: Room,
) -> Result<Unit, CompileError> {
    let Some(multibyte) = options.multibyte else {
        let parsed = php_syntax::parse(arena, source, &syntax_options(options));
        return compile_scanned(arena, &parsed, options, strings, None, room);
    };
    let from = match options.kind {
        SourceKind::File => multibyte::Source::File {
            detect_unicode: multibyte.detect_unicode,
            listed: multibyte.listed,
        },
        SourceKind::Eval(_) => multibyte::Source::Eval,
    };
    let opened =
        multibyte::open(multibyte.encodings, source, from).map_err(|refused| CompileError {
            level: Level::CompileError,
            message: refused.message().into(),
            line: 0,
            raised: Vec::new(),
            thrown: false,
            stage: Stage::Opening,
            declared: None,
            keys_made: 0,
        })?;
    let parsed = php_syntax::parse(arena, &opened.text, &syntax_options(options));
    let scanning = multibyte::Scanning {
        original: source.get(opened.bom..).unwrap_or_default(),
        filter: opened.filter,
    };
    compile_scanned(arena, &parsed, options, strings, Some(scanning), room)
}

/// `zend_compile_string_to_ast` and `zend_ast_export("", ast, "")`
/// (`Z/Zend/zend_language_scanner.l:604`, `Z/Zend/zend_ast.c:2954`):
/// `source` parsed from where `options` begins the scanner, the parse's
/// first fatal its refusal, and php's text of the tree, which is what
/// `assert()`'s description is written by over the whole file. The
/// parse is in `arena`, reset once the text is made.
pub fn export_source(
    arena: &mut Arena,
    source: &[u8],
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> Result<Vec<u8>, CompileError> {
    let made = export_in(arena, source, options, strings);
    arena.reset();
    made
}

/// [`export_source`] in `arena`.
fn export_in(
    arena: &Arena,
    source: &[u8],
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> Result<Vec<u8>, CompileError> {
    let parsed = php_syntax::parse(arena, source, &syntax_options(options));
    if let Some(fatal) = parsed.diagnostics.iter().find(|d| d.level.is_fatal()) {
        return Err(CompileError {
            level: level_of(fatal.level),
            message: fatal.message.clone().into(),
            line: fatal.line,
            raised: Vec::new(),
            thrown: false,
            stage: Stage::Compiling,
            declared: None,
            keys_made: 0,
        });
    }
    emit::export(arena, &parsed, options, strings)
}

/// What the lexer is told of a source the compiler is handed: the
/// entry `CG(short_tags)` reads and where the scanner begins.
fn syntax_options(options: &Options<'_>) -> php_syntax::Options {
    php_syntax::Options {
        lex: php_syntax::LexOptions {
            short_open_tag: options.short_open_tag,
            position: match options.kind {
                SourceKind::File if options.shebang == Shebang::Skipped => {
                    php_syntax::Position::AtShebang
                }
                SourceKind::File => php_syntax::Position::AtOpenTag,
                SourceKind::Eval(position) => position,
            },
        },
        ..php_syntax::Options::default()
    }
}

/// Compile one parsed file into a unit, interning its constant strings
/// into `strings`, with the compiler's temporaries in `arena`, the one
/// the parse lives in.
pub fn compile<'a>(
    arena: &'a Arena,
    parsed: &'a Parsed<'a>,
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    compile_scanned(arena, parsed, options, strings, None, Room::default())
}

/// [`compile`] over a parse of what the scanner read through
/// `scanning`, the file as it was opened, with `arena` the compile's.
fn compile_scanned<'p>(
    arena: &'p Arena,
    parsed: &'p Parsed<'p>,
    options: &Options<'p>,
    strings: &mut dyn Intern,
    scanning: Option<multibyte::Scanning<'p>>,
    room: Room,
) -> Result<Unit, CompileError> {
    // `Parsed::diagnostics` is in php's order (design §6.1): the
    // scanner's and the parser's by position, the compiler's one after
    // them all. php's scanner raises as it scans and its parser stops at
    // the token it did not want, so what stands before the first fatal
    // was raised and what stands after it never was; php compiles
    // nothing of a file its parser refused, and the first fatal is the
    // compile error, in php's words at php's line, with what was raised
    // before it carried beside it.
    //
    // The parser's `declare` action runs as it parses
    // ([`multibyte::scan`]): what it warned stands among the parse's
    // diagnostics, and the fatal it threw stands where it threw it.
    let scanned = multibyte::scan(arena, parsed, options, scanning);
    let parsed = &*scanned.parsed;
    let mut raised = Vec::new();
    for (at, diagnostic) in parsed.diagnostics.iter().enumerate() {
        if let Some(refused) = scanned.refused.as_ref().filter(|r| r.before == at) {
            return Err(refused.error(raised));
        }
        let level = level_of(diagnostic.level);
        if diagnostic.level.is_fatal() {
            return Err(CompileError {
                level,
                message: diagnostic.message.clone().into(),
                line: diagnostic.line,
                raised,
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            });
        }
        raised.push(Diagnostic {
            level,
            line: diagnostic.line,
            message: diagnostic.message.as_str().into(),
        });
    }
    // The scanner's diagnostics stand before the compiler's on the unit,
    // as php raised them: the whole file is scanned before
    // `zend_compile_top_stmt` runs, and opcache records both kinds on a
    // `persistent_script` to replay on a hit.
    if let Some(refused) = &scanned.refused {
        return Err(refused.error(raised));
    }
    let halt = emit::walk::halt_offset(parsed).map(|offset| scanned.original_offset(offset));
    emit::unit(arena, parsed, options, strings, halt, room, raised)
}

/// A compile's refusal with `raised`, what was raised before the compile
/// began, standing before every diagnostic the compiler raised, and the
/// names of the functions it declared made as a file's are
/// ([`intern_names`]); a declared unit whose names the heap refused is
/// not handed back.
pub(crate) fn refused_after(
    mut refused: CompileError,
    mut raised: Vec<Diagnostic>,
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> CompileError {
    if let Some(declared) = &mut refused.declared {
        for before in &mut declared.raised_before {
            *before += raised.len();
        }
        if options.kind == SourceKind::File && intern_names(&declared.unit, strings).is_err() {
            refused.declared = None;
        }
    }
    raised.append(&mut refused.raised);
    refused.raised = raised;
    refused
}

/// The op arrays and declarations of a unit, borrowed where they stand:
/// a [`Unit`]'s, or a compile's before it lays them in one, so what is
/// interned of them is interned before the unit is made, where it is
/// made once and not moved.
#[derive(Clone, Copy)]
pub(crate) struct Parts<'u> {
    /// The main op array's file's name.
    pub(crate) file: &'u php_unit::decl::Name,
    /// The main op array's variables.
    pub(crate) main_vars: &'u [php_unit::decl::Name],
    pub(crate) functions: &'u [(FunctionDecl, php_unit::OpArray)],
    pub(crate) dynamic: &'u [(FunctionDecl, php_unit::OpArray)],
    pub(crate) methods: &'u [php_unit::MethodBody],
    pub(crate) hooks: &'u [php_unit::HookBody],
    pub(crate) classes: &'u [ClassDecl],
}

impl<'u> Parts<'u> {
    fn of(unit: &'u Unit) -> Parts<'u> {
        Parts {
            file: &unit.main.filename,
            main_vars: &unit.main.vars,
            functions: &unit.functions,
            dynamic: &unit.dynamic,
            methods: &unit.methods,
            hooks: &unit.hooks,
            classes: &unit.classes,
        }
    }

    /// What [`intern_names`] interns, or [`intern_declared`] where `kind`
    /// is an `eval`'s.
    pub(crate) fn intern(self, kind: SourceKind, strings: &mut dyn Intern) -> Result<(), Fatal> {
        if kind == SourceKind::File {
            for name in self.names() {
                strings.intern(name)?;
            }
        }
        self.intern_declared(strings)
    }

    /// [`op_array_names`].
    fn names(self) -> impl Iterator<Item = &'u [u8]> {
        let functions = self
            .functions
            .iter()
            .chain(self.dynamic)
            .map(|(decl, _)| decl);
        let methods = self.classes.iter().flat_map(|class| {
            let hooks = class
                .properties
                .iter()
                .filter_map(|property| property.hooks.as_deref())
                .flat_map(|hooks| hooks.get.iter().chain(&hooks.set));
            class
                .methods
                .iter()
                .chain(hooks)
                .map(|method| &method.function)
        });
        std::iter::once(self.file)
            .chain(functions.chain(methods).map(|decl| &decl.name))
            .chain(self.classes.iter().map(|class| &class.name))
            .map(|name| &*name.0)
    }

    /// [`intern_declared`].
    fn intern_declared(self, strings: &mut dyn Intern) -> Result<(), Fatal> {
        for var in self.main_vars {
            strings.intern(&var.0)?;
        }
        let bodies = self
            .functions
            .iter()
            .map(|(_, body)| body)
            .chain(self.dynamic.iter().map(|(_, body)| body))
            .chain(self.methods.iter().map(|method| &method.body))
            .chain(self.hooks.iter().map(|hook| &hook.body));
        for body in bodies {
            for var in &body.vars {
                strings.intern(&var.0)?;
            }
        }
        for class in self.classes {
            for property in &class.properties {
                strings.intern(&property.name.0)?;
                if let Some(prefix) = mangling(property.visibility, &class.name.0) {
                    strings.intern(&[prefix.as_slice(), &property.name.0].concat())?;
                }
            }
        }
        Ok(())
    }
}

/// The names a file's op arrays carry interned into `strings`, the home
/// its constant strings went to: its file's, `op_array->filename`, and
/// each function's, method's, hook's and closure's, `function_name`,
/// and each class's, `ce->name`. php's compile makes each before the
/// file's first line runs and holds it as long as the op array, and
/// opcache's persist interns each beside the op array's literals
/// (`Z/ext/opcache/zend_persist.c:417`, `:632`, `:943`), so a trace row
/// that copies one (`ZVAL_STR_COPY`) costs the request nothing, a
/// throwable a native throws inside a closure among them:
///
/// ```php
/// $a = function () { try { intdiv(1, 0); } catch (DivisionByZeroError $e) { return 1; } };
/// $c = function () { try { json_decode('{', flags: JSON_THROW_ON_ERROR); } catch (JsonException $e) { return 1; } };
/// foreach ([$a, $c, $a] as $f) { $m = memory_get_usage(); $f(); echo memory_get_usage() - $m, "\n"; }
/// ```
/// ```expect
/// 0
/// 0
/// 0
/// ```
///
/// An `eval`'s unit, `-r`'s among them (`compile_string`), interns
/// none of them: php makes its file's name and its closures' for the
/// one execution and frees them with its op array, where a home that
/// holds a string to the request's end would keep each. The file
/// cache's unit is read back with its names interned again, as its
/// constants are.
pub fn intern_names(unit: &Unit, strings: &mut dyn Intern) -> Result<(), Fatal> {
    Parts::of(unit).intern(SourceKind::File, strings)
}

/// The names php's compile interns into whichever home it compiles
/// into, an `eval`'s among them: each op array's variables, which
/// `zend_try_compile_cv` interns (`zval_make_interned_string`) before
/// `lookup_cv` keeps them in `op_array->vars`, and each declared
/// property's name with the key `zend_declare_typed_property` interns
/// as `prop_info->name`, mangled for a protected or a private one. A
/// symbol table keys a variable by its CV's name and an object's table
/// a declared row by its key, so neither costs a read anything. An
/// `eval`'d variable's name is the request's from its compile on:
///
/// ```php
/// $m = memory_get_usage(); eval('$abcd = 1;'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); eval('$abcd = 2;'); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 32
/// 0
/// ```
pub fn intern_declared(unit: &Unit, strings: &mut dyn Intern) -> Result<(), Fatal> {
    Parts::of(unit).intern_declared(strings)
}

/// `zend_mangle_property_name`'s prefix (`Z/Zend/zend_compile.c`) for a
/// property `class` declares under `visibility`, what
/// `zend_declare_typed_property` puts before the name it keys
/// `prop_info->name` by: none for a public one, `\0*\0` for a protected
/// one and `\0Class\0` for a private one.
#[must_use]
pub fn mangling(visibility: Visibility, class: &[u8]) -> Option<Vec<u8>> {
    match visibility {
        Visibility::Public => None,
        Visibility::Protected => Some(b"\0*\0".to_vec()),
        Visibility::Private => Some([&b"\0"[..], class, &b"\0"[..]].concat()),
    }
}

/// The names a unit's op arrays carry, the ones [`intern_names`]
/// interns: its file's, `op_array->filename`, each function's, method's,
/// hook's and closure's, `function_name`, and each class's, `ce->name`.
/// A name two op arrays share stands once for each.
pub fn op_array_names(unit: &Unit) -> impl Iterator<Item = &[u8]> {
    Parts::of(unit).names()
}

/// php's level for what the scanner or the parser raised: the same
/// `E_*` under the unit's name for it.
fn level_of(level: php_syntax::Level) -> Level {
    match level {
        php_syntax::Level::Parse => Level::Parse,
        php_syntax::Level::CompileError => Level::CompileError,
        php_syntax::Level::CompileWarning => Level::CompileWarning,
        php_syntax::Level::Deprecated => Level::Deprecated,
    }
}

/// The sentence a compile ends in where the heap refused it a fatal:
/// the fatal's own message, and php's `Out of memory` for the limit.
pub(crate) fn fatal_message(fatal: Fatal) -> String {
    match fatal.into_reason() {
        FatalReason::Error(message) => message,
        FatalReason::OutOfMemory(_) => String::from("Out of memory"),
    }
}

#[cfg(test)]
mod tests {
    /// `source` parsed as a file under the lexer's defaults and compiled
    /// through [`compile`], the parse and the compile in one arena.
    fn compile_parsed(
        source: &[u8],
        options: &Options<'_>,
        strings: &mut dyn Intern,
    ) -> Result<Unit, CompileError> {
        let arena = Arena::new();
        let parsed = php_syntax::parse(&arena, source, &php_syntax::Options::default());
        compile(&arena, &parsed, options, strings)
    }

    use super::*;
    use php_link::Names;
    use php_unit::decl::{ConstExpr, ConstantName, Name, NameKind, Parameter, Type, Visibility};
    use php_unit::{AutoGlobal, OpArray, Opcode, OperandKind, OplineIdx, frameless_table};

    /// A dump read as text by the tests below, which hold it to php's
    /// words; a byte that is no character reads as U+FFFD here alone.
    trait Text {
        fn text(&self) -> String;
    }

    impl Text for Vec<u8> {
        fn text(&self) -> String {
            String::from_utf8_lossy(self).into_owned()
        }
    }

    /// A unit whose constant strings are one engine's, which is what the
    /// unit cache holds (design §6.3).
    fn unit_of(source: &[u8]) -> Unit {
        unit_and_strings(source).0
    }

    /// php's dump of a one-line file's main op array, the file unnamed
    /// as `Options::default` leaves it: the header with these counts,
    /// then the oplines.
    fn main_dump(lines: usize, vars: u32, tmps: u32, oplines: &str) -> String {
        format!(
            "\n$_main:\n     ; (lines={lines}, args=0, vars={vars}, tmps={tmps})\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n{oplines}"
        )
    }

    /// The dump of one source, read while the table its strings were
    /// interned into still stands, under the frameless table of the
    /// bundle the source compiled against, as the engine hands it to
    /// `compile-identity`.
    fn dump_of(source: &[u8]) -> String {
        let (unit, _strings) = unit_and_strings(source);
        let options = bundled();
        let functions = options.functions.rows();
        let names: Vec<Name> = (options.frameless.iter())
            .filter_map(|entry| functions.get(entry.function as usize))
            .map(|decl| decl.name.clone())
            .collect();
        unit.dump(&names).text()
    }

    fn unit_and_strings(source: &[u8]) -> (Unit, php_heap::Strings) {
        let mut strings = php_heap::Strings::new();
        let unit = compile_parsed(source, &bundled(), &mut strings).unwrap_or_default();
        (unit, strings)
    }

    /// The natives the tests below call that are in no row of
    /// `special::TABLE`: php's `-n` has each, so its dump reads them off
    /// `CG(function_table)` as `INIT_FCALL` and `DO_ICALL`.
    const CALLED: [&[u8]; 6] = [
        b"var_dump",
        b"extension_loaded",
        b"str_repeat",
        b"set_time_limit",
        b"memory_get_usage",
        b"exit",
    ];

    /// A native's parameter as its stub declares it, by value, with its
    /// default where it has one.
    fn parameter(name: &str, ty: Type, default: Option<ConstExpr>) -> Parameter {
        Parameter {
            name: Name::from(name),
            ty: Some(ty),
            by_ref: false,
            prefer_ref: false,
            promoted: false,
            variadic: false,
            default: default.map_or(
                php_unit::decl::ParameterDefault::None,
                php_unit::decl::ParameterDefault::Expr,
            ),
            attributes: Vec::new(),
        }
    }

    /// The natives with a frameless entry the tests below call, as
    /// `ext/standard` declares them, in `basic_functions.stub.php`'s
    /// order, which is `zend_flf_functions`' among them: the name, the
    /// parameters as php's stub has them with a default where the stub
    /// has one, and the arities of `frameless_function_infos_<name>`;
    /// and `join` and `strchr`, aliases php registers with none.
    fn frameless_natives() -> Vec<FunctionDecl> {
        let string = |name| parameter(name, Type::String, None);
        let either = |name| parameter(name, Type::Union(vec![Type::Array, Type::String]), None);
        let nullable =
            |name, ty| parameter(name, Type::Nullable(Box::new(ty)), Some(ConstExpr::Null));
        let long = |name, value| {
            let radix = php_unit::decl::Radix::Decimal;
            parameter(name, Type::Int, Some(ConstExpr::Long { value, radix }))
        };
        let haystack =
            |more: Vec<Parameter>| [vec![string("haystack"), string("needle")], more].concat();
        let implode = || vec![either("separator"), nullable("array", Type::Array)];
        let before = || {
            haystack(vec![parameter(
                "before_needle",
                Type::Bool,
                Some(ConstExpr::Bool(false)),
            )])
        };
        let characters = Some(ConstExpr::Str(b" ".as_slice().into()));
        let count = Parameter {
            by_ref: true,
            ..parameter("count", Type::Mixed, Some(ConstExpr::Null))
        };
        [
            (
                "trim",
                vec![
                    string("string"),
                    parameter("characters", Type::String, characters),
                ],
                vec![1, 2],
            ),
            ("implode", implode(), vec![1, 2]),
            ("join", implode(), vec![]),
            (
                "dirname",
                vec![string("path"), long("levels", 1)],
                vec![1, 2],
            ),
            ("strstr", before(), vec![2, 3]),
            ("strchr", before(), vec![]),
            ("strpos", haystack(vec![long("offset", 0)]), vec![2, 3]),
            ("str_contains", haystack(vec![]), vec![2]),
            ("str_starts_with", haystack(vec![]), vec![2]),
            (
                "substr",
                vec![
                    string("string"),
                    parameter("offset", Type::Int, None),
                    nullable("length", Type::Int),
                ],
                vec![2, 3],
            ),
            (
                "strtr",
                vec![
                    string("string"),
                    either("from"),
                    nullable("to", Type::String),
                ],
                vec![2, 3],
            ),
            (
                "str_replace",
                vec![
                    either("search"),
                    either("replace"),
                    either("subject"),
                    count,
                ],
                vec![3],
            ),
        ]
        .into_iter()
        .map(|(name, parameters, frameless)| FunctionDecl {
            name: Name::from(name),
            parameters,
            return_type: None,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless,
        })
        .collect()
    }

    /// The options of an engine whose bundle declares every name of
    /// `special::TABLE` and every name of [`CALLED`] as a native, which
    /// php's `-n` has, and then [`frameless_natives`]: what
    /// `zend_compile_call` reads as `CG(function_table)`, so the tests
    /// below compile under the table php compiles under; and `INF` and
    /// `NAN` among its persistent constants, as `ext/standard` declares
    /// them (`REGISTER_DOUBLE_CONSTANT("INF", ZEND_INFINITY,
    /// CONST_PERSISTENT)`).
    fn bundled() -> Options<'static> {
        static NATIVES: std::sync::LazyLock<Vec<FunctionDecl>> = std::sync::LazyLock::new(|| {
            special::TABLE
                .iter()
                .map(|(name, _)| *name)
                .chain(CALLED)
                .map(|name| FunctionDecl {
                    name: Name(name.into()),
                    parameters: Vec::new(),
                    return_type: None,
                    returns_by_ref: false,
                    is_generator: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: Name(Box::new([])),
                    line: 0,
                    end_line: 0,
                    frameless: Vec::new(),
                })
                .chain(frameless_natives())
                .collect()
        });
        static NAMES: std::sync::LazyLock<Names<Folded>> =
            std::sync::LazyLock::new(|| Names::over(&NATIVES));
        static FRAMELESS: std::sync::LazyLock<Vec<php_unit::FramelessEntry>> =
            std::sync::LazyLock::new(|| frameless_table(NATIVES.iter()));
        static PERSISTENT: std::sync::LazyLock<Vec<ConstDecl>> = std::sync::LazyLock::new(|| {
            [(&b"INF"[..], f64::INFINITY), (b"NAN", f64::NAN)]
                .into_iter()
                .map(|(name, value)| ConstDecl {
                    name: Name(name.into()),
                    value: php_unit::decl::ConstExpr::Double(value),
                    attributes: Vec::new(),
                })
                .collect()
        });
        static CONSTANT_NAMES: std::sync::LazyLock<Names<Exact>> =
            std::sync::LazyLock::new(|| Names::over(&PERSISTENT));
        Options {
            functions: Indexed::new(&NATIVES, &NAMES),
            frameless: &FRAMELESS,
            persistent: Indexed::new(&PERSISTENT, &CONSTANT_NAMES),
            ..Options::default()
        }
    }

    /// `rows` as the engine hands a table to the compiler, held for the
    /// rest of the test process.
    fn indexed<R: php_link::Named, C: php_link::Case>(rows: Vec<R>) -> Indexed<'static, R, C> {
        let rows: &'static [R] = Vec::leak(rows);
        Indexed::new(rows, Box::leak(Box::new(Names::over(rows))))
    }

    #[test]
    fn an_empty_file_compiles_the_way_php_compiles_one() {
        let unit = unit_of(b"");
        assert_eq!(unit.main.oplines.len(), 1);
        assert_eq!(unit.main.oplines[0].opcode, Opcode::Return);
        assert_eq!(unit.main.oplines[0].op1_type, OperandKind::Const);
        assert_eq!(
            unit.main.oplines[0].extended_value,
            u32::MAX,
            "php sets the implicit final return's extended_value to -1"
        );
        assert!(unit.functions.is_empty());
    }

    #[test]
    fn echo_of_a_number_is_one_echo_and_the_final_return() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n"),
            "the constant 1 is echoed and returned"
        );
        assert_eq!(
            (unit.main.oplines[0].op1, unit.main.oplines[1].op1),
            (0, 1),
            "zend_add_literal gives each its own slot"
        );
    }

    /// A constant subject over a jump table: `zend_emit_op` sets `op1`
    /// before `op2`, so the subject's slot comes before the table's.
    #[test]
    fn a_constant_subject_takes_its_slot_before_the_jump_table() {
        let (unit, _strings) =
            unit_and_strings(b"<?php switch ('z') { case 'a': echo 1; break; case 'b': echo 2; }");
        let switch = unit
            .main
            .oplines
            .iter()
            .find(|op| op.opcode == Opcode::SwitchString)
            .map(|op| (op.op1_type, op.op1, op.op2_type, op.op2));
        assert_eq!(
            switch,
            Some((OperandKind::Const, 0, OperandKind::Const, 1)),
            "a string jump table over two cases"
        );
        assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"z"[..]));
        assert!(unit.main.literals[1].as_arr().is_some());
    }

    /// `zend_resolve_goto_label` nulls the label's slot once it has found
    /// the label (`Z/Zend/zend_compile.c:5907`), and every later
    /// constant is appended after it (`zend_add_literal`), the
    /// same `'a'` twice in two slots, as phpdbg's `info literal` shows
    /// php's table.
    #[test]
    fn a_goto_s_label_slot_is_nulled_and_no_slot_is_shared() {
        let (unit, _strings) = unit_and_strings(b"<?php goto a; a: echo 'a'; echo 'a';");
        assert_eq!(unit.main.literals.len(), 4);
        assert_eq!(unit.main.literals[0], php_heap::Zval::null());
        assert_eq!(literal_text(&unit, 1).as_deref(), Some(&b"a"[..]));
        assert_eq!(literal_text(&unit, 2).as_deref(), Some(&b"a"[..]));
        assert_eq!(unit.main.literals[3].as_long(), Some(1));
    }

    /// The implicit final return stands on the file's last line, where
    /// the scanner stopped (`CG(zend_lineno) = last_lineno`,
    /// `Z/Zend/zend_language_scanner.l:622`), which phpdbg's `print` and
    /// its stepping name: `L0003 0001 RETURN int(1)` for a file whose
    /// last statement is on line 2 and which ends in a newline.
    #[test]
    fn the_implicit_return_stands_on_the_file_s_last_line() {
        let (unit, _strings) = unit_and_strings(b"<?php\necho 1;\n");
        let lines: Vec<u32> = unit.main.oplines.iter().map(|op| op.lineno).collect();
        assert_eq!(lines, [2, 3]);
        assert_eq!((unit.main.line_start, unit.main.line_end), (1, 3));
    }

    /// The scanner stops at `__halt_compiler();`, so the main op array
    /// of a halted file ends on the halt's line and the data after it
    /// counts none, as php's `halt01.phpt` dump reads `:1-5`.
    #[test]
    fn a_halted_file_ends_on_the_halt_s_line() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;\n__halt_compiler();\ndata\n");
        assert_eq!((unit.main.line_start, unit.main.line_end), (1, 2));
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;\n__halt_compiler() ?>\ndata\n");
        assert_eq!(unit.main.line_end, 3);
    }

    /// The dump of one source compiled through [`compile_source`] under
    /// `short_open_tag`, which is the route the lexer is told it by.
    fn dump_under_short_open_tag(source: &[u8], short_open_tag: bool) -> String {
        let mut strings = php_heap::Strings::new();
        let options = Options {
            short_open_tag,
            ..bundled()
        };
        compile_source(&mut Arena::new(), source, &options, &mut strings)
            .unwrap_or_default()
            .dump(&[])
            .text()
    }

    /// [`Options::short_open_tag`] reaching the scanner (#608): with the
    /// entry off the `<?` and everything after it is the one
    /// `T_INLINE_HTML` php's `inline_char_handler` takes it into, and
    /// with it on the same source is the code inside the tag. The
    /// oracle answers the off side beside [`compile_source`], which is
    /// the value the pin's recipe passes; the on side is
    /// `tests/lang/short_tags.001.phpt`, whose `--INI--` turns it on.
    #[test]
    fn a_short_tag_opens_code_only_while_short_open_tag_is_on() {
        let source = b"<?\necho 1;\n";
        let dump = |lines: usize, oplines: &str| {
            format!(
                "\n$_main:\n     ; (lines={lines}, args=0, vars=0, tmps=0)\n\
                 \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n{oplines}"
            )
        };
        assert_eq!(
            dump_under_short_open_tag(source, false),
            dump(
                2,
                "0000 ECHO string(\"<?\\necho 1;\\n\")\n0001 RETURN int(1)\n"
            ),
            "off, the file echoes its own text"
        );
        assert_eq!(
            dump_under_short_open_tag(source, true),
            dump(2, "0000 ECHO int(1)\n0001 RETURN int(1)\n"),
            "on, the tag opens code and the 1 inside it is echoed"
        );
    }

    /// A unit laid down in the room of one given up (#3028) is the unit
    /// laid down in none, and an `eval` of one line in a loop takes its
    /// oplines', its constants' and its file name's room from the last.
    #[test]
    fn a_unit_laid_down_in_a_room_is_the_unit_laid_down_in_none() {
        let options = Options {
            kind: SourceKind::EVAL,
            filename: b"/a.php(2) : eval()'d code",
            ..Options::default()
        };
        let mut arena = Arena::new();
        let mut strings = php_heap::Strings::new();
        let mut compile = |source: &[u8], room: Room| {
            compile_source_into(&mut arena, source, &options, &mut strings, room)
        };
        let fresh = compile(b"echo 'a', 'b'; return 1;", Room::default()).unwrap_or_default();
        let mut given_up = compile(b"echo 'c'; return 2;", Room::default()).unwrap_or_default();
        assert!(!given_up.main.oplines.is_empty() && !fresh.main.oplines.is_empty());
        let oplines = given_up.main.oplines.as_ptr();
        let filename = given_up.main.filename.0.as_ptr();
        let room = Room::of(&mut given_up.main);
        let laid = compile(b"echo 'a', 'b'; return 1;", room).unwrap_or_default();
        assert_eq!(fresh, laid);
        assert_eq!(
            laid.main.oplines.as_ptr(),
            oplines,
            "the oplines' room is the last unit's"
        );
        assert_eq!(
            laid.main.filename.0.as_ptr(),
            filename,
            "the file's name is written over"
        );
    }

    #[test]
    fn a_nested_call_sends_what_the_inner_call_returned() {
        let (unit, _strings) = unit_and_strings(b"<?php var_dump(extension_loaded('demo'));");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                7,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0001 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0002 SEND_VAL string(\"demo\") 1\n\
                 0003 V0 = DO_ICALL\n\
                 0004 SEND_VAR V0 1\n\
                 0005 DO_ICALL\n\
                 0006 RETURN int(1)\n"
            ),
            "php's, the inner call's `V0 = ` and the outer's bare"
        );
        assert_eq!(unit.main.oplines[0].extended_value, 1, "one argument");
        assert_eq!(
            unit.main.oplines[0].op1, 96,
            "zend_vm_calc_used_stack: five header slots and one argument, sixteen bytes each"
        );
        assert_eq!(unit.main.oplines[1].op1, 96);
        assert_eq!(
            unit.main.cache_size, 16,
            "one cache slot per INIT_FCALL, a pointer each"
        );
        assert_eq!(
            unit.main.num_tmps, 2,
            "one temporary per call, as php counts them here"
        );
    }

    #[test]
    fn the_entry_is_safepoint_zero_and_the_return_carries_its_own_number() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
        assert_eq!(
            &*unit.main.safepoints,
            &[OplineIdx(0), OplineIdx(1)],
            "the entry at opline zero, then the return"
        );
        assert_eq!(
            unit.main.oplines[1].out, 1,
            "a site opcode carries its safepoint number in the field ops.toml names"
        );
    }

    /// The one constant a single-expression `echo` leaves in the table.
    fn echoed_literal(source: &[u8]) -> php_heap::Zval {
        let (unit, _strings) = unit_and_strings(source);
        unit.main
            .literals
            .first()
            .copied()
            .unwrap_or(php_heap::Zval::UNDEF)
    }

    #[test]
    fn arithmetic_over_two_constants_is_folded_the_way_php_folds_it() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1 + 2;");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(2, 0, 0, "0000 ECHO int(3)\n0001 RETURN int(1)\n"),
            "the sum is one literal and no ZEND_ADD"
        );
        assert_eq!(echoed_literal(b"<?php echo 1 + 2;").as_long(), Some(3));
        // `div_function`: a long where the division comes out whole, a
        // double where it does not.
        assert_eq!(echoed_literal(b"<?php echo 4/2;").as_long(), Some(2));
        assert_eq!(
            echoed_literal(b"<?php echo 1/3;").as_double(),
            Some(1.0 / 3.0)
        );
        assert_eq!(
            echoed_literal(b"<?php echo 0.1 + 0.2;").as_double(),
            Some(0.1 + 0.2)
        );
        // `fast_long_add_function`: the sum that overflows the long is
        // the double php leaves.
        assert_eq!(
            echoed_literal(b"<?php echo 9223372036854775807 + 1;").as_double(),
            Some(9_223_372_036_854_775_807_f64 + 1.0)
        );
        // `zend_try_ct_eval_unary_pm`: a multiplication by -1, which
        // keeps the sign of a zero.
        assert_eq!(echoed_literal(b"<?php echo -2;").as_long(), Some(-2));
        assert_eq!(
            echoed_literal(b"<?php echo -0.0;")
                .as_double()
                .map(f64::is_sign_negative),
            Some(true)
        );
    }

    /// `zend_try_ct_eval_binary_op` converts an operand as the opcode's
    /// kernel converts it, through the one reader `php_heap` holds, so a
    /// numeric string, a bool and null fold where they stand and the op
    /// array is php's single `ECHO` of the folded constant.
    #[test]
    fn an_operand_php_converts_is_folded_where_it_stands() {
        assert_eq!(
            dump_of(b"<?php echo \"1\" + 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n")
        );
        assert_eq!(echoed_literal(b"<?php echo \"1\" + 1;").as_long(), Some(2));
        assert_eq!(
            dump_of(b"<?php echo true + 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n")
        );
        assert_eq!(echoed_literal(b"<?php echo true + 1;").as_long(), Some(2));
        // The folded 1 and the return's 1 are two literals, as
        // `zend_add_literal` appends each.
        let (unit, _strings) = unit_and_strings(b"<?php echo null + 1;");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n")
        );
        assert_eq!(unit.main.literals.len(), 2);
        assert_eq!(echoed_literal(b"<?php echo null + 1;").as_long(), Some(1));
        assert_eq!(echoed_literal(b"<?php echo false + 1;").as_long(), Some(1));
        // `_is_numeric_string_ex`: whitespace on either side, a sign, a
        // fraction, an exponent, and a long's edge.
        assert_eq!(
            echoed_literal(b"<?php echo \" 1 \" + \"+2\";").as_long(),
            Some(3)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \"1e3\" + \".5\";").as_double(),
            Some(1000.5)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \"9223372036854775808\" + 0;").as_double(),
            Some(9_223_372_036_854_775_808.0)
        );
        assert_eq!(
            echoed_literal(b"<?php echo -\"1.5\";").as_double(),
            Some(-1.5)
        );
        assert_eq!(echoed_literal(b"<?php echo -null;").as_long(), Some(0));
        assert_eq!(
            echoed_literal(b"<?php echo \"3\" ** \"2\";").as_long(),
            Some(9)
        );
    }

    #[test]
    fn a_persistent_constant_stands_for_its_value_where_it_is_written() {
        assert_eq!(
            echoed_literal(b"<?php echo INF;").as_double(),
            Some(f64::INFINITY)
        );
        assert_eq!(
            echoed_literal(b"<?php echo -INF;").as_double(),
            Some(f64::NEG_INFINITY)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \\NAN;")
                .as_double()
                .map(f64::is_nan),
            Some(true),
            "a leading separator names the same global constant"
        );
    }

    #[test]
    fn the_engine_s_persistent_constants_stand_for_their_values_where_they_are_written() {
        use php_unit::decl::ConstExpr;
        let mut strings = php_heap::Strings::new();
        let options = Options {
            persistent: indexed(vec![
                ConstDecl {
                    name: Name(b"T_LNUMBER".to_vec().into_boxed_slice()),
                    value: ConstExpr::long(260),
                    attributes: Vec::new(),
                },
                ConstDecl {
                    name: Name(b"PHP_OS".to_vec().into_boxed_slice()),
                    value: ConstExpr::Str(b"Darwin".to_vec().into_boxed_slice()),
                    attributes: Vec::new(),
                },
            ]),
            ..Options::default()
        };
        let compiled = compile_parsed(
            b"<?php echo T_LNUMBER - 1, \\PHP_OS;",
            &options,
            &mut strings,
        );
        assert!(compiled.is_ok(), "{compiled:?}");
        let unit = compiled.unwrap_or_default();
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=3, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
             0000 ECHO int(259)\n\
             0001 ECHO string(\"Darwin\")\n\
             0002 RETURN int(1)\n"
        );
        assert_eq!(unit.main.literals[0].as_long(), Some(259));
        assert_eq!(
            unit.main.literals[1].as_str().map(|s| s.bytes().to_vec()),
            Some(b"Darwin".to_vec())
        );
        let lowered = compile_parsed(b"<?php echo t_lnumber;", &options, &mut strings);
        assert!(lowered.is_ok(), "{lowered:?}");
        assert_eq!(
            lowered.unwrap_or_default().dump(&[]).text(),
            "\n$_main:\n     ; (lines=3, args=0, vars=0, tmps=1)\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
             0000 T0 = FETCH_CONSTANT string(\"t_lnumber\")\n\
             0001 ECHO T0\n\
             0002 RETURN int(1)\n",
            "a constant's name is case-sensitive, so the lowered spelling folds to nothing \
             and is fetched as the program serves"
        );
    }

    /// `zend_try_ct_eval_magic_const`'s `T_DIR` arm (#366): `__DIR__` is
    /// `zend_dirname` over the file's name, and where that answers `.`,
    /// the working directory of the request, `VCWD_GETCWD`, which is per
    /// request under ZTS and so the option's, never the process's. Two
    /// units compiled for two requests in one process read two
    /// directories.
    #[test]
    fn dir_is_the_option_s_working_directory_where_the_name_has_no_directory() {
        fn dir(filename: &[u8], cwd: &[u8]) -> Option<Vec<u8>> {
            let mut strings = php_heap::Strings::new();
            let options = Options {
                filename,
                cwd,
                ..Options::default()
            };
            let unit =
                compile_parsed(b"<?php echo __DIR__;", &options, &mut strings).unwrap_or_default();
            literal_text(&unit, 0)
        }
        assert_eq!(
            dir(b"witness.php", b"/srv/one").as_deref(),
            Some(b"/srv/one".as_slice())
        );
        assert_eq!(
            dir(b"witness.php", b"/srv/two").as_deref(),
            Some(b"/srv/two".as_slice()),
            "the same name under another request's directory is that directory"
        );
        assert_eq!(
            dir(b"/var/www/index.php", b"/srv/one").as_deref(),
            Some(b"/var/www".as_slice()),
            "a name with a directory in it is its dirname, whatever the request's"
        );
    }

    #[test]
    fn a_modulo_and_a_power_over_constants_fold_the_way_php_folds_them() {
        assert_eq!(echoed_literal(b"<?php echo 7 % 2;").as_long(), Some(1));
        assert_eq!(echoed_literal(b"<?php echo -7 % 2;").as_long(), Some(-1));
        assert_eq!(echoed_literal(b"<?php echo 7 % -2;").as_long(), Some(1));
        // `zend_is_op_long_compatible`: a double or a numeric string
        // that is a whole number in range casts to its long and folds.
        assert_eq!(echoed_literal(b"<?php echo 2.0 % 1;").as_long(), Some(0));
        assert_eq!(echoed_literal(b"<?php echo 7 % 2.0;").as_long(), Some(1));
        assert_eq!(
            echoed_literal(b"<?php echo \"1.0\" % 1;").as_long(),
            Some(0)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \"1e3\" % 7;").as_long(),
            Some(6)
        );
        assert_eq!(echoed_literal(b"<?php echo 2 ** 3;").as_long(), Some(8));
        assert_eq!(
            echoed_literal(b"<?php echo 2 ** 3 ** 2;").as_long(),
            Some(512),
            "`**` reads right to left"
        );
        assert_eq!(
            echoed_literal(b"<?php echo -2 ** 2;").as_long(),
            Some(-4),
            "`**` binds tighter than unary `-`"
        );
        assert_eq!(
            echoed_literal(b"<?php echo 2 ** -1;").as_double(),
            Some(0.5)
        );
        // `pow_function_base`: the step that overflows leaves php's double.
        assert_eq!(
            echoed_literal(b"<?php echo 2 ** 63;").as_double(),
            Some(9_223_372_036_854_775_808_f64)
        );
    }

    /// `zend_try_ct_eval_binary_op` folds `+` over two constant arrays, the
    /// one pair of arrays `zend_binary_op_produces_error` lets through, into
    /// the array `add_function` makes, wherever the sum stands: assigned,
    /// under `isset` and under `empty`, which php compiles as the `!` of it;
    /// `-` over two arrays and `+` over an array and a number stay the
    /// opcodes, which throw. The dump is the oracle's under `[regime.dump]` at
    /// 8.5.9 (#2682).
    #[test]
    fn two_constant_arrays_under_a_plus_fold_to_the_array_php_s_add_makes() {
        assert_eq!(
            dump_of(b"<?php $p = [0, 1] + [2]; $q = [] + []; var_dump(isset(([0, 1] + [])[0]), empty([] + []), [1] - [1], [1] + 1);"),
            "\n$_main:\n\
             \x20    ; (lines=12, args=0, vars=2, tmps=6)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 ASSIGN CV0($p) array(...)\n\
             0001 ASSIGN CV1($q) array(...)\n\
             0002 INIT_FCALL 4 144 string(\"var_dump\")\n\
             0003 T4 = ISSET_ISEMPTY_DIM_OBJ (isset) array(...) int(0)\n\
             0004 SEND_VAL T4 1\n\
             0005 SEND_VAL bool(true) 2\n\
             0006 T5 = SUB array(...) array(...)\n\
             0007 SEND_VAL T5 3\n\
             0008 T6 = ADD array(...) int(1)\n\
             0009 SEND_VAL T6 4\n\
             0010 DO_ICALL\n\
             0011 RETURN int(1)\n\
             "
        );
    }

    #[test]
    fn arithmetic_php_does_not_fold_is_emitted_as_the_opcode_php_emits() {
        assert_eq!(
            dump_of(b"<?php echo 1 + extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 V0 = DO_ICALL\n\
                 0003 T1 = ADD int(1) V0\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            ),
            "php's own op array, its literals in the order the oplines name them"
        );
        // `zend_binary_op_produces_error`: a string that opens with no
        // number, or carries anything past the one it opens with, is left
        // to the opcode, which throws or warns where it stands.
        // The return's `1` is a slot of its own after the operands',
        // even beside an operand that is the same `1`, as
        // `zend_add_literal` appends it.
        for (source, left, right) in [
            (&b"<?php echo \"abc\" + 1;"[..], "string(\"abc\")", "int(1)"),
            (b"<?php echo \"1abc\" + 1;", "string(\"1abc\")", "int(1)"),
            (b"<?php echo \"0x1A\" + 1;", "string(\"0x1A\")", "int(1)"),
            (b"<?php echo \"1e\" + 1;", "string(\"1e\")", "int(1)"),
            (b"<?php echo 1 + \"\";", "int(1)", "string(\"\")"),
        ] {
            let (unit, _strings) = unit_and_strings(source);
            assert_eq!(
                unit.dump(&[]).text(),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = ADD {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
            assert_eq!(
                unit.main.oplines[2].op1,
                2,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // The divisor php's `zval_get_double` reads as zero, whatever
        // its type, and the negative power of a zero.
        for (source, divisor) in [
            (&b"<?php echo 2 / \"0\";"[..], "string(\"0\")"),
            (b"<?php echo 2 / \"0.0\";", "string(\"0.0\")"),
            (b"<?php echo 2 / false;", "bool(false)"),
            (b"<?php echo 2 / null;", "null"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = DIV int(2) {divisor}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `-"abc"` is the multiplication by -1 php emits for an operand
        // it will not fold.
        assert_eq!(
            dump_of(b"<?php echo -\"1abc\";"),
            main_dump(
                3,
                0,
                1,
                "0000 T0 = MUL string(\"1abc\") int(-1)\n0001 ECHO T0\n0002 RETURN int(1)\n"
            )
        );
        // `zend_binary_op_produces_error`: the division php refuses to
        // fold, so that `DivisionByZeroError` is thrown where it stands.
        assert_eq!(
            dump_of(b"<?php echo 1/0;"),
            main_dump(
                3,
                0,
                1,
                "0000 T0 = DIV int(1) int(0)\n0001 ECHO T0\n0002 RETURN int(1)\n"
            )
        );
        // `zend_compile_unary_pm`: the multiplication by -1 php emits
        // where the operand is not a constant.
        assert_eq!(
            dump_of(b"<?php echo -extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 V0 = DO_ICALL\n\
                 0003 T1 = MUL V0 int(-1)\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            )
        );
    }

    #[test]
    fn a_modulo_and_a_power_php_does_not_fold_are_mod_and_pow_where_they_stand() {
        // The modulo php refuses to fold, by whatever `zval_get_long`
        // reads as zero and over an operand `zend_is_op_long_compatible`
        // refuses, and the negative power of a zero: each is the opcode,
        // to throw or deprecate where it stands.
        for (source, left, right) in [
            (&b"<?php echo 1 % 0;"[..], "int(1)", "int(0)"),
            (b"<?php echo 1 % \"0\";", "int(1)", "string(\"0\")"),
            (b"<?php echo 1 % null;", "int(1)", "null"),
            (b"<?php echo 1.5 % 1;", "float(1.5)", "int(1)"),
            (b"<?php echo \"1.5\" % 1;", "string(\"1.5\")", "int(1)"),
            (b"<?php echo 1e19 % 1;", "float(1e+19)", "int(1)"),
            (
                b"<?php echo \"9223372036854775808\" % 1;",
                "string(\"9223372036854775808\")",
                "int(1)",
            ),
            (b"<?php echo INF % 1;", "float(inf)", "int(1)"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = MOD {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for (source, base) in [
            (&b"<?php echo 0 ** -1;"[..], "int(0)"),
            (b"<?php echo \"0\" ** -1;", "string(\"0\")"),
            (b"<?php echo 0.0 ** -1;", "float(0)"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 T0 = POW {base} int(-1)\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `%` and `**` over a call, php's `MOD` and `POW` where `ADD`
        // stood above.
        for (source, opcode, left) in [
            (
                &b"<?php echo 7 % extension_loaded('Core');"[..],
                "MOD",
                "int(7)",
            ),
            (
                b"<?php echo 2 ** extension_loaded('Core');",
                "POW",
                "int(2)",
            ),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    6,
                    0,
                    2,
                    &format!(
                        "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                         0001 SEND_VAL string(\"Core\") 1\n\
                         0002 V0 = DO_ICALL\n\
                         0003 T1 = {opcode} {left} V0\n\
                         0004 ECHO T1\n\
                         0005 RETURN int(1)\n"
                    )
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `shift_left_function` and `shift_right_function` over two
    /// constants, as php folds them: the shift over the longs both sides
    /// are worth, zero or the sign past the long's width, and the opcode
    /// left standing for a negative count, an operand
    /// `zend_is_op_long_compatible` refuses, or a call.
    #[test]
    fn a_shift_over_constants_folds_the_way_php_folds_it_and_the_rest_is_sl_or_sr() {
        assert_eq!(echoed_literal(b"<?php echo 1 << 3;").as_long(), Some(8));
        assert_eq!(echoed_literal(b"<?php echo 1 << 64;").as_long(), Some(0));
        assert_eq!(echoed_literal(b"<?php echo -1 >> 64;").as_long(), Some(-1));
        assert_eq!(echoed_literal(b"<?php echo 8 >> 1;").as_long(), Some(4));
        assert_eq!(echoed_literal(b"<?php echo \"2\" << 1;").as_long(), Some(4));
        assert_eq!(echoed_literal(b"<?php echo 1 << 2.0;").as_long(), Some(4));
        assert_eq!(
            echoed_literal(b"<?php echo 1 + 2 << 3;").as_long(),
            Some(24),
            "`+` binds tighter than `<<`"
        );
        for (source, opcode, left, right) in [
            (&b"<?php echo 1 << -1;"[..], "SL", "int(1)", "int(-1)"),
            (b"<?php echo 1 >> -1;", "SR", "int(1)", "int(-1)"),
            (b"<?php echo 1.5 << 1;", "SL", "float(1.5)", "int(1)"),
            (b"<?php echo 1 << \"1x\";", "SL", "int(1)", "string(\"1x\")"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!(
                        "0000 T0 = {opcode} {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n"
                    )
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        assert_eq!(
            dump_of(b"<?php echo 1 << extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 V0 = DO_ICALL\n\
                 0003 T1 = SL int(1) V0\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            )
        );
    }

    /// The text of the one constant a single-expression `echo` leaves,
    /// read while the table it was interned into still stands.
    fn echoed_string(source: &[u8]) -> Vec<u8> {
        let (unit, _strings) = unit_and_strings(source);
        literal_text(&unit, 0).unwrap_or_default()
    }

    /// The text of the constant at `at`, for a string; nothing otherwise.
    fn literal_text(unit: &Unit, at: usize) -> Option<Vec<u8>> {
        unit.main
            .literals
            .get(at)
            .and_then(|z| z.as_str())
            .map(|s| s.bytes().to_vec())
    }

    /// `zend_compile_binary_op`'s `ZEND_CONCAT` arm: the fold over two
    /// constants through `concat_function`, each constant converted as
    /// `_convert_to_string` converts it, so the op array is php's single
    /// `ECHO` of the folded string; and a constant beside what does not
    /// fold converted where it stands, so the `CONCAT` reads a string.
    #[test]
    fn a_concatenation_over_constants_folds_to_php_s_one_string_and_the_rest_is_concat() {
        for (source, joined) in [
            (&b"<?php echo 'a' . 'b';"[..], "ab"),
            (b"<?php echo \"1\" . 2;", "12"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    2,
                    0,
                    0,
                    &format!("0000 ECHO string(\"{joined}\")\n0001 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        assert_eq!(echoed_string(b"<?php echo 'a' . 'b';"), b"ab");
        assert_eq!(echoed_string(b"<?php echo \"1\" . 2;"), b"12");
        assert_eq!(echoed_string(b"<?php echo 2.5 . 'x';"), b"2.5x");
        assert_eq!(echoed_string(b"<?php echo 1.0 . 'y';"), b"1y");
        assert_eq!(
            echoed_string(b"<?php echo 1 / 3 . '';"),
            b"0.33333333333333"
        );
        assert_eq!(echoed_string(b"<?php echo true . false . null;"), b"1");
        assert_eq!(echoed_string(b"<?php echo 'a' . 'b' . 'c';"), b"abc");
        assert_eq!(
            echoed_string(b"<?php echo 1 . 2 << 3;"),
            b"116",
            "`<<` binds tighter than `.`"
        );
        assert_eq!(
            echoed_literal(b"<?php echo (1 . 2) + 3;").as_long(),
            Some(15),
            "a folded `.` is a constant to the arithmetic around it"
        );
        // The precision the double is rendered at is the compile-time
        // ini entry, as `zend_double_to_str` reads `EG(precision)`.
        let mut strings = php_heap::Strings::new();
        let unit = compile_parsed(
            b"<?php echo 1 / 3 . '';",
            &Options {
                precision: 3,
                ..Options::default()
            },
            &mut strings,
        )
        .unwrap_or_default();
        assert_eq!(literal_text(&unit, 0), Some(b"0.333".to_vec()));
        // A constant beside a CV is converted and the `CONCAT` stands.
        let (unit, _strings) = unit_and_strings(
            b"<?php $a = 1; echo 2 . $a; echo $a . 2.5; echo 'a' . 'b' . $a; echo NAN . $a;",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                10,
                1,
                5,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 T2 = CONCAT string(\"2\") CV0($a)\n\
                 0002 ECHO T2\n\
                 0003 T3 = CONCAT CV0($a) string(\"2.5\")\n\
                 0004 ECHO T3\n\
                 0005 T4 = CONCAT string(\"ab\") CV0($a)\n\
                 0006 ECHO T4\n\
                 0007 T5 = CONCAT string(\"NAN\") CV0($a)\n\
                 0008 ECHO T5\n\
                 0009 RETURN int(1)\n"
            ),
            "php's own op array"
        );
        assert_eq!(
            literal_text(&unit, 1),
            Some(b"2".to_vec()),
            "the long is the string `2`"
        );
        assert_eq!(
            literal_text(&unit, 2),
            Some(b"2.5".to_vec()),
            "the double is the string `2.5`"
        );
        assert_eq!(
            literal_text(&unit, 3),
            Some(b"ab".to_vec()),
            "the left fold is one string"
        );
        assert_eq!(
            literal_text(&unit, 4),
            Some(b"NAN".to_vec()),
            "the NAN is the string `zend_double_to_str` renders"
        );
    }

    /// `zend_double_to_str` over a NAN raises the coercion warning while
    /// the file compiles, and the unit carries it as opcache carries a
    /// `persistent_script`'s `warnings`: `E_WARNING`, php's text, at the
    /// line the fold stood on, once per operand converted and in the
    /// order they were, with the folded string in the op array. A unit
    /// with nothing raised carries nothing.
    #[test]
    fn a_nan_a_fold_converts_is_the_warning_the_unit_carries() {
        use php_unit::{Diagnostic, Level};
        let (unit, _strings) =
            unit_and_strings(b"<?php\necho 'a';\necho NAN . 'z';\necho 1 . NAN . NAN;");
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=4, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-4\n     ; return  [] RANGE[0..0]\n\
             0000 ECHO string(\"a\")\n\
             0001 ECHO string(\"NANz\")\n\
             0002 ECHO string(\"1NANNAN\")\n\
             0003 RETURN int(1)\n"
        );
        let coerced = |line| Diagnostic {
            level: Level::Warning,
            line,
            message: "unexpected NAN value was coerced to string".into(),
        };
        assert_eq!(
            unit.diagnostics,
            vec![coerced(3), coerced(4), coerced(4)],
            "one per operand converted, at the statement's line"
        );
        assert!(unit_of(b"<?php echo 'a' . 'b';").diagnostics.is_empty());
        assert!(
            unit_of(b"<?php echo NAN;").diagnostics.is_empty(),
            "an `ECHO` over a NAN converts nothing at compile time; the kernel warns"
        );
    }

    /// `zend_compile_cast` under the dump recipe: `CAST` with php's type
    /// constant over the operand into a temporary, `BOOL` for `(bool)`,
    /// nothing folded and the `FREE` after one nothing reads, which
    /// `zend_do_free` leaves off a `BOOL`; and the scanner's escapes as
    /// the bytes php reads.
    #[test]
    fn a_cast_is_php_s_opline_and_an_escape_is_php_s_byte() {
        assert_eq!(
            dump_of(
                b"<?php\n$x = 1;\n(integer)$x;\n(float)1;\n$s = (string)$x;\n(boolean)$x;\necho (int)\"12abc\", \"\\777\", NAN . \"z\";\n",
            ),
            "\n$_main:\n     ; (lines=13, args=0, vars=2, tmps=7)\n\
             \x20    ; (before optimizer)\n     ; :1-8\n     ; return  [] RANGE[0..0]\n\
             0000 ASSIGN CV0($x) int(1)\n\
             0001 T3 = CAST (long) CV0($x)\n\
             0002 FREE T3\n\
             0003 T4 = CAST (double) int(1)\n\
             0004 FREE T4\n\
             0005 T5 = CAST (string) CV0($x)\n\
             0006 ASSIGN CV1($s) T5\n\
             0007 T7 = BOOL CV0($x)\n\
             0008 T8 = CAST (long) string(\"12abc\")\n\
             0009 ECHO T8\n\
             0010 ECHO string(\"\\xFF\")\n\
             0011 ECHO string(\"NANz\")\n\
             0012 RETURN int(1)\n"
        );
    }

    /// `zend_compile_cast`'s other two arms: `(array)` and `(object)` are
    /// `CAST` with `IS_ARRAY` and `IS_OBJECT` in `extended_value`, which
    /// the dump prints as php's does.
    #[test]
    fn an_array_and_an_object_cast_are_php_s_oplines() {
        assert_eq!(
            dump_of(b"<?php\n$x = 1;\n$a = (array)$x;\n$o = (object)$a;\n"),
            "\n$_main:\n     ; (lines=6, args=0, vars=3, tmps=5)\n\
             \x20    ; (before optimizer)\n     ; :1-5\n     ; return  [] RANGE[0..0]\n\
             0000 ASSIGN CV0($x) int(1)\n\
             0001 T4 = CAST (array) CV0($x)\n\
             0002 ASSIGN CV1($a) T4\n\
             0003 T6 = CAST (object) CV1($a)\n\
             0004 ASSIGN CV2($o) T6\n\
             0005 RETURN int(1)\n"
        );
    }

    /// The scanner's deprecations and warning stand on the unit before
    /// the compiler's own warning, each at its line, as php raised them:
    /// the whole file is scanned before `zend_compile_top_stmt` runs.
    #[test]
    fn the_scanners_diagnostics_stand_on_the_unit_before_the_compilers() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\n$x = 1;\n(integer)$x;\n(float)1;\n$s = (string)$x;\n(boolean)$x;\necho (int)\"12abc\", \"\\777\", NAN . \"z\";\n",
        );
        let raised = |level, line, message: &str| Diagnostic {
            level,
            line,
            message: message.into(),
        };
        assert_eq!(
            unit.diagnostics,
            vec![
                raised(
                    Level::Deprecated,
                    3,
                    "Non-canonical cast (integer) is deprecated, use the (int) cast instead"
                ),
                raised(
                    Level::Deprecated,
                    6,
                    "Non-canonical cast (boolean) is deprecated, use the (bool) cast instead"
                ),
                raised(
                    Level::CompileWarning,
                    7,
                    "Octal escape sequence overflow \\777 is greater than \\377"
                ),
                raised(
                    Level::Warning,
                    7,
                    "unexpected NAN value was coerced to string"
                ),
            ]
        );
    }

    /// A fatal carries what was raised before it: the scanner's warning
    /// up to the token the parser refused, and none past it, since php's
    /// scanner never reached there; every one of them for the `(unset)`
    /// cast, whose `E_COMPILE_ERROR` php raises once the file has parsed.
    #[test]
    fn a_fatal_carries_what_the_scanner_raised_before_it() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &Options::default(), &mut strings).err()
        };
        let overflow = |line| Diagnostic {
            level: Level::CompileWarning,
            line,
            message: "Octal escape sequence overflow \\777 is greater than \\377".into(),
        };
        assert_eq!(
            refused(b"<?php\necho \"\\777\";\necho 1 +;\necho \"\\777\";"),
            Some(CompileError {
                level: Level::Parse,
                message: "syntax error, unexpected token \";\"".into(),
                line: 3,
                raised: vec![overflow(2)],
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            })
        );
        assert_eq!(
            refused(b"<?php\necho \"\\777\";\n(unset)$x;\necho \"\\777\";"),
            Some(CompileError {
                level: Level::CompileError,
                message: "The (unset) cast is no longer supported".into(),
                line: 3,
                raised: vec![overflow(2), overflow(4)],
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            })
        );
    }

    /// `zend_compile_for` over `for (;;) {}`, php's own oplines under
    /// the dump recipe: the `JMP` forward to the condition, and the
    /// condition, the constant `true` for an empty one, as the `JMPNZ`
    /// back to the body's first opline, which is itself here. php folds
    /// nothing, so the back-edge is a `JMPNZ` and not a `JMP`.
    #[test]
    fn an_empty_for_is_php_s_forward_jump_and_a_conditional_jump_to_itself() {
        let (unit, _strings) = unit_and_strings(b"<?php set_time_limit(1); for (;;) {}");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                6,
                0,
                1,
                "0000 INIT_FCALL 1 96 string(\"set_time_limit\")\n\
                 0001 SEND_VAL int(1) 1\n\
                 0002 DO_ICALL\n\
                 0003 JMP 0004\n\
                 0004 JMPNZ bool(true) 0004\n\
                 0005 RETURN int(1)\n"
            ),
            "the oracle's dump, byte for byte"
        );
        assert_eq!(
            unit.main.oplines[3].op1, 4,
            "the JMP lands on the condition"
        );
        assert_eq!(
            unit.main.oplines[4].op2, 4,
            "the JMPNZ lands on the body's first opline, which is itself"
        );
        assert_eq!(
            unit.main.literals[2],
            php_heap::Zval::bool(true),
            "an empty condition is php's constant true"
        );
        assert_eq!(
            &*unit.main.safepoints,
            &[OplineIdx(0), OplineIdx(4), OplineIdx(5)],
            "the entry, the header the back-edge lands on, which is itself, the return"
        );
        assert_eq!(
            unit.main.oplines[4].out, 1,
            "the back-edge carries the header's number"
        );
        assert_eq!(unit.main.oplines[5].out, 2, "the return's");
    }

    /// The three expression lists and the body, in the order php lays
    /// them down: initialisers, the jump, the body, the step, then the
    /// condition, whose last expression is what the `JMPNZ` tests. The
    /// oracle's dump of this source, under the recipe, is the same
    /// sixteen oplines with `V3 = DO_ICALL` at 13, the one call whose
    /// value is read.
    #[test]
    fn a_for_lays_its_lists_down_in_php_s_order() {
        let (unit, _strings) = unit_and_strings(
            b"<?php for (extension_loaded('i'); extension_loaded('a'), extension_loaded('b'); extension_loaded('s')) echo 'body';",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                16,
                0,
                4,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"i\") 1\n\
                 0002 DO_ICALL\n\
                 0003 JMP 0008\n\
                 0004 ECHO string(\"body\")\n\
                 0005 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0006 SEND_VAL string(\"s\") 1\n\
                 0007 DO_ICALL\n\
                 0008 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0009 SEND_VAL string(\"a\") 1\n\
                 0010 DO_ICALL\n\
                 0011 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0012 SEND_VAL string(\"b\") 1\n\
                 0013 V3 = DO_ICALL\n\
                 0014 JMPNZ V3 0004\n\
                 0015 RETURN int(1)\n"
            )
        );
        assert_eq!(
            unit.main.oplines[3].op1, 8,
            "the JMP lands on the condition"
        );
        assert_eq!(unit.main.oplines[14].op2, 4, "the JMPNZ lands on the body");
    }

    /// php's own loop, the shape #156's bench runs and the witness beside
    /// `php_heap::Str::bytes` is, under the dump recipe: the `$i++` in
    /// the step is `PRE_INC` with its temporary unread, which is
    /// `zend_do_free`'s conversion, and the condition is `IS_SMALLER`
    /// into the temporary the `JMPNZ` tests, `T3 = ` before it.
    #[test]
    fn php_s_own_for_loop_is_an_is_smaller_and_a_pre_inc() {
        let (unit, _strings) =
            unit_and_strings(b"<?php for ($i = 0; $i < 1000; $i++) { echo 'dax'; }");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                7,
                1,
                3,
                "0000 ASSIGN CV0($i) int(0)\n\
                 0001 JMP 0004\n\
                 0002 ECHO string(\"dax\")\n\
                 0003 PRE_INC CV0($i)\n\
                 0004 T3 = IS_SMALLER CV0($i) int(1000)\n\
                 0005 JMPNZ T3 0002\n\
                 0006 RETURN int(1)\n"
            )
        );
        assert_eq!(unit.main.oplines[3].op2_type, OperandKind::Unused);
        assert_eq!(unit.main.oplines[3].out, 1, "the increment's temporary");
        assert_eq!(
            unit.main.oplines[4].out, 2,
            "the comparison's, which the JMPNZ tests"
        );
        assert_eq!(
            &*unit.main.safepoints,
            &[OplineIdx(0), OplineIdx(2), OplineIdx(6)],
            "the entry, the body's first opline as the header, the return"
        );
        assert_eq!(
            unit.main.oplines[5].out, 1,
            "the JMPNZ carries the header's number, not its own opline's"
        );
    }

    /// One rule for `OpArray.safepoints` whatever the loop (#353): the
    /// entry a backward jump names holds the opline it lands on. A
    /// `while`'s `JMPNZ` lands on the body's first opline and a
    /// `foreach`'s `JMP`, and the `continue` inside it, land on the
    /// `FE_FETCH_R`; each entry is that header and never the jump.
    #[test]
    fn a_while_and_a_foreach_with_a_continue_number_their_headers_under_one_rule() {
        let (unit, _strings) = unit_and_strings(b"<?php while ($a) { echo 1; }");
        assert_eq!(
            unit.main.oplines[2].opcode,
            Opcode::Jmpnz,
            "the condition is the back-edge"
        );
        assert_eq!(
            unit.main.oplines[2].op2, 1,
            "landing on the body's first opline"
        );
        assert_eq!(
            &*unit.main.safepoints,
            &[OplineIdx(0), OplineIdx(1), OplineIdx(3)],
            "the entry, the header at the ECHO, the return"
        );
        assert_eq!(
            unit.main.oplines[2].out, 1,
            "the JMPNZ carries the header's number"
        );

        let (unit, _strings) = unit_and_strings(b"<?php foreach ($a as $v) { continue; }");
        assert_eq!(
            &*unit.main.safepoints,
            &[OplineIdx(0), OplineIdx(1), OplineIdx(5)],
            "the entry, the header at the FE_FETCH_R, the return"
        );
        let back_edges: Vec<(Opcode, u32, u32)> = [2, 3]
            .iter()
            .map(|&at| {
                let op = unit.main.oplines[at];
                (op.opcode, op.op1, op.out)
            })
            .collect();
        assert_eq!(
            back_edges,
            vec![(Opcode::Jmp, 1, 1), (Opcode::Jmp, 1, 1)],
            "the continue and the loop's own JMP land on the header and carry its one number"
        );
    }

    /// The oracle's dump of two arrow functions returning `never`
    /// (#1262): `zend_compile_func_decl` wraps neither body in a
    /// `return`, so the `throw` stands as an expression statement and
    /// `42`, a constant, leaves no opline; each ends in the implicit
    /// `VERIFY_NEVER_TYPE`.
    #[test]
    fn an_arrow_function_returning_never_compiles_its_body_as_a_statement() {
        let dump = dump_of(
            b"<?php\n$a = fn(): never => throw new Exception(\"Here\");\n$b = fn(): never => 42;\n",
        );
        let closures: Vec<&str> = dump.split("\n\n").skip(1).collect();
        assert_eq!(closures.len(), 2, "{dump}");
        assert!(
            closures[0].ends_with(
                "     ; (lines=5, args=0, vars=0, tmps=2)\n\
                 \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
                 0000 V0 = NEW 1 string(\"Exception\")\n\
                 0001 SEND_VAL_EX string(\"Here\") 1\n\
                 0002 DO_FCALL\n\
                 0003 THROW V0\n\
                 0004 VERIFY_NEVER_TYPE\n\
                 LIVE RANGES:\n     0: 0001 - 0003 (new)"
            ),
            "{dump}"
        );
        assert!(
            closures[1].ends_with(
                "     ; (lines=1, args=0, vars=0, tmps=0)\n\
                 \x20    ; (before optimizer)\n     ; :3-3\n     ; return  [] RANGE[0..0]\n\
                 0000 VERIFY_NEVER_TYPE\n"
            ),
            "{dump}"
        );
    }

    /// The oracle's dump under the pin's `dump` regime, `vars=1, tmps=4`:
    /// a `<` over a CV is `IS_SMALLER` into a temporary, `$a++` and
    /// `++$a` whose values are read are `POST_INC` and `PRE_INC`, and a
    /// `<` over two constants folds to the bool `zend_compare` orders
    /// them to, a string pair and a double against a string included.
    #[test]
    fn a_comparison_is_is_smaller_where_it_does_not_fold_and_the_bool_where_it_does() {
        assert_eq!(
            dump_of(
                b"<?php echo $a < 2, 1 < $a, $a++, ++$a, 1 < 2, 1.5 < 2, \"a\" < \"b\", 2 < 1, \"10\" < \"9\", 1.5 < \"abc\", null < true;"
            ),
            main_dump(
                16,
                1,
                4,
                "0000 T1 = IS_SMALLER CV0($a) int(2)\n\
                 0001 ECHO T1\n\
                 0002 T2 = IS_SMALLER int(1) CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = POST_INC CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = PRE_INC CV0($a)\n\
                 0007 ECHO T4\n\
                 0008 ECHO bool(true)\n\
                 0009 ECHO bool(true)\n\
                 0010 ECHO bool(true)\n\
                 0011 ECHO bool(false)\n\
                 0012 ECHO bool(false)\n\
                 0013 ECHO bool(true)\n\
                 0014 ECHO bool(true)\n\
                 0015 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump, `vars=1, tmps=19`: `<=` is `IS_SMALLER_OR_EQUAL`
    /// as written, `>` and `>=` are `IS_SMALLER` and `IS_SMALLER_OR_EQUAL`
    /// with the operands swapped (`zend_compile_greater`), and the
    /// equality level is its own opcode each, `<>` being `IS_NOT_EQUAL`.
    /// `2 == $a` dumps as `IS_EQUAL CV0($a) int(2)`, and so do `2 * $a`
    /// and the other three equalities, since `pass_two` swaps the
    /// operands of a `COMMUTATIVE` opcode whose first is the lesser
    /// kind; `2 <=> $a` and `2 + $a` keep their order.
    #[test]
    fn the_rest_of_the_comparison_level_is_the_opcode_php_emits_with_php_s_operand_order() {
        assert_eq!(
            dump_of(
                b"<?php echo $a <= 2, 2 <= $a, $a > 2, 2 > $a, $a >= 2, 2 >= $a, $a == 2, 2 == $a, $a != 2, 2 != $a, $a <> 2, $a === 2, 2 === $a, $a !== 2, 2 !== $a, $a <=> 2, 2 <=> $a, 2 * $a, 2 + $a;"
            ),
            main_dump(
                39,
                1,
                19,
                "0000 T1 = IS_SMALLER_OR_EQUAL CV0($a) int(2)\n\
                 0001 ECHO T1\n\
                 0002 T2 = IS_SMALLER_OR_EQUAL int(2) CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = IS_SMALLER int(2) CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = IS_SMALLER CV0($a) int(2)\n\
                 0007 ECHO T4\n\
                 0008 T5 = IS_SMALLER_OR_EQUAL int(2) CV0($a)\n\
                 0009 ECHO T5\n\
                 0010 T6 = IS_SMALLER_OR_EQUAL CV0($a) int(2)\n\
                 0011 ECHO T6\n\
                 0012 T7 = IS_EQUAL CV0($a) int(2)\n\
                 0013 ECHO T7\n\
                 0014 T8 = IS_EQUAL CV0($a) int(2)\n\
                 0015 ECHO T8\n\
                 0016 T9 = IS_NOT_EQUAL CV0($a) int(2)\n\
                 0017 ECHO T9\n\
                 0018 T10 = IS_NOT_EQUAL CV0($a) int(2)\n\
                 0019 ECHO T10\n\
                 0020 T11 = IS_NOT_EQUAL CV0($a) int(2)\n\
                 0021 ECHO T11\n\
                 0022 T12 = IS_IDENTICAL CV0($a) int(2)\n\
                 0023 ECHO T12\n\
                 0024 T13 = IS_IDENTICAL CV0($a) int(2)\n\
                 0025 ECHO T13\n\
                 0026 T14 = IS_NOT_IDENTICAL CV0($a) int(2)\n\
                 0027 ECHO T14\n\
                 0028 T15 = IS_NOT_IDENTICAL CV0($a) int(2)\n\
                 0029 ECHO T15\n\
                 0030 T16 = SPACESHIP CV0($a) int(2)\n\
                 0031 ECHO T16\n\
                 0032 T17 = SPACESHIP int(2) CV0($a)\n\
                 0033 ECHO T17\n\
                 0034 T18 = MUL CV0($a) int(2)\n\
                 0035 ECHO T18\n\
                 0036 T19 = ADD int(2) CV0($a)\n\
                 0037 ECHO T19\n\
                 0038 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump, `vars=0, tmps=0`: every comparison over two
    /// constants folds, to the bool `zend_compare` orders them to, the
    /// long `compare_function` writes for `<=>`, and `zend_is_identical`
    /// for `===` and `!==`, which reads the type before the value, so
    /// `1 === "1"` and `null === false` are false where `==` says true.
    #[test]
    fn every_comparison_over_two_constants_folds_to_the_value_php_folds_it_to() {
        assert_eq!(
            dump_of(
                b"<?php echo 1 <= 2, 2 <= 1, 1 > 2, 2 > 1, 1 >= 1, 1 == \"1\", 1 != \"1\", \"abc\" == \"ABC\", 1 === 1, 1 === \"1\", 1 !== 1.0, 1 <=> 2, 2 <=> 1, 1 <=> 1, \"10\" <=> \"9\", 1.5 <=> \"abc\", null == false, null === false, \"1e3\" == \"1000\", \"abc\" <= \"abd\", 1.5 >= \"1.5\", 2 > \"10\";"
            ),
            main_dump(
                23,
                0,
                0,
                "0000 ECHO bool(true)\n\
                 0001 ECHO bool(false)\n\
                 0002 ECHO bool(false)\n\
                 0003 ECHO bool(true)\n\
                 0004 ECHO bool(true)\n\
                 0005 ECHO bool(true)\n\
                 0006 ECHO bool(false)\n\
                 0007 ECHO bool(false)\n\
                 0008 ECHO bool(true)\n\
                 0009 ECHO bool(false)\n\
                 0010 ECHO bool(true)\n\
                 0011 ECHO int(-1)\n\
                 0012 ECHO int(1)\n\
                 0013 ECHO int(0)\n\
                 0014 ECHO int(1)\n\
                 0015 ECHO int(-1)\n\
                 0016 ECHO bool(true)\n\
                 0017 ECHO bool(false)\n\
                 0018 ECHO bool(true)\n\
                 0019 ECHO bool(true)\n\
                 0020 ECHO bool(true)\n\
                 0021 ECHO bool(false)\n\
                 0022 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump, `vars=2, tmps=7`: a `===` or `!==` against a
    /// constant null, false or true on either side is the `TYPE_CHECK`
    /// `zend_compile_binary_op` emits in its place, the constant's
    /// `1 << IS_*` in `extended_value` for `===` and `MAY_BE_ANY` less
    /// that bit for `!==`; against any other constant, or a variable,
    /// it is the opcode itself, and a `==` against null is `IS_EQUAL`.
    #[test]
    fn an_identity_against_null_or_a_bool_is_the_type_check_php_emits_in_its_place() {
        assert_eq!(
            dump_of(
                b"<?php echo $a === null, null === $a, $a !== false, true !== $a, $a === 1, $a == null, $a !== $b;"
            ),
            main_dump(
                15,
                2,
                7,
                "0000 T2 = TYPE_CHECK (null) CV0($a)\n\
                 0001 ECHO T2\n\
                 0002 T3 = TYPE_CHECK (null) CV0($a)\n\
                 0003 ECHO T3\n\
                 0004 T4 = TYPE_CHECK TYPE [null, true, long, double, string, array, object, resource] CV0($a)\n\
                 0005 ECHO T4\n\
                 0006 T5 = TYPE_CHECK TYPE [null, false, long, double, string, array, object, resource] CV0($a)\n\
                 0007 ECHO T5\n\
                 0008 T6 = IS_IDENTICAL CV0($a) int(1)\n\
                 0009 ECHO T6\n\
                 0010 T7 = IS_EQUAL CV0($a) null\n\
                 0011 ECHO T7\n\
                 0012 T8 = IS_NOT_IDENTICAL CV0($a) CV1($b)\n\
                 0013 ECHO T8\n\
                 0014 RETURN int(1)\n"
            )
        );
    }

    /// `zend_do_free` over every place php frees a value: an expression
    /// statement, each expression of a `for`'s three lists but the last
    /// of the condition, and the last of the initialisers and the step.
    /// Each `$a++` there is php's `PRE_INC`; the one an `ASSIGN` reads
    /// stays `POST_INC`. The oracle's dump is the same thirteen oplines,
    /// `vars=2, tmps=10`.
    #[test]
    fn a_post_inc_nothing_reads_is_the_pre_inc_zend_do_free_makes_it() {
        assert_eq!(
            dump_of(b"<?php $a++; ++$a; $b = $a++; for ($a++, $b++; $a++, $a < 1; $a++, ++$a) {}"),
            main_dump(
                13,
                2,
                10,
                "0000 PRE_INC CV0($a)\n\
                 0001 PRE_INC CV0($a)\n\
                 0002 T4 = POST_INC CV0($a)\n\
                 0003 ASSIGN CV1($b) T4\n\
                 0004 PRE_INC CV0($a)\n\
                 0005 PRE_INC CV1($b)\n\
                 0006 JMP 0009\n\
                 0007 PRE_INC CV0($a)\n\
                 0008 PRE_INC CV0($a)\n\
                 0009 PRE_INC CV0($a)\n\
                 0010 T11 = IS_SMALLER CV0($a) int(1)\n\
                 0011 JMPNZ T11 0007\n\
                 0012 RETURN int(1)\n"
            )
        );
    }

    /// The same over `--`: `$a--;` as a statement is php's `PRE_DEC
    /// CV0($a)`, `zend_do_free`'s conversion of the `POST_DEC` nothing
    /// reads, and the ones an `ASSIGN`, an `ECHO` or a comparison reads
    /// stay `POST_DEC`. The oracle's dump is the same fifteen oplines,
    /// `vars=2, tmps=10`.
    #[test]
    fn a_post_dec_nothing_reads_is_the_pre_dec_zend_do_free_makes_it() {
        assert_eq!(
            dump_of(b"<?php $a--; --$a; $b = $a--; echo --$a, $a--; for ($a--; $a-- > 1; --$a) {}"),
            main_dump(
                15,
                2,
                10,
                "0000 PRE_DEC CV0($a)\n\
                 0001 PRE_DEC CV0($a)\n\
                 0002 T4 = POST_DEC CV0($a)\n\
                 0003 ASSIGN CV1($b) T4\n\
                 0004 T6 = PRE_DEC CV0($a)\n\
                 0005 ECHO T6\n\
                 0006 T7 = POST_DEC CV0($a)\n\
                 0007 ECHO T7\n\
                 0008 PRE_DEC CV0($a)\n\
                 0009 JMP 0011\n\
                 0010 PRE_DEC CV0($a)\n\
                 0011 T10 = POST_DEC CV0($a)\n\
                 0012 T11 = IS_SMALLER int(1) T10\n\
                 0013 JMPNZ T11 0010\n\
                 0014 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_while`, and a block whose statements keep their own
    /// lines while the loop's own oplines keep the statement's, as php's
    /// `CG(zend_lineno)` is the condition's again once the condition is
    /// compiled; the final return takes the line the last opline left,
    /// as `zend_emit_final_return` does.
    #[test]
    fn a_while_is_the_same_shape_and_the_body_keeps_its_lines() {
        let (unit, _strings) =
            unit_and_strings(b"<?php\nwhile (extension_loaded('a')) {\n  echo 1;\n}");
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=7, args=0, vars=0, tmps=1)\n\
             \x20    ; (before optimizer)\n     ; :1-4\n     ; return  [] RANGE[0..0]\n\
             0000 JMP 0002\n\
             0001 ECHO int(1)\n\
             0002 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
             0003 SEND_VAL string(\"a\") 1\n\
             0004 V0 = DO_ICALL\n\
             0005 JMPNZ V0 0001\n\
             0006 RETURN int(1)\n"
        );
        assert_eq!(unit.main.oplines[0].op1, 2);
        assert_eq!(unit.main.oplines[5].op2, 1);
        let lines: Vec<u32> = unit.main.oplines.iter().map(|op| op.lineno).collect();
        // The implicit return on the file's last line, as phpdbg's `print`
        // of the oracle shows it: `L0004 0006 RETURN int(1)`.
        assert_eq!(lines, vec![2, 3, 2, 2, 2, 2, 4]);
    }

    /// A constant no table here holds was refused until #380 and is now
    /// `zend_compile_const`'s `FETCH_CONSTANT`, looked up as the program
    /// serves. The oracle's dump under `[regime.dump]` at 8.5.9 over
    /// `echo FOO;` is the same three oplines.
    #[test]
    fn a_constant_no_table_holds_is_fetched_as_the_program_serves() {
        let (unit, _strings) = unit_and_strings(b"<?php echo FOO;");
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=3, args=0, vars=0, tmps=1)\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
             0000 T0 = FETCH_CONSTANT string(\"FOO\")\n\
             0001 ECHO T0\n\
             0002 RETURN int(1)\n"
        );
    }

    /// `true`, `false` and `null` written with the leading `\` are the
    /// values `zend_try_ct_eval_const` substitutes, as they are without
    /// it: `zend_resolve_const_name` strips the `\` and
    /// `zend_get_special_const` reads the rest. The oracle's dump under
    /// `[regime.dump]` at 8.5.9 over the same source.
    #[test]
    fn a_special_constant_written_fully_qualified_is_substituted() {
        assert_eq!(
            dump_of(b"<?php echo \\true, \\NULL, \\False, TRUE;"),
            main_dump(
                5,
                0,
                0,
                "0000 ECHO bool(true)\n\
                 0001 ECHO null\n\
                 0002 ECHO bool(false)\n\
                 0003 ECHO bool(true)\n\
                 0004 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump under the pin's `dump` regime of
    /// `$a = 1; echo $a, -$a, $A; $c = $b = $a + 1;`, `vars=4, tmps=5`:
    ///
    /// ```text
    /// 0000 ASSIGN CV0($a) int(1)
    /// 0001 ECHO CV0($a)
    /// 0002 T5 = MUL CV0($a) int(-1)
    /// 0003 ECHO T5
    /// 0004 ECHO CV1($A)
    /// 0005 T6 = ADD CV0($a) int(1)
    /// 0006 T7 = ASSIGN CV3($b) T6
    /// 0007 ASSIGN CV2($c) T7
    /// 0008 RETURN int(1)
    /// ```
    ///
    /// `lookup_cv` numbers a name when it is first met, `$a` and `$A`
    /// apart, and `zend_compile_assign` looks the variable up before it
    /// compiles the value, so `$c` is met before `$b`. Each `ASSIGN`
    /// takes a temporary, read or not, which is the five php counts.
    #[test]
    fn a_variable_is_its_cv_and_an_assignment_is_php_s_assign_in_php_s_order() {
        use OperandKind::{Const, Cv, Tmp, Unused};
        let unit = unit_of(b"<?php $a = 1; echo $a, -$a, $A; $c = $b = $a + 1;");
        assert_eq!(
            unit.main.vars.iter().map(|v| &*v.0).collect::<Vec<_>>(),
            [b"a".as_slice(), b"A", b"c", b"b"]
        );
        assert_eq!(unit.main.num_cvs, 4);
        assert_eq!(unit.main.num_tmps, 5);
        let shape: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.op1_type, op.op1, op.op2_type, op.op2, op.out))
            .collect();
        assert_eq!(
            shape,
            [
                (Opcode::Assign, Cv, 0, Const, 0, 0),
                (Opcode::Echo, Cv, 0, Unused, 0, 0),
                (Opcode::Mul, Cv, 0, Const, 1, 1),
                (Opcode::Echo, Tmp, 1, Unused, 0, 0),
                (Opcode::Echo, Cv, 1, Unused, 0, 0),
                (Opcode::Add, Cv, 0, Const, 2, 2),
                (Opcode::Assign, Cv, 3, Tmp, 2, 3),
                (Opcode::Assign, Cv, 2, Tmp, 3, 4),
                (Opcode::Return, Const, 3, Unused, 0, 1),
            ]
        );
    }

    /// `zend_do_free` over the oracle's dump of `$a = 1; $a + 1;
    /// str_repeat("a", 3) . "b"; extension_loaded('Core'); $a++;`,
    /// `vars=1, tmps=6`:
    ///
    /// ```text
    /// 0000 ASSIGN CV0($a) int(1)
    /// 0001 T2 = ADD CV0($a) int(1)
    /// 0002 FREE T2
    /// 0003 INIT_FCALL 2 112 string("str_repeat")
    /// 0004 SEND_VAL string("a") 1
    /// 0005 SEND_VAL int(3) 2
    /// 0006 V3 = DO_ICALL
    /// 0007 T4 = CONCAT V3 string("b")
    /// 0008 FREE T4
    /// 0009 INIT_FCALL 1 96 string("extension_loaded")
    /// 0010 SEND_VAL string("Core") 1
    /// 0011 DO_ICALL
    /// 0012 PRE_INC CV0($a)
    /// 0013 RETURN int(1)
    /// ```
    ///
    /// The `ASSIGN`, the call and the `PRE_INC` are marked `UNUSED` in
    /// `out_type` and keep their numbers; the `ADD` and the `CONCAT`,
    /// which php does not mark, are followed by a `FREE` of their
    /// temporary.
    #[test]
    fn zend_do_free_marks_an_assign_a_call_and_a_pre_inc_and_frees_the_rest() {
        use OperandKind::{Tmp, Unused, Var};
        let (unit, _strings) = unit_and_strings(
            b"<?php $a = 1; $a + 1; str_repeat(\"a\", 3) . \"b\"; extension_loaded('Core'); $a++;",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                14,
                1,
                6,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 T2 = ADD CV0($a) int(1)\n\
                 0002 FREE T2\n\
                 0003 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0004 SEND_VAL string(\"a\") 1\n\
                 0005 SEND_VAL int(3) 2\n\
                 0006 V3 = DO_ICALL\n\
                 0007 T4 = CONCAT V3 string(\"b\")\n\
                 0008 FREE T4\n\
                 0009 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0010 SEND_VAL string(\"Core\") 1\n\
                 0011 DO_ICALL\n\
                 0012 PRE_INC CV0($a)\n\
                 0013 RETURN int(1)\n"
            )
        );
        let marks: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.out_type, op.out))
            .collect();
        assert_eq!(marks[0], (Opcode::Assign, Unused, 0), "numbered, unread");
        assert_eq!(marks[1], (Opcode::Add, Tmp, 1));
        assert_eq!(marks[2], (Opcode::Free, Unused, 0));
        assert_eq!(unit.main.oplines[2].op1_type, Tmp);
        assert_eq!(unit.main.oplines[2].op1, 1, "the ADD's temporary");
        assert_eq!(marks[6], (Opcode::DoIcall, Var, 2));
        assert_eq!(marks[11], (Opcode::DoIcall, Unused, 4), "numbered, unread");
        assert_eq!(marks[12], (Opcode::PreInc, Unused, 5), "numbered, unread");
        assert_eq!(
            unit.main.oplines[13].out_type, Unused,
            "a site keeps its number bare"
        );
    }

    /// `zend_calc_live_ranges` over the oracle's dump of
    /// `echo str_repeat("a", 2) . str_repeat("b", 2);`, `vars=0,
    /// tmps=3`, which ends in the live-range table:
    ///
    /// ```text
    /// 0000 INIT_FCALL 2 112 string("str_repeat")
    /// 0001 SEND_VAL string("a") 1
    /// 0002 SEND_VAL int(2) 2
    /// 0003 V0 = DO_ICALL
    /// 0004 INIT_FCALL 2 112 string("str_repeat")
    /// 0005 SEND_VAL string("b") 1
    /// 0006 SEND_VAL int(2) 2
    /// 0007 V1 = DO_ICALL
    /// 0008 T2 = CONCAT V0 V1
    /// 0009 ECHO T2
    /// 0010 RETURN int(1)
    /// LIVE RANGES:
    ///      0: 0004 - 0008 (tmp/var)
    /// ```
    ///
    /// The first call's `V0` is defined at 0003 and read at the `CONCAT`
    /// four oplines on, so it is live from the opline after its
    /// definition to its use; `V1` and `T2` are each read by the next
    /// opline, the trivial range php stores nothing for.
    #[test]
    fn a_temporary_read_past_the_next_opline_has_php_s_live_range() {
        use php_unit::{LiveKind, LiveRange};
        let (unit, _strings) =
            unit_and_strings(b"<?php echo str_repeat(\"a\", 2) . str_repeat(\"b\", 2);");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                11,
                0,
                3,
                "0000 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0001 SEND_VAL string(\"a\") 1\n\
                 0002 SEND_VAL int(2) 2\n\
                 0003 V0 = DO_ICALL\n\
                 0004 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0005 SEND_VAL string(\"b\") 1\n\
                 0006 SEND_VAL int(2) 2\n\
                 0007 V1 = DO_ICALL\n\
                 0008 T2 = CONCAT V0 V1\n\
                 0009 ECHO T2\n\
                 0010 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0004 - 0008 (tmp/var)\n"
            )
        );
        assert_eq!(
            *unit.main.live_ranges,
            [LiveRange {
                var: 0,
                kind: LiveKind::TmpVar,
                start: OplineIdx(4),
                end: OplineIdx(8),
            }]
        );
    }

    /// A CV is sent by `SEND_VAR` as a call's `VAR` is
    /// (`zend_compile_args`), and the value a native returned is
    /// assigned from its `VAR`, which is the shape #174's witness
    /// compiles to.
    #[test]
    fn a_cv_is_sent_by_send_var_and_assigned_from_a_call_s_var() {
        use OperandKind::{Const, Cv, Unused, Var};
        let unit = unit_of(b"<?php $b = memory_get_usage(); var_dump(memory_get_usage() - $b);");
        let shape: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.op1_type, op.op2_type))
            .collect();
        assert_eq!(
            shape,
            [
                (Opcode::InitFcall, Unused, Const),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Assign, Cv, Var),
                (Opcode::InitFcall, Unused, Const),
                (Opcode::InitFcall, Unused, Const),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Sub, Var, Cv),
                (Opcode::SendVal, OperandKind::Tmp, Unused),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Return, Const, Unused),
            ]
        );
        let sent = unit_of(b"<?php $a = 1; var_dump($a);");
        assert_eq!(sent.main.oplines[2].opcode, Opcode::SendVar);
        assert_eq!(sent.main.oplines[2].op1_type, Cv);
    }

    /// Two units compiled into one engine's table share the one string
    /// for a constant both carry, and a constant carried twice in one
    /// unit is interned once: that is what interning is for, and it is
    /// what a published unit hands every request serving it (design
    /// §5.2, §6.3).
    #[test]
    fn two_units_interned_into_one_table_share_their_constant_string() {
        let mut strings = php_heap::Strings::new();
        let compiled = [
            compile_source(
                &mut Arena::new(),
                b"<?php echo 'shared';",
                &Options::default(),
                &mut strings,
            ),
            compile_source(
                &mut Arena::new(),
                b"<?php echo 'shared', 'shared';",
                &Options::default(),
                &mut strings,
            ),
        ];
        assert_eq!(strings.count(), 1, "one string, carried by two units");
        let named: Vec<Option<php_heap::Str>> = compiled
            .iter()
            .map(|unit| {
                unit.as_ref()
                    .ok()
                    .and_then(|unit| unit.main.literals.first().copied())
                    .and_then(php_heap::Zval::as_str)
            })
            .collect();
        assert!(named.iter().all(Option::is_some), "{named:?}");
        assert_eq!(
            named[0].map(|s| s.bytes().to_vec()),
            Some(b"shared".to_vec())
        );
        let [Some(one), Some(other)] = [named[0], named[1]] else {
            return;
        };
        assert!(one.is_same(other), "one `zend_string`, named by both units");
    }

    /// The oracle's dump under the pin's `dump` regime of #162's third
    /// program, `echo 1 + strlen('a');`:
    ///
    /// ```text
    /// 0000 ECHO int(2)
    /// 0001 RETURN int(1)
    /// ```
    ///
    /// `zend_try_compile_special_func` reaches `zend_compile_func_strlen`,
    /// which leaves the constant string's length where the call stood,
    /// and `zend_try_ct_eval_binary_op` folds the sum over it.
    #[test]
    fn strlen_over_a_constant_string_is_its_length_and_the_sum_folds_over_it() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1 + strlen('a');");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n"),
            "one ECHO of a constant, no INIT_FCALL and no ADD"
        );
        assert_eq!(unit.main.literals[0].as_long(), Some(2));
    }

    /// The constant a single-expression `echo` leaves, where the whole
    /// expression folded: the first slot, of two with the return's.
    fn folded(source: &[u8]) -> Option<php_heap::Zval> {
        let (unit, _strings) = unit_and_strings(source);
        (unit.main.oplines.len() == 2 && unit.main.oplines[0].opcode == Opcode::Echo)
            .then(|| unit.main.literals.first().copied())
            .flatten()
    }

    /// Whether the source compiles to the ordinary call of `name`: an
    /// `INIT_FCALL` first, over that lowered name, and a `DO_ICALL`.
    fn ordinary_call(source: &[u8], name: &[u8]) -> bool {
        let (unit, _strings) = unit_and_strings(source);
        let oplines = &unit.main.oplines;
        oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcall)
            && literal_text(&unit, 0).as_deref() == Some(name)
            && oplines.iter().any(|op| op.opcode == Opcode::DoIcall)
    }

    /// The four rows of php's table that fold to a constant with no
    /// opcode, each held to what the oracle dumps: `strlen` over a
    /// constant string, `chr` over a long written 0 to 255, `ord` over a
    /// one-byte string, `defined` over a name the compiler substitutes,
    /// and `.` over two literals counting as written, since php's parser
    /// folds it before the compiler looks (`zend_ast_create_concat_op`).
    #[test]
    fn the_four_special_names_fold_to_the_constant_php_dumps() {
        for (source, long) in [
            (&b"<?php echo strlen('a' . 'b');"[..], 2),
            (b"<?php echo strlen('a' . 1);", 2),
            (b"<?php echo strlen('');", 0),
            (b"<?php echo \\strlen('ab');", 2),
            (b"<?php echo STRLEN('ab');", 2),
            (b"<?php echo strlen(chr(65));", 1),
            (b"<?php echo ord('a');", 97),
            (b"<?php echo ord('a' . '');", 97),
        ] {
            assert_eq!(
                folded(source).and_then(php_heap::Zval::as_long),
                Some(long),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for (source, text) in [
            (&b"<?php echo chr(65);"[..], &b"A"[..]),
            (b"<?php echo chr(0);", b"\0"),
            (b"<?php echo chr(255);", b"\xff"),
        ] {
            assert_eq!(
                echoed_string(source),
                text,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for source in [
            &b"<?php echo defined('INF');"[..],
            b"<?php echo defined('NAN');",
            b"<?php echo defined('true');",
            b"<?php echo defined('TRUE');",
            b"<?php echo defined('null');",
            b"<?php echo defined('IN' . 'F');",
        ] {
            assert_eq!(
                folded(source).map(php_heap::Zval::kind),
                Some(php_heap::ZvalType::True),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// What php leaves to the call itself is the ordinary call here,
    /// and never a constant or an opcode it is not: a `chr` whose long
    /// is not written as such or is outside the byte, an `ord` over any
    /// other string, a `defined` over a namespaced or a class name or
    /// over no written value, a wrong count of arguments, an unpacked or
    /// a named argument, and `func_num_args` and `array_slice` over
    /// `func_get_args` outside a function.
    #[test]
    fn a_special_name_php_leaves_to_the_call_is_the_ordinary_call() {
        for (source, name) in [
            (&b"<?php echo strlen();"[..], &b"strlen"[..]),
            (b"<?php echo strlen('a', 'b');", b"strlen"),
            (b"<?php echo chr(1 + 64);", b"chr"),
            (b"<?php echo chr(-0);", b"chr"),
            (b"<?php echo chr(-1);", b"chr"),
            (b"<?php echo chr(256);", b"chr"),
            (b"<?php echo chr('65');", b"chr"),
            (b"<?php echo ord('ab');", b"ord"),
            (b"<?php echo ord('');", b"ord"),
            (b"<?php echo ord(65);", b"ord"),
            (b"<?php echo defined('A\\\\B');", b"defined"),
            (b"<?php echo defined('A::B');", b"defined"),
            (b"<?php echo defined(null);", b"defined"),
            (b"<?php echo defined(INF);", b"defined"),
            (b"<?php echo defined();", b"defined"),
            (b"<?php echo count($a, 1);", b"count"),
            (b"<?php echo is_int();", b"is_int"),
            (b"<?php echo is_int($a, $b);", b"is_int"),
            (b"<?php echo intval($a, 8);", b"intval"),
            (b"<?php echo intval(value: $a);", b"intval"),
            (b"<?php echo boolval(...$a);", b"boolval"),
            (b"<?php echo gettype();", b"gettype"),
            (b"<?php echo get_class($a, $b);", b"get_class"),
            (b"<?php echo get_called_class(1);", b"get_called_class"),
            (b"<?php echo func_num_args();", b"func_num_args"),
            (
                b"<?php echo array_slice(func_get_args(), 1);",
                b"array_slice",
            ),
            (b"<?php echo array_key_exists($k);", b"array_key_exists"),
        ] {
            assert!(
                ordinary_call(source, name),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_compile_call` reads `fbc` off `CG(function_table)` before
    /// `zend_try_compile_special_func` reads its table, so under a bundle
    /// that declares none of the names, which `Options::default` is,
    /// every one is the call by name, the four constant shapes included.
    #[test]
    fn a_special_name_the_bundle_has_not_is_the_call_by_name() {
        for (source, name) in [
            (&b"<?php echo strlen($a);"[..], &b"strlen"[..]),
            (b"<?php echo strlen('a');", b"strlen"),
            (b"<?php echo chr(65);", b"chr"),
            (b"<?php echo ord('a');", b"ord"),
            (b"<?php echo defined('INF');", b"defined"),
            (b"<?php echo defined('NOPE');", b"defined"),
            (b"<?php echo is_int($a);", b"is_int"),
            (b"<?php echo count($a);", b"count"),
        ] {
            let mut strings = php_heap::Strings::new();
            let unit =
                compile_parsed(source, &Options::default(), &mut strings).unwrap_or_default();
            let oplines = &unit.main.oplines;
            assert!(
                oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcallByName)
                    && literal_text(&unit, 0).as_deref() == Some(name)
                    && oplines.iter().any(|op| op.opcode == Opcode::DoFcallByName),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_get_call_op` under each of php's hooks ([`Hooked`]): a
    /// user call, a native one, a call by name and a frameless one, as
    /// php compiles them with neither hook replaced, under phpdbg's
    /// `zend_execute_ex`, and with `zend_execute_internal` set beside it.
    #[test]
    fn a_replaced_hook_makes_every_call_it_would_not_see_do_fcall() {
        use Opcode::{DoFcall, DoFcallByName, DoIcall, DoUcall, FramelessIcall2};
        let source = b"<?php function foo($b) {} foo(1); var_dump($a); bar(); substr($a, 1);";
        let calls = |hooked: Hooked| {
            let mut strings = php_heap::Strings::new();
            let options = Options {
                hooked,
                ..bundled()
            };
            compile_parsed(source, &options, &mut strings)
                .unwrap_or_default()
                .main
                .oplines
                .iter()
                .map(|op| op.opcode)
                .filter(|op| {
                    matches!(
                        op,
                        Opcode::DoUcall
                            | Opcode::DoIcall
                            | Opcode::DoFcall
                            | Opcode::DoFcallByName
                            | Opcode::FramelessIcall2
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            calls(Hooked::NONE),
            [DoUcall, DoIcall, DoFcallByName, FramelessIcall2]
        );
        let phpdbg = Hooked {
            user: true,
            natives: false,
        };
        assert_eq!(calls(phpdbg), [DoFcall, DoIcall, DoFcall, FramelessIcall2]);
        let both = Hooked {
            user: true,
            natives: true,
        };
        assert_eq!(calls(both), [DoFcall, DoFcall, DoFcall, DoFcall]);
    }

    /// The oracle's dump of the four file constructs (#396), each one
    /// `INCLUDE_OR_EVAL` over its operand with the construct in
    /// `extended_value`, into a `VAR` that `zend_do_free` marks unused
    /// where nothing reads it, as it marks a call's; the operand of
    /// `require_once` is the whole `$x . "b"`, so the `CONCAT` stands
    /// before it.
    #[test]
    fn the_four_file_constructs_are_one_include_or_eval_each() {
        assert_eq!(
            dump_of(
                b"<?php $r = include \"a.php\"; require_once $x . \"b\"; include_once \"c\"; require \"d\"; echo include_once \"e\";"
            ),
            main_dump(
                9,
                2,
                7,
                "0000 V2 = INCLUDE_OR_EVAL (include) string(\"a.php\")\n\
                 0001 ASSIGN CV0($r) V2\n\
                 0002 T4 = CONCAT CV1($x) string(\"b\")\n\
                 0003 INCLUDE_OR_EVAL (require_once) T4\n\
                 0004 INCLUDE_OR_EVAL (include_once) string(\"c\")\n\
                 0005 INCLUDE_OR_EVAL (require) string(\"d\")\n\
                 0006 V8 = INCLUDE_OR_EVAL (include_once) string(\"e\")\n\
                 0007 ECHO V8\n\
                 0008 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump of `eval` (#573), the fifth construct of
    /// `zend_compile_include_or_eval`: one `INCLUDE_OR_EVAL (eval)` over
    /// the string, into a `VAR` that `zend_do_free` marks unused where
    /// nothing reads it, as it marks an `include`'s; the operand of the
    /// second is the whole `$x . "b"`, so the `CONCAT` stands before it.
    #[test]
    fn eval_is_the_fifth_include_or_eval() {
        assert_eq!(
            dump_of(b"<?php $r = eval(\"return 1;\"); eval($x . \"b\");"),
            main_dump(
                5,
                2,
                4,
                "0000 V2 = INCLUDE_OR_EVAL (eval) string(\"return 1;\")\n\
                 0001 ASSIGN CV0($r) V2\n\
                 0002 T4 = CONCAT CV1($x) string(\"b\")\n\
                 0003 INCLUDE_OR_EVAL (eval) T4\n\
                 0004 RETURN int(1)\n"
            )
        );
    }

    /// The string an `eval` hands over, compiled as `compile_string`
    /// compiles it (#573): code from its first byte, where a file's is
    /// inline HTML, and a main op array that ends in `RETURN null`,
    /// where a file's ends in `RETURN int(1)`.
    #[test]
    fn an_eval_string_begins_in_code_and_returns_null() {
        let options = Options {
            kind: SourceKind::EVAL,
            ..bundled()
        };
        let mut strings = php_heap::Strings::new();
        let unit = compile_source(&mut Arena::new(), b"echo 1; ?>x", &options, &mut strings)
            .unwrap_or_default();
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                3,
                0,
                0,
                "0000 ECHO int(1)\n0001 ECHO string(\"x\")\n0002 RETURN null\n"
            )
        );
    }

    /// The oracle's dump of `echo nope($a);` (#252), a name php's
    /// function table has not: `zend_compile_dynamic_call`'s
    /// `INIT_FCALL_BY_NAME` with the argument count and no used stack,
    /// `SEND_VAR_EX` for the CV and `DO_FCALL_BY_NAME` into the `VAR`.
    /// The constant pair is `zend_add_func_name_literal`'s, the name as
    /// written and then its lowered form in the slot after it, two slots
    /// even where the two are one string.
    #[test]
    fn a_name_the_bundle_has_not_is_init_fcall_by_name_over_the_constant_pair() {
        let (unit, _strings) = unit_and_strings(b"<?php echo nope($a);");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                5,
                1,
                1,
                "0000 INIT_FCALL_BY_NAME 1 string(\"nope\")\n\
                 0001 SEND_VAR_EX CV0($a) 1\n\
                 0002 V1 = DO_FCALL_BY_NAME\n\
                 0003 ECHO V1\n\
                 0004 RETURN int(1)\n"
            )
        );
        let init = unit.main.oplines[0];
        assert_eq!(
            init.op1, 0,
            "zend_compile_call_common writes used_stack for INIT_FCALL alone"
        );
        assert_eq!(init.op1_type, OperandKind::Unused);
        assert_eq!((init.op2_type, init.op2), (OperandKind::Const, 0));
        assert_eq!(init.out, 0, "one cache slot, zend_alloc_cache_slot");
        assert_eq!(unit.main.cache_size, 8);
        assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"nope"[..]));
        assert_eq!(
            literal_text(&unit, 1).as_deref(),
            Some(&b"nope"[..]),
            "the lowered name is the slot after the written one"
        );
        assert_eq!(
            unit.main.literals.len(),
            3,
            "the pair and the final return's 1"
        );
    }

    /// The oracle's dump of `echo NoPe(nope2($a), 1 + $a, 'x');`: the
    /// pair keeps the case the name was written in and lowers it beside,
    /// what the inner call returned is `SEND_VAR_NO_REF_EX`, a temporary
    /// and a constant are `SEND_VAL_EX`, and the inner call is its own
    /// by-name sequence inside the outer's.
    #[test]
    fn a_call_by_name_sends_each_argument_as_php_sends_it_for_an_unknown_callee() {
        let (unit, _strings) = unit_and_strings(b"<?php echo NoPe(nope2($a), 1 + $a, 'x');");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                11,
                1,
                3,
                "0000 INIT_FCALL_BY_NAME 3 string(\"NoPe\")\n\
                 0001 INIT_FCALL_BY_NAME 1 string(\"nope2\")\n\
                 0002 SEND_VAR_EX CV0($a) 1\n\
                 0003 V1 = DO_FCALL_BY_NAME\n\
                 0004 SEND_VAR_NO_REF_EX V1 1\n\
                 0005 T2 = ADD int(1) CV0($a)\n\
                 0006 SEND_VAL_EX T2 2\n\
                 0007 SEND_VAL_EX string(\"x\") 3\n\
                 0008 V3 = DO_FCALL_BY_NAME\n\
                 0009 ECHO V3\n\
                 0010 RETURN int(1)\n"
            )
        );
        assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"NoPe"[..]));
        assert_eq!(literal_text(&unit, 1).as_deref(), Some(&b"nope"[..]));
        assert_eq!(literal_text(&unit, 2).as_deref(), Some(&b"nope2"[..]));
        assert_eq!(literal_text(&unit, 3).as_deref(), Some(&b"nope2"[..]));
        assert_eq!(
            unit.main.cache_size, 16,
            "one cache slot per INIT_FCALL_BY_NAME"
        );
    }

    /// A name the bundle has keeps `INIT_FCALL` and `DO_ICALL` whatever
    /// its arguments: `strlen('a', 'b')` is in `special::TABLE` and takes
    /// two arguments, which the fold refuses, so it is the ordinary call
    /// over the lowered name with php's used stack, as the oracle dumps
    /// it: `INIT_FCALL 2 112 string("strlen")`.
    #[test]
    fn a_name_the_bundle_has_keeps_init_fcall_and_do_icall() {
        let (unit, _strings) = unit_and_strings(b"<?php echo StrLen('a', 'b');");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                6,
                0,
                1,
                "0000 INIT_FCALL 2 112 string(\"strlen\")\n\
                 0001 SEND_VAL string(\"a\") 1\n\
                 0002 SEND_VAL string(\"b\") 2\n\
                 0003 V0 = DO_ICALL\n\
                 0004 ECHO V0\n\
                 0005 RETURN int(1)\n"
            )
        );
        assert_eq!(
            unit.main.literals.len(),
            4,
            "the lowered name once, no pair"
        );
    }

    /// The oracle's dump under the pin's `dump` regime of #219's program
    /// and of its kin: `STRLEN` over the CV, over a constant
    /// that is not a string and over what a `CONCAT` left, `COUNT` for
    /// `count` and `sizeof` alike, `TYPE_CHECK` with the mask each
    /// `is_*` writes, and `DEFINED` over the name as a string with its
    /// cache slot, `1.5` written as `"1.5"`; and the name in any case,
    /// with or without a leading separator.
    #[test]
    fn the_opcode_shapes_of_the_special_table_are_php_s() {
        assert_eq!(
            dump_of(b"<?php echo strlen($a), count($a), is_int($a), defined('NOPE');"),
            main_dump(
                9,
                1,
                4,
                "0000 T1 = STRLEN CV0($a)\n\
                 0001 ECHO T1\n\
                 0002 T2 = COUNT CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = TYPE_CHECK (long) CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = DEFINED string(\"NOPE\")\n\
                 0007 ECHO T4\n\
                 0008 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(
                b"<?php echo strlen(1), strlen(INF), strlen(null), strlen($a . $b), STRLEN('ab');"
            ),
            main_dump(
                11,
                2,
                5,
                "0000 T2 = STRLEN int(1)\n\
                 0001 ECHO T2\n\
                 0002 T3 = STRLEN float(inf)\n\
                 0003 ECHO T3\n\
                 0004 T4 = STRLEN null\n\
                 0005 ECHO T4\n\
                 0006 T5 = CONCAT CV0($a) CV1($b)\n\
                 0007 T6 = STRLEN T5\n\
                 0008 ECHO T6\n\
                 0009 ECHO int(2)\n\
                 0010 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(
                b"<?php echo defined('inf'), defined(1.5), defined('NOPE'), $x = defined('NOPE');"
            ),
            main_dump(
                10,
                1,
                5,
                "0000 T1 = DEFINED string(\"inf\")\n\
                 0001 ECHO T1\n\
                 0002 T2 = DEFINED string(\"1.5\")\n\
                 0003 ECHO T2\n\
                 0004 T3 = DEFINED string(\"NOPE\")\n\
                 0005 ECHO T3\n\
                 0006 T4 = DEFINED string(\"NOPE\")\n\
                 0007 T5 = ASSIGN CV0($x) T4\n\
                 0008 ECHO T5\n\
                 0009 RETURN int(1)\n"
            )
        );
        let (unit, _strings) =
            unit_and_strings(b"<?php echo sizeof($a), defined('A'), defined('B');");
        let extended: Vec<_> = unit
            .main
            .oplines
            .iter()
            .filter(|op| matches!(op.opcode, Opcode::Count | Opcode::Defined))
            .map(|op| (op.opcode, op.extended_value))
            .collect();
        assert_eq!(
            extended,
            [
                (Opcode::Count, 1),
                (Opcode::Defined, 0),
                (Opcode::Defined, 8)
            ],
            "sizeof's bit, and one cache slot per DEFINED, eight bytes each"
        );
        assert_eq!(unit.main.cache_size, 16);
    }

    /// The oracle's dump of every `is_*` row and of `count` and `sizeof`
    /// over a constant and a CV: `TYPE_CHECK` with the mask each name
    /// writes, `is_bool` two bits and `is_scalar` five, over a constant
    /// where one is written, and `COUNT` the same for both names.
    #[test]
    fn the_type_check_and_count_rows_are_php_s_masks() {
        assert_eq!(
            dump_of(
                b"<?php echo sizeof($a), is_scalar($a), is_bool($a), is_int(1), count(\"x\"), is_null($u), \\is_integer($a), Is_Float($a), is_double($a), is_string($a), is_array($a), is_object($a), is_resource($a), is_long($a);"
            ),
            main_dump(
                29,
                2,
                14,
                "0000 T2 = COUNT CV0($a)\n\
                 0001 ECHO T2\n\
                 0002 T3 = TYPE_CHECK TYPE [bool, long, double, string] CV0($a)\n\
                 0003 ECHO T3\n\
                 0004 T4 = TYPE_CHECK (bool) CV0($a)\n\
                 0005 ECHO T4\n\
                 0006 T5 = TYPE_CHECK (long) int(1)\n\
                 0007 ECHO T5\n\
                 0008 T6 = COUNT string(\"x\")\n\
                 0009 ECHO T6\n\
                 0010 T7 = TYPE_CHECK (null) CV1($u)\n\
                 0011 ECHO T7\n\
                 0012 T8 = TYPE_CHECK (long) CV0($a)\n\
                 0013 ECHO T8\n\
                 0014 T9 = TYPE_CHECK (double) CV0($a)\n\
                 0015 ECHO T9\n\
                 0016 T10 = TYPE_CHECK (double) CV0($a)\n\
                 0017 ECHO T10\n\
                 0018 T11 = TYPE_CHECK (string) CV0($a)\n\
                 0019 ECHO T11\n\
                 0020 T12 = TYPE_CHECK (array) CV0($a)\n\
                 0021 ECHO T12\n\
                 0022 T13 = TYPE_CHECK (object) CV0($a)\n\
                 0023 ECHO T13\n\
                 0024 T14 = TYPE_CHECK (resource) CV0($a)\n\
                 0025 ECHO T14\n\
                 0026 T15 = TYPE_CHECK (long) CV0($a)\n\
                 0027 ECHO T15\n\
                 0028 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_func_cast` as the oracle at 8.5.9 dumps it (#1251):
    /// `BOOL` for `boolval`, `CAST` with its type for `intval`,
    /// `floatval`, `doubleval` and `strval`, a fully qualified name
    /// alike; a second argument, an unpacked one or none is the call.
    #[test]
    fn the_cast_rows_are_bool_and_cast() {
        assert_eq!(
            dump_of(
                b"<?php echo boolval($a), intval($a), floatval($a), doubleval($a), strval($a), intval($a, 8), boolval(...$a), \\strval(1.5), strval();"
            ),
            main_dump(
                26,
                1,
                9,
                "0000 T1 = BOOL CV0($a)\n\
                 0001 ECHO T1\n\
                 0002 T2 = CAST (long) CV0($a)\n\
                 0003 ECHO T2\n\
                 0004 T3 = CAST (double) CV0($a)\n\
                 0005 ECHO T3\n\
                 0006 T4 = CAST (double) CV0($a)\n\
                 0007 ECHO T4\n\
                 0008 T5 = CAST (string) CV0($a)\n\
                 0009 ECHO T5\n\
                 0010 INIT_FCALL 2 112 string(\"intval\")\n\
                 0011 SEND_VAR CV0($a) 1\n\
                 0012 SEND_VAL int(8) 2\n\
                 0013 V6 = DO_ICALL\n\
                 0014 ECHO V6\n\
                 0015 INIT_FCALL 0 80 string(\"boolval\")\n\
                 0016 SEND_UNPACK CV0($a)\n\
                 0017 CHECK_UNDEF_ARGS\n\
                 0018 V7 = DO_ICALL\n\
                 0019 ECHO V7\n\
                 0020 T8 = CAST (string) float(1.5)\n\
                 0021 ECHO T8\n\
                 0022 INIT_FCALL 0 80 string(\"strval\")\n\
                 0023 V9 = DO_ICALL\n\
                 0024 ECHO V9\n\
                 0025 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_cast` folds nothing, so the unary operators over a
    /// cast fold nothing either and `-(float)"0"` is the `CAST` and the
    /// `MUL` over it (#1251, `Zend/tests/bug52355.phpt`), while an array
    /// of constants, which `zend_eval_const_expr` folds, folds the cast
    /// inside it; as the oracle at 8.5.9 dumps it.
    #[test]
    fn a_cast_under_a_unary_operator_is_its_opline() {
        assert_eq!(
            dump_of(
                b"<?php var_dump(-(float)\"0\", +(float)\"-0\", !(bool)\"1\", ~(int)\"1\", -(int)1.5, (int)\"1\" + 1, [(int)\"2\"]);"
            ),
            main_dump(
                22,
                0,
                13,
                "0000 INIT_FCALL 7 192 string(\"var_dump\")\n\
                 0001 T0 = CAST (double) string(\"0\")\n\
                 0002 T1 = MUL T0 int(-1)\n\
                 0003 SEND_VAL T1 1\n\
                 0004 T2 = CAST (double) string(\"-0\")\n\
                 0005 T3 = MUL T2 int(1)\n\
                 0006 SEND_VAL T3 2\n\
                 0007 T4 = BOOL string(\"1\")\n\
                 0008 T5 = BOOL_NOT T4\n\
                 0009 SEND_VAL T5 3\n\
                 0010 T6 = CAST (long) string(\"1\")\n\
                 0011 T7 = BW_NOT T6\n\
                 0012 SEND_VAL T7 4\n\
                 0013 T8 = CAST (long) float(1.5)\n\
                 0014 T9 = MUL T8 int(-1)\n\
                 0015 SEND_VAL T9 5\n\
                 0016 T10 = CAST (long) string(\"1\")\n\
                 0017 T11 = ADD T10 int(1)\n\
                 0018 SEND_VAL T11 6\n\
                 0019 SEND_VAL array(...) 7\n\
                 0020 DO_ICALL\n\
                 0021 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_func_get_class`, `_get_called_class` and `_gettype`
    /// as the oracle at 8.5.9 dumps them (#1251): `GET_CLASS` with no
    /// operand or over the one argument, `GET_CALLED_CLASS` and
    /// `GET_TYPE`, in a method and in the main op array alike, since
    /// none of the three reads the op array; a count they refuse is the
    /// call.
    #[test]
    fn get_class_get_called_class_and_gettype_are_their_opcodes() {
        assert_eq!(
            dump_of(
                b"<?php class C { function f() { return [get_class(), get_class($this), get_called_class(), gettype($x), get_class($a, $b), get_called_class(1), gettype()]; } } echo get_class(), get_called_class(), gettype(1);"
            ),
            "\n\
             $_main:\n\
             \x20    ; (lines=7, args=0, vars=0, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T0 = GET_CLASS\n\
             0001 ECHO T0\n\
             0002 T1 = GET_CALLED_CLASS\n\
             0003 ECHO T1\n\
             0004 T2 = GET_TYPE int(1)\n\
             0005 ECHO T2\n\
             0006 RETURN int(1)\n\
             \n\
             C::f:\n\
             \x20    ; (lines=23, args=0, vars=3, tmps=9)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T3 = GET_CLASS\n\
             0001 T4 = INIT_ARRAY 7 (packed) T3 NEXT\n\
             0002 T5 = FETCH_THIS\n\
             0003 T6 = GET_CLASS T5\n\
             0004 T4 = ADD_ARRAY_ELEMENT T6 NEXT\n\
             0005 T7 = GET_CALLED_CLASS\n\
             0006 T4 = ADD_ARRAY_ELEMENT T7 NEXT\n\
             0007 T8 = GET_TYPE CV0($x)\n\
             0008 T4 = ADD_ARRAY_ELEMENT T8 NEXT\n\
             0009 INIT_FCALL 2 112 string(\"get_class\")\n\
             0010 SEND_VAR CV1($a) 1\n\
             0011 SEND_VAR CV2($b) 2\n\
             0012 V9 = DO_ICALL\n\
             0013 T4 = ADD_ARRAY_ELEMENT V9 NEXT\n\
             0014 INIT_FCALL 1 96 string(\"get_called_class\")\n\
             0015 SEND_VAL int(1) 1\n\
             0016 V10 = DO_ICALL\n\
             0017 T4 = ADD_ARRAY_ELEMENT V10 NEXT\n\
             0018 INIT_FCALL 0 80 string(\"gettype\")\n\
             0019 V11 = DO_ICALL\n\
             0020 T4 = ADD_ARRAY_ELEMENT V11 NEXT\n\
             0021 RETURN T4\n\
             0022 RETURN null\n\
             LIVE RANGES:\n\
             \x20    4: 0002 - 0021 (tmp/var)\n"
        );
    }

    /// `zend_delayed_compile_dim` over `$this` as the oracle at 8.5.9
    /// dumps it (#2574): the container is `zend_compile_simple_var`'s
    /// `FETCH_THIS` into a temporary, in a method and in the main op
    /// array alike, and never the `UNUSED` receiver a property fetch
    /// takes, which `ISSET_ISEMPTY_DIM_OBJ` and `FETCH_DIM_IS` do not
    /// list.
    #[test]
    fn this_under_a_silent_dimension_is_fetched_into_a_temporary() {
        assert_eq!(
            dump_of(
                b"<?php class A { function f() { return [isset($this[0]), empty($this[0]), isset($this[0][1]), $this[0] ?? 2]; } } var_dump(isset($this[0]));"
            ),
            "\n\
             $_main:\n\
             \x20    ; (lines=6, args=0, vars=0, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
             0001 T0 = FETCH_THIS\n\
             0002 T1 = ISSET_ISEMPTY_DIM_OBJ (isset) T0 int(0)\n\
             0003 SEND_VAL T1 1\n\
             0004 DO_ICALL\n\
             0005 RETURN int(1)\n\
             \n\
             A::f:\n\
             \x20    ; (lines=17, args=0, vars=0, tmps=11)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T0 = FETCH_THIS\n\
             0001 T1 = ISSET_ISEMPTY_DIM_OBJ (isset) T0 int(0)\n\
             0002 T2 = INIT_ARRAY 4 (packed) T1 NEXT\n\
             0003 T3 = FETCH_THIS\n\
             0004 T4 = ISSET_ISEMPTY_DIM_OBJ (empty) T3 int(0)\n\
             0005 T2 = ADD_ARRAY_ELEMENT T4 NEXT\n\
             0006 T5 = FETCH_THIS\n\
             0007 T6 = FETCH_DIM_IS T5 int(0)\n\
             0008 T7 = ISSET_ISEMPTY_DIM_OBJ (isset) T6 int(1)\n\
             0009 T2 = ADD_ARRAY_ELEMENT T7 NEXT\n\
             0010 T8 = FETCH_THIS\n\
             0011 T9 = FETCH_DIM_IS T8 int(0)\n\
             0012 T10 = COALESCE T9 0014\n\
             0013 T10 = QM_ASSIGN int(2)\n\
             0014 T2 = ADD_ARRAY_ELEMENT T10 NEXT\n\
             0015 RETURN T2\n\
             0016 RETURN null\n\
             LIVE RANGES:\n\
             \x20    2: 0003 - 0015 (tmp/var)\n"
        );
    }

    /// `zend_compile_func_num_args` and `zend_compile_func_array_slice`
    /// as the oracle at 8.5.9 dumps them (#1251): in a function,
    /// `FUNC_NUM_ARGS`, and `FUNC_GET_ARGS` with the offset as a constant
    /// for `array_slice(func_get_args(), n)`, `n` a long written as such
    /// and not below zero, the inner name in any case and fully
    /// qualified; an offset written as a string or negated, an argument
    /// to `func_get_args`, a third argument, and the main op array are
    /// the calls, `func_get_args` served inside `array_slice`'s as its
    /// own row serves it.
    #[test]
    fn func_num_args_and_array_slice_of_func_get_args_are_their_opcodes() {
        assert_eq!(
            dump_of(
                b"<?php function f() { return [func_num_args(), func_num_args(1), array_slice(func_get_args(), 1), array_slice(FUNC_GET_ARGS(), 0), array_slice(func_get_args(), \"1\"), array_slice(func_get_args(), -1), array_slice(func_get_args(1), 1), array_slice(\\func_get_args(), 2), array_slice(func_get_args(), 1, 2)]; } echo func_num_args(), array_slice(func_get_args(), 1);"
            ),
            "\n\
             $_main:\n\
             \x20    ; (lines=11, args=0, vars=0, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 INIT_FCALL 0 80 string(\"func_num_args\")\n\
             0001 V0 = DO_ICALL\n\
             0002 ECHO V0\n\
             0003 INIT_FCALL 2 112 string(\"array_slice\")\n\
             0004 INIT_FCALL 0 80 string(\"func_get_args\")\n\
             0005 V1 = DO_ICALL\n\
             0006 SEND_VAR V1 1\n\
             0007 SEND_VAL int(1) 2\n\
             0008 V2 = DO_ICALL\n\
             0009 ECHO V2\n\
             0010 RETURN int(1)\n\
             \n\
             f:\n\
             \x20    ; (lines=41, args=0, vars=0, tmps=14)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T0 = FUNC_NUM_ARGS\n\
             0001 T1 = INIT_ARRAY 9 (packed) T0 NEXT\n\
             0002 INIT_FCALL 1 96 string(\"func_num_args\")\n\
             0003 SEND_VAL int(1) 1\n\
             0004 V2 = DO_ICALL\n\
             0005 T1 = ADD_ARRAY_ELEMENT V2 NEXT\n\
             0006 T3 = FUNC_GET_ARGS int(1)\n\
             0007 T1 = ADD_ARRAY_ELEMENT T3 NEXT\n\
             0008 T4 = FUNC_GET_ARGS int(0)\n\
             0009 T1 = ADD_ARRAY_ELEMENT T4 NEXT\n\
             0010 INIT_FCALL 2 112 string(\"array_slice\")\n\
             0011 T5 = FUNC_GET_ARGS\n\
             0012 SEND_VAL T5 1\n\
             0013 SEND_VAL string(\"1\") 2\n\
             0014 V6 = DO_ICALL\n\
             0015 T1 = ADD_ARRAY_ELEMENT V6 NEXT\n\
             0016 INIT_FCALL 2 112 string(\"array_slice\")\n\
             0017 T7 = FUNC_GET_ARGS\n\
             0018 SEND_VAL T7 1\n\
             0019 SEND_VAL int(-1) 2\n\
             0020 V8 = DO_ICALL\n\
             0021 T1 = ADD_ARRAY_ELEMENT V8 NEXT\n\
             0022 INIT_FCALL 2 112 string(\"array_slice\")\n\
             0023 INIT_FCALL 1 96 string(\"func_get_args\")\n\
             0024 SEND_VAL int(1) 1\n\
             0025 V9 = DO_ICALL\n\
             0026 SEND_VAR V9 1\n\
             0027 SEND_VAL int(1) 2\n\
             0028 V10 = DO_ICALL\n\
             0029 T1 = ADD_ARRAY_ELEMENT V10 NEXT\n\
             0030 T11 = FUNC_GET_ARGS int(2)\n\
             0031 T1 = ADD_ARRAY_ELEMENT T11 NEXT\n\
             0032 INIT_FCALL 3 128 string(\"array_slice\")\n\
             0033 T12 = FUNC_GET_ARGS\n\
             0034 SEND_VAL T12 1\n\
             0035 SEND_VAL int(1) 2\n\
             0036 SEND_VAL int(2) 3\n\
             0037 V13 = DO_ICALL\n\
             0038 T1 = ADD_ARRAY_ELEMENT V13 NEXT\n\
             0039 RETURN T1\n\
             0040 RETURN null\n\
             LIVE RANGES:\n\
             \x20    1: 0002 - 0039 (tmp/var)\n"
        );
    }

    /// Inside a namespace the unqualified names wait for the program
    /// (`zend_compile_ns_call`) and reach no row, and the `func_get_args`
    /// inside a fully qualified `array_slice` resolves into the
    /// namespace, which `zend_compile_func_array_slice` refuses; a fully
    /// qualified name is served, as the oracle at 8.5.9 dumps it (#1251).
    #[test]
    fn a_special_name_waiting_for_its_namespace_is_no_row() {
        assert_eq!(
            dump_of(
                b"<?php namespace N; function f() { return [\\array_slice(func_get_args(), 1), \\array_slice(\\func_get_args(), 1), intval($a), \\intval($a), \\get_class()]; }"
            ),
            "\n\
             $_main:\n\
             \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \n\
             N\\f:\n\
             \x20    ; (lines=19, args=0, vars=1, tmps=7)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 INIT_FCALL 2 112 string(\"array_slice\")\n\
             0001 INIT_NS_FCALL_BY_NAME 0 string(\"N\\\\func_get_args\")\n\
             0002 V1 = DO_FCALL_BY_NAME\n\
             0003 SEND_VAR V1 1\n\
             0004 SEND_VAL int(1) 2\n\
             0005 V2 = DO_ICALL\n\
             0006 T3 = INIT_ARRAY 5 (packed) V2 NEXT\n\
             0007 T4 = FUNC_GET_ARGS int(1)\n\
             0008 T3 = ADD_ARRAY_ELEMENT T4 NEXT\n\
             0009 INIT_NS_FCALL_BY_NAME 1 string(\"N\\\\intval\")\n\
             0010 SEND_VAR_EX CV0($a) 1\n\
             0011 V5 = DO_FCALL_BY_NAME\n\
             0012 T3 = ADD_ARRAY_ELEMENT V5 NEXT\n\
             0013 T6 = CAST (long) CV0($a)\n\
             0014 T3 = ADD_ARRAY_ELEMENT T6 NEXT\n\
             0015 T7 = GET_CLASS\n\
             0016 T3 = ADD_ARRAY_ELEMENT T7 NEXT\n\
             0017 RETURN T3\n\
             0018 RETURN null\n\
             LIVE RANGES:\n\
             \x20    3: 0007 - 0017 (tmp/var)\n"
        );
    }

    /// `zend_compile_func_array_key_exists` as the oracle at 8.5.9 dumps
    /// it (#1251): `ARRAY_KEY_EXISTS` over the key and then the array,
    /// each compiled first, read by a `JMPZ` and freed where nothing
    /// reads it; one argument is the call.
    #[test]
    fn array_key_exists_of_two_arguments_is_its_opcode() {
        assert_eq!(
            dump_of(
                b"<?php echo array_key_exists($k, $a), array_key_exists(1, [1]), array_key_exists($k), array_key_exists($k . \"\", $a[\"x\"]); if (array_key_exists($k, $a)) echo 1; array_key_exists($k, $a);"
            ),
            main_dump(
                18,
                2,
                8,
                "0000 T2 = ARRAY_KEY_EXISTS CV0($k) CV1($a)\n\
                 0001 ECHO T2\n\
                 0002 T3 = ARRAY_KEY_EXISTS int(1) array(...)\n\
                 0003 ECHO T3\n\
                 0004 INIT_FCALL 1 96 string(\"array_key_exists\")\n\
                 0005 SEND_VAR CV0($k) 1\n\
                 0006 V4 = DO_ICALL\n\
                 0007 ECHO V4\n\
                 0008 T5 = CONCAT CV0($k) string(\"\")\n\
                 0009 T6 = FETCH_DIM_R CV1($a) string(\"x\")\n\
                 0010 T7 = ARRAY_KEY_EXISTS T5 T6\n\
                 0011 ECHO T7\n\
                 0012 T8 = ARRAY_KEY_EXISTS CV0($k) CV1($a)\n\
                 0013 JMPZ T8 0015\n\
                 0014 ECHO int(1)\n\
                 0015 T9 = ARRAY_KEY_EXISTS CV0($k) CV1($a)\n\
                 0016 FREE T9\n\
                 0017 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    5: 0009 - 0010 (tmp/var)\n"
            )
        );
    }

    /// `zend_do_free` over what the four opcodes leave: none of them is
    /// one php marks `UNUSED`, so a value nothing reads is a `FREE`.
    #[test]
    fn a_special_opcode_nothing_reads_is_freed() {
        assert_eq!(
            dump_of(b"<?php $n = strlen($a); strlen($a); is_int($a); count($a); defined('NOPE');"),
            main_dump(
                11,
                2,
                6,
                "0000 T2 = STRLEN CV1($a)\n\
                 0001 ASSIGN CV0($n) T2\n\
                 0002 T4 = STRLEN CV1($a)\n\
                 0003 FREE T4\n\
                 0004 T5 = TYPE_CHECK (long) CV1($a)\n\
                 0005 FREE T5\n\
                 0006 T6 = COUNT CV1($a)\n\
                 0007 FREE T6\n\
                 0008 T7 = DEFINED string(\"NOPE\")\n\
                 0009 FREE T7\n\
                 0010 RETURN int(1)\n"
            )
        );
    }

    /// The table is `zend_try_compile_special_func_ex`'s names in its
    /// order, `strlen` first and `clone` last, and a name is found by the
    /// row it stands in.
    #[test]
    fn the_special_table_is_php_s_names_in_php_s_order() {
        use special::{Special, TABLE, row};
        assert_eq!(TABLE.len(), 35);
        assert_eq!(TABLE.first().map(|(n, _)| *n), Some(&b"strlen"[..]));
        assert_eq!(TABLE.last().map(|(n, _)| *n), Some(&b"clone"[..]));
        assert_eq!(row(b"defined"), Some(Special::Defined));
        assert_eq!(row(b"is_integer"), Some(Special::TypeCheck(0x10)));
        assert_eq!(row(b"is_scalar"), Some(Special::TypeCheck(0x7c)));
        assert_eq!(row(b"sizeof"), Some(Special::Count { sizeof: true }));
        assert_eq!(row(b"intval"), Some(Special::Cast(emit::CastTo::Long)));
        assert_eq!(row(b"doubleval"), Some(Special::Cast(emit::CastTo::Double)));
        assert_eq!(row(b"var_dump"), None);
        let names: Vec<&[u8]> = TABLE.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "each name once");
    }

    /// The oracle's dump under `[regime.dump]` of a call of each
    /// frameless arity `ext/standard` declares: `str_contains` of two,
    /// `trim` of one and of two, `substr` of two, and `str_contains`
    /// over a temporary; each a `FRAMELESS_ICALL_<n>` into a temporary
    /// with the table's row in `extended_value`, the one nothing reads
    /// freed by a `FREE` (`zend_do_free`), and the temporaries numbered
    /// as php numbers them, one skipped per assignment.
    #[test]
    fn a_call_a_frameless_entry_serves_is_frameless_icall_with_its_row() {
        let source = b"<?php $a = \"x\"; $b = str_contains($a, \"y\"); $t = trim($a); $u = trim($a, \"x\"); substr($a, 1); echo str_contains($a . \"z\", \"z\");";
        assert_eq!(
            dump_of(source),
            main_dump(
                13,
                4,
                10,
                "0000 ASSIGN CV0($a) string(\"x\")\n\
                 0001 T5 = FRAMELESS_ICALL_2(str_contains) CV0($a) string(\"y\")\n\
                 0002 ASSIGN CV1($b) T5\n\
                 0003 T7 = FRAMELESS_ICALL_1(trim) CV0($a)\n\
                 0004 ASSIGN CV2($t) T7\n\
                 0005 T9 = FRAMELESS_ICALL_2(trim) CV0($a) string(\"x\")\n\
                 0006 ASSIGN CV3($u) T9\n\
                 0007 T11 = FRAMELESS_ICALL_2(substr) CV0($a) int(1)\n\
                 0008 FREE T11\n\
                 0009 T12 = CONCAT CV0($a) string(\"z\")\n\
                 0010 T13 = FRAMELESS_ICALL_2(str_contains) T12 string(\"z\")\n\
                 0011 ECHO T13\n\
                 0012 RETURN int(1)\n"
            )
        );
        let unit = unit_of(source);
        let rows: Vec<(Opcode, u32)> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode.is_frameless_icall())
            .map(|op| (op.opcode, op.extended_value))
            .collect();
        assert_eq!(
            rows,
            [
                (Opcode::FramelessIcall2, 10),
                (Opcode::FramelessIcall1, 0),
                (Opcode::FramelessIcall2, 1),
                (Opcode::FramelessIcall2, 12),
                (Opcode::FramelessIcall2, 10),
            ],
            "zend_flf_functions in registration order: trim 1, trim 2, implode 1, implode 2, dirname 1, dirname 2, strstr 2, strstr 3, strpos 2, strpos 3, str_contains 2, str_starts_with 2, substr 2, substr 3"
        );
    }

    /// A call of each of the seven natives #2673 found with no entry, at
    /// each arity php declares for it, is a `FRAMELESS_ICALL_<n>` with
    /// its row of `zend_flf_functions` as `basic_functions.stub.php`
    /// orders them; a call of `join` or `strchr`, aliases php declares
    /// no entry for, and `str_replace` with its fourth argument, above
    /// every arity, stay the ordinary call.
    #[test]
    fn each_arity_php_declares_is_a_frameless_call_with_its_row() {
        let source = b"<?php implode($a); implode($a, $b); dirname($a); dirname($a, 2); strstr($a, $b); strstr($a, $b, true); strpos($a, $b); strpos($a, $b, 1); str_starts_with($a, $b); strtr($a, $b); strtr($a, $b, $c); str_replace($a, $b, $c); join($a, $b); strchr($a, $b); str_replace($a, $b, $c, $d);";
        let unit = unit_of(source);
        let rows: Vec<(Opcode, u32)> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode.is_frameless_icall())
            .map(|op| (op.opcode, op.extended_value))
            .collect();
        assert_eq!(
            rows,
            [
                (Opcode::FramelessIcall1, 2),
                (Opcode::FramelessIcall2, 3),
                (Opcode::FramelessIcall1, 4),
                (Opcode::FramelessIcall2, 5),
                (Opcode::FramelessIcall2, 6),
                (Opcode::FramelessIcall3, 7),
                (Opcode::FramelessIcall2, 8),
                (Opcode::FramelessIcall3, 9),
                (Opcode::FramelessIcall2, 11),
                (Opcode::FramelessIcall2, 14),
                (Opcode::FramelessIcall3, 15),
                (Opcode::FramelessIcall3, 16),
            ]
        );
        let ordinary = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::InitFcall)
            .count();
        assert_eq!(ordinary, 3, "join, strchr and str_replace of four");
    }

    /// A third argument rides in an `OP_DATA` after the opline
    /// (`zend_emit_op_data`), and `zend_do_free` steps back over it to
    /// the call that left the value. The oracle's dump prints a live
    /// range for the temporary across the `OP_DATA`, #255's table, and
    /// the dump here is the oracle's under `[regime.dump]`, live ranges
    /// and all.
    #[test]
    fn a_third_argument_is_an_op_data_after_the_frameless_call() {
        let source = b"<?php $a = \"x\"; $s = substr($a, 1, 2); substr($a, 1, 2);";
        assert_eq!(
            dump_of(source),
            main_dump(
                8,
                2,
                4,
                "0000 ASSIGN CV0($a) string(\"x\")\n\
                 0001 T3 = FRAMELESS_ICALL_3(substr) CV0($a) int(1)\n\
                 0002 OP_DATA int(2)\n\
                 0003 ASSIGN CV1($s) T3\n\
                 0004 T5 = FRAMELESS_ICALL_3(substr) CV0($a) int(1)\n\
                 0005 OP_DATA int(2)\n\
                 0006 FREE T5\n\
                 0007 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0002 - 0003 (tmp/var)\n\
                 \x20    5: 0005 - 0006 (tmp/var)\n"
            )
        );
    }

    /// The oracle's dump of `zend_compile_func_in_array`'s shape:
    /// `IN_ARRAY` with `$strict` before its operands, over a haystack
    /// of non-numeric strings with no third argument, of strings and
    /// longs under `true`, of nothing under `null`, of a numeric string
    /// and a long under `\TRUE`, and of a string under `''`, each third
    /// argument read by `zend_is_true`. The arms that are the call are the
    /// witness's on [`special`], since php's call there is frameless.
    #[test]
    fn in_array_over_a_literal_haystack_is_in_array_with_its_flag() {
        assert_eq!(
            dump_of(
                b"<?php echo in_array($a, [\"x\", \"y\"]), in_array($a, [1, \"b\"], true), in_array($a, [], null), in_array($a, [\"1\", 2], \\TRUE), in_array($a, ['y'], '');"
            ),
            main_dump(
                11,
                1,
                5,
                "0000 T1 = IN_ARRAY 0 CV0($a) array(...)\n\
                 0001 ECHO T1\n\
                 0002 T2 = IN_ARRAY 1 CV0($a) array(...)\n\
                 0003 ECHO T2\n\
                 0004 T3 = IN_ARRAY 0 CV0($a) array(...)\n\
                 0005 ECHO T3\n\
                 0006 T4 = IN_ARRAY 1 CV0($a) array(...)\n\
                 0007 ECHO T4\n\
                 0008 T5 = IN_ARRAY 0 CV0($a) array(...)\n\
                 0009 ECHO T5\n\
                 0010 RETURN int(1)\n"
            )
        );
    }

    /// A count no entry serves is the ordinary call, whatever the
    /// name: `str_contains` of one argument is below its required
    /// count, `trim` of three is above every arity, and a name the
    /// bundle has not is the call by name.
    #[test]
    fn a_count_no_entry_serves_is_the_ordinary_call() {
        assert_eq!(
            dump_of(b"<?php str_contains($a); trim($a, $b, $c); nope($a, $b);"),
            main_dump(
                13,
                3,
                3,
                "0000 INIT_FCALL 1 96 string(\"str_contains\")\n\
                 0001 SEND_VAR CV0($a) 1\n\
                 0002 DO_ICALL\n\
                 0003 INIT_FCALL 3 128 string(\"trim\")\n\
                 0004 SEND_VAR CV0($a) 1\n\
                 0005 SEND_VAR CV1($b) 2\n\
                 0006 SEND_VAR CV2($c) 3\n\
                 0007 DO_ICALL\n\
                 0008 INIT_FCALL_BY_NAME 2 string(\"nope\")\n\
                 0009 SEND_VAR_EX CV0($a) 1\n\
                 0010 SEND_VAR_EX CV1($b) 2\n\
                 0011 DO_FCALL_BY_NAME\n\
                 0012 RETURN int(1)\n"
            )
        );
    }

    /// The oracle's dump of `zend_compile_ns_call`'s shape for an
    /// unqualified name in a namespace: `JMP_FRAMELESS` over the
    /// lowered name with its cache slot, printed in bytes, the call by
    /// name through `INIT_NS_FCALL_BY_NAME`, `SEND_*_EX` and
    /// `DO_FCALL_BY_NAME`, the `JMP` past the branch, and the
    /// `FRAMELESS_ICALL_<n>` into the same `VAR`, freed by a `FREE`
    /// where nothing reads it. A `\`-qualified name resolves at compile
    /// time as outside a namespace, and a call nested in the branch
    /// takes no branch of its own (`in_jmp_frameless_branch`).
    #[test]
    fn an_unqualified_name_in_a_namespace_is_jmp_frameless_around_the_call_by_name() {
        assert_eq!(
            dump_of(b"<?php namespace Foo; $a = \"x\"; $b = str_contains($a, \"y\"); \\trim($a); trim(nope($a));"),
            main_dump(
                25,
                2,
                7,
                "0000 ASSIGN CV0($a) string(\"x\")\n\
                 0001 JMP_FRAMELESS 8 string(\"foo\\\\str_contains\") 0007\n\
                 0002 INIT_NS_FCALL_BY_NAME 2 string(\"Foo\\\\str_contains\")\n\
                 0003 SEND_VAR_EX CV0($a) 1\n\
                 0004 SEND_VAL_EX string(\"y\") 2\n\
                 0005 V3 = DO_FCALL_BY_NAME\n\
                 0006 JMP 0008\n\
                 0007 V3 = FRAMELESS_ICALL_2(str_contains) CV0($a) string(\"y\")\n\
                 0008 ASSIGN CV1($b) V3\n\
                 0009 T5 = FRAMELESS_ICALL_1(trim) CV0($a)\n\
                 0010 FREE T5\n\
                 0011 JMP_FRAMELESS 40 string(\"foo\\\\trim\") 0019\n\
                 0012 INIT_NS_FCALL_BY_NAME 1 string(\"Foo\\\\trim\")\n\
                 0013 INIT_NS_FCALL_BY_NAME 1 string(\"Foo\\\\nope\")\n\
                 0014 SEND_VAR_EX CV0($a) 1\n\
                 0015 V6 = DO_FCALL_BY_NAME\n\
                 0016 SEND_VAR_NO_REF_EX V6 1\n\
                 0017 V7 = DO_FCALL_BY_NAME\n\
                 0018 JMP 0023\n\
                 0019 INIT_NS_FCALL_BY_NAME 1 string(\"Foo\\\\nope\")\n\
                 0020 SEND_VAR_EX CV0($a) 1\n\
                 0021 V8 = DO_FCALL_BY_NAME\n\
                 0022 V7 = FRAMELESS_ICALL_1(trim) V8\n\
                 0023 FREE V7\n\
                 0024 RETURN int(1)\n"
            )
        );
    }

    /// The three constants `zend_add_ns_func_name_literal` lays down,
    /// in a row: the name with its namespace as written, lowered, and
    /// the lowered name alone; a second `namespace` statement moves the
    /// namespace, and a qualified name is the namespace before it,
    /// resolved at compile time as `INIT_FCALL_BY_NAME`.
    #[test]
    fn a_namespace_call_carries_php_s_three_constants() {
        let (unit, _strings) =
            unit_and_strings(b"<?php namespace A; namespace B\\C; Str_Contains(\"a\", \"b\"); D\\e(); namespace\\f();");
        let literal = |at: usize| {
            unit.main.literals[at]
                .as_str()
                .map(|s| String::from_utf8_lossy(s.bytes()).into_owned())
        };
        assert_eq!(
            unit.main.oplines[1].opcode,
            Opcode::InitNsFcallByName,
            "after the JMP_FRAMELESS"
        );
        let at = unit.main.oplines[1].op2 as usize;
        assert_eq!(literal(at).as_deref(), Some("B\\C\\Str_Contains"));
        assert_eq!(literal(at + 1).as_deref(), Some("b\\c\\str_contains"));
        assert_eq!(literal(at + 2).as_deref(), Some("str_contains"));
        let by_name: Vec<String> = unit
            .main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::InitFcallByName)
            .filter_map(|op| literal(op.op2 as usize))
            .collect();
        assert_eq!(by_name, ["B\\C\\D\\e", "B\\C\\f"]);
    }

    /// `zend_compile_namespace`: a first `namespace` after any statement
    /// is php's `E_COMPILE_ERROR` in php's words at the statement's line.
    #[test]
    fn a_namespace_after_a_statement_is_php_s_compile_error() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile_parsed(b"<?php 1;\nnamespace Foo;", &bundled(), &mut strings).err();
        assert_eq!(
            trouble,
            Some(CompileError {
                level: Level::CompileError,
                message: "Namespace declaration statement has to be the very first statement or after any declare call in the script".into(),
                line: 2,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            })
        );
    }

    #[test]
    fn what_the_reader_cannot_read_is_a_compile_error_at_its_line() {
        let mut strings = php_heap::Strings::new();
        let trouble =
            compile_parsed(b"<?php\n$x . list($a);", &Options::default(), &mut strings).err();
        assert_eq!(trouble.map(|e| e.line), Some(2));
    }

    /// A file php's parser refuses is refused here in php's words, the
    /// first diagnostic `parse` handed back, before the walk is asked
    /// anything: `echo 1 +;` is php's `unexpected token ";"` and not the
    /// walk's sentence about the expression.
    #[test]
    fn what_php_refuses_is_the_first_parse_error_in_phps_words() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile_parsed(b"<?php\necho 1 +;", &Options::default(), &mut strings).err();
        assert_eq!(
            trouble,
            Some(CompileError {
                level: Level::Parse,
                message: "syntax error, unexpected token \";\"".into(),
                line: 2,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            })
        );
    }

    /// #302's program under the oracle's `[regime.dump]`: `@` as
    /// `zend_compile_silence` shapes it, `BEGIN_SILENCE` into a
    /// temporary, the call, `END_SILENCE` over that temporary, and the
    /// `silence` live range from the opline after the begin to the end,
    /// read off a dump of the oracle's for the first time.
    #[test]
    fn a_silenced_call_is_php_s_op_array_ending_in_its_silence_live_range() {
        let (unit, _strings) = unit_and_strings(b"<?php echo @str_repeat(\"a\", 2);");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                8,
                0,
                2,
                "0000 T0 = BEGIN_SILENCE\n\
                 0001 INIT_FCALL 2 112 string(\"str_repeat\")\n\
                 0002 SEND_VAL string(\"a\") 1\n\
                 0003 SEND_VAL int(2) 2\n\
                 0004 V1 = DO_ICALL\n\
                 0005 END_SILENCE T0\n\
                 0006 ECHO V1\n\
                 0007 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0005 (silence)\n\
                 \x20    1: 0005 - 0006 (tmp/var)\n"
            )
        );
    }

    /// The rest of `zend_compile_silence` against the oracle's dump: a
    /// variable under `@` is `zend_compile_simple_var_no_cv`'s
    /// `FETCH_R (local)` over its name into a temporary and no CV, `$this`
    /// among them; and `zend_do_free` steps back over the `END_SILENCE`,
    /// so a call's `VAR` nothing reads is marked unused, a `$a++` becomes
    /// `PRE_INC`, an `ASSIGN`'s temporary is marked unused, and a fetch's
    /// temporary is freed after the end. Two `@` nest, each its own range;
    /// `$b`, the one CV, moves every temporary's number up by one.
    #[test]
    fn a_variable_under_a_silence_is_fetched_by_name_and_its_value_freed_past_the_end() {
        assert_eq!(
            oplines_of(
                b"<?php echo @$a, @$this; @$a; @str_repeat(\"a\", 2); @$b++; @$b = 1; @@$c;"
            ),
            "0000 T1 = BEGIN_SILENCE\n\
             0001 T2 = FETCH_R (local) string(\"a\")\n\
             0002 END_SILENCE T1\n\
             0003 ECHO T2\n\
             0004 T3 = BEGIN_SILENCE\n\
             0005 T4 = FETCH_R (local) string(\"this\")\n\
             0006 END_SILENCE T3\n\
             0007 ECHO T4\n\
             0008 T5 = BEGIN_SILENCE\n\
             0009 T6 = FETCH_R (local) string(\"a\")\n\
             0010 END_SILENCE T5\n\
             0011 FREE T6\n\
             0012 T7 = BEGIN_SILENCE\n\
             0013 INIT_FCALL 2 112 string(\"str_repeat\")\n\
             0014 SEND_VAL string(\"a\") 1\n\
             0015 SEND_VAL int(2) 2\n\
             0016 DO_ICALL\n\
             0017 END_SILENCE T7\n\
             0018 T9 = BEGIN_SILENCE\n\
             0019 PRE_INC CV0($b)\n\
             0020 END_SILENCE T9\n\
             0021 T11 = BEGIN_SILENCE\n\
             0022 ASSIGN CV0($b) int(1)\n\
             0023 END_SILENCE T11\n\
             0024 T13 = BEGIN_SILENCE\n\
             0025 T14 = BEGIN_SILENCE\n\
             0026 T15 = FETCH_R (local) string(\"c\")\n\
             0027 END_SILENCE T14\n\
             0028 END_SILENCE T13\n\
             0029 FREE T15\n\
             0030 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    1: 0001 - 0002 (silence)\n\
             \x20    2: 0002 - 0003 (tmp/var)\n\
             \x20    3: 0005 - 0006 (silence)\n\
             \x20    4: 0006 - 0007 (tmp/var)\n\
             \x20    5: 0009 - 0010 (silence)\n\
             \x20    6: 0010 - 0011 (tmp/var)\n\
             \x20    7: 0013 - 0017 (silence)\n\
             \x20    9: 0019 - 0020 (silence)\n\
             \x20    11: 0022 - 0023 (silence)\n\
             \x20    13: 0025 - 0028 (silence)\n\
             \x20    14: 0026 - 0027 (silence)\n\
             \x20    15: 0027 - 0029 (tmp/var)\n"
        );
        assert_eq!(
            refused(b"<?php class C { const X = @1; }"),
            Some(CompileError {
                level: Level::CompileError,
                message: "Constant expression contains invalid operations".into(),
                line: 1,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }),
            "`zend_is_allowed_in_const_expr` has no `ZEND_AST_SILENCE`"
        );
    }

    /// #303's program under the oracle's `[regime.dump]`: a five-piece
    /// interpolation as `zend_compile_encaps_list` shapes it, `ROPE_INIT`
    /// with the count, three `ROPE_ADD` and `ROPE_END`, the rope's
    /// temporary taken after the value's and two more slots reserved
    /// for its five string pointers, and the `rope` live range from the
    /// init to the end, read off a dump of the oracle's for the first
    /// time.
    #[test]
    fn a_three_part_interpolation_is_php_s_op_array_ending_in_its_rope_live_range() {
        let (unit, _strings) = unit_and_strings(b"<?php $a = 1; $b = 2; echo \"$a-$b-$a\";");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                9,
                2,
                6,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 ASSIGN CV1($b) int(2)\n\
                 0002 T5 = ROPE_INIT 5 CV0($a)\n\
                 0003 T5 = ROPE_ADD 1 T5 string(\"-\")\n\
                 0004 T5 = ROPE_ADD 2 T5 CV1($b)\n\
                 0005 T5 = ROPE_ADD 3 T5 string(\"-\")\n\
                 0006 T4 = ROPE_END 4 T5 CV0($a)\n\
                 0007 ECHO T4\n\
                 0008 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    5: 0002 - 0006 (rope)\n"
            )
        );
    }

    /// The rest of `zend_compile_encaps_list` and `zend_compile_rope_finalize`
    /// against the oracle's dump: one piece is a `CAST (string)` and no
    /// rope; two are a `FAST_CONCAT` over the `NOP` the first piece's
    /// opline becomes, and no live range; a constant piece is folded onto
    /// the constant before it, so the escaped `\{` and the plain `$`,
    /// `{` and `}` are one string; `$this` is a `FETCH_THIS` piece; a
    /// rope's value nothing reads is freed after its end; and `${a}` is
    /// php's deprecation at compile time. In a class constant an
    /// interpolation is php's `Constant expression contains invalid
    /// operations`, since `zend_is_allowed_in_const_expr` has no
    /// `ZEND_AST_ENCAPS_LIST`.
    #[test]
    fn one_and_two_piece_interpolations_are_a_cast_and_a_fast_concat_and_the_rest_a_rope() {
        assert_eq!(
            oplines_of(
                b"<?php $x = \"{$a}\" . str_repeat(\"p$a\", \"$a$a\"); \"$a$b$c\"; \"x$a\"; echo \"a\\{$a}b\\$x{ $a}$ $1{$this}\";"
            ),
            "0000 T4 = CAST (string) CV1($a)\n\
             0001 INIT_FCALL 2 112 string(\"str_repeat\")\n\
             0002 NOP\n\
             0003 T5 = FAST_CONCAT string(\"p\") CV1($a)\n\
             0004 SEND_VAL T5 1\n\
             0005 NOP\n\
             0006 T6 = FAST_CONCAT CV1($a) CV1($a)\n\
             0007 SEND_VAL T6 2\n\
             0008 V7 = DO_ICALL\n\
             0009 T8 = CONCAT T4 V7\n\
             0010 ASSIGN CV0($x) T8\n\
             0011 T11 = ROPE_INIT 3 CV1($a)\n\
             0012 T11 = ROPE_ADD 1 T11 CV2($b)\n\
             0013 T10 = ROPE_END 2 T11 CV3($c)\n\
             0014 FREE T10\n\
             0015 NOP\n\
             0016 T13 = FAST_CONCAT string(\"x\") CV1($a)\n\
             0017 FREE T13\n\
             0018 T16 = ROPE_INIT 6 string(\"a\\\\{\")\n\
             0019 T16 = ROPE_ADD 1 T16 CV1($a)\n\
             0020 T16 = ROPE_ADD 2 T16 string(\"}b$x{ \")\n\
             0021 T16 = ROPE_ADD 3 T16 CV1($a)\n\
             0022 T16 = ROPE_ADD 4 T16 string(\"}$ $1\")\n\
             0023 T14 = FETCH_THIS\n\
             0024 T15 = ROPE_END 5 T16 T14\n\
             0025 ECHO T15\n\
             0026 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0001 - 0009 (tmp/var)\n\
             \x20    11: 0011 - 0013 (rope)\n\
             \x20    16: 0018 - 0024 (rope)\n"
        );
        let (unit, _strings) = unit_and_strings(b"<?php\necho \"{$a}x${a}\";");
        assert_eq!(
            unit.diagnostics,
            vec![Diagnostic {
                level: Level::Deprecated,
                line: 2,
                message: "Using ${var} in strings is deprecated, use {$var} instead".into(),
            }]
        );
        assert_eq!(
            refused(b"<?php class C { const X = \"$a\"; }"),
            Some(CompileError {
                level: Level::CompileError,
                message: "Constant expression contains invalid operations".into(),
                line: 1,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }),
            "`zend_is_allowed_in_const_expr` has no `ZEND_AST_ENCAPS_LIST`"
        );
    }

    /// #356's program under the oracle's `[regime.dump]`: a dimension
    /// piece is `zend_compile_dim` under `BP_VAR_R`, `FETCH_DIM_R` over
    /// the CV and the offset as `encaps_var_offset` read it, an integer
    /// for `0` and a string for `k`; a property piece is
    /// `zend_compile_prop`, `FETCH_OBJ_R`; and `{$a[0]}` and `{$o->p}`
    /// are the same two; each piece's opline before its `ROPE_ADD`, the
    /// rope's temporary taken after every piece's and the live range
    /// from the init to the end.
    #[test]
    fn a_dimension_and_a_property_piece_are_fetched_before_their_rope_add() {
        let (unit, _strings) = unit_and_strings(
            b"<?php $a = [1]; $o = new stdClass; echo \"$a[0]-$a[k]-$o->p-{$a[0]}-{$o->p}\";",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                20,
                2,
                15,
                "0000 ASSIGN CV0($a) array(...)\n\
                 0001 V3 = NEW 0 string(\"stdClass\")\n\
                 0002 DO_FCALL\n\
                 0003 ASSIGN CV1($o) V3\n\
                 0004 T6 = FETCH_DIM_R CV0($a) int(0)\n\
                 0005 T12 = ROPE_INIT 9 T6\n\
                 0006 T12 = ROPE_ADD 1 T12 string(\"-\")\n\
                 0007 T7 = FETCH_DIM_R CV0($a) string(\"k\")\n\
                 0008 T12 = ROPE_ADD 2 T12 T7\n\
                 0009 T12 = ROPE_ADD 3 T12 string(\"-\")\n\
                 0010 T8 = FETCH_OBJ_R CV1($o) string(\"p\")\n\
                 0011 T12 = ROPE_ADD 4 T12 T8\n\
                 0012 T12 = ROPE_ADD 5 T12 string(\"-\")\n\
                 0013 T9 = FETCH_DIM_R CV0($a) int(0)\n\
                 0014 T12 = ROPE_ADD 6 T12 T9\n\
                 0015 T12 = ROPE_ADD 7 T12 string(\"-\")\n\
                 0016 T10 = FETCH_OBJ_R CV1($o) string(\"p\")\n\
                 0017 T11 = ROPE_END 8 T12 T10\n\
                 0018 ECHO T11\n\
                 0019 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0002 - 0003 (new)\n\
                 \x20    12: 0005 - 0017 (rope)\n"
            )
        );
    }

    /// The rest of `encaps_var` against the oracle's dump: the offset
    /// negated as `zend_negate_num_string` negates it, `-0` and a
    /// decimal past `ZEND_LONG_MAX` string keys; a method call under
    /// `{…}` its `INIT_METHOD_CALL` and `DO_FCALL` before the `CAST`;
    /// `${a[0]}` a `FETCH_DIM_R` with php's `${var}` deprecation on the
    /// unit; and `${expr}` `zend_compile_simple_var` with the variable
    /// variables one: `${$b}` and `${$b[0]}` a `FETCH_R (local)` over
    /// the value, `${'a'}`, `${1}` and `${'a' . 'b'}` the CV php's
    /// parser names, `${'_GET'}` the auto-global's `FETCH_R (global)`.
    #[test]
    fn the_string_offset_spellings_and_the_variable_variable_are_php_s() {
        assert_eq!(
            oplines_of(
                b"<?php $a = [1]; echo \"$a[-1]$a[-0]$a[9223372036854775808]\"; echo \"{$o->m()}\"; echo \"$a[$i]\";"
            ),
            "0000 ASSIGN CV0($a) array(...)\n\
             0001 T4 = FETCH_DIM_R CV0($a) int(-1)\n\
             0002 T8 = ROPE_INIT 3 T4\n\
             0003 T5 = FETCH_DIM_R CV0($a) string(\"-0\")\n\
             0004 T8 = ROPE_ADD 1 T8 T5\n\
             0005 T6 = FETCH_DIM_R CV0($a) string(\"9223372036854775808\")\n\
             0006 T7 = ROPE_END 2 T8 T6\n\
             0007 ECHO T7\n\
             0008 INIT_METHOD_CALL 0 CV1($o) string(\"m\")\n\
             0009 V10 = DO_FCALL\n\
             0010 T11 = CAST (string) V10\n\
             0011 ECHO T11\n\
             0012 T12 = FETCH_DIM_R CV0($a) CV2($i)\n\
             0013 T13 = CAST (string) T12\n\
             0014 ECHO T13\n\
             0015 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    8: 0002 - 0006 (rope)\n"
        );
        let (unit, _strings) = unit_and_strings(b"<?php\necho \"${a[0]}\";");
        assert_eq!(
            unit.dump(&[])
                .text()
                .split_once("RANGE[0..0]\n")
                .map(|(_, rest)| rest),
            Some(
                "0000 T1 = FETCH_DIM_R CV0($a) int(0)\n\
                 0001 T2 = CAST (string) T1\n\
                 0002 ECHO T2\n\
                 0003 RETURN int(1)\n"
            )
        );
        assert_eq!(
            unit.diagnostics,
            vec![Diagnostic {
                level: Level::Deprecated,
                line: 2,
                message: "Using ${var} in strings is deprecated, use {$var} instead".into(),
            }]
        );
        let (unit, _strings) = unit_and_strings(
            b"<?php\necho \"${$b}${$b[0]}${'a'}${1}${'a' . 'b'}${'_GET'}${'this'}\";",
        );
        assert_eq!(
            unit.dump(&[])
                .text()
                .split_once("RANGE[0..0]\n")
                .map(|(_, rest)| rest),
            Some(
                "0000 T4 = FETCH_R (local) CV0($b)\n\
                 0001 T10 = ROPE_INIT 7 T4\n\
                 0002 T5 = FETCH_DIM_R CV0($b) int(0)\n\
                 0003 T6 = FETCH_R (local) T5\n\
                 0004 T10 = ROPE_ADD 1 T10 T6\n\
                 0005 T10 = ROPE_ADD 2 T10 CV1($a)\n\
                 0006 T10 = ROPE_ADD 3 T10 CV2($1)\n\
                 0007 T10 = ROPE_ADD 4 T10 CV3($ab)\n\
                 0008 T7 = FETCH_R (global) string(\"_GET\")\n\
                 0009 T10 = ROPE_ADD 5 T10 T7\n\
                 0010 T8 = FETCH_THIS\n\
                 0011 T9 = ROPE_END 6 T10 T8\n\
                 0012 ECHO T9\n\
                 0013 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    10: 0001 - 0011 (rope)\n"
            )
        );
        assert_eq!(
            unit.diagnostics.len(),
            7,
            "one variable-variables deprecation per `${{expr}}` piece"
        );
        assert_eq!(
            unit.diagnostics.first(),
            Some(&Diagnostic {
                level: Level::Deprecated,
                line: 2,
                message:
                    "Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead"
                        .into(),
            })
        );
    }

    /// The oplines and the live-range table of one source's main op
    /// array, the header left off: what the oracle's dump prints after
    /// `; return  [] RANGE[0..0]`.
    fn oplines_of(source: &[u8]) -> String {
        let dump = dump_of(source);
        dump.split_once("RANGE[0..0]\n")
            .map_or(dump.clone(), |(_, rest)| rest.to_owned())
    }

    /// The message one source refuses with, or nothing.
    fn refused_message(source: &[u8]) -> Option<String> {
        refused(source).map(|e| e.message.into_owned())
    }

    /// The compile error one source refuses with, or nothing.
    fn refused(source: &[u8]) -> Option<CompileError> {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings).err()
    }

    /// #3029: a refusal carries what landed in php's tables before it,
    /// each top-level function and each class bound early in the order
    /// it landed, beside what was raised by then; a class its opline
    /// binds, and a closure slot the refused statement took, are not.
    #[test]
    fn a_refusal_carries_the_functions_and_the_early_bound_classes_that_landed() {
        use php_unit::TopLevel;
        let source = b"<?php\nfunction f(int $a = null) { return fn() => 1; }\nclass A {}\ninterface I {}\nclass C implements I {}\nfunction g() {}\n$h = function () { break 2; };\n";
        let declared = refused(source).and_then(|error| error.declared);
        assert!(declared.is_some(), "the refusal carries what landed");
        let Some(declared) = declared else {
            return;
        };
        assert_eq!(
            declared.unit.top_level,
            vec![
                TopLevel::Function(0),
                TopLevel::Class(0),
                TopLevel::Class(1),
                TopLevel::Function(1),
            ]
        );
        assert_eq!(declared.raised_before, vec![1, 1, 1, 1]);
        assert_eq!(declared.unit.dynamic.len(), 1, "f's closure, not $h's");
        assert_eq!(declared.unit.runtime_keys.len(), 1, "C, its opline's");
        let keys = refused(source).map(|error| error.keys_made);
        assert_eq!(keys, Some(1), "the key counted with what landed");
    }

    /// #3029: a refusal php throws is its parser's, before `zend_compile`
    /// declared anything, so it carries nothing that landed.
    #[test]
    fn a_thrown_refusal_carries_nothing_declared() {
        let source = b"<?php\nfunction f() {}\nclass A {}\nif (1) { __HALT_COMPILER(); }\n";
        let refused = refused(source);
        assert!(refused.as_ref().is_some_and(|error| error.thrown));
        assert!(refused.is_some_and(|error| error.declared.is_none()));
    }

    /// #3041: a refusal carries the keys its compile had made whatever it
    /// declared, an anonymous class's among them and one whose body the
    /// compile refused in; a parse error made none.
    #[test]
    fn a_refusal_carries_the_keys_it_made_declared_or_not() {
        let source = b"<?php\n$o = new class {};\n$p = new class { function f() { break 2; } };\n";
        let error = refused(source);
        assert!(error.as_ref().is_some_and(|error| error.declared.is_none()));
        assert_eq!(error.map(|error| error.keys_made), Some(2));
        let parse = refused(b"<?php\n$o = new class {};\n$p = ;\n").map(|error| error.keys_made);
        assert_eq!(parse, Some(0), "a parse error compiled nothing");
    }

    /// #256's program under the oracle's `[regime.dump]`: a `switch` of
    /// fewer cases than `should_use_jumptable` asks for is the
    /// `IS_EQUAL` and `JMPNZ` chain into one shared temporary, each
    /// case's `break` a `JMP` to the switch's end, and a `match` of two
    /// constant conditions is `MATCH` over its jump table with a
    /// `QM_ASSIGN` per arm into one temporary and the live range over it.
    #[test]
    fn a_switch_of_few_cases_is_the_is_equal_chain_and_a_match_is_its_jump_table() {
        let source = b"<?php\n$a = 1;\nswitch ($a) { case 1: echo \"a\"; break; case 2: echo \"b\"; break; default: echo \"c\"; }\necho match ($a) { 1 => \"x\", \"s\" => \"y\", default => \"z\" };\n";
        let (unit, _strings) = unit_and_strings(source);
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=20, args=0, vars=1, tmps=4)\n\
             \x20    ; (before optimizer)\n     ; :1-5\n     ; return  [] RANGE[0..0]\n\
             0000 ASSIGN CV0($a) int(1)\n\
             0001 T2 = IS_EQUAL CV0($a) int(1)\n\
             0002 JMPNZ T2 0006\n\
             0003 T2 = IS_EQUAL CV0($a) int(2)\n\
             0004 JMPNZ T2 0008\n\
             0005 JMP 0010\n\
             0006 ECHO string(\"a\")\n\
             0007 JMP 0011\n\
             0008 ECHO string(\"b\")\n\
             0009 JMP 0011\n\
             0010 ECHO string(\"c\")\n\
             0011 MATCH CV0($a) 1: 0012, \"s\": 0014, default: 0016\n\
             0012 T4 = QM_ASSIGN string(\"x\")\n\
             0013 JMP 0018\n\
             0014 T4 = QM_ASSIGN string(\"y\")\n\
             0015 JMP 0018\n\
             0016 T4 = QM_ASSIGN string(\"z\")\n\
             0017 JMP 0018\n\
             0018 ECHO T4\n\
             0019 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0017 - 0018 (tmp/var)\n"
        );
        let table = &unit.main.oplines[11];
        assert_eq!(table.opcode, Opcode::Match);
        assert_eq!(table.op2_type, OperandKind::Const);
        let table = unit.main.literals[table.op2 as usize].as_arr();
        assert!(
            table.is_some_and(|table| table.is_immutable()),
            "a jump table is IS_ARRAY_IMMUTABLE"
        );
        assert_eq!(table.map(|table| table.len()), Some(2));
    }

    /// #395's program under the oracle's `[regime.dump]`:
    /// `zend_compile_conditional` is `JMPZ` past the true arm, both arms
    /// writing the one temporary, and a `JMP` over the false arm;
    /// `zend_compile_shorthand_conditional` is `JMP_SET` over the left
    /// operand with the right operand's `QM_ASSIGN` into the same
    /// temporary; and a parenthesised conditional as a condition is the
    /// `JMPZ` over the temporary the inner one left.
    #[test]
    fn a_conditional_is_jmpz_between_two_qm_assigns_and_a_shorthand_one_is_jmp_set() {
        assert_eq!(
            oplines_of(
                b"<?php\n$a = 1;\necho $a ? \"t\" : \"f\";\necho $a ?: \"f\";\necho ($a ? \"t\" : \"f\") ? \"y\" : \"n\";\n"
            ),
            "0000 ASSIGN CV0($a) int(1)\n\
             0001 JMPZ CV0($a) 0004\n\
             0002 T2 = QM_ASSIGN string(\"t\")\n\
             0003 JMP 0005\n\
             0004 T2 = QM_ASSIGN string(\"f\")\n\
             0005 ECHO T2\n\
             0006 T3 = JMP_SET CV0($a) 0008\n\
             0007 T3 = QM_ASSIGN string(\"f\")\n\
             0008 ECHO T3\n\
             0009 JMPZ CV0($a) 0012\n\
             0010 T4 = QM_ASSIGN string(\"t\")\n\
             0011 JMP 0013\n\
             0012 T4 = QM_ASSIGN string(\"f\")\n\
             0013 JMPZ T4 0016\n\
             0014 T5 = QM_ASSIGN string(\"y\")\n\
             0015 JMP 0017\n\
             0016 T5 = QM_ASSIGN string(\"n\")\n\
             0017 ECHO T5\n\
             0018 RETURN int(1)\n"
        );
    }

    /// The three sentences `zend_compile_conditional` refuses an
    /// unparenthesised nested conditional with, by which of the two
    /// carried a middle arm; `(a ?: b) ?: c` is the fourth grouping,
    /// which php lets stand. A conditional between parentheses carries
    /// php's `ZEND_PARENTHESIZED_CONDITIONAL` and is no refusal either.
    #[test]
    fn an_unparenthesised_nested_conditional_is_refused_in_php_s_words() {
        for (source, message) in [
            (
                &b"<?php\necho 1 ? 2 : 3 ? 4 : 5;\n"[..],
                Some(
                    "Unparenthesized `a ? b : c ? d : e` is not supported. Use either `(a ? b : c) ? d : e` or `a ? b : (c ? d : e)`",
                ),
            ),
            (
                b"<?php\necho 1 ? 2 : 3 ?: 4;\n",
                Some(
                    "Unparenthesized `a ? b : c ?: d` is not supported. Use either `(a ? b : c) ?: d` or `a ? b : (c ?: d)`",
                ),
            ),
            (
                b"<?php\necho 1 ?: 2 ? 3 : 4;\n",
                Some(
                    "Unparenthesized `a ?: b ? c : d` is not supported. Use either `(a ?: b) ? c : d` or `a ?: (b ? c : d)`",
                ),
            ),
            (b"<?php\necho 1 ?: 2 ?: 3;\n", None),
            (b"<?php\necho (1 ? 2 : 3) ? 4 : 5;\n", None),
            (b"<?php\necho 1 ? 2 : (3 ? 4 : 5);\n", None),
        ] {
            match (refused(source), message) {
                (Some(error), Some(message)) => {
                    assert_eq!(error.level, Level::CompileError, "{message}");
                    assert_eq!(error.message, message);
                    assert_eq!(error.line, 2, "{message}");
                }
                (None, None) => {}
                (found, wanted) => assert_eq!(
                    found.map(|error| error.message.into_owned()),
                    wanted.map(str::to_owned),
                    "{:?}",
                    String::from_utf8_lossy(source)
                ),
            }
        }
    }

    /// An expression the walk cannot emit yet inside parentheses: the
    /// refusal names the expression itself, in the sentence the phpt
    /// lane reads as a construct not read yet, where the reader named
    /// the parenthesis before (#395). A parenthesis nothing closes is
    /// the parser's own refusal, raised before the walk runs.
    #[test]
    fn an_unread_expression_inside_parentheses_names_the_expression() {
        let compiled = || CompileError {
            level: Level::Parse,
            message: String::from("compiled").into(),
            line: 0,
            raised: Vec::new(),
            thrown: false,
            stage: crate::Stage::Compiling,
            declared: None,
            keys_made: 0,
        };
        let error = refused(b"<?php\n$a = (function () { declare(ticks=0): enddeclare; });\n")
            .unwrap_or_else(compiled);
        assert_eq!(error.level, Level::Parse);
        assert_eq!(
            error.message,
            "a `declare` in the alternative syntax this compiler cannot read yet"
        );
        assert_eq!(error.line, 2);
        let unclosed = refused(b"<?php\n$a = (1 + 2;\n").unwrap_or_else(compiled);
        assert_eq!(unclosed.message, "syntax error, unexpected token \";\"");
    }

    /// `global $a, $b;` is `zend_compile_global_var`'s `BIND_GLOBAL` per
    /// variable, the CV in `op1` and its name in `op2`, where the
    /// statement was refused as not read before; a name written out
    /// under `${…}` is the same CV.
    #[test]
    fn a_global_statement_binds_each_cv_by_name() {
        assert_eq!(
            oplines_of(b"<?php global $a, ${'b'}; echo $a;"),
            "0000 BIND_GLOBAL CV0($a) string(\"a\")\n\
             0001 BIND_GLOBAL CV1($b) string(\"b\")\n\
             0002 ECHO CV0($a)\n\
             0003 RETURN int(1)\n"
        );
    }

    /// `++` and `--` over an element and over a property, which were
    /// refused as over what is no variable, as the oracle's
    /// `[regime.dump]` has them: an element is `FETCH_DIM_RW` per link
    /// and the step over the last `VAR`, a property is the last fetch
    /// turned into `PRE_INC_OBJ` and its kin, and a `POST_*` nothing
    /// reads is the `PRE_*` `zend_do_free` makes of it.
    #[test]
    fn a_step_over_an_element_or_a_property_is_the_fetch_under_rw() {
        assert_eq!(
            oplines_of(
                b"<?php $a[0]++; $x = $a[0][1]--; $o->p++; $y = --$o->q->r; ++$a[0]->p; $b[]++;"
            ),
            "0000 V5 = FETCH_DIM_RW CV0($a) int(0)\n\
             0001 PRE_INC V5\n\
             0002 V7 = FETCH_DIM_RW CV0($a) int(0)\n\
             0003 V8 = FETCH_DIM_RW V7 int(1)\n\
             0004 T9 = POST_DEC V8\n\
             0005 ASSIGN CV1($x) T9\n\
             0006 PRE_INC_OBJ CV2($o) string(\"p\")\n\
             0007 V12 = FETCH_OBJ_RW CV2($o) string(\"q\")\n\
             0008 T13 = PRE_DEC_OBJ V12 string(\"r\")\n\
             0009 ASSIGN CV3($y) T13\n\
             0010 V15 = FETCH_DIM_RW CV0($a) int(0)\n\
             0011 PRE_INC_OBJ V15 string(\"p\")\n\
             0012 V17 = FETCH_DIM_RW CV4($b) NEXT\n\
             0013 PRE_INC V17\n\
             0014 RETURN int(1)\n"
        );
    }

    /// A class written in parentheses is an expression, `'(' expr ')'` in
    /// `class_name_reference` and `fully_dereferenceable`, so `new (C)`,
    /// `(C)::X` and `instanceof (C)` read the constant `C` and fetch the
    /// class it names (`FETCH_CONSTANT`, `FETCH_CLASS`) where `new C` names
    /// the class `C`; a string written where the class goes, folded by the
    /// parser from `"A" . "B"` too, is the class it names as written
    /// (`ZEND_NAME_FQ`). The dump is the oracle's under `[regime.dump]` at
    /// 8.5.9 (#2680).
    #[test]
    fn a_class_in_parentheses_is_the_expression_and_a_string_the_class_it_names() {
        assert_eq!(
            dump_of(b"<?php new (C); new C; echo (C)::X, (\"A\" . \"B\")::class, \"Foo\"::X, (\"\\\\Foo\")::X; var_dump($a instanceof (C));"),
            "\n$_main:\n\
             \x20    ; (lines=24, args=0, vars=1, tmps=15)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T1 = FETCH_CONSTANT string(\"C\")\n\
             0001 V2 = FETCH_CLASS (exception) T1\n\
             0002 V3 = NEW 0 V2\n\
             0003 DO_FCALL\n\
             0004 FREE V3\n\
             0005 V5 = NEW 0 string(\"C\")\n\
             0006 DO_FCALL\n\
             0007 FREE V5\n\
             0008 T7 = FETCH_CONSTANT string(\"C\")\n\
             0009 V8 = FETCH_CLASS (exception) T7\n\
             0010 T9 = FETCH_CLASS_CONSTANT V8 string(\"X\")\n\
             0011 ECHO T9\n\
             0012 ECHO string(\"AB\")\n\
             0013 T10 = FETCH_CLASS_CONSTANT string(\"Foo\") string(\"X\")\n\
             0014 ECHO T10\n\
             0015 T11 = FETCH_CLASS_CONSTANT string(\"Foo\") string(\"X\")\n\
             0016 ECHO T11\n\
             0017 INIT_FCALL 1 96 string(\"var_dump\")\n\
             0018 T12 = FETCH_CONSTANT string(\"C\")\n\
             0019 V13 = FETCH_CLASS (no-autoload) (silent) (exception) T12\n\
             0020 T14 = INSTANCEOF CV0($a) V13\n\
             0021 SEND_VAL T14 1\n\
             0022 DO_ICALL\n\
             0023 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    3: 0003 - 0004 (new)\n\
             \x20    5: 0006 - 0007 (new)\n\
             "
        );
    }

    /// A hook with no body, an interface's or an abstract property's, is
    /// an op array `zend_foreach_op_array` walks, since it walks a
    /// property's hooks by scope and holds only the function table to
    /// `ZEND_ACC_ABSTRACT`: `get` prints `VERIFY_RETURN_TYPE` where the
    /// property is typed and `RETURN null`, `set` its `RECV` and
    /// `RETURN null`, and the abstract method prints nothing. The dump
    /// is the oracle's under `[regime.dump]` at 8.5.9 (#2674).
    #[test]
    fn an_abstract_hook_is_dumped_and_an_abstract_method_is_not() {
        assert_eq!(
            dump_of(
                b"<?php interface I { public mixed $i { get; } } \
                  abstract class A { abstract public int $p { get; set; } abstract function m(); }"
            ),
            "\n$_main:\n\
             \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \n\
             I::$i::get:\n\
             \x20    ; (lines=2, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 VERIFY_RETURN_TYPE\n\
             0001 RETURN null\n\
             \n\
             A::$p::get:\n\
             \x20    ; (lines=2, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 VERIFY_RETURN_TYPE\n\
             0001 RETURN null\n\
             \n\
             A::$p::set:\n\
             \x20    ; (lines=2, args=1, vars=1, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 CV0($value) = RECV 1\n\
             0001 RETURN null\n"
        );
    }

    /// A class completes after the classes its hooks declare, so php's
    /// `CG(class_table)` holds an anonymous class written in a hook ahead
    /// of the class around it, and the dump prints the anonymous class's
    /// hook and method before the hook it stands in. The dump is the
    /// oracle's under `[regime.dump]` at 8.5.9 (#2690).
    #[test]
    fn a_class_declared_in_a_hook_is_dumped_before_the_hook_s_class() {
        assert_eq!(
            dump_of(
                b"<?php class T { public $p { get => new class { \
                  public $q { get => 1; } function m() {} }; } }"
            ),
            "\n$_main:\n\
             \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \n\
             class@anonymous::m:\n\
             \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 RETURN null\n\
             \n\
             class@anonymous::$q::get:\n\
             \x20    ; (lines=2, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             0001 RETURN null\n\
             \n\
             T::$p::get:\n\
             \x20    ; (lines=5, args=0, vars=0, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 V0 = DECLARE_ANON_CLASS string(\"class@anonymous\\x00:1$0\")\n\
             0001 V1 = NEW 0 V0\n\
             0002 DO_FCALL\n\
             0003 RETURN V1\n\
             0004 RETURN null\n\
             LIVE RANGES:\n\
             \x20    1: 0002 - 0003 (new)\n"
        );
    }

    /// `zend_compile_class_const` runs `zend_eval_const_expr` over the class
    /// before it compiles it, so `"ab"[0]::d` reads the class `a` the
    /// dimension folds to, one `FETCH_CLASS_CONSTANT string("a")
    /// string("d")`, while the same dimension under a `new`, which runs no
    /// evaluator, is the `FETCH_DIM_R` and the `FETCH_CLASS` over it. The
    /// dump is the oracle's under `[regime.dump]` at 8.5.9 (#2691).
    #[test]
    fn the_class_of_a_class_constant_is_read_as_the_constant_evaluator_folds_it() {
        assert_eq!(
            dump_of(b"<?php echo \"ab\"[0]::d; new (\"ab\"[0]);"),
            "\n$_main:\n\
             \x20    ; (lines=8, args=0, vars=0, tmps=5)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T0 = FETCH_CLASS_CONSTANT string(\"a\") string(\"d\")\n\
             0001 ECHO T0\n\
             0002 T1 = FETCH_DIM_R string(\"ab\") int(0)\n\
             0003 V2 = FETCH_CLASS (exception) T1\n\
             0004 V3 = NEW 0 V2\n\
             0005 DO_FCALL\n\
             0006 FREE V3\n\
             0007 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    3: 0005 - 0006 (new)\n\
             "
        );
    }

    /// `C::class`, which was refused as not read, as the oracle's
    /// `[regime.dump]` has it: a name written out folds to its resolved
    /// spelling, `self` in a known scope to the class being compiled,
    /// and `static`, `self` where the scope is the includer's and a
    /// value are `FETCH_CLASS_NAME`.
    #[test]
    fn a_class_name_folds_where_php_resolves_it_and_is_fetched_where_not() {
        assert_eq!(
            oplines_of(b"<?php namespace N; echo Foo::class, \\Foo::class, self::class, static::class, $o::class;"),
            "0000 ECHO string(\"N\\\\Foo\")\n\
             0001 ECHO string(\"Foo\")\n\
             0002 T1 = FETCH_CLASS_NAME (self)\n\
             0003 ECHO T1\n\
             0004 T2 = FETCH_CLASS_NAME (static)\n\
             0005 ECHO T2\n\
             0006 T3 = FETCH_CLASS_NAME CV0($o)\n\
             0007 ECHO T3\n\
             0008 RETURN int(1)\n"
        );
    }

    /// `print` is `zend_compile_print`'s `ECHO` and the constant `1` as
    /// the expression's value, which a condition over it reads as it
    /// stands, the oracle's `[regime.dump]` of the same source.
    #[test]
    fn print_is_an_echo_whose_value_is_the_constant_one() {
        assert_eq!(
            oplines_of(b"<?php $r = print \"a\"; if (print $r) echo \"y\";"),
            "0000 ECHO string(\"a\")\n\
             0001 ASSIGN CV0($r) int(1)\n\
             0002 ECHO CV0($r)\n\
             0003 JMPZ int(1) 0005\n\
             0004 ECHO string(\"y\")\n\
             0005 RETURN int(1)\n"
        );
    }

    /// Five `long` cases, the default among them, are what
    /// `should_use_jumptable` asks for: a `SWITCH_LONG` over the table
    /// before the chain, its `extended_value` the default's target.
    #[test]
    fn a_switch_of_five_long_cases_is_switch_long_over_its_table_before_the_chain() {
        assert_eq!(
            oplines_of(b"<?php\n$a = 3;\nswitch ($a) { case 1: echo \"a\"; break; case 2: echo \"b\"; break; case 3: echo \"c\"; break; case 4: echo \"d\"; break; case 5: echo \"e\"; break; default: echo \"f\"; }\n"),
            "0000 ASSIGN CV0($a) int(3)\n\
             0001 SWITCH_LONG CV0($a) 1: 0013, 2: 0015, 3: 0017, 4: 0019, 5: 0021, default: 0023\n\
             0002 T2 = IS_EQUAL CV0($a) int(1)\n\
             0003 JMPNZ T2 0013\n\
             0004 T2 = IS_EQUAL CV0($a) int(2)\n\
             0005 JMPNZ T2 0015\n\
             0006 T2 = IS_EQUAL CV0($a) int(3)\n\
             0007 JMPNZ T2 0017\n\
             0008 T2 = IS_EQUAL CV0($a) int(4)\n\
             0009 JMPNZ T2 0019\n\
             0010 T2 = IS_EQUAL CV0($a) int(5)\n\
             0011 JMPNZ T2 0021\n\
             0012 JMP 0023\n\
             0013 ECHO string(\"a\")\n\
             0014 JMP 0024\n\
             0015 ECHO string(\"b\")\n\
             0016 JMP 0024\n\
             0017 ECHO string(\"c\")\n\
             0018 JMP 0024\n\
             0019 ECHO string(\"d\")\n\
             0020 JMP 0024\n\
             0021 ECHO string(\"e\")\n\
             0022 JMP 0024\n\
             0023 ECHO string(\"f\")\n\
             0024 RETURN int(1)\n"
        );
    }

    /// Two `string` cases are a `SWITCH_STRING`; with no default the
    /// table's default target is the switch's end. A numeric string
    /// case, which `==` compares as a number, keeps the chain alone
    /// (`determine_switch_jumptable_type`).
    #[test]
    fn a_switch_of_two_string_cases_is_switch_string_and_a_numeric_string_case_is_not() {
        assert_eq!(
            oplines_of(b"<?php\n$a = \"y\";\nswitch ($a) { case \"x\": echo \"a\"; break; case \"y\": echo \"b\"; break; }\n"),
            "0000 ASSIGN CV0($a) string(\"y\")\n\
             0001 SWITCH_STRING CV0($a) \"x\": 0007, \"y\": 0009, default: 0011\n\
             0002 T2 = IS_EQUAL CV0($a) string(\"x\")\n\
             0003 JMPNZ T2 0007\n\
             0004 T2 = IS_EQUAL CV0($a) string(\"y\")\n\
             0005 JMPNZ T2 0009\n\
             0006 JMP 0011\n\
             0007 ECHO string(\"a\")\n\
             0008 JMP 0011\n\
             0009 ECHO string(\"b\")\n\
             0010 JMP 0011\n\
             0011 RETURN int(1)\n"
        );
        assert_eq!(
            oplines_of(
                b"<?php\nswitch ($a) { case \"1\": echo \"a\"; break; case \"x\": echo \"b\"; }\n"
            ),
            "0000 T1 = IS_EQUAL CV0($a) string(\"1\")\n\
             0001 JMPNZ T1 0005\n\
             0002 T1 = IS_EQUAL CV0($a) string(\"x\")\n\
             0003 JMPNZ T1 0007\n\
             0004 JMP 0008\n\
             0005 ECHO string(\"a\")\n\
             0006 JMP 0008\n\
             0007 ECHO string(\"b\")\n\
             0008 RETURN int(1)\n"
        );
    }

    /// A constant `true` or `false` subject tests each case alone, a
    /// `JMPNZ` or a `JMPZ` over it; any other constant is an `IS_EQUAL`
    /// over two constants, swapped where `pass_two` swaps them; and a
    /// value written as two cases keeps the first target in the table,
    /// `zend_hash_index_add`, while the chain tests both.
    #[test]
    fn a_constant_subject_is_tested_as_php_tests_it_and_a_repeated_case_keeps_its_first_target() {
        assert_eq!(
            oplines_of(b"<?php\nswitch (true) { case $a: echo \"a\"; break; case 2: echo \"b\"; }\nswitch (false) { case $a: echo \"a\"; }\nswitch (3) { case $a: echo \"a\"; case 1: echo \"b\"; }\nswitch ($a) { case 1: case 2: case 3: case 4: case 4: echo \"x\"; }\n"),
            "0000 JMPNZ CV0($a) 0003\n\
             0001 JMPNZ int(2) 0005\n\
             0002 JMP 0006\n\
             0003 ECHO string(\"a\")\n\
             0004 JMP 0006\n\
             0005 ECHO string(\"b\")\n\
             0006 JMPZ CV0($a) 0008\n\
             0007 JMP 0009\n\
             0008 ECHO string(\"a\")\n\
             0009 T3 = IS_EQUAL CV0($a) int(3)\n\
             0010 JMPNZ T3 0014\n\
             0011 T3 = IS_EQUAL int(3) int(1)\n\
             0012 JMPNZ T3 0015\n\
             0013 JMP 0016\n\
             0014 ECHO string(\"a\")\n\
             0015 ECHO string(\"b\")\n\
             0016 SWITCH_LONG CV0($a) 1: 0028, 2: 0028, 3: 0028, 4: 0028, default: 0029\n\
             0017 T4 = IS_EQUAL CV0($a) int(1)\n\
             0018 JMPNZ T4 0028\n\
             0019 T4 = IS_EQUAL CV0($a) int(2)\n\
             0020 JMPNZ T4 0028\n\
             0021 T4 = IS_EQUAL CV0($a) int(3)\n\
             0022 JMPNZ T4 0028\n\
             0023 T4 = IS_EQUAL CV0($a) int(4)\n\
             0024 JMPNZ T4 0028\n\
             0025 T4 = IS_EQUAL CV0($a) int(4)\n\
             0026 JMPNZ T4 0028\n\
             0027 JMP 0029\n\
             0028 ECHO string(\"x\")\n\
             0029 RETURN int(1)\n"
        );
    }

    /// A temporary subject is compared by `CASE`, which keeps it alive
    /// for the next case, and freed at the switch's end, which is where
    /// `break` lands; `continue` in a `for` lands on the step; a
    /// `break 2` or `continue 2` out of a `switch` frees its subject on
    /// the way, `ZEND_FREE_ON_RETURN`, and the live range is split at
    /// that `FREE` with the distance to the loop's end in its `op2`.
    #[test]
    fn a_temporary_subject_is_case_and_freed_where_break_lands_and_on_the_way_out_of_a_switch() {
        assert_eq!(
            oplines_of(b"<?php\nswitch (strlen($a)) { case 1: echo \"a\"; break; case 2: echo \"b\"; }\nfor ($i = 0; $i < 3; $i++) { continue; break 1; }\nwhile ($i) { switch (strlen($a)) { case 1: continue 2; case 2: break 2; default: break; } }\n"),
            "0000 T2 = STRLEN CV0($a)\n\
             0001 T3 = CASE T2 int(1)\n\
             0002 JMPNZ T3 0006\n\
             0003 T3 = CASE T2 int(2)\n\
             0004 JMPNZ T3 0008\n\
             0005 JMP 0009\n\
             0006 ECHO string(\"a\")\n\
             0007 JMP 0009\n\
             0008 ECHO string(\"b\")\n\
             0009 FREE T2\n\
             0010 ASSIGN CV1($i) int(0)\n\
             0011 JMP 0015\n\
             0012 JMP 0014\n\
             0013 JMP 0017\n\
             0014 PRE_INC CV1($i)\n\
             0015 T6 = IS_SMALLER CV1($i) int(3)\n\
             0016 JMPNZ T6 0012\n\
             0017 JMP 0030\n\
             0018 T7 = STRLEN CV0($a)\n\
             0019 T8 = CASE T7 int(1)\n\
             0020 JMPNZ T8 0024\n\
             0021 T8 = CASE T7 int(2)\n\
             0022 JMPNZ T8 0026\n\
             0023 JMP 0028\n\
             0024 FREE T7 loop-end(+5)\n\
             0025 JMP 0030\n\
             0026 FREE T7 loop-end(+3)\n\
             0027 JMP 0031\n\
             0028 JMP 0029\n\
             0029 FREE T7\n\
             0030 JMPNZ CV1($i) 0018\n\
             0031 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    2: 0001 - 0009 (tmp/var)\n\
             \x20    7: 0019 - 0024 (tmp/var)\n\
             \x20    7: 0028 - 0029 (tmp/var)\n"
        );
    }

    /// `match` as `zend_compile_match` shapes it: two conditions of one
    /// arm share a target; no default arm is a `MATCH_ERROR` emitted
    /// first, the table's default target; one condition is the
    /// `IS_IDENTICAL` chain with its default `JMP` to the `MATCH_ERROR`;
    /// a default arm alone is the `JMP` to it; and no arm at all is the
    /// constant `null`.
    #[test]
    fn a_match_is_its_table_or_the_is_identical_chain_and_match_error_where_no_arm_is_default() {
        assert_eq!(
            oplines_of(b"<?php\n$a = 2;\necho match ($a) { 1, 2 => \"x\", \"s\" => \"y\" };\necho match (true) { default => \"d\" };\necho match ($a) { 1 => \"one\" };\n$x = match ($a) {};\n"),
            "0000 ASSIGN CV0($a) int(2)\n\
             0001 MATCH CV0($a) 1: 0003, 2: 0003, \"s\": 0005, default: 0002\n\
             0002 MATCH_ERROR CV0($a)\n\
             0003 T4 = QM_ASSIGN string(\"x\")\n\
             0004 JMP 0007\n\
             0005 T4 = QM_ASSIGN string(\"y\")\n\
             0006 JMP 0007\n\
             0007 ECHO T4\n\
             0008 JMP 0009\n\
             0009 T6 = QM_ASSIGN string(\"d\")\n\
             0010 JMP 0011\n\
             0011 ECHO T6\n\
             0012 T7 = IS_IDENTICAL CV0($a) int(1)\n\
             0013 JMPNZ T7 0016\n\
             0014 JMP 0015\n\
             0015 MATCH_ERROR CV0($a)\n\
             0016 T8 = QM_ASSIGN string(\"one\")\n\
             0017 JMP 0018\n\
             0018 ECHO T8\n\
             0019 JMP 0020\n\
             0020 MATCH_ERROR CV0($a)\n\
             0021 ASSIGN CV1($x) null\n\
             0022 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0006 - 0007 (tmp/var)\n\
             \x20    6: 0010 - 0011 (tmp/var)\n\
             \x20    8: 0017 - 0018 (tmp/var)\n"
        );
    }

    /// The lines are `zend_compile_expr_inner`'s, set as each expression
    /// compiles and never restored: a case's opline carries the line its
    /// condition starts on, a `MATCH` and its `MATCH_ERROR` the
    /// subject's, an arm's `QM_ASSIGN` and `JMP` the body's, and the
    /// `ECHO` after a `match` the last arm's, which the oracle's
    /// `Undefined variable` warnings and `Unhandled match case` fatals
    /// place there.
    #[test]
    fn a_case_and_an_arm_carry_the_lines_php_gives_them() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\n$a = 9;\nswitch ($a) {\n  case\n   $u:\n    echo \"a\";\n  case $v:\n    echo \"b\";\n}\necho match ($a) {\n  $w => \"x\",\n  default => $z,\n};\necho match (\n  $a\n) {\n  1 => \"x\",\n  2 => \"y\",\n};\n",
        );
        let lines: Vec<(Opcode, u32)> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.lineno))
            .collect();
        assert_eq!(
            lines,
            vec![
                (Opcode::Assign, 2),
                (Opcode::IsEqual, 5),
                (Opcode::Jmpnz, 5),
                (Opcode::IsEqual, 7),
                (Opcode::Jmpnz, 7),
                (Opcode::Jmp, 7),
                (Opcode::Echo, 6),
                (Opcode::Echo, 8),
                (Opcode::IsIdentical, 11),
                (Opcode::Jmpnz, 11),
                (Opcode::Jmp, 11),
                (Opcode::QmAssign, 11),
                (Opcode::Jmp, 11),
                (Opcode::QmAssign, 12),
                (Opcode::Jmp, 12),
                (Opcode::Echo, 12),
                (Opcode::Match, 15),
                (Opcode::MatchError, 15),
                (Opcode::QmAssign, 17),
                (Opcode::Jmp, 17),
                (Opcode::QmAssign, 18),
                (Opcode::Jmp, 18),
                (Opcode::Echo, 18),
                (Opcode::Return, 20),
            ]
        );
    }

    /// `zend_compile_break_continue`'s refusals, each at `E_COMPILE_ERROR`
    /// in php's words, and `zend_compile_switch`'s and
    /// `zend_compile_match`'s one default each.
    #[test]
    fn what_php_s_compiler_refuses_in_a_break_a_switch_and_a_match_is_refused_in_its_words() {
        for (source, message) in [
            (
                &b"<?php\nbreak;\n"[..],
                "'break' not in the 'loop' or 'switch' context",
            ),
            (
                b"<?php\ncontinue;\n",
                "'continue' not in the 'loop' or 'switch' context",
            ),
            (b"<?php\nfor (;;) { break 2; }\n", "Cannot 'break' 2 levels"),
            (
                b"<?php\nfor (;;) { continue 3; }\n",
                "Cannot 'continue' 3 levels",
            ),
            (
                b"<?php\nfor (;;) { break 0; }\n",
                "'break' operator accepts only positive integers",
            ),
            (
                b"<?php\nfor (;;) { break \"1\"; }\n",
                "'break' operator accepts only positive integers",
            ),
            (
                b"<?php\nfor (;;) { break $a; }\n",
                "'break' operator with non-integer operand is no longer supported",
            ),
            (
                b"<?php\nfor (;;) { switch ($a) { case 1: default: break; default: } }\n",
                "Switch statements may only contain one default clause",
            ),
            (
                b"<?php\necho match ($a) { 1 => 2, default => 3, default => 4 };\n",
                "Match expressions may only contain one default arm",
            ),
        ] {
            let error = refused(source).unwrap_or_else(|| CompileError {
                level: Level::Parse,
                message: String::from("compiled").into(),
                line: 0,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            });
            assert_eq!(error.level, Level::CompileError, "{message}");
            assert_eq!(error.message, message);
            assert_eq!(error.line, 2, "{message}");
        }
    }

    /// A `continue` that targets a `switch` is the `break` it is, with
    /// php's warning on the unit, the hint about the loop outside where
    /// there is one. php raises it as `E_WARNING`, not
    /// `E_COMPILE_WARNING` (`Z/Zend/zend_compile.c:5860`), so it is one a
    /// user error handler is handed.
    #[test]
    fn a_continue_targeting_a_switch_is_a_break_with_php_s_warning() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\nfor (;;) { switch ($a) { case 1: continue; } }\nswitch ($a) { case 1: continue; }\nfor (;;) { switch ($a) { case 1: continue 2; } }\nfor (;;) { switch ($a) { case 1: for (;;) { continue 2; } } }\n",
        );
        let raised: Vec<(Level, u32, &str)> = unit
            .diagnostics
            .iter()
            .map(|d| (d.level, d.line, &*d.message))
            .collect();
        assert_eq!(
            raised,
            vec![
                (
                    Level::Warning,
                    2,
                    "\"continue\" targeting switch is equivalent to \"break\". Did you mean to use \"continue 2\"?"
                ),
                (
                    Level::Warning,
                    3,
                    "\"continue\" targeting switch is equivalent to \"break\""
                ),
                (
                    Level::Warning,
                    5,
                    "\"continue 2\" targeting switch is equivalent to \"break 2\". Did you mean to use \"continue 3\"?"
                ),
            ]
        );
        assert_eq!(
            oplines_of(b"<?php\nfor (;;) { switch ($a) { case 1: continue; } }\nswitch ($a) { case 1: continue; }\n"),
            "0000 JMP 0005\n\
             0001 T1 = IS_EQUAL CV0($a) int(1)\n\
             0002 JMPNZ T1 0004\n\
             0003 JMP 0005\n\
             0004 JMP 0005\n\
             0005 JMPNZ bool(true) 0001\n\
             0006 T2 = IS_EQUAL CV0($a) int(1)\n\
             0007 JMPNZ T2 0009\n\
             0008 JMP 0010\n\
             0009 JMP 0010\n\
             0010 RETURN int(1)\n"
        );
    }

    /// `zend_compile_class_decl` names a class `zend_prefix_with_ns` over
    /// the written name, so `class M` under `namespace A;` declares
    /// `A\M`, the name `zend_resolve_class_name` gives `new M` and `M::K`
    /// in the same namespace (#322). The dump is the oracle's under
    /// `[regime.dump]` at 8.5.9 of this program, `NEW 0 string("A\M")`
    /// with its `DO_FCALL` and `FREE` (#322's own `echo M::K` stands
    /// aside here and is the test below's); a file written under
    /// `opcache.file_update_protection`'s two seconds before the
    /// oracle reads it prints no dump at all, which is the empty dump
    /// #322 measured, and not the class. `__CLASS__` in the body folds
    /// to the prefixed name, and a fatal names it as php's does.
    #[test]
    fn a_class_declared_in_a_namespace_takes_the_namespace_on_its_name() {
        let source: &[u8] = b"<?php namespace A; class M {} new M;";
        let (unit, _strings) = unit_and_strings(source);
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                4,
                0,
                2,
                "0000 V0 = NEW 0 string(\"A\\\\M\")\n\
                 0001 DO_FCALL\n\
                 0002 FREE V0\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (new)\n"
            )
        );
        let names: Vec<&[u8]> = unit.classes.iter().map(|c| c.name.0.as_ref()).collect();
        assert_eq!(
            names,
            [b"A\\M".as_slice()],
            "declared as A\\M, got {names:?}"
        );

        let (unit, _strings) =
            unit_and_strings(b"<?php namespace A; class M { const C = __CLASS__; }");
        let names: Vec<&[u8]> = unit.classes.iter().map(|c| c.name.0.as_ref()).collect();
        assert_eq!(names, [b"A\\M".as_slice()]);
        assert_eq!(
            unit.classes[0].constants[0].value,
            ConstExpr::Str(b"A\\M".as_slice().into())
        );
        let mut strings = php_heap::Strings::new();
        let refused = compile_parsed(
            b"<?php namespace A;\nclass M { const K = 1;\n const K = 2; }",
            &bundled(),
            &mut strings,
        )
        .err()
        .map(|e| (e.level, e.message.into_owned(), e.line));
        assert_eq!(
            refused,
            Some((
                Level::CompileError,
                "Cannot redefine class constant A\\M::K".to_string(),
                3
            ))
        );
    }

    /// `zend_compile_class_const` under `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`,
    /// which opcache sets, so `zend_try_ct_eval_class_const` folds
    /// nothing and `M::K` is fetched as the program serves (#372): the
    /// class's constant pair in `op1` (`zend_set_class_name_op1` over
    /// `zend_resolve_class_name`, which takes the namespace as
    /// `zend_compile_class_decl` took it on the declaration), the
    /// constant's name in `op2`, into a temporary. The dump is the
    /// oracle's under `[regime.dump]` at 8.5.9 of this program, which is
    /// #322's own with the `echo M::K` it stood aside from.
    #[test]
    fn a_class_constant_is_fetch_class_constant_over_the_resolved_class_name() {
        let (unit, _strings) =
            unit_and_strings(b"<?php namespace A; class M { const K = 1; } new M; echo M::K;");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                6,
                0,
                3,
                "0000 V0 = NEW 0 string(\"A\\\\M\")\n\
                 0001 DO_FCALL\n\
                 0002 FREE V0\n\
                 0003 T2 = FETCH_CLASS_CONSTANT string(\"A\\\\M\") string(\"K\")\n\
                 0004 ECHO T2\n\
                 0005 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (new)\n"
            )
        );
    }

    /// The constant table `zend_compile_class_const` leaves (#392):
    /// `zend_emit_op_tmp` adds the constant's name, then
    /// `zend_set_class_name_op1` the class's pair, so the name stands
    /// before the pair, as it does under `zend_compile_static_prop`.
    /// The dump prints values and not slots, so it reads no order; the
    /// table is read here, whole: `NEW`'s pair, the fetch's name and
    /// pair, and the final return's `1`, each slot its own as
    /// `zend_add_literal` keeps them.
    #[test]
    fn a_class_constant_fetch_adds_the_constant_s_name_before_the_class_s_pair() {
        let (unit, _strings) =
            unit_and_strings(b"<?php namespace A; class M { const K = 1; } new M; echo M::K;");
        let table: Vec<String> = unit
            .main
            .literals
            .iter()
            .map(|z| match (z.as_str(), z.as_long()) {
                (Some(s), _) => String::from_utf8_lossy(s.bytes()).into_owned(),
                (None, Some(n)) => n.to_string(),
                (None, None) => String::from("?"),
            })
            .collect();
        assert_eq!(
            table,
            ["A\\M", "a\\m", "K", "A\\M", "a\\m", "1"],
            "php's table, the constant's name before the class's pair"
        );
        let fetch = &unit.main.oplines[3];
        assert_eq!(fetch.op2, 2, "the name is the slot added first");
        assert_eq!(
            fetch.op1, 3,
            "the pair's written name follows it, its lowered form after"
        );
    }

    /// `zend_compile_static_prop` under `BP_VAR_R` (#372): the
    /// property's name in `op1`, the class's constant pair in `op2`, into
    /// a temporary (`zend_adjust_for_fetch_type`). The dump is the
    /// oracle's under `[regime.dump]` at 8.5.9 of `<?php echo C::$p;`,
    /// whose class the table has not, which php compiles all the same
    /// and fails on only as it runs. Under a namespace the class takes
    /// it, as every other class reference does.
    #[test]
    fn a_static_property_read_is_fetch_static_prop_r_over_the_resolved_class_name() {
        assert_eq!(
            dump_of(b"<?php echo C::$p;"),
            main_dump(
                3,
                0,
                1,
                "0000 T0 = FETCH_STATIC_PROP_R string(\"p\") string(\"C\")\n\
                 0001 ECHO T0\n\
                 0002 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(b"<?php namespace A; class C { } echo C::$p;"),
            main_dump(
                3,
                0,
                1,
                "0000 T0 = FETCH_STATIC_PROP_R string(\"p\") string(\"A\\\\C\")\n\
                 0001 ECHO T0\n\
                 0002 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_const_decl` (#476): one `DECLARE_CONST` per
    /// declarator, in written order, before anything else the file
    /// does, the name prefixed with the namespace the statement stands
    /// in and the value folded. The dumps are the oracle's under
    /// `[regime.dump]` at 8.5.9.
    #[test]
    fn a_const_statement_is_one_declare_const_per_declarator_over_the_folded_value() {
        assert_eq!(
            dump_of(b"<?php const K = 1;"),
            main_dump(
                2,
                0,
                0,
                "0000 DECLARE_CONST string(\"K\") int(1)\n0001 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(b"<?php const K = 1, L = 2;"),
            main_dump(
                3,
                0,
                0,
                "0000 DECLARE_CONST string(\"K\") int(1)\n\
                 0001 DECLARE_CONST string(\"L\") int(2)\n\
                 0002 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(b"<?php const K = 1 + 2;"),
            main_dump(
                2,
                0,
                0,
                "0000 DECLARE_CONST string(\"K\") int(3)\n0001 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(b"<?php namespace A; const K = 1;"),
            main_dump(
                2,
                0,
                0,
                "0000 DECLARE_CONST string(\"A\\\\K\") int(1)\n0001 RETURN int(1)\n"
            )
        );
    }

    /// What stands beside a `const` statement and is refused: php's
    /// `zend_get_special_const` fatal over the three names no file may
    /// redeclare, php's own parse error for the statement written where
    /// its grammar has no `top_statement`, and what this compiler
    /// cannot fold. Each sentence is the oracle's at 8.5.9.
    #[test]
    fn what_a_const_statement_refuses_is_what_php_refuses() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &bundled(), &mut strings)
                .err()
                .map(|e| e.message.into_owned())
        };
        assert_eq!(
            refused(b"<?php const true = 1;").as_deref(),
            Some("Cannot redeclare constant 'true'")
        );
        assert_eq!(
            refused(b"<?php const NULL = 1;").as_deref(),
            Some("Cannot redeclare constant 'NULL'")
        );
        assert_eq!(
            refused(b"<?php if (1) { const K = 1; }").as_deref(),
            Some("syntax error, unexpected token \"const\"")
        );
        assert_eq!(
            refused(b"<?php function g() { const K = 1; }").as_deref(),
            Some("syntax error, unexpected token \"const\"")
        );
        // The `;` php's grammar requires after the list, whose absence
        // php's parser names and this one names the same way.
        assert_eq!(
            refused(b"<?php const K = 1").as_deref(),
            Some("syntax error, unexpected end of file, expecting \",\" or \";\"")
        );
    }

    /// `zend_compile_declare` (#411): the statement leaves no opline,
    /// so the oracle's dump of a file that opens with one is the dump of
    /// the file without it, and `declare(strict_types=1)` shows in the
    /// bit every op array of the file then carries. The dumps are the
    /// oracle's under `[regime.dump]` at 8.5.9.
    #[test]
    fn a_declare_leaves_no_opline_and_strict_types_marks_every_op_array_of_the_file() {
        assert_eq!(
            dump_of(b"<?php declare(strict_types=1); echo 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n")
        );
        assert_eq!(
            dump_of(b"<?php declare(ticks=0, strict_types=1); echo 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n")
        );
        let (unit, _strings) = unit_and_strings(b"<?php declare(strict_types=1); function f() {}");
        assert!(unit.main.strict_types);
        assert!(unit.functions.iter().all(|(_, code)| code.strict_types));
        let (unit, _strings) = unit_and_strings(b"<?php declare(strict_types=0); function f() {}");
        assert!(!unit.main.strict_types);
        assert!(unit.functions.iter().all(|(_, code)| !code.strict_types));
        let (unit, _strings) = unit_and_strings(b"<?php function f() {}");
        assert!(!unit.main.strict_types);
    }

    /// What `zend_compile_declare` refuses, in php's words, and what it
    /// warns about and compiles on from; each measured against the
    /// oracle at 8.5.9. The value is held to `ZEND_AST_ZVAL` before the
    /// name is looked at, so `1 + 0` and `true` are the same fatal as
    /// each other and not a fold.
    #[test]
    fn what_a_declare_refuses_is_what_php_refuses() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &bundled(), &mut strings)
                .err()
                .map(|e| e.message.into_owned())
        };
        assert_eq!(
            refused(b"<?php echo 1; declare(strict_types=1);").as_deref(),
            Some("strict_types declaration must be the very first statement in the script")
        );
        assert_eq!(
            refused(b"<?php namespace A; declare(strict_types=1);").as_deref(),
            Some("strict_types declaration must be the very first statement in the script")
        );
        assert_eq!(
            refused(b"<?php function g() { declare(strict_types=1); }").as_deref(),
            Some("strict_types declaration must be the very first statement in the script")
        );
        assert_eq!(
            refused(b"<?php declare(strict_types=1) { }").as_deref(),
            Some("strict_types declaration must not use block mode")
        );
        assert_eq!(
            refused(b"<?php declare(strict_types=1) echo 1;").as_deref(),
            Some("strict_types declaration must not use block mode")
        );
        assert_eq!(
            refused(b"<?php declare(strict_types=2);").as_deref(),
            Some("strict_types declaration must have 0 or 1 as its value")
        );
        assert_eq!(
            refused(b"<?php declare(strict_types=\"1\");").as_deref(),
            Some("strict_types declaration must have 0 or 1 as its value")
        );
        assert_eq!(
            refused(b"<?php declare(strict_types=1+0);").as_deref(),
            Some("declare(strict_types) value must be a literal")
        );
        assert_eq!(
            refused(b"<?php declare(strict_types=true);").as_deref(),
            Some("declare(strict_types) value must be a literal")
        );
        assert_eq!(
            refused(b"<?php declare(ticks=-1);").as_deref(),
            Some("declare(ticks) value must be a literal")
        );
        // What php compiles and this compiler does not: the alternative
        // syntax. A tick count was one of them until #1190, the encoding
        // pragma until #1272, which is held to `zend_is_first_statement`
        // without `allow_nop` (the parser's own reading of it is
        // `multibyte`'s tests).
        assert_eq!(refused(b"<?php declare(ticks=1); echo 1;").as_deref(), None);
        let first =
            Some("Encoding declaration pragma must be the very first statement in the script");
        assert_eq!(
            refused(b"<?php declare(ticks=1); declare(encoding=\"x\");").as_deref(),
            None
        );
        assert_eq!(
            refused(b"<?php ; declare(encoding=\"x\");").as_deref(),
            first
        );
        assert_eq!(
            refused(b"<?php ?><?php declare(encoding=\"x\");").as_deref(),
            first
        );
        assert_eq!(
            refused(b"<?php echo 1; declare(encoding=\"x\");").as_deref(),
            first
        );
        assert_eq!(
            refused(b"<?php declare(encoding=\"x\") { declare(encoding=\"y\"); }").as_deref(),
            first
        );
        assert_eq!(
            refused(b"<?php declare(ticks=0): echo 1; enddeclare;").as_deref(),
            Some("a `declare` in the alternative syntax this compiler cannot read yet")
        );
        // `E_COMPILE_WARNING` and the file compiles on, which is the
        // diagnostic the unit carries for the engine to raise.
        let (unit, _strings) = unit_and_strings(b"<?php declare(foo=1); echo 1;");
        assert_eq!(
            unit.diagnostics
                .iter()
                .map(|d| (d.level, d.message.to_string()))
                .collect::<Vec<_>>(),
            vec![(
                Level::CompileWarning,
                String::from("Unsupported declare 'foo'")
            )]
        );
    }

    /// `zend_compile_assign`'s `ZEND_AST_STATIC_PROP` arm (#470): the
    /// fetch's own opline turned into `ASSIGN_STATIC_PROP`, its
    /// `result` a temporary and the value in the `OP_DATA` beside it.
    /// The dump is the oracle's under `[regime.dump]` at 8.5.9; the
    /// read's temporary is `T1` because `T0` is the write's, numbered
    /// whether or not anything reads it and marked `UNUSED` here by
    /// `zend_do_free`.
    #[test]
    fn a_static_property_write_is_assign_static_prop_over_the_op_data_beside_it() {
        assert_eq!(
            dump_of(b"<?php class T { public static $a = 1; } T::$a = 5; var_dump(T::$a);"),
            main_dump(
                7,
                0,
                3,
                "0000 ASSIGN_STATIC_PROP string(\"a\") string(\"T\")\n\
                 0001 OP_DATA int(5)\n\
                 0002 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0003 T1 = FETCH_STATIC_PROP_R string(\"a\") string(\"T\")\n\
                 0004 SEND_VAL T1 1\n\
                 0005 DO_ICALL\n\
                 0006 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_compound_assign`, `zend_compile_pre_incdec`,
    /// `zend_compile_post_incdec` and `zend_compile_assign_ref` over a
    /// `ZEND_AST_STATIC_PROP` (#487): one opline each, the fetch's own
    /// with its opcode set, the operator where the cache slot stood for
    /// the compound form and the slot moved onto its `OP_DATA`. The
    /// dump is the oracle's under `[regime.dump]` at 8.5.9; `C::$p++`
    /// and `C::$p--` written as statements are the `PRE_*` opcodes,
    /// since `zend_do_free` turns a step nothing reads into the one
    /// that leaves the value after it, and the one `var_dump` reads is
    /// `POST_INC_STATIC_PROP` into `T8`.
    #[test]
    fn the_compound_step_and_reference_writes_to_a_static_property_are_the_oracle_s_oplines() {
        assert_eq!(
            dump_of(
                b"<?php class C { public static $p = 1; } C::$p += 2; ++C::$p; --C::$p; \
                  C::$p++; C::$p--; $x = 1; C::$p = &$x; var_dump(C::$p++);"
            ),
            main_dump(
                14,
                1,
                9,
                "0000 ASSIGN_STATIC_PROP_OP (ADD) string(\"p\") string(\"C\")\n\
                 0001 OP_DATA int(2)\n\
                 0002 PRE_INC_STATIC_PROP string(\"p\") string(\"C\")\n\
                 0003 PRE_DEC_STATIC_PROP string(\"p\") string(\"C\")\n\
                 0004 PRE_INC_STATIC_PROP string(\"p\") string(\"C\")\n\
                 0005 PRE_DEC_STATIC_PROP string(\"p\") string(\"C\")\n\
                 0006 ASSIGN CV0($x) int(1)\n\
                 0007 ASSIGN_STATIC_PROP_REF string(\"p\") string(\"C\")\n\
                 0008 OP_DATA CV0($x)\n\
                 0009 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0010 T8 = POST_INC_STATIC_PROP string(\"p\") string(\"C\")\n\
                 0011 SEND_VAL T8 1\n\
                 0012 DO_ICALL\n\
                 0013 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_unary_op` for `ZEND_BW_NOT` (#478): one `BW_NOT`
    /// where the operand is no constant, and
    /// `zend_try_ct_eval_unary_op` where it is one php's
    /// `zend_unary_op_produces_error` lets through. `~1` and `~1.0` are
    /// `int(-2)` in the op array, `~"ab"` the bytes negated, and `~1.5`
    /// the opline over `float(1.5)`, which the guard refuses to fold
    /// because the long conversion loses something. `-~$a` is the
    /// `MUL` by -1 `zend_compile_unary_pm` emits over the `BW_NOT`, and
    /// `~$a instanceof C` is the `INSTANCEOF` over it, since `~` binds
    /// tighter. The dump is the oracle's under `[regime.dump]` at
    /// 8.5.9.
    #[test]
    fn a_bitwise_not_is_the_oracle_s_opline_or_its_fold() {
        assert_eq!(
            dump_of(
                b"<?php $a = 6; $b = ~$a; $c = ~1; $d = ~\"ab\"; $e = ~1.5; $f = ~~$a; \
                  $g = -~$a; $h = ~$a instanceof C; $i = ~1.0;"
            ),
            main_dump(
                18,
                9,
                17,
                "0000 ASSIGN CV0($a) int(6)\n\
                 0001 T10 = BW_NOT CV0($a)\n\
                 0002 ASSIGN CV1($b) T10\n\
                 0003 ASSIGN CV2($c) int(-2)\n\
                 0004 ASSIGN CV3($d) string(\"\\x9E\\x9D\")\n\
                 0005 T14 = BW_NOT float(1.5)\n\
                 0006 ASSIGN CV4($e) T14\n\
                 0007 T16 = BW_NOT CV0($a)\n\
                 0008 T17 = BW_NOT T16\n\
                 0009 ASSIGN CV5($f) T17\n\
                 0010 T19 = BW_NOT CV0($a)\n\
                 0011 T20 = MUL T19 int(-1)\n\
                 0012 ASSIGN CV6($g) T20\n\
                 0013 T22 = BW_NOT CV0($a)\n\
                 0014 T23 = INSTANCEOF T22 string(\"C\")\n\
                 0015 ASSIGN CV7($h) T23\n\
                 0016 ASSIGN CV8($i) int(-2)\n\
                 0017 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_compound_assign`, the `ZEND_AST_VAR` arm (#479):
    /// the twelve operators over a plain variable, each one `ASSIGN_OP`
    /// with the binary operator's own opcode in `extended_value`, which
    /// the dump prints in brackets. The temporary is numbered for every
    /// one of them and `zend_do_free` marks all but the last `UNUSED`,
    /// so the one `$b` reads is `T15`. The dump is the oracle's under
    /// `[regime.dump]` at 8.5.9.
    #[test]
    fn the_twelve_compound_assignments_to_a_variable_are_the_oracle_s_oplines() {
        assert_eq!(
            dump_of(
                b"<?php $a = 6; $a += 1; $a -= 1; $a *= 2; $a /= 3; $a %= 5; $a **= 2; \
                  $a .= \"x\"; $a |= 3; $a ^= 1; $a &= 6; $a <<= 2; $a >>= 1; $b = $a += 2;"
            ),
            main_dump(
                16,
                2,
                15,
                "0000 ASSIGN CV0($a) int(6)\n\
                 0001 ASSIGN_OP (ADD) CV0($a) int(1)\n\
                 0002 ASSIGN_OP (SUB) CV0($a) int(1)\n\
                 0003 ASSIGN_OP (MUL) CV0($a) int(2)\n\
                 0004 ASSIGN_OP (DIV) CV0($a) int(3)\n\
                 0005 ASSIGN_OP (MOD) CV0($a) int(5)\n\
                 0006 ASSIGN_OP (POW) CV0($a) int(2)\n\
                 0007 ASSIGN_OP (CONCAT) CV0($a) string(\"x\")\n\
                 0008 ASSIGN_OP (BW_OR) CV0($a) int(3)\n\
                 0009 ASSIGN_OP (BW_XOR) CV0($a) int(1)\n\
                 0010 ASSIGN_OP (BW_AND) CV0($a) int(6)\n\
                 0011 ASSIGN_OP (SL) CV0($a) int(2)\n\
                 0012 ASSIGN_OP (SR) CV0($a) int(1)\n\
                 0013 T15 = ASSIGN_OP (ADD) CV0($a) int(2)\n\
                 0014 ASSIGN CV1($b) T15\n\
                 0015 RETURN int(1)\n"
            )
        );
    }

    /// `zend_delayed_compile_var` as the base of a chain of dimensions
    /// (#451), the oracle's dump under `[regime.dump]` at 8.5.9: a
    /// property is the one fetch php holds back, so its `FETCH_OBJ_R`
    /// stands after the oplines of whatever the dimension compiled to,
    /// which is what puts `f`'s call before the fetch of `p` in
    /// `$o->p[f()]` while the fetch keeps the lower temporary; under a
    /// write it is `FETCH_OBJ_W` carrying `ZEND_FETCH_DIM_WRITE`, the
    /// bit the dump prints as `(dim write)`. A call is compiled where
    /// it stands and takes `SEPARATE` over its own `VAR` under a write
    /// (`zend_separate_if_call_and_write`), which the dump prints into
    /// the same `VAR` it reads; `$g()` is php's `ZEND_AST_CALL` the
    /// same as `f()` is, so `zend_is_call` separates it the same way
    /// (#336).
    #[test]
    fn the_base_of_a_chain_of_dimensions_is_the_oracle_s_delayed_fetch() {
        let source: &[u8] = b"<?php $o = new stdClass; $x = $o->p[0]; $o->p[1] = 2; \
                              $y = f()[0]; f()[1] = 3; $z = $o->p[f()]; $g()[2] = 4; \
                              function f() { return [1]; }";
        assert_eq!(
            dump_of(source),
            main_dump(
                29,
                5,
                19,
                "0000 V5 = NEW 0 string(\"stdClass\")\n\
                 0001 DO_FCALL\n\
                 0002 ASSIGN CV0($o) V5\n\
                 0003 T8 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0004 T9 = FETCH_DIM_R T8 int(0)\n\
                 0005 ASSIGN CV1($x) T9\n\
                 0006 V11 = FETCH_OBJ_W (dim write) CV0($o) string(\"p\")\n\
                 0007 ASSIGN_DIM V11 int(1)\n\
                 0008 OP_DATA int(2)\n\
                 0009 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0010 V13 = DO_FCALL_BY_NAME\n\
                 0011 T14 = FETCH_DIM_R V13 int(0)\n\
                 0012 ASSIGN CV2($y) T14\n\
                 0013 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0014 V16 = DO_FCALL_BY_NAME\n\
                 0015 V16 = SEPARATE V16\n\
                 0016 ASSIGN_DIM V16 int(1)\n\
                 0017 OP_DATA int(3)\n\
                 0018 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0019 V19 = DO_FCALL_BY_NAME\n\
                 0020 T18 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0021 T20 = FETCH_DIM_R T18 V19\n\
                 0022 ASSIGN CV3($z) T20\n\
                 0023 INIT_DYNAMIC_CALL 0 CV4($g)\n\
                 0024 V22 = DO_FCALL\n\
                 0025 V22 = SEPARATE V22\n\
                 0026 ASSIGN_DIM V22 int(2)\n\
                 0027 OP_DATA int(4)\n\
                 0028 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    5: 0001 - 0002 (new)\n\
                 \x20    16: 0015 - 0016 (tmp/var)\n\
                 \x20    19: 0020 - 0021 (tmp/var)\n\
                 \x20    22: 0025 - 0026 (tmp/var)\n"
            ) + "\nf:\n     ; (lines=2, args=0, vars=0, tmps=0)\n\
                 \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
                 0000 RETURN array(...)\n\
                 0001 RETURN null\n"
        );
    }

    /// The bases php refuses under a write, `zend_compile_var`'s
    /// default arm and `zend_separate_if_call_and_write`: a temporary
    /// expression and a call php answered in one, each php's own
    /// compile error, under a dimension and under a property alike
    /// (#336); and the one this compiler has not, a `static`
    /// property under a write, whose fetch is `FETCH_STATIC_PROP_W`
    /// (#507). `new C` is no call however its `VAR` is written, since
    /// `zend_is_call` reads the shape of the expression and not where
    /// its value landed.
    #[test]
    fn a_base_no_write_can_stand_on_is_php_s_compile_error() {
        let refused = |source: &[u8]| refused_message(source);
        assert_eq!(
            refused(b"<?php (new C)[0] = 1;").as_deref(),
            Some("Cannot use temporary expression in write context")
        );
        assert_eq!(
            refused(b"<?php \"ab\"[0] = 1;").as_deref(),
            Some("Cannot use temporary expression in write context")
        );
        assert_eq!(
            refused(b"<?php strlen(\"a\")[0] = 1;").as_deref(),
            Some("Cannot use result of built-in function in write context")
        );
        assert_eq!(
            refused(b"<?php (new C)->p = 1;").as_deref(),
            Some("Cannot use temporary expression in write context")
        );
        assert_eq!(
            refused(b"<?php (1)->p = 1;").as_deref(),
            Some("Cannot use temporary expression in write context")
        );
        assert_eq!(
            refused(b"<?php [1, 2]->p = 1;").as_deref(),
            Some("Cannot use temporary expression in write context")
        );
        assert_eq!(
            refused(b"<?php strlen(\"a\")->p = 1;").as_deref(),
            Some("Cannot use result of built-in function in write context")
        );
        assert_eq!(
            oplines_of(b"<?php C::$s[0] += 1; C::$s[1]++;"),
            "0000 V0 = FETCH_STATIC_PROP_RW string(\"s\") string(\"C\")\n\
             0001 ASSIGN_DIM_OP (ADD) V0 int(0)\n\
             0002 OP_DATA int(1)\n\
             0003 V2 = FETCH_STATIC_PROP_RW string(\"s\") string(\"C\")\n\
             0004 V3 = FETCH_DIM_RW V2 int(1)\n\
             0005 PRE_INC V3\n\
             0006 RETURN int(1)\n"
        );
    }

    /// `zend_compile_compound_assign`, the `ZEND_AST_DIM` and
    /// `ZEND_AST_PROP` arms (#498): the chain's last delayed opline and
    /// the delayed property fetch, each with its opcode set to
    /// `ASSIGN_DIM_OP` or `ASSIGN_OBJ_OP`, the operator where the
    /// dimension has no cache slot and where the property's slot stood,
    /// and an `OP_DATA` after it carrying the value; the property's
    /// three slots move onto that `OP_DATA`, which the dump does not
    /// print. `$a[] .= "x"` is the append, `NEXT` in the dump, and the
    /// one the `$b` reads is the temporary `zend_do_free` leaves
    /// standing. The dump is the oracle's under `[regime.dump]` at
    /// 8.5.9.
    #[test]
    fn a_compound_assignment_to_a_dimension_and_to_a_property_are_the_oracle_s_oplines() {
        assert_eq!(
            dump_of(b"<?php $a = [1]; $a[0] += 1; $a[] .= \"x\"; $o->p |= 3; $b = $a[2] **= 2;"),
            main_dump(
                11,
                3,
                6,
                "0000 ASSIGN CV0($a) array(...)\n\
                 0001 ASSIGN_DIM_OP (ADD) CV0($a) int(0)\n\
                 0002 OP_DATA int(1)\n\
                 0003 ASSIGN_DIM_OP (CONCAT) CV0($a) NEXT\n\
                 0004 OP_DATA string(\"x\")\n\
                 0005 ASSIGN_OBJ_OP (BW_OR) CV1($o) string(\"p\")\n\
                 0006 OP_DATA int(3)\n\
                 0007 T7 = ASSIGN_DIM_OP (POW) CV0($a) int(2)\n\
                 0008 OP_DATA int(2)\n\
                 0009 ASSIGN CV2($b) T7\n\
                 0010 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    7: 0008 - 0009 (tmp/var)\n"
            )
        );
    }

    /// `zend_adjust_for_fetch_type` under `BP_VAR_RW` (#504): every
    /// link of a compound assignment's target but the last is
    /// `FETCH_DIM_RW` or `FETCH_OBJ_RW`, the `_W` opcode plus one, into
    /// a `VAR` the link over it stands on. The dump is the oracle's
    /// under `[regime.dump]` at 8.5.9.
    #[test]
    fn the_links_under_a_compound_assignment_s_last_are_the_oracle_s_rw_fetches() {
        assert_eq!(
            dump_of(b"<?php $a = [[1]]; $a[0][1] *= 2; $o->p->q += 1;"),
            main_dump(
                8,
                2,
                5,
                "0000 ASSIGN CV0($a) array(...)\n\
                 0001 V3 = FETCH_DIM_RW CV0($a) int(0)\n\
                 0002 ASSIGN_DIM_OP (MUL) V3 int(1)\n\
                 0003 OP_DATA int(2)\n\
                 0004 V5 = FETCH_OBJ_RW CV1($o) string(\"p\")\n\
                 0005 ASSIGN_OBJ_OP (ADD) V5 string(\"q\")\n\
                 0006 OP_DATA int(1)\n\
                 0007 RETURN int(1)\n"
            )
        );
    }

    /// `zend_delayed_compile_var`'s last two bases under a write
    /// (#507): a `static` property is `FETCH_STATIC_PROP_W` carrying
    /// `ZEND_FETCH_DIM_WRITE`, the bit the dump prints as `(dim write)`,
    /// and `$this` is `FETCH_THIS` into the `VAR` the dimension stands
    /// on, never the `UNUSED` receiver a property fetch takes. The dump
    /// is the oracle's under `[regime.dump]` at 8.5.9.
    #[test]
    fn a_static_property_and_this_under_a_written_dimension_are_the_oracle_s_bases() {
        assert_eq!(
            dump_of(b"<?php class C { public static $s = [1]; } C::$s[0] = 1;"),
            main_dump(
                4,
                0,
                2,
                "0000 V0 = FETCH_STATIC_PROP_W (dim write) string(\"s\") string(\"C\")\n\
                 0001 ASSIGN_DIM V0 int(0)\n\
                 0002 OP_DATA int(1)\n\
                 0003 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump_of(b"<?php $this[0] = 1;"),
            main_dump(
                4,
                0,
                2,
                "0000 V0 = FETCH_THIS\n\
                 0001 ASSIGN_DIM V0 int(0)\n\
                 0002 OP_DATA int(1)\n\
                 0003 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_const`'s second half (#380), the oracle's dump
    /// under `[regime.dump]` at 8.5.9: a name `zend_try_ct_eval_const`
    /// cannot fold is `FETCH_CONSTANT` into a temporary over the name
    /// `zend_resolve_const_name` resolved. A bare name inside a
    /// namespace is the namespace's spelling under the bit
    /// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`, which the handler reads
    /// to try the global spelling after the namespaced one; a name
    /// written with a leading `\` is the global one and carries no
    /// bit; and a name with a `\` in it takes the namespace before it,
    /// so `n\Q` inside `n` is `n\n\Q`.
    #[test]
    fn a_constant_the_compiler_cannot_fold_is_the_oracle_s_fetch_constant() {
        assert_eq!(
            dump_of(b"<?php namespace n; define(\"n\\\\K\", 2); var_dump(K, \\W, n\\Q, NOPE);"),
            main_dump(
                15,
                0,
                6,
                "0000 INIT_NS_FCALL_BY_NAME 2 string(\"n\\\\define\")\n\
                 0001 SEND_VAL_EX string(\"n\\\\K\") 1\n\
                 0002 SEND_VAL_EX int(2) 2\n\
                 0003 DO_FCALL_BY_NAME\n\
                 0004 INIT_NS_FCALL_BY_NAME 4 string(\"n\\\\var_dump\")\n\
                 0005 T1 = FETCH_CONSTANT (unqualified-in-namespace) string(\"n\\\\K\")\n\
                 0006 SEND_VAL_EX T1 1\n\
                 0007 T2 = FETCH_CONSTANT string(\"W\")\n\
                 0008 SEND_VAL_EX T2 2\n\
                 0009 T3 = FETCH_CONSTANT string(\"n\\\\n\\\\Q\")\n\
                 0010 SEND_VAL_EX T3 3\n\
                 0011 T4 = FETCH_CONSTANT (unqualified-in-namespace) string(\"n\\\\NOPE\")\n\
                 0012 SEND_VAL_EX T4 4\n\
                 0013 DO_FCALL_BY_NAME\n\
                 0014 RETURN int(1)\n"
            )
        );
    }

    /// What stands beside a `C::$p` read and is not read yet is refused
    /// as such (#372): a compound assignment over what is no variable,
    /// `self`, `parent` and `static` as the class, and such a fetch in a
    /// constant expression, which php refuses outright. `C::$p = &$o->q`
    /// was one of them until #611 made the source of a `= &`
    /// `zend_compile_var`'s, `C::$p()`, whose method the local `$p`
    /// names, until it was read as `INIT_STATIC_METHOD_CALL` over the
    /// CV, and the property the program names as it serves, `C::$$p`
    /// and `C::${'p'}`, until #1189.
    #[test]
    fn what_stands_beside_a_static_property_read_is_refused_as_not_yet_read() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &bundled(), &mut strings)
                .err()
                .map(|e| e.message.into_owned())
        };
        assert_eq!(
            refused(b"<?php $this += 1;").as_deref(),
            Some("a compound assignment to what is no variable this compiler cannot read yet")
        );
        assert_eq!(refused(b"<?php C::$$p;").as_deref(), None);
        assert_eq!(refused(b"<?php C::${'p'};").as_deref(), None);
        assert_eq!(refused(b"<?php C::$p();").as_deref(), None);
        assert_eq!(refused(b"<?php echo self::$p;").as_deref(), None);
        assert_eq!(
            refused(b"<?php class D { const X = C::$p; }").as_deref(),
            Some("Constant expression contains invalid operations")
        );
    }

    /// `zend_get_compatible_func_or_null` under a namespace (#322): the
    /// callee is known by the name the fetch resolves to, so `M::f(1)`
    /// under `namespace A;` finds the public `f` of the `A\M` the engine
    /// declared and sends `SEND_VAL`, where `\M::f(1)` names the global
    /// `M` the table has not.
    #[test]
    fn a_method_of_a_namespaced_class_is_a_known_callee_in_its_namespace() {
        use php_unit::decl::{ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl};
        let options = Options {
            classes: indexed(vec![ClassDecl {
                name: Name(b"A\\M".as_slice().into()),
                kind: ClassKind::Class,
                flags: ClassFlags::default(),
                parent: None,
                interfaces: Vec::new(),
                implementors: Vec::new(),
                aliases: Vec::new(),
                traits: Vec::new(),
                adaptations: Vec::new(),
                backing: EnumBacking::Pure,
                constants: Vec::new(),
                properties: Vec::new(),
                methods: vec![MethodDecl {
                    function: FunctionDecl {
                        name: Name(b"f".as_slice().into()),
                        parameters: Vec::new(),
                        return_type: None,
                        returns_by_ref: false,
                        is_generator: false,
                        attributes: Vec::new(),
                        doc_comment: None,
                        file: Name(Box::new([])),
                        line: 0,
                        end_line: 0,
                        frameless: Vec::new(),
                    },
                    visibility: Visibility::Public,
                    is_instance: false,
                    is_abstract: false,
                    is_final: false,
                    tentative_return: false,
                }],
                attributes: Vec::new(),
                doc_comment: None,
                file: Name(Box::new([])),
                line: 0,
                end_line: 0,
                object_size: 0,
                object_beside: Box::default(),
                attribute_validator: None,
            }]),
            ..Options::default()
        };
        let mut strings = php_heap::Strings::new();
        let dump = compile_parsed(
            b"<?php namespace A; M::f(1); \\M::f(1);",
            &options,
            &mut strings,
        )
        .unwrap_or_default()
        .dump(&[])
        .text();
        assert_eq!(
            dump,
            main_dump(
                7,
                0,
                2,
                "0000 INIT_STATIC_METHOD_CALL 1 string(\"A\\\\M\") string(\"f\")\n\
                 0001 SEND_VAL int(1) 1\n\
                 0002 DO_FCALL\n\
                 0003 INIT_STATIC_METHOD_CALL 1 string(\"M\") string(\"f\")\n\
                 0004 SEND_VAL_EX int(1) 1\n\
                 0005 DO_FCALL\n\
                 0006 RETURN int(1)\n"
            )
        );
    }

    /// `zend_compile_class_decl` for the class the doc-comment witness
    /// of `php_syntax` declares (#257): bound at compile time with no
    /// opline, so the main op array is the oracle's `ECHO` and `RETURN`
    /// alone; the declaration among the unit's classes with each
    /// declarator's doc comment as `backup_doc_comment` took it (probed
    /// 8.5.9: `/** P */ public $x, $y;` gives P to `$x` alone), the
    /// constants' values as written, and a `}` clearing what is pending.
    #[test]
    fn a_class_with_properties_and_constants_is_bound_with_no_opline() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\nclass M { /** P */ public $x, $y = 1 /** P2 */; /** K */ const A = 1, B = 2; static $s; /** F */ final protected const C = -A; }\n/** D */ { } class N /** D2 */ { var $v /** V */; }\necho 1;\n",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; :1-5\n     ; return  [] RANGE[0..0]\n0000 ECHO int(1)\n0001 RETURN int(1)\n",
            "the class emits no opline; php binds it at compile time"
        );
        assert_eq!(unit.classes.len(), 2, "two classes, got {:?}", unit.classes);
        let (m, n) = (&unit.classes[0], &unit.classes[1]);
        assert_eq!(
            (m.name.0.as_ref(), m.line, m.doc_comment.as_deref()),
            (b"M".as_slice(), 2, None)
        );
        let props: Vec<_> = m
            .properties
            .iter()
            .map(|p| {
                (
                    p.name.0.as_ref(),
                    p.is_instance,
                    p.default.clone(),
                    p.doc_comment.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            props,
            vec![
                (b"x".as_slice(), true, None, Some(b"/** P */".as_slice())),
                (b"y", true, Some(ConstExpr::long(1)), Some(b"/** P2 */")),
                (b"s", false, None, None),
            ]
        );
        let consts: Vec<_> = m
            .constants
            .iter()
            .map(|c| {
                (
                    c.name.0.as_ref(),
                    c.visibility,
                    c.is_final,
                    c.value.clone(),
                    c.doc_comment.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            consts,
            vec![
                (
                    b"A".as_slice(),
                    Visibility::Public,
                    false,
                    ConstExpr::long(1),
                    Some(b"/** K */".as_slice())
                ),
                (b"B", Visibility::Public, false, ConstExpr::long(2), None),
                (
                    b"C",
                    Visibility::Protected,
                    true,
                    ConstExpr::Unary {
                        op: Name(b"-".as_slice().into()),
                        operand: Box::new(ConstExpr::Constant {
                            name: ConstantName::Resolved(Name(b"A".as_slice().into())),
                            line: 2,
                        }),
                        line: 2,
                    },
                    Some(b"/** F */"),
                ),
            ]
        );
        assert_eq!(
            (n.name.0.as_ref(), n.line, n.doc_comment.as_deref()),
            (b"N".as_slice(), 3, Some(b"/** D2 */".as_slice())),
            "the close brace before it cleared D, and a class takes before its open brace"
        );
        assert_eq!(
            n.properties[0].doc_comment.as_deref(),
            Some(b"/** V */".as_slice())
        );
        assert_eq!(
            n.properties[0].visibility,
            Visibility::Public,
            "`var` is public"
        );
    }

    /// What `zend_compile_prop_decl`, `zend_compile_class_const_decl`,
    /// `zend_assert_valid_class_name` and `zend_add_member_modifier`
    /// refuse, in php's words at the declarator's line, and what this
    /// compiler does not read yet, refused as such rather than compiled
    /// to something else.
    #[test]
    #[allow(clippy::too_many_lines)] // one assertion per refusal: the table is the test
    fn what_php_s_compiler_refuses_in_a_class_is_refused_in_its_words() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &bundled(), &mut strings)
                .err()
                .map(|e| (e.level, e.message.into_owned(), e.line))
        };
        let compile_error =
            |message: &str, line| Some((Level::CompileError, message.to_string(), line));
        assert_eq!(
            refused(b"<?php\nclass M {\n public $x;\n public $x; }"),
            compile_error("Cannot redeclare M::$x", 4)
        );
        assert_eq!(
            refused(b"<?php\nclass M { const A = 1;\n const A = 2; }"),
            compile_error("Cannot redefine class constant M::A", 3)
        );
        assert_eq!(
            refused(b"<?php\nclass Int {}"),
            compile_error("Cannot use \"Int\" as a class name as it is reserved", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { public $x = $y; }"),
            compile_error("Constant expression contains invalid operations", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { const A = f(); }"),
            compile_error("Constant expression contains invalid operations", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { public private $x; }"),
            compile_error("Multiple access type modifiers are not allowed", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M {}\nclass m {}"),
            None,
            "a second declaration of a name is the DECLARE_CLASS that refuses it as the program serves"
        );
        assert_eq!(
            refused(b"<?php\nclass M extends N {} class N {}"),
            None,
            "a parent bound later in the file is the DECLARE_CLASS at the class's line"
        );
        assert_eq!(
            refused(b"<?php\nclass M extends self {}"),
            compile_error("Cannot use \"self\" as class name, as it is reserved", 2)
        );
        assert_eq!(
            refused(b"<?php\ninterface I {}\nclass M extends I {}"),
            compile_error("Class M cannot extend interface I", 3),
            "the early bind links the class, so what zend_do_inheritance_ex refuses is the file's"
        );
        assert_eq!(
            refused(b"<?php\nclass M { abstract function f(); }"),
            compile_error(
                "Class M declares abstract method f() and must therefore be declared abstract",
                2
            )
        );
        assert_eq!(
            refused(b"<?php\nabstract class M { abstract function f(); }"),
            None
        );
        assert_eq!(
            refused(b"<?php\nclass M { function f(): (A&B)|C {} }"),
            None
        );
        assert_eq!(
            refused(b"<?php\nclass M { function f(public int $p) {} }"),
            compile_error("Cannot declare promoted property outside a constructor", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { function f() {} function F() {} }"),
            compile_error("Cannot redeclare M::F()", 2)
        );
        assert_eq!(refused(b"<?php\n#[A] #[B(1), C] class M {}"), None);
        assert_eq!(refused(b"<?php\n#[A(x: 1)] class M {}"), None);
        assert_eq!(
            refused(b"<?php\n#[\\AllowDynamicProperties] class M {}"),
            None,
            "the one attribute the compiler reads is a bit on the class"
        );
        assert_eq!(
            refused(b"<?php\n#[\\AllowDynamicProperties]\ninterface I {}"),
            compile_error("Cannot apply #[\\AllowDynamicProperties] to interface I", 3)
        );
        assert_eq!(refused(b"<?php\n#[A] function f() {}"), None);
        assert_eq!(refused(b"<?php\nclass M { #[A] public $x; }"), None);
        assert_eq!(
            refused(b"<?php\nclass M { #[\\Override] public $x; }"),
            compile_error(
                "M::$x has #[\\Override] attribute, but no matching parent property exists",
                2
            )
        );
    }

    /// What `zend_declare_typed_property` refuses in a property group's
    /// modifiers and type, in php's words at the declarator's line.
    #[test]
    fn what_php_s_compiler_refuses_in_a_property_group_is_refused_in_its_words() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &bundled(), &mut strings)
                .err()
                .map(|e| (e.level, e.message.into_owned(), e.line))
        };
        let compile_error =
            |message: &str, line| Some((Level::CompileError, message.to_string(), line));
        assert_eq!(
            refused(b"<?php\nclass M { public int $x; }"),
            None,
            "a property group's type is read where a modifier stood before it"
        );
        assert_eq!(refused(b"<?php\nclass M { public readonly int $x; }"), None);
        assert_eq!(
            refused(b"<?php\nclass M { public readonly int $x = 1; }"),
            compile_error("Readonly property M::$x cannot have default value", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { private(set) protected(set) int $x; }"),
            compile_error("Multiple access type modifiers are not allowed", 2)
        );
        assert_eq!(
            refused(b"<?php\nclass M { public private(set) $x; }"),
            compile_error(
                "Property with asymmetric visibility M::$x must have type",
                2
            )
        );
        assert_eq!(
            refused(b"<?php\nclass M { protected public(set) int $x; }"),
            compile_error(
                "Visibility of property M::$x must not be weaker than set visibility",
                2
            )
        );
        assert_eq!(
            refused(b"<?php\nclass M { public private(set) int $x; }"),
            None,
            "a set visibility stricter than the read visibility stands"
        );
    }

    /// `zend_compile_new`, `zend_compile_static_call`,
    /// `zend_compile_method_call` and `zend_compile_instanceof` as the
    /// oracle at 8.5.9 dumps them under `opcache.opt_debug_level=0x10000`,
    /// each source read once: `NEW` into a `VAR` with its `DO_FCALL`
    /// unused and the `FREE` or the live range `zend_do_free` leaves;
    /// the two `INIT_*_METHOD_CALL`s with their arguments sent for a
    /// callee nothing is known of; `INSTANCEOF` into a temporary, a
    /// chain of them grouped to the left with the second over the
    /// first's temporary, and `false` where the left side is a constant.
    #[test]
    fn new_compiles_to_php_s_op_array() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $o = new E;",
                4,
                1,
                3,
                "0000 V1 = NEW 0 string(\"E\")\n\
                 0001 DO_FCALL\n\
                 0002 ASSIGN CV0($o) V1\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    1: 0001 - 0002 (new)\n",
            ),
            (
                b"<?php new E;",
                4,
                0,
                2,
                "0000 V0 = NEW 0 string(\"E\")\n\
                 0001 DO_FCALL\n\
                 0002 FREE V0\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (new)\n",
            ),
            (
                b"<?php echo (new E(1))->m();",
                7,
                0,
                3,
                "0000 V0 = NEW 1 string(\"E\")\n\
                 0001 SEND_VAL_EX int(1) 1\n\
                 0002 DO_FCALL\n\
                 0003 INIT_METHOD_CALL 0 V0 string(\"m\")\n\
                 0004 V2 = DO_FCALL\n\
                 0005 ECHO V2\n\
                 0006 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0003 (new)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        let (unit, _strings) = unit_and_strings(b"<?php new E; $o instanceof E;");
        assert_eq!(
            unit.main.cache_size, 16,
            "NEW and INSTANCEOF take one cache slot each, eight bytes a slot"
        );
    }

    /// `zend_compile_func_decl` as the oracle at 8.5.9 dumps it under
    /// `opcache.opt_debug_level=0x10000` (#309): a function and a
    /// class's methods each an op array of its own after the main one,
    /// named `f` and `C::m`, with `RECV` and `RECV_INIT` for the
    /// parameters, the body, and php's implicit `RETURN null` on the
    /// closing brace's line; a call to a function the file has declared
    /// is `INIT_FCALL` with `zend_vm_calc_used_stack`'s number and
    /// `DO_UCALL`, a call on the class to a method it has is `DO_UCALL`
    /// too, and a method call is `DO_FCALL` still; `$this` inside an
    /// instance method is the frame's own, `THIS`, where it is a
    /// receiver, and `FETCH_THIS` where it is read.
    #[test]
    fn a_function_and_a_method_compile_to_their_own_op_arrays() {
        let source: &[u8] = b"<?php
function f($a, $b = 2) {
    echo $a;
    return $a + $b;
}
class C {
    public $p = 1;
    public function m($x) {
        echo $this->p;
        return $this->m2();
    }
    public static function s() { return 3; }
    private function m2() {}
}
$o = new C;
echo f(1), C::s(), $o->m(1), $o->p;
";
        let header = |name: &str, lines: u32, args: u32, vars: u32, tmps: u32, span: &str| {
            format!(
                "\n{name}:\n     ; (lines={lines}, args={args}, vars={vars}, tmps={tmps})\n\
                 \x20    ; (before optimizer)\n     ; :{span}\n     ; return  [] RANGE[0..0]\n"
            )
        };
        let expected = format!(
            "{}{}{}{}{}{}{}{}{}{}",
            header("$_main", 17, 0, 1, 7, "1-17"),
            "0000 V1 = NEW 0 string(\"C\")\n\
             0001 DO_FCALL\n\
             0002 ASSIGN CV0($o) V1\n\
             0003 INIT_FCALL 1 128 string(\"f\")\n\
             0004 SEND_VAL int(1) 1\n\
             0005 V4 = DO_UCALL\n\
             0006 ECHO V4\n\
             0007 INIT_STATIC_METHOD_CALL 0 string(\"C\") string(\"s\")\n\
             0008 V5 = DO_UCALL\n\
             0009 ECHO V5\n\
             0010 INIT_METHOD_CALL 1 CV0($o) string(\"m\")\n\
             0011 SEND_VAL_EX int(1) 1\n\
             0012 V6 = DO_FCALL\n\
             0013 ECHO V6\n\
             0014 T7 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
             0015 ECHO T7\n\
             0016 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    1: 0001 - 0002 (new)\n",
            header("f", 6, 2, 2, 1, "2-5"),
            "0000 CV0($a) = RECV 1\n\
             0001 CV1($b) = RECV_INIT 2 int(2)\n\
             0002 ECHO CV0($a)\n\
             0003 T2 = ADD CV0($a) CV1($b)\n\
             0004 RETURN T2\n\
             0005 RETURN null\n",
            header("C::m", 7, 1, 1, 2, "8-11"),
            "0000 CV0($x) = RECV 1\n\
             0001 T1 = FETCH_OBJ_R THIS string(\"p\")\n\
             0002 ECHO T1\n\
             0003 INIT_METHOD_CALL 0 THIS string(\"m2\")\n\
             0004 V2 = DO_FCALL\n\
             0005 RETURN V2\n\
             0006 RETURN null\n",
            header("C::s", 2, 0, 0, 0, "12-12"),
            "0000 RETURN int(3)\n\
             0001 RETURN null\n",
            header("C::m2", 1, 0, 0, 0, "13-13"),
            "0000 RETURN null\n",
        );
        assert_eq!(dump_of(source), expected);
        let (unit, _strings) = unit_and_strings(source);
        assert_eq!(
            unit.functions[0].1.safepoints.first(),
            Some(&OplineIdx(2)),
            "the entry safepoint stands after the RECV prologue"
        );
        assert_eq!(unit.functions[0].0.parameters.len(), 2);
        assert_eq!(
            unit.classes[0]
                .methods
                .iter()
                .map(|m| m.is_instance)
                .collect::<Vec<_>>(),
            vec![true, false, true]
        );
        assert_eq!(unit.methods.len(), 3);
    }

    /// `zend_compile_method_call` and `zend_compile_static_call` against
    /// the class being compiled (#2675): `$this->p()` to a private and
    /// `$this->f()` to a final method the class declared before the call
    /// are `DO_UCALL` with the arguments sent by the parameters, by
    /// reference to `&$a`, where a public one stays `DO_FCALL` and so
    /// does the method calling itself, which is public; `self::s()` and
    /// `C::s()` inside `class C` are `DO_UCALL` too, the second in `s`'s
    /// own body, which `zend_begin_method_decl` put in the table before
    /// it compiled.
    #[test]
    fn a_call_the_class_being_compiled_pins_down_is_do_ucall() {
        let dump = dump_of(
            b"<?php
class C {
    private function p($a) {}
    final public function f(&$a) {}
    public function g($a) {}
    public static function s($a) { return C::s($a); }
    public function m($a) {
        $this->p($a);
        $this->f($a);
        $this->g($a);
        self::s(1);
        return $this->m($a);
    }
}
",
        );
        let s = "0000 CV0($a) = RECV 1\n\
                 0001 INIT_STATIC_METHOD_CALL 1 string(\"C\") string(\"s\")\n\
                 0002 SEND_VAR CV0($a) 1\n\
                 0003 V1 = DO_UCALL\n\
                 0004 RETURN V1\n\
                 0005 RETURN null\n";
        let m = "0000 CV0($a) = RECV 1\n\
                 0001 INIT_METHOD_CALL 1 THIS string(\"p\")\n\
                 0002 SEND_VAR CV0($a) 1\n\
                 0003 DO_UCALL\n\
                 0004 INIT_METHOD_CALL 1 THIS string(\"f\")\n\
                 0005 SEND_REF CV0($a) 1\n\
                 0006 DO_UCALL\n\
                 0007 INIT_METHOD_CALL 1 THIS string(\"g\")\n\
                 0008 SEND_VAR_EX CV0($a) 1\n\
                 0009 DO_FCALL\n\
                 0010 INIT_STATIC_METHOD_CALL 1 (self) (exception) string(\"s\")\n\
                 0011 SEND_VAL int(1) 1\n\
                 0012 DO_UCALL\n\
                 0013 INIT_METHOD_CALL 1 THIS string(\"m\")\n\
                 0014 SEND_VAR_EX CV0($a) 1\n\
                 0015 V5 = DO_FCALL\n\
                 0016 RETURN V5\n\
                 0017 RETURN null\n";
        assert!(dump.contains(s), "php's C::s, {dump}");
        assert!(dump.contains(m), "php's C::m, {dump}");
    }

    /// `zend_enum_register_funcs` runs as the enum links (#2678): an
    /// enum declaring its own `cases()`, or a backed one its own
    /// `from()`, compiles, its `DECLARE_CLASS` and its method dumped as
    /// `Zend/tests/enum/no-cases.phpt` and `no-from.phpt` dump them, and
    /// the link refuses it when the opline runs.
    #[test]
    fn an_enum_declaring_its_own_cases_or_from_compiles() {
        let cases = dump_of(
            b"<?php
enum Foo {
    case Bar;
    public static function cases(): array { return []; }
}
",
        );
        assert!(
            cases.contains("0000 DECLARE_CLASS string(\"foo\")\n0001 RETURN int(1)\n"),
            "php's $_main, {cases}"
        );
        assert!(
            cases.contains(
                "\nFoo::cases:\n     ; (lines=3, args=0, vars=0, tmps=0)\n\
                 \x20    ; (before optimizer)\n     ; :4-4\n     ; return  [] RANGE[0..0]\n\
                 0000 RETURN array(...)\n\
                 0001 VERIFY_RETURN_TYPE\n\
                 0002 RETURN null\n"
            ),
            "php's Foo::cases, {cases}"
        );
        let from = dump_of(
            b"<?php
enum Foo: int {
    case Bar = 0;
    public static function from(string|int $value): self { return $this; }
}
",
        );
        assert!(
            from.contains("0000 DECLARE_CLASS string(\"foo\")\n0001 RETURN int(1)\n"),
            "php's $_main, {from}"
        );
        assert!(
            from.contains(
                "0000 CV0($value) = RECV 1\n\
                 0001 T1 = FETCH_THIS\n\
                 0002 VERIFY_RETURN_TYPE T1\n\
                 0003 RETURN T1\n\
                 0004 VERIFY_RETURN_TYPE\n\
                 0005 RETURN null\n"
            ),
            "php's Foo::from, {from}"
        );
    }

    /// `zend_compile_class_decl` compiles the body in the order written
    /// (#2797), so a property hook sees in `CG(active_class_entry)`'s
    /// `function_table` the methods declared above its property and
    /// none below: `$this->p()` to a private method above is php's
    /// `DO_UCALL`, with its argument sent by the parameter, and
    /// `$this->q($a)` to one below stays `DO_FCALL`.
    #[test]
    fn a_hook_knows_the_methods_declared_above_its_property() {
        let dump = dump_of(
            b"<?php
class C {
    private function p(&$r) { return 1; }
    public $x { get => $this->p($a); }
    public $y { get => $this->q($a); }
    private function q(&$r) { return 2; }
}
",
        );
        let x = "0000 INIT_METHOD_CALL 1 THIS string(\"p\")\n\
                 0001 SEND_REF CV0($a) 1\n\
                 0002 V1 = DO_UCALL\n";
        let y = "0000 INIT_METHOD_CALL 1 THIS string(\"q\")\n\
                 0001 SEND_VAR_EX CV0($a) 1\n\
                 0002 V1 = DO_FCALL\n";
        assert!(dump.contains(x), "php's C::$x::get, {dump}");
        assert!(dump.contains(y), "php's C::$y::get, {dump}");
        let call = |opcode: &str| {
            format!(
                "0000 INIT_METHOD_CALL 0 THIS string(\"p\")\n\
                 0001 V0 = {opcode}\n\
                 0002 RETURN V0\n"
            )
        };
        let above = dump_of(
            b"<?php
class C {
    private function p() { return 1; }
    public $x { get => $this->p(); }
}
",
        );
        assert!(above.contains(&call("DO_UCALL")), "php's dump, {above}");
        let below = dump_of(
            b"<?php
class C {
    public $x { get => $this->p(); }
    private function p() { return 1; }
}
",
        );
        assert!(below.contains(&call("DO_FCALL")), "php's dump, {below}");
        let scoped = dump_of(
            b"<?php
class C {
    private static function p() { return 1; }
    public $x { get => self::p() + C::p(); }
}
",
        );
        assert!(
            scoped.contains(
                "0000 INIT_STATIC_METHOD_CALL 0 (self) (exception) string(\"p\")\n\
                 0001 V0 = DO_UCALL\n\
                 0002 INIT_STATIC_METHOD_CALL 0 string(\"C\") string(\"p\")\n\
                 0003 V1 = DO_UCALL\n"
            ),
            "php's dump, {scoped}"
        );
    }

    /// `zend_compile_static_call`'s `self::` reads
    /// `CG(active_class_entry)` itself, never the table's class of its
    /// name: in a second `class A` declared under an `if`, `self::f($x)`
    /// is the class being compiled's, which has no `f`, so `DO_FCALL`
    /// with `$x` sent as to an unknown callee, though the `A` bound
    /// early has `f(&$r)`; a hook's `self::h()` to its own private `h`
    /// above is `DO_UCALL`.
    #[test]
    fn self_in_a_class_redeclaring_a_bound_name_is_the_class_compiling() {
        let dump = dump_of(
            b"<?php
class A { public static function f(&$r) {} }
if (true) {
    class A { function g() { self::f($x); } private function h() {} public $p { get => self::h(); } }
}
",
        );
        let g = "0000 INIT_STATIC_METHOD_CALL 1 (self) (exception) string(\"f\")\n\
                 0001 SEND_VAR_EX CV0($x) 1\n\
                 0002 DO_FCALL\n";
        let p = "0000 INIT_STATIC_METHOD_CALL 0 (self) (exception) string(\"h\")\n\
                 0001 V0 = DO_UCALL\n";
        assert!(dump.contains(g), "php's A::g, {dump}");
        assert!(dump.contains(p), "php's A::$p::get, {dump}");
    }

    /// `zend_compile_static_call` over a class bound early (#2016):
    /// `zend_get_compatible_func_or_null` finds the method in the linked
    /// class's `function_table`, which holds what it inherited, so
    /// `B::foo($x)` over `A::foo(&$a)` is `DO_UCALL` with `$x` sent by
    /// reference, as `Zend/tests/lsb/lsb_013.phpt` dumps, and a private
    /// method inherited stays `DO_FCALL`.
    #[test]
    fn a_static_call_to_an_inherited_method_is_do_ucall() {
        let dump = dump_of(
            b"<?php
class A { static function foo(&$a) {} private static function p() {} }
class B extends A {}
B::foo($x);
B::p();
",
        );
        assert!(
            dump.contains(
                "0000 INIT_STATIC_METHOD_CALL 1 string(\"B\") string(\"foo\")\n\
                 0001 SEND_REF CV0($x) 1\n\
                 0002 DO_UCALL\n\
                 0003 INIT_STATIC_METHOD_CALL 0 string(\"B\") string(\"p\")\n\
                 0004 DO_FCALL\n"
            ),
            "php's $_main, {dump}"
        );
    }

    /// `zend_try_early_bind` while the file compiles (#2689): the class
    /// being bound is in no table yet, so a type its parent's method
    /// writes naming it finds nothing, `zend_can_early_bind` comes to
    /// `INHERITANCE_UNRESOLVED`, and the class keeps its
    /// `DECLARE_CLASS_DELAYED`, as
    /// `Zend/tests/type_declarations/union_types/variance/valid.phpt`
    /// dumps; `D`, which `A`'s `B` names neither, is left the same.
    #[test]
    fn a_parent_naming_the_class_being_bound_leaves_it_delayed() {
        let dump = dump_of(
            b"<?php
class A { public function m(B|string $a): A|string {} }
class B extends A { public function m(A|string $a): B|string {} }
class D extends A { public function m(A|string $a): D|string {} }
",
        );
        assert!(
            dump.contains(
                "0000 DECLARE_CLASS_DELAYED string(\"b\") string(\"a\")\n\
                 0001 DECLARE_CLASS_DELAYED string(\"d\") string(\"a\")\n\
                 0002 RETURN int(1)\n"
            ),
            "php's $_main, {dump}"
        );
    }

    /// `zend_compile_property_hooks`' `num_hooked_prop_variance_checks`
    /// (#2688): a `set` hook whose parameter's type names a class not
    /// declared yet keeps the class from the early bind and from
    /// opcache's delayed one, so its opline is `DECLARE_CLASS` with no
    /// parent operand, as
    /// `Zend/tests/property_hooks/set_value_parameter_type_variance_003.phpt`
    /// dumps.
    #[test]
    fn a_set_hook_unresolved_while_compiling_is_declare_class() {
        let dump = dump_of(
            b"<?php
class Test {
    public X $prop {
        set(Y $prop) {}
    }
}
interface X {}
interface Y {}
",
        );
        assert!(
            dump.contains("0000 DECLARE_CLASS string(\"test\")\n0001 RETURN int(1)\n"),
            "php's $_main, {dump}"
        );
    }

    /// `zend_delayed_compile_prop` under `zend_compile_encaps_list`
    /// (#2676): `"$this->p"` inside a method reads the property off the
    /// `IS_UNUSED` receiver, `FETCH_OBJ_R THIS`, as `"{$this->p}"` does.
    #[test]
    fn this_s_property_in_a_string_is_fetched_off_this() {
        let dump = dump_of(
            b"<?php
class C {
    public function m() { return \"$this->p {$this->p}\"; }
}
",
        );
        let m = "0000 T0 = FETCH_OBJ_R THIS string(\"p\")\n\
                 0001 T3 = ROPE_INIT 3 T0\n\
                 0002 T3 = ROPE_ADD 1 T3 string(\" \")\n\
                 0003 T1 = FETCH_OBJ_R THIS string(\"p\")\n\
                 0004 T2 = ROPE_END 2 T3 T1\n\
                 0005 RETURN T2\n\
                 0006 RETURN null\n";
        assert!(dump.contains(m), "php's C::m, {dump}");
    }

    /// `zend_compile_foreach` over `$this` (#2687): by reference the
    /// subject is compiled under `BP_VAR_W`, `FETCH_THIS` into a `VAR`
    /// and `FE_RESET_RW` over it; by value under `BP_VAR_R`, into a
    /// temporary.
    #[test]
    fn foreach_over_this_by_reference_fetches_it_into_a_var() {
        let dump = dump_of(
            b"<?php
class C {
    public function g() { foreach ($this as &$v) {} foreach ($this as $v) {} }
}
",
        );
        let g = "0000 V1 = FETCH_THIS\n\
                 0001 V2 = FE_RESET_RW V1 0004\n\
                 0002 FE_FETCH_RW V2 CV0($v) 0004\n\
                 0003 JMP 0002\n\
                 0004 FE_FREE V2\n\
                 0005 T3 = FETCH_THIS\n\
                 0006 V4 = FE_RESET_R T3 0009\n\
                 0007 FE_FETCH_R V4 CV0($v) 0009\n\
                 0008 JMP 0007\n\
                 0009 FE_FREE V4\n\
                 0010 RETURN null\n";
        assert!(dump.contains(g), "php's C::g, {dump}");
    }

    /// `zend_is_assign_to_self` through a property (#2683): `$a->x[] =
    /// $a` and `$a[0]->p[1] = $a` read `$a` into a temporary first,
    /// numbered after the delayed fetches, and `OP_DATA` carries it.
    #[test]
    fn an_assignment_to_self_through_a_property_reads_the_value_first() {
        let dump = dump_of(
            b"<?php
$a = new stdClass;
$a->x[] = $a;
$a[0]->p[1] = $a;
",
        );
        let main = "0003 T6 = QM_ASSIGN CV0($a)\n\
                    0004 V4 = FETCH_OBJ_W (dim write) CV0($a) string(\"x\")\n\
                    0005 ASSIGN_DIM V4 NEXT\n\
                    0006 OP_DATA T6\n\
                    0007 T10 = QM_ASSIGN CV0($a)\n\
                    0008 V7 = FETCH_DIM_W CV0($a) int(0)\n\
                    0009 V8 = FETCH_OBJ_W (dim write) V7 string(\"p\")\n\
                    0010 ASSIGN_DIM V8 int(1)\n\
                    0011 OP_DATA T10\n\
                    0012 RETURN int(1)\n";
        assert!(dump.contains(main), "php's main, {dump}");
        // `zend_is_variable` takes no call, so the walk stops at
        // `$a->m()`, and a `static` property's class is walked into.
        let dump = dump_of(b"<?php\n$a->m()->x[0] = $a;\n$a::$s[0] = $a;\n");
        let main = "0000 INIT_METHOD_CALL 0 CV0($a) string(\"m\")\n\
                    0001 V1 = DO_FCALL\n\
                    0002 V1 = SEPARATE V1\n\
                    0003 V2 = FETCH_OBJ_W (dim write) V1 string(\"x\")\n\
                    0004 ASSIGN_DIM V2 int(0)\n\
                    0005 OP_DATA CV0($a)\n\
                    0006 V4 = FETCH_CLASS (exception) CV0($a)\n\
                    0007 T7 = QM_ASSIGN CV0($a)\n\
                    0008 V5 = FETCH_STATIC_PROP_W (dim write) string(\"s\") V4\n\
                    0009 ASSIGN_DIM V5 int(0)\n\
                    0010 OP_DATA T7\n\
                    0011 RETURN int(1)\n";
        assert!(dump.contains(main), "php's main, {dump}");
    }

    /// `zend_eval_const_expr`'s `ZEND_AST_CAST` arm (#357): the walk
    /// inside an array expression folds every key and value in place
    /// (`zend_try_ct_eval_array`'s first pass), so a cast over a
    /// constant is the value it casts to even where the array itself is
    /// no constant, and a cast over a variable stays the `CAST` opline.
    /// `zend_compile_cast` folds nothing, so the same cast written
    /// outside such a walk is the opline still.
    #[test]
    fn a_cast_inside_an_array_expression_folds_and_one_outside_it_does_not() {
        assert_eq!(
            oplines_of(b"<?php\n$a = [(int)\"1\", $b]; $c = [(string)1, (float)$b];\n"),
            "0000 T3 = INIT_ARRAY 2 (packed) int(1) NEXT\n\
             0001 T3 = ADD_ARRAY_ELEMENT CV1($b) NEXT\n\
             0002 ASSIGN CV0($a) T3\n\
             0003 T5 = INIT_ARRAY 2 (packed) string(\"1\") NEXT\n\
             0004 T6 = CAST (double) CV1($b)\n\
             0005 T5 = ADD_ARRAY_ELEMENT T6 NEXT\n\
             0006 ASSIGN CV2($c) T5\n\
             0007 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    3: 0001 - 0002 (tmp/var)\n\
             \x20    5: 0004 - 0006 (tmp/var)\n"
        );
        assert_eq!(
            oplines_of(b"<?php\n$x = (int)\"1\";\necho (int)\"2\";\n"),
            "0000 T1 = CAST (long) string(\"1\")\n\
             0001 ASSIGN CV0($x) T1\n\
             0002 T3 = CAST (long) string(\"2\")\n\
             0003 ECHO T3\n\
             0004 RETURN int(1)\n"
        );
        // `zend_try_ct_eval_cast`: `(bool)`, `(int)`, `(float)`,
        // `(string)` and `(array)` fold, a `(string)` of a double or an
        // array does not, since one reads `precision` and the other
        // warns, an `(int)` of a double past the long's range does not,
        // no cast over a NAN does, and there is no `(object)` arm.
        let (unit, mut strings) = unit_and_strings(
            b"<?php\nclass C { const B = (bool)\"a\"; const I = (int)\"12abc\"; const F = (float)\"1e3\"; const S = (string)true; const A = (array)1; }\n",
        );
        let array = strings
            .constant_array(&[(php_heap::Key::Long(0), php_heap::Zval::long(1))])
            .map(php_heap::Zval::array)
            .ok()
            .and_then(php_unit::decl::ImmutableArray::new)
            .map_or(ConstExpr::Null, ConstExpr::ImmutableArray);
        assert_eq!(
            unit.classes[0]
                .constants
                .iter()
                .map(|c| c.value.clone())
                .collect::<Vec<_>>(),
            vec![
                ConstExpr::Bool(true),
                ConstExpr::long(12),
                ConstExpr::Double(1000.0),
                ConstExpr::Str(b"1".as_slice().into()),
                array,
            ]
        );
    }

    /// `zend_compile_params` under
    /// Every optional parameter before the last required one is a
    /// required `RECV` with no default (`zend_compile_params`,
    /// `Z/Zend/zend_compile.c:7886`), the `Type $x = null` spelling as
    /// much as `?Type $x = null`: the first only skips the deprecation.
    /// The dump is the oracle's under `[regime.dump]` at 8.5.9 (#2684).
    #[test]
    fn an_optional_parameter_before_a_required_one_is_a_recv_whatever_its_spelling() {
        assert_eq!(
            dump_of(b"<?php function f(?int $a = null, $b, Foo $c = null, $d) {}"),
            "\n$_main:\n\
            \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
            \x20    ; (before optimizer)\n\
            \x20    ; :1-1\n\
            \x20    ; return  [] RANGE[0..0]\n\
            0000 RETURN int(1)\n\
            \nf:\n\
            \x20    ; (lines=5, args=4, vars=4, tmps=0)\n\
            \x20    ; (before optimizer)\n\
            \x20    ; :1-1\n\
            \x20    ; return  [] RANGE[0..0]\n\
            0000 CV0($a) = RECV 1\n\
            0001 CV1($b) = RECV 2\n\
            0002 CV2($c) = RECV 3\n\
            0003 CV3($d) = RECV 4\n\
            0004 RETURN null\n\
            "
        );
    }

    /// A parameter's default is `zend_const_expr_to_zval`'s: under
    /// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION` no constant is
    /// substituted, so `self::X` stays the `IS_CONSTANT_AST` the dump prints
    /// as `zval(type=11)`, while a class's name is none and
    /// `zend_try_compile_const_expr_resolve_class_name` resolves `self::class`
    /// and `parent::class` in a class with a parent, and `Baz::class`, to
    /// their strings. The dump is the oracle's under `[regime.dump]` at 8.5.9
    /// (#2685).
    #[test]
    fn a_class_name_in_a_parameter_default_folds_to_its_string() {
        assert_eq!(
            dump_of(b"<?php class P {} class C extends P { function f($a = self::class, $b = parent::class, $c = Baz::class, $d = self::X) {} }"),
            "\n$_main:\n\
             \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \nC::f:\n\
             \x20    ; (lines=5, args=4, vars=4, tmps=0)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 CV0($a) = RECV_INIT 1 string(\"C\")\n\
             0001 CV1($b) = RECV_INIT 2 string(\"P\")\n\
             0002 CV2($c) = RECV_INIT 3 string(\"Baz\")\n\
             0003 CV3($d) = RECV_INIT 4 zval(type=11)\n\
             0004 RETURN null\n\
             "
        );
    }

    /// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION` (#384): a
    /// default naming a constant is not folded, so its `RECV_INIT`
    /// carries the `IS_CONSTANT_AST` value the oracle dumps as
    /// `zval(type=11)`, the constant expression beside the op array's
    /// constant table in the order the parameters were compiled, and the
    /// name as written for Reflection. A default that folds is the value
    /// it folds to, as it always was.
    #[test]
    fn a_parameter_default_naming_a_constant_is_an_unevaluated_constant() {
        let source: &[u8] =
            b"<?php\nclass A { const X = 5; }\nfunction f($a = PHP_INT_MAX, $b = A::X, $c = 1) { echo $a; }\n";
        assert_eq!(
            dump_of(source),
            "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-4\n     ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \nf:\n     ; (lines=5, args=3, vars=3, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :3-3\n     ; return  [] RANGE[0..0]\n\
             0000 CV0($a) = RECV_INIT 1 zval(type=11)\n\
             0001 CV1($b) = RECV_INIT 2 zval(type=11)\n\
             0002 CV2($c) = RECV_INIT 3 int(1)\n\
             0003 ECHO CV0($a)\n\
             0004 RETURN null\n"
        );
        let (unit, _strings) = unit_and_strings(source);
        let body = &unit.functions[0].1;
        let unevaluated: Vec<&ConstExpr> = body
            .literals
            .iter()
            .filter_map(|constant| constant.as_const_expr())
            .filter_map(|at| body.const_exprs.get(at as usize))
            .collect();
        assert_eq!(
            unevaluated,
            vec![
                &ConstExpr::Constant {
                    name: ConstantName::Resolved(Name(b"PHP_INT_MAX".as_slice().into())),
                    line: 3,
                },
                &ConstExpr::ClassConstant {
                    class: Name(b"A".as_slice().into()),
                    class_kind: NameKind::NotFullyQualified,
                    name: Name(b"X".as_slice().into()),
                    line: 3,
                },
            ]
        );
        assert_eq!(
            unit.functions[0]
                .0
                .parameters
                .iter()
                .map(|p| p.default.expr().cloned())
                .collect::<Vec<_>>(),
            vec![
                Some(ConstExpr::Constant {
                    name: ConstantName::Resolved(Name(b"PHP_INT_MAX".as_slice().into())),
                    line: 3,
                }),
                Some(ConstExpr::ClassConstant {
                    class: Name(b"A".as_slice().into()),
                    class_kind: NameKind::NotFullyQualified,
                    name: Name(b"X".as_slice().into()),
                    line: 3,
                }),
                Some(ConstExpr::long(1)),
            ]
        );
    }

    /// `zend_compile_params` and `zend_compile_return_type` as the
    /// oracle at 8.5.9 dumps them under
    /// `opcache.opt_debug_level=0x10000` (#385): a typed parameter is a
    /// `RECV` still, one with a default a `RECV_INIT`, and a `...` a
    /// `RECV_VARIADIC` numbered after the rest and left out of `args`;
    /// a declared return type is a `VERIFY_RETURN_TYPE` before each
    /// `RETURN` that needs one and before php's implicit final return,
    /// `void` needs none, and `function &g` returns by reference, whose
    /// implicit return carries `-1` and prints `(function)`; a promoted
    /// parameter is the `ASSIGN_OBJ`/`OP_DATA` pair after the prologue.
    #[test]
    fn typed_parameters_and_a_return_type_compile_to_php_s_op_arrays() {
        let source: &[u8] = b"<?php
function f(int $a, ?string $b = null, float ...$rest): int {
    return $a;
}
function &g(array &$x) { return $x; }
function h(): void {}
class C {
    public function __construct(public int $p, protected readonly string $q = \"s\") {}
}
";
        let header = |name: &str, lines: u32, args: u32, vars: u32, tmps: u32, span: &str| {
            format!(
                "\n{name}:\n     ; (lines={lines}, args={args}, vars={vars}, tmps={tmps})\n\
                 \x20    ; (before optimizer)\n     ; :{span}\n     ; return  [] RANGE[0..0]\n"
            )
        };
        let expected = format!(
            "{}{}{}{}{}{}{}{}{}{}",
            header("$_main", 1, 0, 0, 0, "1-10"),
            "0000 RETURN int(1)\n",
            header("f", 7, 2, 3, 0, "2-4"),
            "0000 CV0($a) = RECV 1\n\
             0001 CV1($b) = RECV_INIT 2 null\n\
             0002 CV2($rest) = RECV_VARIADIC 3\n\
             0003 VERIFY_RETURN_TYPE CV0($a)\n\
             0004 RETURN CV0($a)\n\
             0005 VERIFY_RETURN_TYPE\n\
             0006 RETURN null\n",
            header("g", 3, 1, 1, 0, "5-5"),
            "0000 CV0($x) = RECV 1\n\
             0001 RETURN_BY_REF CV0($x)\n\
             0002 RETURN_BY_REF (function) null\n",
            header("h", 1, 0, 0, 0, "6-6"),
            "0000 RETURN null\n",
            header("C::__construct", 7, 2, 2, 0, "8-8"),
            "0000 CV0($p) = RECV 1\n\
             0001 CV1($q) = RECV_INIT 2 string(\"s\")\n\
             0002 ASSIGN_OBJ THIS string(\"p\")\n\
             0003 OP_DATA CV0($p)\n\
             0004 ASSIGN_OBJ THIS string(\"q\")\n\
             0005 OP_DATA CV1($q)\n\
             0006 RETURN null\n",
        );
        assert_eq!(dump_of(source), expected);
        let (unit, _strings) = unit_and_strings(source);
        let f = &unit.functions[0].0;
        assert_eq!(f.return_type, Some(Type::Int));
        assert!(!f.returns_by_ref);
        assert_eq!(
            f.parameters
                .iter()
                .map(|p| (
                    p.ty.clone(),
                    p.by_ref,
                    p.variadic,
                    p.default.expr().is_some()
                ))
                .collect::<Vec<_>>(),
            vec![
                (Some(Type::Int), false, false, false),
                (
                    Some(Type::Nullable(Box::new(Type::String))),
                    false,
                    false,
                    true
                ),
                (Some(Type::Float), false, true, false),
            ],
            "the implicit `?string` of a default of null, and the variadic's own type"
        );
        let g = &unit.functions[1].0;
        assert!(g.returns_by_ref, "`function &g` returns by reference");
        assert!(g.parameters[0].by_ref, "`&$x` is taken by reference");
        assert_eq!(
            unit.functions[0].1.safepoints.first(),
            Some(&OplineIdx(3)),
            "the entry safepoint stands after the whole RECV prologue"
        );
        assert_eq!(
            unit.classes[0]
                .properties
                .iter()
                .map(|p| (
                    String::from_utf8_lossy(&p.name.0).into_owned(),
                    p.visibility,
                    p.is_readonly
                ))
                .collect::<Vec<_>>(),
            vec![
                ("p".to_owned(), Visibility::Public, false),
                ("q".to_owned(), Visibility::Protected, true),
            ],
            "a promoted parameter is a declared property of the class"
        );
    }

    /// `ZEND_TYPE_PURE_MASK(arg_info[-1].type)` on every op array: the
    /// declared return type's mask wherever one is declared, the
    /// `string` `zend_compile_params` falls back to for `__toString` and
    /// the `void` a `set` hook is given among them, and 0 where none is
    /// declared, the main op array's included.
    #[test]
    fn an_op_array_s_return_mask_is_its_declared_return_type_s_mask() {
        let (unit, _strings) = unit_and_strings(
            b"<?php
function a(): int { return 1; }
function b(): ?int { return null; }
function c(): int|string|null { return 1; }
function d(): Countable { return new ArrayObject; }
function e() { return 1; }
class K {
    public int $p { get => 1; set { } }
    public function s(): static { return $this; }
    public function m(): self|false { return $this; }
    public function __toString() { return \"\"; }
}
$f = fn (): float => 1.5;
",
        );
        let mask = |decl: &FunctionDecl| decl.return_type.as_ref().map_or(0, Type::mask);
        let mut held: Vec<(String, u32, u32)> = Vec::new();
        let mut take = |decl: &FunctionDecl, code: &OpArray| {
            let name = String::from_utf8_lossy(&decl.name.0).into_owned();
            held.push((name, mask(decl), code.return_mask));
        };
        for (decl, code) in unit.functions.iter().chain(&unit.dynamic) {
            take(decl, code);
        }
        for body in &unit.methods {
            let class = &unit.classes[body.class as usize];
            take(&class.methods[body.method as usize].function, &body.body);
        }
        for body in &unit.hooks {
            let property = &unit.classes[body.class as usize].properties[body.property as usize];
            let hook = property
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.of(body.kind));
            assert!(hook.is_some(), "a hook's body has its declaration");
            if let Some(hook) = hook {
                take(&hook.function, &body.body);
            }
        }
        for (name, declared, carried) in &held {
            assert_eq!(declared, carried, "{name}");
        }
        let carried = |name: &str| {
            let found = held.iter().find(|(held, ..)| held == name);
            found.map(|(.., carried)| *carried)
        };
        assert_eq!(unit.main.return_mask, 0, "the main op array declares none");
        assert_eq!(carried("a"), Some(1 << 4));
        assert_eq!(carried("b"), Some((1 << 4) | (1 << 1)));
        assert_eq!(carried("c"), Some((1 << 4) | (1 << 6) | (1 << 1)));
        assert_eq!(carried("d"), Some(0), "a class name contributes no bit");
        assert_eq!(carried("e"), Some(0), "no return type is declared");
        assert_eq!(carried("s"), Some(1 << 15), "`static` is a bit of its own");
        assert_eq!(carried("m"), Some(1 << 2), "`self` contributes no bit");
        assert_eq!(carried("__toString"), Some(1 << 6), "the fallback `string`");
        assert_eq!(carried("$p::get"), Some(1 << 4), "the property's type");
        assert_eq!(
            carried("$p::set"),
            Some(1 << 14),
            "the `void` of a `set` hook"
        );
        let closure = held.iter().find(|(name, ..)| name.starts_with("{closure:"));
        assert_eq!(closure.map(|(.., carried)| *carried), Some(1 << 5));
        assert_eq!(held.len(), 11, "every function, method, hook and closure");
    }

    /// The two `INIT_*_METHOD_CALL`s, read as
    /// [`new_compiles_to_php_s_op_array`]'s cases were, and `INSTANCEOF`
    /// as an argument sent.
    #[test]
    fn the_method_calls_compile_to_php_s_op_arrays() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $x = E::m();",
                4,
                1,
                2,
                "0000 INIT_STATIC_METHOD_CALL 0 string(\"E\") string(\"m\")\n\
                 0001 V1 = DO_FCALL\n\
                 0002 ASSIGN CV0($x) V1\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php E::__construct();",
                3,
                0,
                1,
                "0000 INIT_STATIC_METHOD_CALL 0 string(\"E\") CONSTRUCTOR\n\
                 0001 DO_FCALL\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; $o->m(1, $o)->n();",
                8,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 INIT_METHOD_CALL 2 CV0($o) string(\"m\")\n\
                 0002 SEND_VAL_EX int(1) 1\n\
                 0003 SEND_VAR_EX CV0($o) 2\n\
                 0004 V2 = DO_FCALL\n\
                 0005 INIT_METHOD_CALL 0 V2 string(\"n\")\n\
                 0006 DO_FCALL\n\
                 0007 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; $o->m(f($o));",
                8,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 INIT_METHOD_CALL 1 CV0($o) string(\"m\")\n\
                 0002 INIT_FCALL_BY_NAME 1 string(\"f\")\n\
                 0003 SEND_VAR_EX CV0($o) 1\n\
                 0004 V2 = DO_FCALL_BY_NAME\n\
                 0005 SEND_VAR_NO_REF_EX V2 1\n\
                 0006 DO_FCALL\n\
                 0007 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; \\E::m($o instanceof \\E);",
                6,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 INIT_STATIC_METHOD_CALL 1 string(\"E\") string(\"m\")\n\
                 0002 T2 = INSTANCEOF CV0($o) string(\"E\")\n\
                 0003 SEND_VAL_EX T2 1\n\
                 0004 DO_FCALL\n\
                 0005 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        let (unit, _strings) = unit_and_strings(b"<?php E::m(); $o->n();");
        assert_eq!(
            unit.main.cache_size, 32,
            "each INIT takes two cache slots, the class and the method, eight bytes a slot"
        );
    }

    /// `INSTANCEOF` as the oracle at 8.5.9 dumps it: into a temporary,
    /// freed where the test is a statement; `false` where the left side
    /// is a constant; a chain grouped to the left, the second over the
    /// first's temporary.
    #[test]
    fn instanceof_compiles_to_php_s_op_array() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $o = 1; $o instanceof E;",
                4,
                1,
                2,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 T2 = INSTANCEOF CV0($o) string(\"E\")\n\
                 0002 FREE T2\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; echo 1 instanceof E, strlen('a') instanceof E;",
                4,
                1,
                1,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 ECHO bool(false)\n\
                 0002 ECHO bool(false)\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $o = 1; echo $o instanceof E instanceof F;",
                5,
                1,
                3,
                "0000 ASSIGN CV0($o) int(1)\n\
                 0001 T2 = INSTANCEOF CV0($o) string(\"E\")\n\
                 0002 T3 = INSTANCEOF T2 string(\"F\")\n\
                 0003 ECHO T3\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_delayed_compile_prop` as the oracle at 8.5.9 dumps it under
    /// `opcache.opt_debug_level=0x10000`: `FETCH_OBJ_R` into a temporary
    /// with the name as one constant, the receiver's operand whatever it
    /// is, the temporary freed where the fetch is a statement and sent
    /// by `SEND_VAL` to a callee the table has.
    #[test]
    fn property_fetches_compile_to_php_s_op_arrays() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $x = $o->p;",
                3,
                2,
                2,
                "0000 T2 = FETCH_OBJ_R CV1($o) string(\"p\")\n\
                 0001 ASSIGN CV0($x) T2\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $x = $o->p + $o->q;",
                5,
                2,
                4,
                "0000 T2 = FETCH_OBJ_R CV1($o) string(\"p\")\n\
                 0001 T3 = FETCH_OBJ_R CV1($o) string(\"q\")\n\
                 0002 T4 = ADD T2 T3\n\
                 0003 ASSIGN CV0($x) T4\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    2: 0001 - 0002 (tmp/var)\n",
            ),
            (
                b"<?php (new E)->p;",
                5,
                0,
                3,
                "0000 V0 = NEW 0 string(\"E\")\n\
                 0001 DO_FCALL\n\
                 0002 T2 = FETCH_OBJ_R V0 string(\"p\")\n\
                 0003 FREE T2\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (new)\n",
            ),
            (
                b"<?php $o->p->q()->r;",
                6,
                1,
                3,
                "0000 T1 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0001 INIT_METHOD_CALL 0 T1 string(\"q\")\n\
                 0002 V2 = DO_FCALL\n\
                 0003 T3 = FETCH_OBJ_R V2 string(\"r\")\n\
                 0004 FREE T3\n\
                 0005 RETURN int(1)\n",
            ),
            (
                b"<?php var_dump($o->p);",
                5,
                1,
                2,
                "0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0001 T1 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0002 SEND_VAL T1 1\n\
                 0003 DO_ICALL\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        let (unit, _strings) = unit_and_strings(b"<?php $o->p;");
        assert_eq!(
            unit.main.cache_size, 24,
            "a property fetch takes three cache slots (zend_alloc_cache_slots(3)), eight bytes a slot"
        );
    }

    /// `FETCH_THIS` as the oracle dumps it: `$this` into a temporary
    /// where it is read and into a `VAR` where it is a receiver or is
    /// sent to a callee nothing is known of (`zend_compile_simple_var`,
    /// `zend_compile_args`).
    #[test]
    fn this_compiles_to_fetch_this_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php echo $this;",
                3,
                0,
                1,
                "0000 T0 = FETCH_THIS\n\
                 0001 ECHO T0\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $this->p;",
                4,
                0,
                2,
                "0000 V0 = FETCH_THIS\n\
                 0001 T1 = FETCH_OBJ_R V0 string(\"p\")\n\
                 0002 FREE T1\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $this->m();",
                4,
                0,
                2,
                "0000 V0 = FETCH_THIS\n\
                 0001 INIT_METHOD_CALL 0 V0 string(\"m\")\n\
                 0002 DO_FCALL\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php f($this);",
                5,
                0,
                2,
                "0000 INIT_FCALL_BY_NAME 1 string(\"f\")\n\
                 0001 V0 = FETCH_THIS\n\
                 0002 SEND_VAR_EX V0 1\n\
                 0003 DO_FCALL_BY_NAME\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `$b = &$a` as the oracle dumps it (`zend_compile_assign_ref`):
    /// `ASSIGN_REF` over the target's CV and the source's, the target
    /// numbered first, its `VAR` marked unused where nothing reads it
    /// and kept where an assignment, an `ECHO` or an operator does; the
    /// same with the `&` spaced either way and with one CV as both.
    ///
    /// What stands on the right is `zend_compile_var` under `BP_VAR_W`
    /// with `by_ref`, so a dimension is the `FETCH_DIM_W` chain, a
    /// property and a `static` property their `_W` fetches carrying
    /// `ZEND_FETCH_REF`, and `$this` a `FETCH_THIS` into a `VAR`. The
    /// target is held back while that compiles, which is why
    /// `$a[1] = &$a[0]` fetches the source first and why `MAKE_REF`
    /// stands between them wherever the target is no plain `$name`.
    #[test]
    #[allow(clippy::too_many_lines)] // one case per shape: the table is the test
    fn a_by_reference_assignment_compiles_to_assign_ref_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $b = &$a;",
                2,
                2,
                1,
                "0000 ASSIGN_REF CV0($b) CV1($a)\n\
                 0001 RETURN int(1)\n",
            ),
            (
                b"<?php $b =& $a;",
                2,
                2,
                1,
                "0000 ASSIGN_REF CV0($b) CV1($a)\n\
                 0001 RETURN int(1)\n",
            ),
            (
                b"<?php $a = &$a;",
                2,
                1,
                1,
                "0000 ASSIGN_REF CV0($a) CV0($a)\n\
                 0001 RETURN int(1)\n",
            ),
            (
                b"<?php $c = $b = &$a;",
                3,
                3,
                2,
                "0000 V3 = ASSIGN_REF CV1($b) CV2($a)\n\
                 0001 ASSIGN CV0($c) V3\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php echo $b = &$a;",
                3,
                2,
                1,
                "0000 V2 = ASSIGN_REF CV0($b) CV1($a)\n\
                 0001 ECHO V2\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $x = ($b = &$a) + 1;",
                4,
                3,
                3,
                "0000 V3 = ASSIGN_REF CV1($b) CV2($a)\n\
                 0001 T4 = ADD V3 int(1)\n\
                 0002 ASSIGN CV0($x) T4\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $b = &$a; $a = 1; var_dump($b);",
                6,
                2,
                3,
                "0000 ASSIGN_REF CV0($b) CV1($a)\n\
                 0001 ASSIGN CV1($a) int(1)\n\
                 0002 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0003 SEND_VAR CV0($b) 1\n\
                 0004 DO_ICALL\n\
                 0005 RETURN int(1)\n",
            ),
            // `zend_compile_var(BP_VAR_W, 1)` over the source: the
            // target's own fetch is held back until it has compiled, so
            // the source is numbered second and fetched first, with
            // `MAKE_REF` between the two (bug #71539).
            (
                b"<?php $a[1] = &$a[0];",
                5,
                1,
                4,
                "0000 V2 = FETCH_DIM_W CV0($a) int(0)\n\
                 0001 V3 = MAKE_REF V2\n\
                 0002 V1 = FETCH_DIM_W CV0($a) int(1)\n\
                 0003 ASSIGN_REF V1 V3\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0002 - 0003 (tmp/var)\n",
            ),
            (
                b"<?php $b = &$o->p;",
                3,
                2,
                2,
                "0000 V2 = FETCH_OBJ_W (ref) CV1($o) string(\"p\")\n\
                 0001 ASSIGN_REF CV0($b) V2\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $b = &C::$s;",
                3,
                1,
                2,
                "0000 V1 = FETCH_STATIC_PROP_W (ref) string(\"s\") string(\"C\")\n\
                 0001 ASSIGN_REF CV0($b) V1\n\
                 0002 RETURN int(1)\n",
            ),
            // Only the outermost link of the chain is the reference's:
            // the `FETCH_OBJ_W` under it is `ZEND_FETCH_DIM_WRITE` as
            // any property under a dimension is.
            (
                b"<?php $b = &$o->p[0];",
                4,
                2,
                3,
                "0000 V2 = FETCH_OBJ_W (dim write) CV1($o) string(\"p\")\n\
                 0001 V3 = FETCH_DIM_W V2 int(0)\n\
                 0002 ASSIGN_REF CV0($b) V3\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $b = &$this;",
                3,
                1,
                2,
                "0000 V1 = FETCH_THIS\n\
                 0001 ASSIGN_REF CV0($b) V1\n\
                 0002 RETURN int(1)\n",
            ),
            // A `static` property on the left is php's
            // `ASSIGN_STATIC_PROP_REF` arm, whose source rides in the
            // `OP_DATA` after it and is `MAKE_REF`'d like any other.
            (
                b"<?php C::$t = &$a[0];",
                5,
                1,
                3,
                "0000 V2 = FETCH_DIM_W CV0($a) int(0)\n\
                 0001 V3 = MAKE_REF V2\n\
                 0002 ASSIGN_STATIC_PROP_REF string(\"t\") string(\"C\")\n\
                 0003 OP_DATA V3\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `zend_is_call`: a call is the source php marks
        // `ZEND_RETURNS_FUNCTION`, and the name it resolves as the
        // program serves is `INIT_FCALL_BY_NAME`.
        assert_eq!(
            dump_of(b"<?php $b = &f();"),
            main_dump(
                4,
                1,
                2,
                "0000 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0001 V1 = DO_FCALL_BY_NAME\n\
                 0002 ASSIGN_REF (function) CV0($b) V1\n\
                 0003 RETURN int(1)\n"
            )
        );
        // `zend_compile_var_inner`'s default arm under `BP_VAR_W`, and
        // the arm after `MAKE_REF` over a call php answered in a
        // temporary: php's own words for each.
        assert_eq!(
            refused_message(b"<?php $b = &count($x);").as_deref(),
            Some("Cannot use result of built-in function in write context")
        );
        for source in [
            b"<?php $b = &$a = 1;".as_slice(),
            b"<?php $b = &&$a;".as_slice(),
        ] {
            assert!(
                refused(source).is_some(),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// An argument the callee the table has declares by reference, as the
    /// oracle dumps it (`zend_compile_args` under
    /// `ARG_SHOULD_BE_SENT_BY_REF`): `SEND_REF` over the variable's CV,
    /// the variadic's bit for every argument past the declared ones, and
    /// `SEND_VAL_EX` for an argument that is no variable, which php
    /// leaves to the handler to refuse. A by-reference return is
    /// `RETURN_BY_REF` at each `return` and at the end of the body, and
    /// `$b = &f($a)` is `ASSIGN_REF` over the call's `VAR` carrying
    /// `ZEND_RETURNS_FUNCTION`.
    #[test]
    fn a_by_reference_parameter_is_sent_by_ref_as_php_emits_it() {
        let cases: &[(&[u8], &str)] = &[
            (
                b"<?php function f(&$x) {} $a = 1; f($a);",
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 INIT_FCALL 1 96 string(\"f\")\n\
                 0002 SEND_REF CV0($a) 1\n\
                 0003 DO_UCALL\n\
                 0004 RETURN int(1)\n",
            ),
            (
                b"<?php function f(&...$r) {} $a = 1; f($a, $a);",
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 INIT_FCALL 2 128 string(\"f\")\n\
                 0002 SEND_REF CV0($a) 1\n\
                 0003 SEND_REF CV0($a) 2\n\
                 0004 DO_UCALL\n\
                 0005 RETURN int(1)\n",
            ),
            (
                b"<?php function f(&$x) {} f(1);",
                "0000 INIT_FCALL 1 96 string(\"f\")\n\
                 0001 SEND_VAL_EX int(1) 1\n\
                 0002 DO_UCALL\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php function &f(&$x) { return $x; } $a = 1; $b = &f($a);",
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 INIT_FCALL 1 96 string(\"f\")\n\
                 0002 SEND_REF CV0($a) 1\n\
                 0003 V3 = DO_UCALL\n\
                 0004 ASSIGN_REF (function) CV1($b) V3\n\
                 0005 RETURN int(1)\n",
            ),
        ];
        for (source, oplines) in cases {
            let dumped = dump_of(source);
            let main = dumped.split("\n\nf:").next().unwrap_or_default();
            assert!(
                main.trim_end().ends_with(oplines.trim_end()),
                "{}\n{dumped}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// The body of a function written `function &f`: `RETURN_BY_REF` at
    /// each `return` and the one `zend_emit_final_return` closes it with,
    /// which carries `ZEND_RETURNS_FUNCTION` as php's does.
    #[test]
    fn a_by_reference_return_is_return_by_ref_as_php_emits_it() {
        let dumped = dump_of(b"<?php function &f(&$x) { return $x; }");
        assert!(
            dumped.trim_end().ends_with(
                "0000 CV0($x) = RECV 1\n\
                 0001 RETURN_BY_REF CV0($x)\n\
                 0002 RETURN_BY_REF (function) null"
            ),
            "{dumped}"
        );
    }

    /// `$a[k] = v`, `$a[] = v` and `$a[] = &$x` as the oracle dumps them
    /// (`zend_compile_assign`'s and `zend_compile_assign_ref`'s
    /// `ZEND_AST_DIM` arms): the value's oplines, then `ASSIGN_DIM` over
    /// the container with the dimension in `op2` and `OP_DATA` carrying
    /// the value, the append an `UNUSED` `op2` the dump prints as `NEXT`;
    /// a chain is `FETCH_DIM_W` into a `VAR` per link but the last, each
    /// `VAR` numbered after its own dimension and before the value's
    /// temporaries; a constant string dimension that is a canonical
    /// decimal integer is that integer (`zend_handle_numeric_dim`); the
    /// temporary `ASSIGN_DIM` writes is marked unused where nothing reads
    /// it and kept where an assignment does; `$a[0] = $a` reads the right
    /// `$a` first through a `QM_ASSIGN`; and a by-reference write leaves
    /// the last link a `FETCH_DIM_W` with `ASSIGN_REF` over its `VAR`.
    #[test]
    fn a_dimension_assignment_compiles_to_assign_dim_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $a[0] = 2;",
                3,
                1,
                1,
                "0000 ASSIGN_DIM CV0($a) int(0)\n\
                 0001 OP_DATA int(2)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $a[] = 3;",
                3,
                1,
                1,
                "0000 ASSIGN_DIM CV0($a) NEXT\n\
                 0001 OP_DATA int(3)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $a[\"10\"] = 1;",
                3,
                1,
                1,
                "0000 ASSIGN_DIM CV0($a) int(10)\n\
                 0001 OP_DATA int(1)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $x = $a[$k] = $v;",
                4,
                4,
                2,
                "0000 T4 = ASSIGN_DIM CV1($a) CV2($k)\n\
                 0001 OP_DATA CV3($v)\n\
                 0002 ASSIGN CV0($x) T4\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    4: 0001 - 0002 (tmp/var)\n",
            ),
            (
                b"<?php $a[1][2] = 3;",
                4,
                1,
                2,
                "0000 V1 = FETCH_DIM_W CV0($a) int(1)\n\
                 0001 ASSIGN_DIM V1 int(2)\n\
                 0002 OP_DATA int(3)\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $a[0] = $a;",
                4,
                1,
                2,
                "0000 T2 = QM_ASSIGN CV0($a)\n\
                 0001 ASSIGN_DIM CV0($a) int(0)\n\
                 0002 OP_DATA T2\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $a[] = &$a;",
                3,
                1,
                2,
                "0000 V1 = FETCH_DIM_W CV0($a) NEXT\n\
                 0001 ASSIGN_REF V1 CV0($a)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $a[0][] = &$b;",
                4,
                2,
                3,
                "0000 V2 = FETCH_DIM_W CV0($a) int(0)\n\
                 0001 V3 = FETCH_DIM_W V2 NEXT\n\
                 0002 ASSIGN_REF V3 CV1($b)\n\
                 0003 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_handle_numeric_dim`'s pair (`Z/Zend/zend_compile.c:2788`):
    /// a constant dimension whose string is a canonical decimal integer
    /// is the long in the operand with the string as written in the slot
    /// right after it, and the long's slot is that pair's alone,
    /// so `$o[10]` beside `$o["10"]` reads the long. A dimension written
    /// as an integer, and one that is no canonical integer, make no pair.
    #[test]
    fn a_constant_numeric_dimension_keeps_the_written_string_after_the_long() {
        use php_heap::Zval;
        use php_unit::ConstIdx;
        let (unit, _strings) =
            unit_and_strings(b"<?php $a[\"10\"] = 1; $a[10] = 2; $a[\"k\"] = 3;");
        let main = &unit.main;
        assert_eq!(
            main.numeric_dims.len(),
            1,
            "one pair, for the written \"10\""
        );
        let paired = main.numeric_dims[0];
        assert_eq!(
            main.dimension_written(paired)
                .and_then(Zval::as_str)
                .map(|s| s.bytes().to_vec()),
            Some(b"10".to_vec()),
            "the written form stands after the long"
        );
        let dims: Vec<Zval> = main
            .oplines
            .iter()
            .filter(|op| op.opcode == Opcode::AssignDim && op.op2_type == OperandKind::Const)
            .filter_map(|op| main.literals.get(op.op2 as usize).copied())
            .collect();
        assert_eq!(dims.len(), 3);
        assert_eq!(dims[0].as_long(), Some(10), "the written \"10\" folded");
        assert_eq!(dims[1].as_long(), Some(10), "10 as written");
        assert_eq!(
            main.dimension_written(ConstIdx(
                main.oplines
                    .iter()
                    .filter(|op| op.opcode == Opcode::AssignDim)
                    .nth(1)
                    .map_or(0, |op| op.op2)
            ))
            .and_then(Zval::as_long),
            Some(10),
            "a dimension written as an integer keeps no pair and reads as itself"
        );
        assert_eq!(
            dims[2].as_str().map(|s| s.bytes().to_vec()),
            Some(b"k".to_vec())
        );
    }

    /// php passes a `static` variable's initializer through
    /// `zend_eval_const_expr` before it compiles it, and the evaluator
    /// replaces a `??` whose left side is `null` with its right side, so
    /// `static $v = null ?? A[1] ?? 1;` compiles `A[1] ?? 1`: one `COALESCE`
    /// over the `FETCH_DIM_IS`. Outside an initializer nothing folds the
    /// `??`, and `$x = null ?? $a ?? 1;` keeps both. The dump is the oracle's
    /// under `[regime.dump]` at 8.5.9 (#2681).
    #[test]
    fn a_static_initializer_compiles_what_the_constant_evaluator_left_of_it() {
        assert_eq!(
            dump_of(b"<?php function f() { static $v = null ?? A[1] ?? 1; } $x = null ?? $a ?? 1;"),
            "\n$_main:\n\
             \x20    ; (lines=6, args=0, vars=2, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T2 = COALESCE null 0004\n\
             0001 T3 = COALESCE CV1($a) 0003\n\
             0002 T3 = QM_ASSIGN int(1)\n\
             0003 T2 = QM_ASSIGN T3\n\
             0004 ASSIGN CV0($x) T2\n\
             0005 RETURN int(1)\n\
             \nf:\n\
             \x20    ; (lines=7, args=0, vars=1, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 BIND_INIT_STATIC_OR_JMP CV0($v) 0006\n\
             0001 T1 = FETCH_CONSTANT string(\"A\")\n\
             0002 T2 = FETCH_DIM_IS T1 int(1)\n\
             0003 T3 = COALESCE T2 0005\n\
             0004 T3 = QM_ASSIGN int(1)\n\
             0005 BIND_STATIC (ref) CV0($v) T3\n\
             0006 RETURN null\n\
             "
        );
    }

    /// `zend_try_ct_eval_array` evaluates every key and value of an array
    /// before it knows whether the array folds, and what the evaluator left
    /// of each is what `zend_compile_array` compiles where it does not: a
    /// conditional over a constant is the branch it picks, a cast over a
    /// constant its value, a `??` over `null` its right side. The dump is the
    /// oracle's under `[regime.dump]` at 8.5.9 (#2681).
    #[test]
    fn an_array_element_compiles_what_the_constant_evaluator_left_of_it() {
        assert_eq!(
            dump_of(b"<?php $x = [true ? $a : 2, $k => (int) \"4\"]; $d = [&$e, 1 ? 2 : 3]; static $s = [null ?? $b];"),
            "\n$_main:\n\
             \x20    ; (lines=10, args=0, vars=7, tmps=5)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 T7 = INIT_ARRAY 2 (packed) CV1($a) NEXT\n\
             0001 T7 = ADD_ARRAY_ELEMENT int(4) CV2($k)\n\
             0002 ASSIGN CV0($x) T7\n\
             0003 T9 = INIT_ARRAY 2 (packed) (ref) CV4($e) NEXT\n\
             0004 T9 = ADD_ARRAY_ELEMENT int(2) NEXT\n\
             0005 ASSIGN CV3($d) T9\n\
             0006 BIND_INIT_STATIC_OR_JMP CV5($s) 0009\n\
             0007 T11 = INIT_ARRAY 1 (packed) CV6($b) NEXT\n\
             0008 BIND_STATIC (ref) CV5($s) T11\n\
             0009 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    7: 0001 - 0002 (tmp/var)\n\
             \x20    9: 0004 - 0005 (tmp/var)\n\
             "
        );
    }

    /// `determine_switch_jumptable_type` and `can_match_use_jumptable` read
    /// each case through `zend_eval_const_expr`, which folds a conditional
    /// over a constant, so five long cases the last of which is `true ? 5 :
    /// 6` are a `SWITCH_LONG` and the case compiles to `int(5)`, and folds no
    /// call, so a `match` over `1` and `strlen("ab")` is the chain of
    /// `IS_IDENTICAL` though the compile folds the call to `int(2)`. The dump
    /// is the oracle's under `[regime.dump]` at 8.5.9 (#2681).
    #[test]
    fn a_switch_and_a_match_read_their_cases_as_the_constant_evaluator_folds_them() {
        assert_eq!(
            dump_of(b"<?php switch ($x) { case 1: case 2: case 3: case 4: case true ? 5 : 6: echo 1; } echo match ($x) { 1 => 1, strlen(\"ab\") => 2, default => 3 };"),
            "\n$_main:\n\
             \x20    ; (lines=26, args=0, vars=1, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 SWITCH_LONG CV0($x) 1: 0012, 2: 0012, 3: 0012, 4: 0012, 5: 0012, default: 0013\n\
             0001 T1 = IS_EQUAL CV0($x) int(1)\n\
             0002 JMPNZ T1 0012\n\
             0003 T1 = IS_EQUAL CV0($x) int(2)\n\
             0004 JMPNZ T1 0012\n\
             0005 T1 = IS_EQUAL CV0($x) int(3)\n\
             0006 JMPNZ T1 0012\n\
             0007 T1 = IS_EQUAL CV0($x) int(4)\n\
             0008 JMPNZ T1 0012\n\
             0009 T1 = IS_EQUAL CV0($x) int(5)\n\
             0010 JMPNZ T1 0012\n\
             0011 JMP 0013\n\
             0012 ECHO int(1)\n\
             0013 T2 = IS_IDENTICAL CV0($x) int(1)\n\
             0014 JMPNZ T2 0018\n\
             0015 T2 = IS_IDENTICAL CV0($x) int(2)\n\
             0016 JMPNZ T2 0020\n\
             0017 JMP 0022\n\
             0018 T3 = QM_ASSIGN int(1)\n\
             0019 JMP 0024\n\
             0020 T3 = QM_ASSIGN int(2)\n\
             0021 JMP 0024\n\
             0022 T3 = QM_ASSIGN int(3)\n\
             0023 JMP 0024\n\
             0024 ECHO T3\n\
             0025 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    3: 0023 - 0024 (tmp/var)\n\
             "
        );
    }

    /// php's `dereferenceable_scalar` under a read (#482): a string
    /// written out, an array written out, a chain over one, a
    /// parenthesised expression and the `BP_VAR_IS` chain a `??` puts
    /// over one. Nothing is folded -- `"ab"[0]` is the `FETCH_DIM_R`
    /// over the constant string and never `string("a")` -- and the
    /// concatenation inside the parentheses is folded where it stands,
    /// so `("a" . "b")[0]` fetches over `string("ab")`. The dump is the
    /// oracle's under `[regime.dump]` at 8.5.9.
    #[test]
    fn a_dimension_behind_a_literal_is_the_oracle_s_oplines() {
        assert_eq!(
            dump_of(
                b"<?php $a = \"ab\"[0]; $b = [1, 2][0]; $c = [[1], 2][0][0]; \
                  $d = \"ab\"[0] ?? \"d\"; $e = (\"a\" . \"b\")[0];"
            ),
            main_dump(
                14,
                5,
                12,
                "0000 T5 = FETCH_DIM_R string(\"ab\") int(0)\n\
                 0001 ASSIGN CV0($a) T5\n\
                 0002 T7 = FETCH_DIM_R array(...) int(0)\n\
                 0003 ASSIGN CV1($b) T7\n\
                 0004 T9 = FETCH_DIM_R array(...) int(0)\n\
                 0005 T10 = FETCH_DIM_R T9 int(0)\n\
                 0006 ASSIGN CV2($c) T10\n\
                 0007 T12 = FETCH_DIM_IS string(\"ab\") int(0)\n\
                 0008 T13 = COALESCE T12 0010\n\
                 0009 T13 = QM_ASSIGN string(\"d\")\n\
                 0010 ASSIGN CV3($d) T13\n\
                 0011 T15 = FETCH_DIM_R string(\"ab\") int(0)\n\
                 0012 ASSIGN CV4($e) T15\n\
                 0013 RETURN int(1)\n"
            )
        );
    }

    /// What a dimension refuses: `[]` where a value is wanted, php's
    /// `Cannot use [] for reading`; under a write the bases php
    /// refuses outright, a temporary expression and a call answered in
    /// one. `$this` and a `static` property under a write, a compound
    /// assignment and a `++` (`FETCH_STATIC_PROP_RW`), a property and a
    /// call under a write, and every base php compiles where it stands
    /// under a read, compile since #451, #507 and #667.
    #[test]
    fn a_dimension_this_compiler_cannot_read_yet_is_refused() {
        for source in [
            b"<?php $a[] = $b[];".as_slice(),
            b"<?php echo $a[];".as_slice(),
            b"<?php \"ab\"[0] = 1;".as_slice(),
            b"<?php [1, 2][0] = 1;".as_slice(),
        ] {
            assert!(
                refused(source).is_some(),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `$a[k]` where a value is wanted as the oracle dumps it
    /// (`zend_compile_dim` under `BP_VAR_R`): one `FETCH_DIM_R` per link
    /// of the chain into a temporary, each reading the temporary the
    /// link under it wrote; a constant string dimension that is a
    /// canonical decimal integer is that integer
    /// (`zend_handle_numeric_dim`) and one that is not stands as written;
    /// a dimension that is itself a dimension is read before the chain
    /// over it; and a read as the value of a write is the value's oplines
    /// before the write's, the write's own temporary numbered first.
    #[test]
    fn a_dimension_read_compiles_to_fetch_dim_r_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php echo $a[0];",
                3,
                1,
                1,
                "0000 T1 = FETCH_DIM_R CV0($a) int(0)\n\
                 0001 ECHO T1\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php echo $a[\"10\"];",
                3,
                1,
                1,
                "0000 T1 = FETCH_DIM_R CV0($a) int(10)\n\
                 0001 ECHO T1\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php echo $a[\"k\"];",
                3,
                1,
                1,
                "0000 T1 = FETCH_DIM_R CV0($a) string(\"k\")\n\
                 0001 ECHO T1\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $x = $a[$k][1];",
                4,
                3,
                3,
                "0000 T3 = FETCH_DIM_R CV1($a) CV2($k)\n\
                 0001 T4 = FETCH_DIM_R T3 int(1)\n\
                 0002 ASSIGN CV0($x) T4\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php echo $a[$b[0]];",
                4,
                2,
                2,
                "0000 T2 = FETCH_DIM_R CV1($b) int(0)\n\
                 0001 T3 = FETCH_DIM_R CV0($a) T2\n\
                 0002 ECHO T3\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $a[0] = $b[1];",
                4,
                2,
                2,
                "0000 T3 = FETCH_DIM_R CV1($b) int(1)\n\
                 0001 ASSIGN_DIM CV0($a) int(0)\n\
                 0002 OP_DATA T3\n\
                 0003 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `$o->p = v` as the oracle dumps it (`zend_compile_assign`, the
    /// `ZEND_AST_PROP` arm): the value's oplines, then `ASSIGN_OBJ` over
    /// the receiver with the name as one constant and `OP_DATA` carrying
    /// the value; a chain is `FETCH_OBJ_W` into a `VAR` per link but the
    /// last, each `VAR` numbered before the value's temporaries; the
    /// temporary `ASSIGN_OBJ` writes is marked unused where nothing
    /// reads it and kept where an assignment does; `$this` under the chain is
    /// `FETCH_THIS` into a `VAR` where it stands, before the value.
    #[test]
    fn a_property_assignment_compiles_to_assign_obj_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $o->p = 1;",
                3,
                1,
                1,
                "0000 ASSIGN_OBJ CV0($o) string(\"p\")\n\
                 0001 OP_DATA int(1)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $x = $o->q->p = $y;",
                5,
                3,
                3,
                "0000 V3 = FETCH_OBJ_W CV1($o) string(\"q\")\n\
                 0001 T4 = ASSIGN_OBJ V3 string(\"p\")\n\
                 0002 OP_DATA CV2($y)\n\
                 0003 ASSIGN CV0($x) T4\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    4: 0002 - 0003 (tmp/var)\n",
            ),
            (
                b"<?php $this->p = 1;",
                4,
                0,
                2,
                "0000 V0 = FETCH_THIS\n\
                 0001 ASSIGN_OBJ V0 string(\"p\")\n\
                 0002 OP_DATA int(1)\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $o->q->r->p = 1;",
                5,
                1,
                3,
                "0000 V1 = FETCH_OBJ_W CV0($o) string(\"q\")\n\
                 0001 V2 = FETCH_OBJ_W V1 string(\"r\")\n\
                 0002 ASSIGN_OBJ V2 string(\"p\")\n\
                 0003 OP_DATA int(1)\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_separate_if_call_and_write` under a property write (#336),
    /// the oracle's dump under `[regime.dump]` at 8.5.9: a call under
    /// the chain is compiled where it stands and `SEPARATE` reads and
    /// writes the one `VAR` it returned into, before the fetches the
    /// chain delays, so the `VAR` the write lands in is the request's
    /// own. Every shape `zend_is_call` names is separated the same way:
    /// a call by name, a call through a value, a method call and a call
    /// on a class, each with the live range the dump prints over the
    /// `SEPARATE` and the fetch after it.
    #[test]
    fn a_call_under_a_property_write_is_the_oracle_s_separate() {
        assert_eq!(
            dump_of(
                b"<?php f()->p = 1; f()->a->b = 2; $o->m()->p = 3; $f()->p = 4; C::m()->p = 5;"
            ),
            main_dump(
                27,
                2,
                11,
                "0000 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0001 V2 = DO_FCALL_BY_NAME\n\
                 0002 V2 = SEPARATE V2\n\
                 0003 ASSIGN_OBJ V2 string(\"p\")\n\
                 0004 OP_DATA int(1)\n\
                 0005 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0006 V4 = DO_FCALL_BY_NAME\n\
                 0007 V4 = SEPARATE V4\n\
                 0008 V5 = FETCH_OBJ_W V4 string(\"a\")\n\
                 0009 ASSIGN_OBJ V5 string(\"b\")\n\
                 0010 OP_DATA int(2)\n\
                 0011 INIT_METHOD_CALL 0 CV0($o) string(\"m\")\n\
                 0012 V7 = DO_FCALL\n\
                 0013 V7 = SEPARATE V7\n\
                 0014 ASSIGN_OBJ V7 string(\"p\")\n\
                 0015 OP_DATA int(3)\n\
                 0016 INIT_DYNAMIC_CALL 0 CV1($f)\n\
                 0017 V9 = DO_FCALL\n\
                 0018 V9 = SEPARATE V9\n\
                 0019 ASSIGN_OBJ V9 string(\"p\")\n\
                 0020 OP_DATA int(4)\n\
                 0021 INIT_STATIC_METHOD_CALL 0 string(\"C\") string(\"m\")\n\
                 0022 V11 = DO_FCALL\n\
                 0023 V11 = SEPARATE V11\n\
                 0024 ASSIGN_OBJ V11 string(\"p\")\n\
                 0025 OP_DATA int(5)\n\
                 0026 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    2: 0002 - 0003 (tmp/var)\n\
                 \x20    4: 0007 - 0008 (tmp/var)\n\
                 \x20    7: 0013 - 0014 (tmp/var)\n\
                 \x20    9: 0018 - 0019 (tmp/var)\n\
                 \x20    11: 0023 - 0024 (tmp/var)\n"
            )
        );
    }

    /// `zend_get_compatible_func_or_null`: a public method of a class
    /// the engine declared is a callee the compiler knows, so its
    /// arguments are sent as `SEND_VAL` and `SEND_VAR`; a private one,
    /// a class the table has not and a class the file declares with no
    /// such method are sent for a callee nothing is known of. The
    /// class's name is matched whatever its case, as `CG(class_table)`
    /// is keyed by the lowered name.
    #[test]
    fn a_public_method_of_a_declared_class_is_a_known_callee() {
        use php_unit::decl::{
            ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl, Visibility,
        };
        let method = |name: &[u8], visibility| MethodDecl {
            function: FunctionDecl {
                name: Name(name.into()),
                parameters: Vec::new(),
                return_type: None,
                returns_by_ref: false,
                is_generator: false,
                attributes: Vec::new(),
                doc_comment: None,
                file: Name(Box::new([])),
                line: 0,
                end_line: 0,
                frameless: Vec::new(),
            },
            visibility,
            is_instance: false,
            is_abstract: false,
            is_final: false,
            tentative_return: false,
        };
        let options = Options {
            classes: indexed(vec![ClassDecl {
                name: Name(b"PhpToken".as_slice().into()),
                kind: ClassKind::Class,
                flags: ClassFlags::default(),
                parent: None,
                interfaces: Vec::new(),
                implementors: Vec::new(),
                aliases: Vec::new(),
                traits: Vec::new(),
                adaptations: Vec::new(),
                backing: EnumBacking::Pure,
                constants: Vec::new(),
                properties: Vec::new(),
                methods: vec![
                    method(b"tokenize", Visibility::Public),
                    method(b"hidden", Visibility::Private),
                ],
                attributes: Vec::new(),
                doc_comment: None,
                file: Name(Box::new([])),
                line: 0,
                end_line: 0,
                object_size: 0,
                object_beside: Box::default(),
                attribute_validator: None,
            }]),
            ..Options::default()
        };
        let dump = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &options, &mut strings)
                .unwrap_or_default()
                .dump(&[])
                .text()
        };
        assert_eq!(
            dump(b"<?php $a = 1; phptoken::TOKENIZE('<?php ', $a);"),
            main_dump(
                6,
                1,
                2,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 INIT_STATIC_METHOD_CALL 2 string(\"phptoken\") string(\"TOKENIZE\")\n\
                 0002 SEND_VAL string(\"<?php \") 1\n\
                 0003 SEND_VAR CV0($a) 2\n\
                 0004 DO_FCALL\n\
                 0005 RETURN int(1)\n"
            )
        );
        assert_eq!(
            dump(b"<?php PhpToken::hidden(1); Nope::m(1); class M {} M::m(1);"),
            main_dump(
                10,
                0,
                3,
                "0000 INIT_STATIC_METHOD_CALL 1 string(\"PhpToken\") string(\"hidden\")\n\
                 0001 SEND_VAL_EX int(1) 1\n\
                 0002 DO_FCALL\n\
                 0003 INIT_STATIC_METHOD_CALL 1 string(\"Nope\") string(\"m\")\n\
                 0004 SEND_VAL_EX int(1) 1\n\
                 0005 DO_FCALL\n\
                 0006 INIT_STATIC_METHOD_CALL 1 string(\"M\") string(\"m\")\n\
                 0007 SEND_VAL_EX int(1) 1\n\
                 0008 DO_FCALL\n\
                 0009 RETURN int(1)\n"
            )
        );
    }

    /// A member and a callee the program names, as the oracle's
    /// `[regime.dump]` has them: the name's CV in `op2` of `FETCH_OBJ_R`,
    /// `INIT_METHOD_CALL` and `INIT_STATIC_METHOD_CALL`, none of them
    /// with a cache slot but the class's one, and a callee that is no
    /// name, a parenthesised property among them, in `INIT_DYNAMIC_CALL`.
    #[test]
    fn a_member_and_a_callee_named_at_run_time_are_the_oracle_s_operands() {
        assert_eq!(
            dump_of(b"<?php echo $o->$p; $o->$m(); C::$m(); $f(); ($o->p)();"),
            main_dump(
                12,
                4,
                6,
                "0000 T4 = FETCH_OBJ_R CV0($o) CV1($p)\n\
                 0001 ECHO T4\n\
                 0002 INIT_METHOD_CALL 0 CV0($o) CV2($m)\n\
                 0003 DO_FCALL\n\
                 0004 INIT_STATIC_METHOD_CALL 0 string(\"C\") CV2($m)\n\
                 0005 DO_FCALL\n\
                 0006 INIT_DYNAMIC_CALL 0 CV3($f)\n\
                 0007 DO_FCALL\n\
                 0008 T8 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
                 0009 INIT_DYNAMIC_CALL 0 T8\n\
                 0010 DO_FCALL\n\
                 0011 RETURN int(1)\n"
            )
        );
    }

    /// What stands beside these four and is not read yet is refused as
    /// such.
    #[test]
    fn what_stands_beside_new_and_the_calls_is_refused_as_not_yet_read() {
        let refused = |source: &[u8]| {
            let mut strings = php_heap::Strings::new();
            compile_parsed(source, &bundled(), &mut strings)
                .err()
                .map(|e| e.message.into_owned())
        };
        assert_eq!(refused(b"<?php echo E::class;").as_deref(), None);
        assert_eq!(refused(b"<?php f($o->p, name: $o->q);").as_deref(), None);
    }

    /// A class reference of the scope and a class the program computes,
    /// which were refused as not read, as the oracle's `[regime.dump]`
    /// has them in the main op array: `self`, `parent` and `static` are
    /// their fetch type in an unused operand, the scope being the
    /// includer's and so no compile-time error, and an expression is
    /// `FETCH_CLASS` into a `VAR`, `instanceof`'s under
    /// `ZEND_FETCH_CLASS_NO_AUTOLOAD` and `_SILENT` beside
    /// `_EXCEPTION`.
    #[test]
    fn a_class_reference_of_the_scope_or_an_expression_compiles_as_php_emits_it() {
        assert_eq!(
            oplines_of(
                b"<?php echo self::$p; new $c; var_dump($o instanceof $c); echo $o::X, static::Y; $o::m(); parent::$q = 1; new static;"
            ),
            "0000 T2 = FETCH_STATIC_PROP_R string(\"p\") (self) (exception)\n\
             0001 ECHO T2\n\
             0002 V3 = FETCH_CLASS (exception) CV0($c)\n\
             0003 V4 = NEW 0 V3\n\
             0004 DO_FCALL\n\
             0005 FREE V4\n\
             0006 INIT_FCALL 1 96 string(\"var_dump\")\n\
             0007 V6 = FETCH_CLASS (no-autoload) (silent) (exception) CV0($c)\n\
             0008 T7 = INSTANCEOF CV1($o) V6\n\
             0009 SEND_VAL T7 1\n\
             0010 DO_ICALL\n\
             0011 V9 = FETCH_CLASS (exception) CV1($o)\n\
             0012 T10 = FETCH_CLASS_CONSTANT V9 string(\"X\")\n\
             0013 ECHO T10\n\
             0014 T11 = FETCH_CLASS_CONSTANT (static) (exception) string(\"Y\")\n\
             0015 ECHO T11\n\
             0016 V12 = FETCH_CLASS (exception) CV1($o)\n\
             0017 INIT_STATIC_METHOD_CALL 0 V12 string(\"m\")\n\
             0018 DO_FCALL\n\
             0019 ASSIGN_STATIC_PROP string(\"q\")\n\
             0020 OP_DATA int(1)\n\
             0021 V15 = NEW 0 (static) (exception)\n\
             0022 DO_FCALL\n\
             0023 FREE V15\n\
             0024 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0004 - 0005 (new)\n\
             \x20    15: 0022 - 0023 (new)\n"
        );
        let refused_message = |source: &[u8]| refused(source).map(|e| e.message.into_owned());
        assert_eq!(
            refused_message(b"<?php function f() { return self::X; }").as_deref(),
            Some("Cannot use \"self\" when no class scope is active")
        );
        assert_eq!(
            refused_message(b"<?php class A { function f() { new parent; } }").as_deref(),
            Some("Cannot use \"parent\" when current class scope has no parent")
        );
        assert_eq!(
            refused_message(b"<?php class A { const X = static::Y; }").as_deref(),
            Some("\"static::\" is not allowed in compile-time constants")
        );
    }

    /// #301's program under the oracle's `[regime.dump]`: `foreach` as
    /// `zend_compile_foreach` shapes it, the constant array folded by
    /// `zend_try_ct_eval_array`, `FE_RESET_R` into a `VAR` whose `op2`
    /// and the fetch's `extended_value` point past the loop, the `JMP`
    /// back to the fetch, `FE_FREE` at the end, and the `loop` live range
    /// from the fetch to the free.
    #[test]
    fn a_foreach_is_php_s_op_array_ending_in_its_loop_live_range() {
        let (unit, _strings) = unit_and_strings(b"<?php foreach ([1, 2] as $v) { echo $v; }");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                6,
                1,
                1,
                "0000 V1 = FE_RESET_R array(...) 0004\n\
                 0001 FE_FETCH_R V1 CV0($v) 0004\n\
                 0002 ECHO CV0($v)\n\
                 0003 JMP 0001\n\
                 0004 FE_FREE V1\n\
                 0005 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    1: 0001 - 0004 (loop)\n"
            )
        );
        let array = unit.main.literals[0]
            .as_arr()
            .map(|a| a.iter().collect::<Vec<_>>());
        assert_eq!(
            array,
            Some(vec![
                (php_heap::Key::Long(0), php_heap::Zval::long(1)),
                (php_heap::Key::Long(1), php_heap::Zval::long(2)),
            ]),
            "the literal is the array php folds"
        );
        assert_eq!(
            &*unit.main.safepoints,
            &[OplineIdx(0), OplineIdx(1), OplineIdx(5)],
            "the entry, the header the back-edge lands on, the return"
        );
        assert_eq!(
            unit.main.oplines[3].out, 1,
            "the back-edge carries the header's number"
        );
        assert_eq!(unit.main.oplines[5].out, 2, "the return's");
    }

    /// The same program with `return` inside the loop, the oracle's
    /// dump: `FE_FREE` flagged `ZEND_FREE_ON_RETURN` before the `RETURN`,
    /// its `op2` the distance to the loop's own `FE_FREE`, and the `loop`
    /// range split around the return (#255's split, read off a dump of
    /// the oracle's for the first time).
    #[test]
    fn an_early_return_in_a_foreach_splits_the_loop_range_around_its_fe_free() {
        let (unit, _strings) =
            unit_and_strings(b"<?php\nforeach ([1, 2] as $v) {\n    echo $v;\n    return;\n}\n");
        assert_eq!(
            oplines_of(b"<?php\nforeach ([1, 2] as $v) {\n    echo $v;\n    return;\n}\n"),
            "0000 V1 = FE_RESET_R array(...) 0006\n\
             0001 FE_FETCH_R V1 CV0($v) 0006\n\
             0002 ECHO CV0($v)\n\
             0003 FE_FREE V1 loop-end(+3)\n\
             0004 RETURN null\n\
             0005 JMP 0001\n\
             0006 FE_FREE V1\n\
             0007 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    1: 0001 - 0003 (loop)\n\
             \x20    1: 0005 - 0006 (loop)\n"
        );
        assert_eq!(
            unit.main.oplines[3].op2, 3,
            "the distance to the loop's end"
        );
        assert_eq!(
            unit.main.oplines[4].out, 2,
            "the early return is a site of its own"
        );
        assert_eq!(
            unit.main.oplines[5].out, 1,
            "the back-edge carries the header's number"
        );
        assert_eq!(unit.main.oplines[7].out, 3, "and the final return its own");
        let lines: Vec<u32> = unit.main.oplines[..7].iter().map(|op| op.lineno).collect();
        assert_eq!(
            lines,
            vec![2, 2, 3, 4, 4, 2, 2],
            "the JMP and the FE_FREE stand on the foreach's line"
        );
    }

    /// The rest of `zend_compile_foreach` against the oracle's dump: a
    /// key is the temporary the fetch writes, assigned to its CV; a
    /// `continue` in a `foreach` is a second backward `JMP` to the
    /// header carrying the header's number, and a `break` a forward one;
    /// `&$v` is `FE_RESET_RW` and `FE_FETCH_RW`, with `SEPARATE` after a
    /// call's `VAR`; `break 2` out of the inner loop and `continue 2`
    /// free the inner iterator on the way, and a `return` frees every
    /// open loop's, innermost first, a `switch` subject among them.
    #[test]
    fn the_key_the_jumps_and_the_reference_of_a_foreach_are_php_s() {
        let source = b"<?php\nforeach ([1, 2] as $k => $v) {\n    echo $v;\n    continue;\n}\nforeach (f($a) as &$v) { $v = 1; break; }\nforeach ($a as $v) { switch (strlen($v)) { case 1: foreach ($a as $w) { return $v; } } continue 1; }\n";
        assert_eq!(
            oplines_of(source),
            "0000 V4 = FE_RESET_R array(...) 0006\n\
             0001 T5 = FE_FETCH_R V4 CV0($v) 0006\n\
             0002 ASSIGN CV1($k) T5\n\
             0003 ECHO CV0($v)\n\
             0004 JMP 0001\n\
             0005 JMP 0001\n\
             0006 FE_FREE V4\n\
             0007 INIT_FCALL_BY_NAME 1 string(\"f\")\n\
             0008 SEND_VAR_EX CV2($a) 1\n\
             0009 V7 = DO_FCALL_BY_NAME\n\
             0010 V7 = SEPARATE V7\n\
             0011 V8 = FE_RESET_RW V7 0016\n\
             0012 FE_FETCH_RW V8 CV0($v) 0016\n\
             0013 ASSIGN CV0($v) int(1)\n\
             0014 JMP 0016\n\
             0015 JMP 0012\n\
             0016 FE_FREE V8\n\
             0017 V10 = FE_RESET_R CV2($a) 0034\n\
             0018 FE_FETCH_R V10 CV0($v) 0034\n\
             0019 T11 = STRLEN CV0($v)\n\
             0020 T12 = CASE T11 int(1)\n\
             0021 JMPNZ T12 0023\n\
             0022 JMP 0031\n\
             0023 V13 = FE_RESET_R CV2($a) 0030\n\
             0024 FE_FETCH_R V13 CV3($w) 0030\n\
             0025 FE_FREE V13 loop-end(+5)\n\
             0026 FREE T11 loop-end(+5)\n\
             0027 FE_FREE V10 loop-end(+7)\n\
             0028 RETURN CV0($v)\n\
             0029 JMP 0024\n\
             0030 FE_FREE V13\n\
             0031 FREE T11\n\
             0032 JMP 0018\n\
             0033 JMP 0018\n\
             0034 FE_FREE V10\n\
             0035 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0001 - 0006 (loop)\n\
             \x20    7: 0010 - 0011 (tmp/var)\n\
             \x20    8: 0012 - 0016 (loop)\n\
             \x20    10: 0018 - 0027 (loop)\n\
             \x20    11: 0020 - 0026 (tmp/var)\n\
             \x20    13: 0024 - 0025 (loop)\n\
             \x20    13: 0029 - 0030 (loop)\n\
             \x20    11: 0029 - 0031 (tmp/var)\n\
             \x20    10: 0029 - 0034 (loop)\n"
        );
        let (unit, _strings) = unit_and_strings(source);
        let jumps: Vec<(u32, u32)> = [4, 5, 15, 29, 32, 33]
            .iter()
            .map(|&at| (unit.main.oplines[at].op1, unit.main.oplines[at].out))
            .collect();
        assert_eq!(
            jumps,
            vec![(1, 1), (1, 1), (12, 2), (24, 4), (18, 3), (18, 3)],
            "two back-edges to one header carry one number, the header's"
        );
        assert_eq!(
            &*unit.main.safepoints,
            &[
                OplineIdx(0),
                OplineIdx(1),
                OplineIdx(12),
                OplineIdx(18),
                OplineIdx(24),
                OplineIdx(28),
                OplineIdx(35),
            ],
            "the entry, each foreach's header as its loop begins, the two returns"
        );
        assert_eq!(
            unit.main.oplines[14].out, 0,
            "a break is a forward jump and no site"
        );
    }

    /// `zend_try_ct_eval_array`'s keys: a decimal string is its integer,
    /// a whole double is truncated, `false` and `true` are 0 and 1, a
    /// repeated key keeps its first place with the last value, and the
    /// keyless element takes the next index past the largest so far, as
    /// `nNextFreeElement` has it since php 8.3 for a negative one too.
    #[test]
    fn a_constant_array_s_keys_are_the_ones_php_writes() {
        let (unit, _strings) = unit_and_strings(
            b"<?php echo ['a' => 1, '1' => 2, 1.0 => 3, true => 4, -5 => 6, 7, '08' => 8, '-0' => 9, false => 10, -7 => 11, 12];",
        );
        let array = unit.main.literals[0].as_arr().map(|a| {
            a.iter()
                .map(|(k, v)| {
                    let key = match k {
                        php_heap::Key::Long(n) => n.to_string(),
                        php_heap::Key::Str(s) => String::from_utf8_lossy(s.bytes()).into_owned(),
                    };
                    (key, v.as_long().unwrap_or(-1))
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            array,
            Some(vec![
                ("a".to_owned(), 1),
                ("1".to_owned(), 4),
                ("-5".to_owned(), 6),
                ("2".to_owned(), 7),
                ("08".to_owned(), 8),
                ("-0".to_owned(), 9),
                ("0".to_owned(), 10),
                ("-7".to_owned(), 11),
                ("3".to_owned(), 12),
            ])
        );
    }

    #[test]
    fn what_php_s_compiler_refuses_in_a_foreach_and_an_array_is_refused_in_its_words() {
        for (source, message) in [
            (
                &b"<?php\nforeach ($a as &$k => $v) {}\n"[..],
                "Key element cannot be a reference",
            ),
            (
                b"<?php\nforeach ($a as $this) {}\n",
                "Cannot re-assign $this",
            ),
            (
                b"<?php\nforeach ($a as $this => $v) {}\n",
                "Cannot re-assign $this",
            ),
            (
                b"<?php\nforeach ([1, 2] as $v) { break 2; }\n",
                "Cannot 'break' 2 levels",
            ),
            (
                b"<?php\nforeach (strlen($a) as &$v) {}\n",
                "Cannot use result of built-in function in write context",
            ),
            (b"<?php\necho [[] => 2];\n", "Illegal offset type"),
        ] {
            let error = refused(source).unwrap_or_else(|| CompileError {
                level: Level::Parse,
                message: String::from("compiled").into(),
                line: 0,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            });
            assert_eq!(error.level, Level::CompileError, "{message}");
            assert_eq!(error.message, message);
            assert_eq!(error.line, 2, "{message}");
        }
        // `[[] => 2]` under a variable is not php's compile error: the
        // first pass of `zend_try_ct_eval_array` finds the variable and
        // leaves the literal to the request, where it is a `TypeError`.
        assert_eq!(refused(b"<?php echo [[] => 2, $a];"), None);
    }

    /// `zend_compile_array` against the oracle's dump over the programs
    /// of #304: an array expression with a variable in it is `INIT_ARRAY`
    /// over the first element and `ADD_ARRAY_ELEMENT` over each after,
    /// the temporary numbered after the first element's own and shared
    /// by every opline, the count above `ZEND_ARRAY_SIZE_SHIFT`, and
    /// the live range running from the `INIT_ARRAY` to the opline that
    /// reads the temporary.
    #[test]
    fn an_array_with_a_variable_is_init_array_and_add_array_element_as_php_emits_them() {
        let (unit, _strings) = unit_and_strings(b"<?php $a = ['x', 'y']; $b = [1, $a, 'k' => 2];");
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                6,
                2,
                3,
                "0000 ASSIGN CV0($a) array(...)\n\
                 0001 T3 = INIT_ARRAY 3 int(1) NEXT\n\
                 0002 T3 = ADD_ARRAY_ELEMENT CV0($a) NEXT\n\
                 0003 T3 = ADD_ARRAY_ELEMENT int(2) string(\"k\")\n\
                 0004 ASSIGN CV1($b) T3\n\
                 0005 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0002 - 0004 (tmp/var)\n"
            )
        );
        assert_eq!(
            unit.main.oplines.get(1).map(|op| op.extended_value),
            Some((3 << 2) | 2),
            "three elements, and ZEND_ARRAY_NOT_PACKED for the string key"
        );
        assert_eq!(
            oplines_of(b"<?php var_dump(['x', 'y'], [1, $a = 2, 'k' => 3]);"),
            "0000 INIT_FCALL 2 112 string(\"var_dump\")\n\
             0001 SEND_VAL array(...) 1\n\
             0002 T1 = INIT_ARRAY 3 int(1) NEXT\n\
             0003 T2 = ASSIGN CV0($a) int(2)\n\
             0004 T1 = ADD_ARRAY_ELEMENT T2 NEXT\n\
             0005 T1 = ADD_ARRAY_ELEMENT int(3) string(\"k\")\n\
             0006 SEND_VAL T1 2\n\
             0007 DO_ICALL\n\
             0008 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    1: 0003 - 0006 (tmp/var)\n"
        );
    }

    /// `zend_compile_array`'s `...` and `&` arms and `zend_compile_args`'
    /// unpack arm against the oracle's dump: a `...` first is an
    /// `INIT_ARRAY` with no operand and no count, the string key still
    /// marks it `ZEND_ARRAY_NOT_PACKED`, a `&` element is the write fetch
    /// and `ZEND_ARRAY_ELEMENT_REF`, and an argument after an unpacked
    /// one is sent by name for a callee nothing is known of, under a
    /// `CHECK_UNDEF_ARGS`. A constant array spread into a constant array
    /// folds.
    #[test]
    fn a_spread_a_reference_element_and_an_unpacked_argument_are_emitted_as_php_emits_them() {
        assert_eq!(
            oplines_of(b"<?php $z = [...$x, 'k' => &$x[0], &$w]; f(1, ...$x, c: 2);"),
            "0000 T3 = INIT_ARRAY 0 NEXT\n\
             0001 T3 = ADD_ARRAY_UNPACK CV1($x)\n\
             0002 V4 = FETCH_DIM_W CV1($x) int(0)\n\
             0003 T3 = ADD_ARRAY_ELEMENT (ref) V4 string(\"k\")\n\
             0004 T3 = ADD_ARRAY_ELEMENT (ref) CV2($w) NEXT\n\
             0005 ASSIGN CV0($z) T3\n\
             0006 INIT_FCALL_BY_NAME 1 string(\"f\")\n\
             0007 SEND_VAL_EX int(1) 1\n\
             0008 SEND_UNPACK CV1($x)\n\
             0009 SEND_VAL_EX int(2) string(\"c\")\n\
             0010 CHECK_UNDEF_ARGS\n\
             0011 DO_FCALL_BY_NAME\n\
             0012 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    3: 0001 - 0005 (tmp/var)\n"
        );
        assert_eq!(
            oplines_of(b"<?php $q = [...[1, 2], 3, ...['k' => 4]];"),
            "0000 ASSIGN CV0($q) array(...)\n\
             0001 RETURN int(1)\n"
        );
        assert_eq!(
            refused_message(b"<?php\nf(...$a, 1);\n").as_deref(),
            Some("Cannot use positional argument after argument unpacking")
        );
    }

    /// `T_ARRAY '(' array_pair_list ')'`
    /// (`Z/Zend/zend_language_parser.y:1491`) is the same
    /// `ZEND_AST_ARRAY` over the same `array_pair_list` as `[…]`, with
    /// `ZEND_ARRAY_SYNTAX_LONG` in `attr`, which `zend_compile_array`
    /// reads only to refuse a `list()` where an array is meant: the
    /// long spelling emits what the short one above emits, opline for
    /// opline, and the word is matched whatever its case.
    #[test]
    fn the_long_array_spelling_emits_what_the_short_one_emits() {
        let short = oplines_of(b"<?php var_dump(['x', 'y'], [1, $a = 2, 'k' => 3]);");
        assert_eq!(
            oplines_of(b"<?php var_dump(array('x', 'y'), array(1, $a = 2, 'k' => 3));"),
            short
        );
        assert_eq!(
            oplines_of(b"<?php var_dump(ARRAY('x', 'y'), Array(1, $a = 2, 'k' => 3));"),
            short
        );
        assert_eq!(
            oplines_of(b"<?php $x = array(); $y = array(1, 'k' => 2);"),
            oplines_of(b"<?php $x = []; $y = [1, 'k' => 2];")
        );
    }

    /// The rest of `zend_compile_array` against the oracle's dump: what
    /// `zend_try_ct_eval_array` leaves to the request, a `null` key, a
    /// float key that is no whole number, and a next index past
    /// `ZEND_LONG_MAX`, is emitted with the key as written, since the
    /// handler warns over it; a constant string key that is a decimal is
    /// the long `zend_handle_numeric_op` makes of it, and one that is
    /// not sets `ZEND_ARRAY_NOT_PACKED` wherever it stands; a variable
    /// key is compiled before the value and sets nothing; and an array
    /// expression nothing reads is freed after its last element.
    #[test]
    fn the_keys_of_an_array_with_a_variable_are_emitted_as_php_emits_them() {
        let (unit, _strings) = unit_and_strings(
            b"<?php $k = '2'; $x = [1, $k, '3' => 4, 'a' => 5, 1.5 => 6, null => 7, true => 8]; $y = [$k => 1, 5 => 2, 'b' => 3]; [9223372036854775807 => 1, 2];",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            main_dump(
                17,
                3,
                6,
                "0000 ASSIGN CV0($k) string(\"2\")\n\
                 0001 T4 = INIT_ARRAY 7 int(1) NEXT\n\
                 0002 T4 = ADD_ARRAY_ELEMENT CV0($k) NEXT\n\
                 0003 T4 = ADD_ARRAY_ELEMENT int(4) int(3)\n\
                 0004 T4 = ADD_ARRAY_ELEMENT int(5) string(\"a\")\n\
                 0005 T4 = ADD_ARRAY_ELEMENT int(6) float(1.5)\n\
                 0006 T4 = ADD_ARRAY_ELEMENT int(7) null\n\
                 0007 T4 = ADD_ARRAY_ELEMENT int(8) bool(true)\n\
                 0008 ASSIGN CV1($x) T4\n\
                 0009 T6 = INIT_ARRAY 3 int(1) CV0($k)\n\
                 0010 T6 = ADD_ARRAY_ELEMENT int(2) int(5)\n\
                 0011 T6 = ADD_ARRAY_ELEMENT int(3) string(\"b\")\n\
                 0012 ASSIGN CV2($y) T6\n\
                 0013 T8 = INIT_ARRAY 2 (packed) int(1) int(9223372036854775807)\n\
                 0014 T8 = ADD_ARRAY_ELEMENT int(2) NEXT\n\
                 0015 FREE T8\n\
                 0016 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    4: 0002 - 0008 (tmp/var)\n\
                 \x20    6: 0010 - 0012 (tmp/var)\n\
                 \x20    8: 0014 - 0015 (tmp/var)\n"
            )
        );
        let flags: Vec<u32> = [1usize, 9, 13]
            .iter()
            .filter_map(|&at| unit.main.oplines.get(at))
            .map(|op| op.extended_value & 3)
            .collect();
        assert_eq!(
            flags,
            vec![2, 2, 0],
            "ZEND_ARRAY_NOT_PACKED on the two with a constant string key"
        );
    }
    /// #399's program under the oracle's `[regime.dump]`, whose dump of
    /// `try { throw new Exception("x"); } catch (ValueError $e) { echo 1; }
    /// catch (TypeError|Error $e) { echo 2; }` and
    /// `function f() { try { return 1; } finally { echo 2; } }` is:
    ///
    /// ```text
    /// 0000 V1 = NEW 1 string("Exception")
    /// 0001 SEND_VAL_EX string("x") 1
    /// 0002 DO_FCALL
    /// 0003 THROW V1
    /// 0004 JMP 0012
    /// 0005 CV0($e) = CATCH string("ValueError") 0008
    /// 0006 ECHO int(1)
    /// 0007 JMP 0012
    /// 0008 CV0($e) = CATCH string("TypeError") 0010
    /// 0009 JMP 0011
    /// 0010 CV0($e) = CATCH string("Error")
    /// 0011 ECHO int(2)
    /// 0012 RETURN int(1)
    /// LIVE RANGES:
    ///      1: 0001 - 0003 (new)
    /// EXCEPTION TABLE:
    ///      0000, 0005, -, -
    ///
    /// f:
    ///      ; (lines=7, args=0, vars=0, tmps=1)
    /// 0000 T0 = FAST_CALL 0004
    /// 0001 RETURN int(1)
    /// 0002 T0 = FAST_CALL 0004
    /// 0003 JMP 0006
    /// 0004 ECHO int(2)
    /// 0005 FAST_RET T0
    /// 0006 RETURN null
    /// EXCEPTION TABLE:
    ///      0000, -, 0004, 0005
    /// ```
    ///
    /// The clause with two classes is two `CATCH`es, the first jumping
    /// to the clause's body; the last `CATCH` of the `try` names no next
    /// and carries `ZEND_LAST_CATCH`. The `return` inside a `try` with a
    /// `finally` calls the block before the `RETURN`, so `f` carries two
    /// `FAST_CALL`s into one block.
    /// The oracle's dump under `[regime.dump]` of `exit`, `exit(0)`,
    /// `exit("msg")` and `die`: each is `INIT_FCALL` over `exit`, the
    /// function php 8.4 made of the keyword, with `die` given the same
    /// name by the parser and no opcode of its own anywhere.
    #[test]
    fn an_exit_or_a_die_is_php_s_init_fcall_over_exit() {
        assert_eq!(
            dump_of(b"<?php exit; exit(0); exit(\"msg\"); die;"),
            main_dump(
                11,
                0,
                4,
                "0000 INIT_FCALL 0 80 string(\"exit\")\n\
                 0001 DO_ICALL\n\
                 0002 INIT_FCALL 1 96 string(\"exit\")\n\
                 0003 SEND_VAL int(0) 1\n\
                 0004 DO_ICALL\n\
                 0005 INIT_FCALL 1 96 string(\"exit\")\n\
                 0006 SEND_VAL string(\"msg\") 1\n\
                 0007 DO_ICALL\n\
                 0008 INIT_FCALL 0 80 string(\"exit\")\n\
                 0009 DO_ICALL\n\
                 0010 RETURN int(1)\n"
            )
        );
    }

    #[test]
    fn a_try_is_phps_catch_chain_and_a_finally_is_its_fast_call_and_fast_ret() {
        let source = b"<?php\ntry { throw new Exception(\"x\"); } catch (ValueError $e) { echo 1; } catch (TypeError|Error $e) { echo 2; }\n";
        let (unit, _strings) = unit_and_strings(source);
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=13, args=0, vars=1, tmps=2)\n\
             \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
             0000 V1 = NEW 1 string(\"Exception\")\n\
             0001 SEND_VAL_EX string(\"x\") 1\n\
             0002 DO_FCALL\n\
             0003 THROW V1\n\
             0004 JMP 0012\n\
             0005 CV0($e) = CATCH string(\"ValueError\") 0008\n\
             0006 ECHO int(1)\n\
             0007 JMP 0012\n\
             0008 CV0($e) = CATCH string(\"TypeError\") 0010\n\
             0009 JMP 0011\n\
             0010 CV0($e) = CATCH string(\"Error\")\n\
             0011 ECHO int(2)\n\
             0012 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    1: 0001 - 0003 (new)\n\
             EXCEPTION TABLE:\n\
             \x20    0000, 0005, -, -\n"
        );
        let last = unit.main.oplines[10];
        assert_eq!(
            last.extended_value & 1,
            1,
            "ZEND_LAST_CATCH on the last CATCH of the try"
        );
        assert_eq!(
            unit.main.oplines[5].extended_value & 1,
            0,
            "and on no earlier one"
        );
        assert_eq!(
            unit.main.try_catch[0].try_op.0, 0,
            "the element opens at the body's first opline"
        );
        assert_eq!(unit.main.try_catch[0].catch_op.0, 5);
    }

    #[test]
    fn a_return_inside_a_try_with_a_finally_calls_the_block_before_the_return() {
        let (unit, _strings) =
            unit_and_strings(b"<?php\nfunction f() { try { return 1; } finally { echo 2; } }\n");
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \nf:\n     ; (lines=7, args=0, vars=0, tmps=1)\n\
             \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
             0000 T0 = FAST_CALL 0004\n\
             0001 RETURN int(1)\n\
             0002 T0 = FAST_CALL 0004\n\
             0003 JMP 0006\n\
             0004 ECHO int(2)\n\
             0005 FAST_RET T0\n\
             0006 RETURN null\n\
             EXCEPTION TABLE:\n\
             \x20    0000, -, 0004, 0005\n"
        );
    }

    /// #421's programs under the oracle's `[regime.dump]`: a `break`
    /// out of a `try` with a `finally` is the block's `FAST_CALL` before
    /// the `JMP`, and a `break 2` out of two is both blocks' `FAST_CALL`s
    /// innermost first, then the inner `foreach`'s `FE_FREE` flagged
    /// `ZEND_FREE_ON_RETURN`, then the `JMP` to the outer loop's end,
    /// the walk of `zend_handle_loops_and_finally_ex` to a depth of two.
    #[test]
    fn a_break_out_of_a_try_with_a_finally_calls_the_block_before_the_jump() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\nforeach ([1,2] as $v) { try { break; } finally { echo \"f\"; } }\n",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=11, args=0, vars=1, tmps=2)\n\
             \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
             0000 V1 = FE_RESET_R array(...) 0009\n\
             0001 FE_FETCH_R V1 CV0($v) 0009\n\
             0002 T2 = FAST_CALL 0006\n\
             0003 JMP 0009\n\
             0004 T2 = FAST_CALL 0006\n\
             0005 JMP 0008\n\
             0006 ECHO string(\"f\")\n\
             0007 FAST_RET T2\n\
             0008 JMP 0001\n\
             0009 FE_FREE V1\n\
             0010 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    1: 0001 - 0009 (loop)\n\
             EXCEPTION TABLE:\n\
             \x20    0002, -, 0006, 0007\n"
        );
        let (unit, _strings) = unit_and_strings(
            b"<?php\nforeach ([1,2] as $v) { foreach ([3] as $w) { try { try { break 2; } finally { echo \"i\"; } } finally { echo \"o\"; } } }\n",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=21, args=0, vars=2, tmps=4)\n\
             \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
             0000 V2 = FE_RESET_R array(...) 0019\n\
             0001 FE_FETCH_R V2 CV0($v) 0019\n\
             0002 V3 = FE_RESET_R array(...) 0017\n\
             0003 FE_FETCH_R V3 CV1($w) 0017\n\
             0004 T5 = FAST_CALL 0010\n\
             0005 T4 = FAST_CALL 0014\n\
             0006 FE_FREE V3 loop-end(+11)\n\
             0007 JMP 0019\n\
             0008 T5 = FAST_CALL 0010\n\
             0009 JMP 0012\n\
             0010 ECHO string(\"i\")\n\
             0011 FAST_RET T5 try-catch(0)\n\
             0012 T4 = FAST_CALL 0014\n\
             0013 JMP 0016\n\
             0014 ECHO string(\"o\")\n\
             0015 FAST_RET T4\n\
             0016 JMP 0003\n\
             0017 FE_FREE V3\n\
             0018 JMP 0001\n\
             0019 FE_FREE V2\n\
             0020 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    2: 0001 - 0019 (loop)\n\
             \x20    3: 0003 - 0006 (loop)\n\
             \x20    3: 0008 - 0017 (loop)\n\
             EXCEPTION TABLE:\n\
             \x20    0004, -, 0014, 0015\n\
             \x20    0004, -, 0010, 0011\n"
        );
    }

    /// `function &f(array &$a): array { try { return $a; } finally { echo "f"; } }`
    /// under the oracle's `[regime.dump]`: the CV made a reference by
    /// `MAKE_REF` into a `VAR`, the return type checked, the block's
    /// `FAST_CALL` carrying the `VAR`, and the type checked again after
    /// it, since the block may have written the cell.
    #[test]
    fn a_by_reference_return_past_a_finally_is_a_make_ref_and_a_second_type_check() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\nfunction &f(array &$a): array { try { return $a; } finally { echo \"f\"; } }\n",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \nf:\n     ; (lines=12, args=1, vars=1, tmps=2)\n\
             \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
             0000 CV0($a) = RECV 1\n\
             0001 V2 = MAKE_REF CV0($a)\n\
             0002 VERIFY_RETURN_TYPE V2\n\
             0003 T1 = FAST_CALL 0008 V2\n\
             0004 VERIFY_RETURN_TYPE V2\n\
             0005 RETURN_BY_REF V2\n\
             0006 T1 = FAST_CALL 0008\n\
             0007 JMP 0010\n\
             0008 ECHO string(\"f\")\n\
             0009 FAST_RET T1\n\
             0010 VERIFY_RETURN_TYPE\n\
             0011 RETURN_BY_REF (function) null\n\
             LIVE RANGES:\n\
             \x20    2: 0002 - 0005 (tmp/var)\n\
             EXCEPTION TABLE:\n\
             \x20    0001, -, 0008, 0009\n"
        );
    }

    #[test]
    fn a_jump_out_of_a_finally_block_is_phps_compile_error() {
        let out = refused(b"<?php\nforeach ([1] as $v) { try { echo 1; } finally {\nbreak; } }\n");
        assert_eq!(
            out.map(|e| (e.message.into_owned(), e.line)),
            Some((String::from("jump out of a finally block is disallowed"), 3))
        );
        let inside = refused(
            b"<?php\nforeach ([1] as $v) { try { echo 1; } finally { while (true) { break; } } }\n",
        );
        assert!(
            inside.is_none(),
            "a loop inside the block is left by nothing"
        );
    }

    /// `function f() { try { try { goto x; } finally { echo "a"; } x: echo "b"; } finally { echo "c"; } }`
    /// under the oracle's `[regime.dump]`: the `goto` emitted both
    /// blocks' `FAST_CALL`s, and `zend_resolve_goto_label` made a `NOP`
    /// of the outer one, whose `try` holds the label too.
    #[test]
    fn a_goto_that_stays_inside_a_try_has_that_finally_call_made_a_nop() {
        let (unit, _strings) = unit_and_strings(
            b"<?php\nfunction f() { try { try { goto x; } finally { echo \"a\"; } x: echo \"b\"; } finally { echo \"c\"; } }\n",
        );
        assert_eq!(
            unit.dump(&[]).text(),
            "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
             0000 RETURN int(1)\n\
             \nf:\n     ; (lines=13, args=0, vars=0, tmps=2)\n\
             \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
             0000 T1 = FAST_CALL 0005\n\
             0001 NOP\n\
             0002 JMP 0007\n\
             0003 T1 = FAST_CALL 0005\n\
             0004 JMP 0007\n\
             0005 ECHO string(\"a\")\n\
             0006 FAST_RET T1 try-catch(0)\n\
             0007 ECHO string(\"b\")\n\
             0008 T0 = FAST_CALL 0010\n\
             0009 JMP 0012\n\
             0010 ECHO string(\"c\")\n\
             0011 FAST_RET T0\n\
             0012 RETURN null\n\
             EXCEPTION TABLE:\n\
             \x20    0000, -, 0010, 0011\n\
             \x20    0000, -, 0005, 0006\n"
        );
    }

    #[test]
    fn a_goto_php_refuses_is_phps_compile_error() {
        let undefined = refused(b"<?php\ngoto missing;\n");
        assert_eq!(
            undefined.map(|e| (e.message.into_owned(), e.line)),
            Some((String::from("'goto' to undefined label 'missing'"), 2))
        );
        let into = refused(b"<?php\ngoto inside;\nwhile (true) { inside: break; }\n");
        assert_eq!(
            into.map(|e| (e.message.into_owned(), e.line)),
            Some((
                String::from("'goto' into loop or switch statement is disallowed"),
                2
            ))
        );
        let twice = refused(b"<?php\na:\na:\n");
        assert_eq!(
            twice.map(|e| (e.message.into_owned(), e.line)),
            Some((String::from("Label 'a' already defined"), 3))
        );
        let out = refused(b"<?php\ntry { echo 1; } finally { goto out; }\nout: echo 2;\n");
        assert_eq!(
            out.map(|e| e.message.into_owned()),
            Some(String::from("jump out of a finally block is disallowed"))
        );
        let inside = refused(b"<?php\ngoto inside;\ntry { echo 1; } finally { inside: echo 2; }\n");
        assert_eq!(
            inside.map(|e| e.message.into_owned()),
            Some(String::from("jump into a finally block is disallowed"))
        );
    }

    /// [`Options::assertions`] off, php's `zend.assertions = -1`: an
    /// `assert()` is the constant `true` and nothing of its arguments
    /// compiles (`zend_compile_assert`), so `f()` is never called. The
    /// entry moves only at startup, so no witness runs php under it; the
    /// dump is the oracle's under `-d zend.assertions=-1` and the dump
    /// regime, and its behaviour `Zend/tests/assert/expect_017.phpt` and
    /// `expect_019.phpt`, whose `--INI--` sets it. The bare name in a
    /// namespace is `zend_compile_assert`'s whatever the table holds.
    #[test]
    fn an_assertion_under_assertions_off_is_the_constant_true() {
        let mut strings = php_heap::Strings::new();
        let options = Options {
            assertions: false,
            ..bundled()
        };
        let dump = compile_source(
            &mut Arena::new(),
            b"<?php namespace A; $a = assert(f());",
            &options,
            &mut strings,
        )
        .unwrap_or_default()
        .dump(&[])
        .text();
        assert_eq!(
            dump,
            main_dump(
                2,
                1,
                1,
                "0000 ASSIGN CV0($a) bool(true)\n0001 RETURN int(1)\n"
            )
        );
    }

    #[test]
    fn a_try_with_neither_a_catch_nor_a_finally_is_phps_compile_error() {
        let refused = refused(b"<?php\ntry { echo 1; }\n");
        assert_eq!(
            refused.map(|e| e.message.into_owned()),
            Some(String::from("Cannot use try without catch or finally"))
        );
    }

    /// `zend_compile_list_assign`'s oplines over the four shapes #583
    /// names, as the oracle's `[regime.dump]` prints them: a plain list
    /// reads the right side's copy once per element with `FETCH_LIST_R`
    /// and frees it after the last, a keyed list reads by the key, a
    /// nested list reads the element and then its elements, and an
    /// element taken by reference is `FETCH_LIST_W` and `MAKE_REF` over
    /// the right side made a reference; a `foreach` value that is a list
    /// is fetched into a `VAR` and read the same way.
    #[test]
    fn a_list_compiles_to_what_zend_compile_list_assign_emits() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php [$a, $b] = $x;",
                7,
                3,
                5,
                "0000 T3 = QM_ASSIGN CV0($x)\n\
                 0001 V4 = FETCH_LIST_R T3 int(0)\n\
                 0002 ASSIGN CV1($a) V4\n\
                 0003 V6 = FETCH_LIST_R T3 int(1)\n\
                 0004 ASSIGN CV2($b) V6\n\
                 0005 FREE T3\n\
                 0006 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0001 - 0005 (tmp/var)\n",
            ),
            (
                b"<?php list('k' => $a) = $x;",
                5,
                2,
                3,
                "0000 T2 = QM_ASSIGN CV0($x)\n\
                 0001 V3 = FETCH_LIST_R T2 string(\"k\")\n\
                 0002 ASSIGN CV1($a) V3\n\
                 0003 FREE T2\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    2: 0001 - 0003 (tmp/var)\n",
            ),
            (
                b"<?php [$a, [$b, $c]] = $x;",
                11,
                4,
                8,
                "0000 T4 = QM_ASSIGN CV0($x)\n\
                 0001 V5 = FETCH_LIST_R T4 int(0)\n\
                 0002 ASSIGN CV1($a) V5\n\
                 0003 V7 = FETCH_LIST_R T4 int(1)\n\
                 0004 V8 = FETCH_LIST_R V7 int(0)\n\
                 0005 ASSIGN CV2($b) V8\n\
                 0006 V10 = FETCH_LIST_R V7 int(1)\n\
                 0007 ASSIGN CV3($c) V10\n\
                 0008 FREE V7\n\
                 0009 FREE T4\n\
                 0010 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    4: 0001 - 0009 (tmp/var)\n\
                 \x20    7: 0004 - 0008 (tmp/var)\n",
            ),
            (
                b"<?php [&$a] = $x;",
                6,
                2,
                4,
                "0000 V2 = MAKE_REF CV0($x)\n\
                 0001 V3 = FETCH_LIST_W V2 int(0)\n\
                 0002 V4 = MAKE_REF V3\n\
                 0003 ASSIGN_REF CV1($a) V4\n\
                 0004 FREE V2\n\
                 0005 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    2: 0001 - 0004 (tmp/var)\n",
            ),
            (
                b"<?php foreach ($xs as $k => [$a, $b]) {}",
                11,
                4,
                8,
                "0000 V4 = FE_RESET_R CV0($xs) 0009\n\
                 0001 T10 = FE_FETCH_R V4 V5 0009\n\
                 0002 V6 = FETCH_LIST_R V5 int(0)\n\
                 0003 ASSIGN CV1($a) V6\n\
                 0004 V8 = FETCH_LIST_R V5 int(1)\n\
                 0005 ASSIGN CV2($b) V8\n\
                 0006 FREE V5\n\
                 0007 ASSIGN CV3($k) T10\n\
                 0008 JMP 0001\n\
                 0009 FE_FREE V4\n\
                 0010 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    4: 0001 - 0009 (loop)\n\
                 \x20    5: 0002 - 0006 (tmp/var)\n\
                 \x20    10: 0002 - 0007 (tmp/var)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// php's compile errors over a list's shape, each in php's words.
    #[test]
    fn a_list_of_the_wrong_shape_is_phps_compile_error() {
        let cases: &[(&[u8], &str)] = &[
            (b"<?php [] = $x;", "Cannot use empty list"),
            (
                b"<?php [$a, 'k' => $b] = $x;",
                "Cannot mix keyed and unkeyed array entries in assignments",
            ),
            (
                b"<?php $x = [list($a)];",
                "Cannot use list() as standalone expression",
            ),
            (
                b"<?php [...$a] = $x;",
                "Spread operator is not supported in assignments",
            ),
            (
                b"<?php [array($a)] = $x;",
                "Cannot assign to array(), use [] instead",
            ),
            (b"<?php [list($a)] = $x;", "Cannot mix [] and list()"),
            (
                b"<?php ['k' => $a, , ] = $x;",
                "Cannot use empty array entries in keyed array assignment",
            ),
            (
                b"<?php [f()] = $x;",
                "Can't use function return value in write context",
            ),
            (
                b"<?php [1] = $x;",
                "Assignments can only happen to writable values",
            ),
            (
                b"<?php [&$a] = 1;",
                "Cannot assign reference to non referenceable value",
            ),
            (
                b"<?php foreach ($x as [$a] => $b) {}",
                "Cannot use list as key element",
            ),
        ];
        for (source, message) in cases {
            assert_eq!(
                refused(source).map(|e| e.message.into_owned()),
                Some(String::from(*message)),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// #531's program under the oracle's `[regime.dump]`: a `JMP_NULL`
    /// over the object before each `?->`, its target the opline after
    /// the chain and its `result` the chain's own, a call's arguments
    /// inside the jump, and `"$a?->b"` a chain of its own inside the
    /// string.
    #[test]
    fn a_nullsafe_chain_is_jmp_null_over_each_link_as_php_emits_it() {
        assert_eq!(
            dump_of(b"<?php $a = null; echo $a?->b, $a?->m(1)?->c, \"$a?->b\";"),
            main_dump(
                16,
                1,
                6,
                "0000 ASSIGN CV0($a) null\n\
                 0001 T2 = JMP_NULL CV0($a) 0003\n\
                 0002 T2 = FETCH_OBJ_R CV0($a) string(\"b\")\n\
                 0003 ECHO T2\n\
                 0004 T4 = JMP_NULL CV0($a) 0010\n\
                 0005 INIT_METHOD_CALL 1 CV0($a) string(\"m\")\n\
                 0006 SEND_VAL_EX int(1) 1\n\
                 0007 V3 = DO_FCALL\n\
                 0008 T4 = JMP_NULL V3 0010\n\
                 0009 T4 = FETCH_OBJ_R V3 string(\"c\")\n\
                 0010 ECHO T4\n\
                 0011 T5 = JMP_NULL CV0($a) 0013\n\
                 0012 T5 = FETCH_OBJ_R CV0($a) string(\"b\")\n\
                 0013 T6 = CAST (string) T5\n\
                 0014 ECHO T6\n\
                 0015 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    3: 0008 - 0009 (tmp/var)\n",
            )
        );
    }

    /// php's compile errors where a `?->` chain is written to or bound.
    #[test]
    fn a_nullsafe_chain_written_to_is_phps_compile_error() {
        let cases: &[(&[u8], &str)] = &[
            (
                b"<?php $a?->b = 1;",
                "Can't use nullsafe operator in write context",
            ),
            (
                b"<?php $a?->b++;",
                "Can't use nullsafe operator in write context",
            ),
            (
                b"<?php $a?->b .= 1;",
                "Can't use nullsafe operator in write context",
            ),
            (
                b"<?php unset($a?->b);",
                "Can't use nullsafe operator in write context",
            ),
            (
                b"<?php $a?->b[0] = 1;",
                "Can't use nullsafe operator in write context",
            ),
            (
                b"<?php foreach ($x as $a?->b) {}",
                "Can't use nullsafe operator in write context",
            ),
            (
                b"<?php $x = &$a?->b;",
                "Cannot take reference of a nullsafe chain",
            ),
            (
                b"<?php function &f() { return $a?->b[0]; }",
                "Cannot take reference of a nullsafe chain",
            ),
            (
                b"<?php [$a?->b] = $x;",
                "Assignments can only happen to writable values",
            ),
            (
                b"<?php $a?->m()++;",
                "Can't use method return value in write context",
            ),
        ];
        for (source, message) in cases {
            assert_eq!(
                refused(source).map(|e| e.message.into_owned()),
                Some(String::from(*message)),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_try_compile_cv` takes a CV only for a name php's tree holds as
    /// `ZEND_AST_ZVAL`: `${1}` and `${'1'}` are the CV `$1`, while `${true}`
    /// names it by `ZEND_AST_CONST`, which `zend_compile_simple_var_no_cv`
    /// compiles, converts to a string and fetches by name, `FETCH_W (local)`
    /// under the write and `FETCH_R (local)` under the read. The dump is the
    /// oracle's under `[regime.dump]` at 8.5.9 (#2686).
    #[test]
    fn a_name_written_as_a_constant_other_than_a_string_is_fetched_by_name_not_a_cv() {
        assert_eq!(
            dump_of(b"<?php ${true} = 1; ${1}; ${'1'}; echo ${true}, ${1};"),
            "\n$_main:\n\
             \x20    ; (lines=6, args=0, vars=1, tmps=3)\n\
             \x20    ; (before optimizer)\n\
             \x20    ; :1-1\n\
             \x20    ; return  [] RANGE[0..0]\n\
             0000 V1 = FETCH_W (local) string(\"1\")\n\
             0001 ASSIGN V1 int(1)\n\
             0002 T3 = FETCH_R (local) string(\"1\")\n\
             0003 ECHO T3\n\
             0004 ECHO CV0($1)\n\
             0005 RETURN int(1)\n\
             "
        );
    }

    /// #690's shapes under the oracle's `[regime.dump]`: a `global` over
    /// a name no CV has is `FETCH_W (global+lock)` and an `ASSIGN_REF`
    /// over a fetch by the same name, the local variable's or, for an
    /// auto-global, the global's; `$$x++` is `FETCH_RW (local)` and the
    /// step over the `VAR`.
    #[test]
    fn a_variable_named_at_run_time_is_fetched_by_name_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php global ${$x[0]};",
                5,
                1,
                4,
                "0000 T1 = FETCH_DIM_R CV0($x) int(0)\n\
                 0001 V2 = FETCH_W (global+lock) T1\n\
                 0002 V3 = FETCH_W (local) T1\n\
                 0003 ASSIGN_REF V3 V2\n\
                 0004 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    1: 0001 - 0002 (tmp/var)\n\
                 \x20    2: 0002 - 0003 (tmp/var)\n",
            ),
            (
                b"<?php global $_SERVER;",
                4,
                0,
                3,
                "0000 V0 = FETCH_W (global+lock) string(\"_SERVER\")\n\
                 0001 V1 = FETCH_W (global) string(\"_SERVER\")\n\
                 0002 ASSIGN_REF V1 V0\n\
                 0003 RETURN int(1)\n\
                 LIVE RANGES:\n\
                 \x20    0: 0001 - 0002 (tmp/var)\n",
            ),
            (
                b"<?php $$x++;",
                3,
                1,
                2,
                "0000 V1 = FETCH_RW (local) CV0($x)\n\
                 0001 PRE_INC V1\n\
                 0002 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// #633's shapes under the oracle's `[regime.dump]`: an auto-global
    /// under an element's write is `FETCH_W (global)`, under a compound
    /// assignment `FETCH_RW (global)`, under an `unset` of its element
    /// `FETCH_UNSET (global)`; `isset` and `empty` over a variable fetched
    /// by name are `ISSET_ISEMPTY_VAR`, and `unset` over one `UNSET_VAR`.
    #[test]
    fn an_auto_global_is_fetched_by_name_from_the_global_table_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $_SERVER[\"x\"] = 1;",
                4,
                0,
                2,
                "0000 V0 = FETCH_W (global) string(\"_SERVER\")\n\
                 0001 ASSIGN_DIM V0 string(\"x\")\n\
                 0002 OP_DATA int(1)\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php $_SERVER[\"n\"] .= \"a\";",
                4,
                0,
                2,
                "0000 V0 = FETCH_RW (global) string(\"_SERVER\")\n\
                 0001 ASSIGN_DIM_OP (CONCAT) V0 string(\"n\")\n\
                 0002 OP_DATA string(\"a\")\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php unset($_SERVER[\"x\"]);",
                3,
                0,
                1,
                "0000 V0 = FETCH_UNSET (global) string(\"_SERVER\")\n\
                 0001 UNSET_DIM V0 string(\"x\")\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $a = isset($_SERVER) && empty($$a);",
                6,
                1,
                3,
                "0000 T1 = ISSET_ISEMPTY_VAR (global) (isset) string(\"_SERVER\")\n\
                 0001 T1 = JMPZ_EX T1 0004\n\
                 0002 T2 = ISSET_ISEMPTY_VAR (local) (empty) CV0($a)\n\
                 0003 T1 = BOOL T2\n\
                 0004 ASSIGN CV0($a) T1\n\
                 0005 RETURN int(1)\n",
            ),
            (
                b"<?php unset($$x, $_ENV);",
                3,
                1,
                0,
                "0000 UNSET_VAR (local) CV0($x)\n\
                 0001 UNSET_VAR (global) string(\"_ENV\")\n\
                 0002 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `$GLOBALS` under the oracle's `[regime.dump]`: read whole it is
    /// `FETCH_GLOBALS` into a temporary; `$GLOBALS['a']` is the fetch by
    /// name over `'a'` from the global table wherever it stands, the
    /// variable of an `ASSIGN`, the base of a dimension's compound
    /// assignment, `UNSET_VAR` and `ISSET_ISEMPTY_VAR`; `isset($GLOBALS)`
    /// is `true` before the request runs.
    #[test]
    fn globals_is_a_copy_of_the_table_and_its_element_a_fetch_by_name_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $x = $GLOBALS;",
                3,
                1,
                2,
                "0000 T1 = FETCH_GLOBALS\n\
                 0001 ASSIGN CV0($x) T1\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $GLOBALS[\"a\"] = 1;",
                3,
                0,
                2,
                "0000 V0 = FETCH_W (global) string(\"a\")\n\
                 0001 ASSIGN V0 int(1)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $GLOBALS[\"a\"][\"b\"] .= \"c\";",
                4,
                0,
                2,
                "0000 V0 = FETCH_RW (global) string(\"a\")\n\
                 0001 ASSIGN_DIM_OP (CONCAT) V0 string(\"b\")\n\
                 0002 OP_DATA string(\"c\")\n\
                 0003 RETURN int(1)\n",
            ),
            (
                b"<?php unset($GLOBALS[\"a\"]);",
                2,
                0,
                0,
                "0000 UNSET_VAR (global) string(\"a\")\n\
                 0001 RETURN int(1)\n",
            ),
            (
                b"<?php $b = isset($GLOBALS[\"a\"]) || isset($GLOBALS);",
                5,
                1,
                2,
                "0000 T1 = ISSET_ISEMPTY_VAR (global) (isset) string(\"a\")\n\
                 0001 T1 = JMPNZ_EX T1 0003\n\
                 0002 T1 = BOOL bool(true)\n\
                 0003 ASSIGN CV0($b) T1\n\
                 0004 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// `zend_is_auto_global` arms what the compiler meets, in the order
    /// it meets it, once, whether or not the line runs: the unit carries
    /// that order ([`Unit::arms`]), which is the order php lays the
    /// variables made just in time into `EG(symbol_table)`.
    #[test]
    fn the_unit_arms_each_auto_global_it_names_in_the_order_it_met_them() {
        let (unit, _strings) = unit_and_strings(
            b"<?php if (0) { $_REQUEST; } $_ENV['k'] ?? 1; \"$_REQUEST[x]\"; $n = '_GET'; $$n;",
        );
        assert_eq!(unit.arms, [AutoGlobal::Request, AutoGlobal::Env]);
    }

    /// #444's shapes under the oracle's `[regime.dump]`: a property as
    /// the target of `= &` is `ASSIGN_OBJ_REF` over its object with the
    /// source in an `OP_DATA`, `(function)` and a `MAKE_REF` over a call.
    #[test]
    fn a_property_bound_by_reference_is_assign_obj_ref_as_php_emits_it() {
        let cases: &[(&[u8], usize, u32, u32, &str)] = &[
            (
                b"<?php $o->p = &$x;",
                3,
                2,
                1,
                "0000 ASSIGN_OBJ_REF CV0($o) string(\"p\")\n\
                 0001 OP_DATA CV1($x)\n\
                 0002 RETURN int(1)\n",
            ),
            (
                b"<?php $o->p = &f();",
                6,
                1,
                3,
                "0000 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
                 0001 V2 = DO_FCALL_BY_NAME\n\
                 0002 V3 = MAKE_REF V2\n\
                 0003 ASSIGN_OBJ_REF (function) CV0($o) string(\"p\")\n\
                 0004 OP_DATA V3\n\
                 0005 RETURN int(1)\n",
            ),
        ];
        for (source, lines, vars, tmps, oplines) in cases {
            assert_eq!(
                dump_of(source),
                main_dump(*lines, *vars, *tmps, oplines),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }
}
