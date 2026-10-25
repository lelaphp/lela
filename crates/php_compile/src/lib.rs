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
/// A test's skip for a fact this machine lacks, shared with the tests
/// that cannot depend on xtask (#3180).
#[cfg(test)]
#[path = "../tests/support/skip.rs"]
mod skip;
mod special;
mod stack;
pub mod values;

pub use emit::is_reserved_class_name;
pub use stack::{Reentered, StackLimit};

/// `MAXPATHLEN` (`Z/Zend/zend_virtual_cwd.h:39`), the host's
/// `PATH_MAX`: 4096 on Linux, 1024 on macOS and the BSDs. The room
/// `__DIR__` takes where the directory is the working one, and the one
/// value the engine's path checks, `open_basedir`, `include` and `dl()`
/// read and `PHP_MAXPATHLEN` says.
pub const MAXPATHLEN: usize = if cfg!(target_os = "linux") {
    4096
} else {
    1024
};

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
use std::collections::HashMap;

use php_heap::{Fatal, FatalReason, Intern, Sizing};
use php_link::{Exact, Folded, Indexed};
use php_syntax::{Arena, Parsed};
use php_unit::decl::{ClassDecl, ConstDecl, FunctionDecl, HookKind, Visibility};
use php_unit::{Diagnostic, Level, Unit};
use values::Holder;

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
    /// The engine's persistent constants, [`Persistent`], which
    /// `zend_try_ct_eval_const` substitutes where they are written in
    /// every compile, a file the cache keeps among them; a name not here
    /// is the request's ([`Options::request_constants`]) or fetched as
    /// the program serves.
    pub persistent: Persistent<'a>,
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
    /// The constants the request the source compiles for has defined by
    /// then, [`RequestConstants`], for a compile php runs without
    /// `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`; `None` is the option set,
    /// as opcache sets it around a file it caches, where only the
    /// persistent constants and the active class's own are substituted.
    pub request_constants: Option<RequestConstants<'a>>,
    /// The stack limit of the thread the compiler runs on, php's
    /// `EG(stack_limit)`, which the engine works out once per request
    /// (`zend_call_stack_init`) and the compiler's recursion is held to
    /// (`zend_check_stack_limit`); unchecked where a caller says nothing.
    pub stack: StackLimit,
    /// `CG(rtd_key_counter)` as the source starts compiling: how many
    /// runtime definition keys and anonymous class names were made before
    /// it, which the `$` of each one this source makes counts on from. A
    /// file compiled for no request starts from none; a source compiled
    /// for a request, an `eval`'s string or a file opcache's miss
    /// compiles, continues the request's count, so two `eval`s of one
    /// string declare two anonymous classes, as php's one
    /// `CG(rtd_key_counter)` makes them.
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
    /// `zend_op_array_extension_handles`: the runtime cache slots every
    /// op array keeps for the extensions that took one at startup
    /// (`zend_get_op_array_extension_handle`), which `init_op_array`
    /// starts `cache_size` at and each slot the compiler allocates
    /// follows (`Z/Zend/zend_opcode.c:95`); the tier takes one as
    /// it starts. None for an engine where nothing took one.
    pub op_array_extensions: u32,
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
            persistent: Persistent::default(),
            functions: Indexed::default(),
            frameless: &[],
            classes: Indexed::default(),
            request_classes: RequestClasses::default(),
            request_constants: None,
            stack: StackLimit::UNCHECKED,
            keys_before: 0,
            multibyte: None,
            hooked: Hooked::NONE,
            extended_info: false,
            op_array_extensions: 0,
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

/// The rows of `EG(zend_constants)` php registers `CONST_PERSISTENT`,
/// which `can_ct_eval_const`'s first arm (`Z/Zend/zend_compile.c:1650`)
/// lets `zend_try_ct_eval_const` substitute in every compile, one opcache
/// caches under `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION` among them: the
/// constants the engine's extensions declared, and `PHP_SAPI`, which
/// `php_module_startup` registers from `sapi_module.name`
/// (`Z/main/main_arginfo.h:85`). php marks `PHP_SAPI` `CONST_NO_FILE_CACHE`
/// as well, which only `opcache.file_cache`'s compile reads, and the
/// engine has no file cache. One engine is built for one host
/// ([`Persistent::host`]), so a unit its cache shares across requests
/// substitutes the one name every request of it reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct Persistent<'a> {
    /// The constants the engine's extensions declared, each a scalar
    /// written out or a declaration the compiler does not fold.
    pub declared: Indexed<'a, ConstDecl, Exact>,
    /// `PHP_SAPI`'s value, the name of the host the engine was built for,
    /// a string nothing counts owners of; none for a compile no host's
    /// engine runs, where the name is fetched as the program serves.
    pub host: Option<php_heap::Zval>,
}

/// The classes a request has declared, as the compiler asks after one
/// while a source compiles for it: php's `CG(class_table)` is
/// `EG(class_table)` then, so a top-level class whose name the request
/// already holds fails the `zend_hash_add_ptr` that would bind it early
/// (`Z/Zend/zend_compile.c:9381`), and is left to its
/// `DECLARE_CLASS_DELAYED`, which binds it as the unit loads where the
/// name is free by then and raises the redeclaration where it stands
/// where it is not (GH-8846). A compile php runs without
/// `ZEND_COMPILE_IGNORE_OTHER_FILES`, an `eval`'s string and `-r`'s code,
/// reads the classes themselves too ([`RequestClasses::seeing`]): a
/// parent `zend_try_early_bind` binds a class to, and a class whose
/// method is a callee the compile knows (`zend_compile_ignore_class`
/// ignores none). opcache compiles a file it caches with that option, so
/// another file's class is no parent an early bind takes there. Asked by
/// the lowered name; a source compiled for no request holds none.
#[derive(Clone, Copy)]
pub struct RequestClasses<'a> {
    holds: &'a dyn Fn(&[u8]) -> bool,
    declared: Option<&'a dyn DeclaredClasses>,
}

/// The declarations of the classes a request holds, as
/// [`RequestClasses::seeing`] reads them.
pub trait DeclaredClasses {
    /// The linked declaration of the class by the lowered `name`.
    fn declared(&self, name: &[u8]) -> Option<&ClassDecl>;
}

impl<'a> RequestClasses<'a> {
    /// The request's classes as `holds` answers for a lowered name, their
    /// declarations unread, as a file the cache keeps is compiled.
    #[must_use]
    pub fn new(holds: &'a dyn Fn(&[u8]) -> bool) -> RequestClasses<'a> {
        RequestClasses {
            holds,
            declared: None,
        }
    }

    /// These classes with their declarations, as `declared` answers: for
    /// a compile without
    /// `ZEND_COMPILE_IGNORE_OTHER_FILES`.
    #[must_use]
    pub fn seeing(self, declared: &'a dyn DeclaredClasses) -> RequestClasses<'a> {
        RequestClasses {
            declared: Some(declared),
            ..self
        }
    }

    /// Whether the request has declared a class by the lowered `name`.
    #[must_use]
    pub fn holds(&self, name: &[u8]) -> bool {
        (self.holds)(name)
    }

    /// The declaration of the class the request holds by the lowered
    /// `name`, linked, where the compile reads it.
    #[must_use]
    pub fn declared(&self, name: &[u8]) -> Option<&'a ClassDecl> {
        self.declared?.declared(name)
    }
}

impl Default for RequestClasses<'_> {
    fn default() -> Self {
        RequestClasses::new(&|_| false)
    }
}

impl std::fmt::Debug for RequestClasses<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestClasses")
    }
}

/// The constants a request has defined, as `zend_try_ct_eval_const` and
/// `zend_try_ct_eval_class_const` find one while a source compiles for the
/// request without `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`: an `eval`'s
/// string and `-r`'s code, compiled under `ZEND_COMPILE_DEFAULT_FOR_EVAL`,
/// and a file no cache keeps. opcache sets the option around every file it
/// caches (`persistent_compile_file`), so a file compiled for the cache
/// is handed none ([`Options::request_constants`]) and fetches each such
/// name as the program serves. A global constant is asked by the name the compile
/// resolved, a leading `\` stripped, from `EG(zend_constants)` past the
/// engine's persistent ones ([`Options::persistent`]); a class constant
/// by the class's lowered name and the constant's, from any class
/// `EG(class_table)` holds, the engine's among them. Each answer is the
/// constant as the table holds it ([`Defined`], [`DefinedClassConstant`]),
/// and the compiler applies `can_ct_eval_const` and
/// `zend_verify_ct_const_access` to it.
#[derive(Clone, Copy)]
pub struct RequestConstants<'a> {
    constants: &'a dyn Fn(&[u8]) -> Option<Defined>,
    class_constants: &'a ClassConstantLookup<'a>,
}

/// What [`RequestConstants`] asks a class constant of: the class's
/// lowered name and the constant's.
pub type ClassConstantLookup<'a> = dyn Fn(&[u8], &[u8]) -> Option<DefinedClassConstant> + 'a;

/// One constant a request defined as [`RequestConstants`] answers: its
/// value, a view the table's owner keeps alive while the compile runs,
/// and whether `CONST_DEPRECATED` marks it, which `#[\Deprecated]` on its
/// declaration raises.
#[derive(Clone, Copy, Debug)]
pub struct Defined {
    pub value: php_heap::Zval,
    pub deprecated: bool,
}

/// One class constant as [`RequestConstants`] answers it, what
/// `zend_try_ct_eval_class_const` reads off its `zend_class_constant`:
/// `c->value`, a view the class keeps alive while the compile runs, where
/// it holds a value and no constant expression still to evaluate; the
/// constant's visibility and `ZEND_ACC_DEPRECATED`; and whether `c->ce`,
/// the class that declared it, is a trait.
#[derive(Clone, Copy, Debug)]
pub struct DefinedClassConstant {
    pub value: php_heap::Zval,
    pub visibility: Visibility,
    pub deprecated: bool,
    pub in_trait: bool,
}

impl<'a> RequestConstants<'a> {
    /// The request's constants as `constants` answers for a global name
    /// and `class_constants` for a lowered class name and a constant's.
    #[must_use]
    pub fn new(
        constants: &'a dyn Fn(&[u8]) -> Option<Defined>,
        class_constants: &'a ClassConstantLookup<'a>,
    ) -> RequestConstants<'a> {
        RequestConstants {
            constants,
            class_constants,
        }
    }

    /// The constant the request defined under `name`, if it did.
    #[must_use]
    pub fn find(&self, name: &[u8]) -> Option<Defined> {
        (self.constants)(name)
    }

    /// The constant `name` of the class the request's table holds under
    /// the lowered `class`, where it holds a value.
    #[must_use]
    pub fn class_constant(&self, class: &[u8], name: &[u8]) -> Option<DefinedClassConstant> {
        (self.class_constants)(class, name)
    }
}

impl std::fmt::Debug for RequestConstants<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestConstants")
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
///
/// The refusal is a pointer's width, its [`Refused`] boxed and read
/// through `Deref`: every compile answers a `Result<_, CompileError>`,
/// and a nested expression keeps one answer a level on the stack while
/// the level below compiles, so the answer is as wide as what succeeds,
/// the 24 bytes of where an expression's value stands, and not the 72
/// of a refusal held in place (#3208).
#[derive(Clone, PartialEq)]
pub struct CompileError(Box<Refused>);

impl From<Refused> for CompileError {
    fn from(refused: Refused) -> CompileError {
        CompileError(Box::new(refused))
    }
}

impl std::ops::Deref for CompileError {
    type Target = Refused;

    fn deref(&self) -> &Refused {
        &self.0
    }
}

impl std::ops::DerefMut for CompileError {
    fn deref_mut(&mut self) -> &mut Refused {
        &mut self.0
    }
}

impl std::fmt::Debug for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What a [`CompileError`] says, each field read through it.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub level: Level,
    /// php's text, a constant one borrowed where it is one, so a
    /// refusal in the compiler's own constant words allocates no text.
    pub message: Cow<'static, str>,
    pub line: u32,
    pub raised: Vec<Diagnostic>,
    /// How php meets the refusal, or that php would have met none.
    pub raise: Raise,
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

impl Refused {
    /// Whether php's compile bailed out at the refusal, `zend_bailout`
    /// out of `zend_compile` past what it had declared and counted: a
    /// compile error, a heap fatal. A parse error, this compiler's own
    /// refusal of what it cannot read yet, and a refusal php throws are
    /// the parser's, made before `zend_compile` declared or counted
    /// anything, which leave it as it returns: an `include` or an `eval`
    /// throws the one php throws, and a parse error as the `ParseError`.
    #[must_use]
    pub fn bailed(&self) -> bool {
        match self.raise {
            Raise::Fatal => self.level != Level::Parse,
            Raise::Thrown | Raise::Unread => false,
        }
    }
}

/// How a [`Refused`] reaches the request: php's fatal, php's thrown
/// `CompileError`, or this compiler's own sentence where php would have
/// compiled the source. Decided where the refusal is made and matched by
/// each reader, never read back off the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raise {
    /// php raises it, `zend_error` at [`Refused::level`], and the
    /// request ends.
    Fatal,
    /// php throws it as a `CompileError`,
    /// `zend_throw_exception(zend_ce_compile_error)` from the parser's
    /// modifier lists and two statements: a `try` around the `include` or
    /// the `eval` catches it, and uncaught it renders as the fatal at
    /// [`Refused::level`].
    Thrown,
    /// php would have compiled what stands here, and this compiler cannot
    /// read it yet: its own sentence, `… this compiler cannot read yet`,
    /// raised at `Level::Parse` as #163 framed it, which the phpt lane
    /// counts as unread. A fold that meets one leaves the node unfolded
    /// for the compile to read or refuse, as it leaves a node that is no
    /// constant.
    Unread,
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
    /// refusal's [`Refused::raised`] php had raised when it landed.
    pub raised_before: Vec<usize>,
}

/// Where php stood when it refused a source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stage {
    /// Compiling it, `zend_is_compiling()`: the refusal stands in the
    /// file at [`Refused::line`].
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
    let opened = multibyte::open(multibyte.encodings, source, from).map_err(|refused| {
        CompileError::from(Refused {
            level: Level::CompileError,
            message: refused.message().into(),
            line: 0,
            raised: Vec::new(),
            raise: Raise::Fatal,
            stage: Stage::Opening,
            declared: None,
            keys_made: 0,
        })
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
        return Err(Refused {
            level: level_of(fatal.level),
            message: fatal.message.clone().into(),
            line: fatal.line,
            raised: Vec::new(),
            raise: Raise::Fatal,
            stage: Stage::Compiling,
            declared: None,
            keys_made: 0,
        }
        .into());
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
            return Err(Refused {
                level,
                message: diagnostic.message.clone().into(),
                line: diagnostic.line,
                raised,
                raise: Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into());
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
/// names of what it declared handed over ([`Parts::intern`]): those
/// it made as it went, `own`, and for a file every other as a file's are
/// made ([`intern_names`]); a declared unit whose names the heap refused
/// is not handed back.
pub(crate) fn refused_after(
    mut refused: CompileError,
    mut raised: Vec<Diagnostic>,
    options: &Options<'_>,
    own: &[(Named, php_heap::Str)],
    strings: &mut dyn Intern,
) -> CompileError {
    if let Some(declared) = &mut refused.declared {
        for before in &mut declared.raised_before {
            *before += raised.len();
        }
        if Parts::of(&declared.unit)
            .intern(Making::refused(options.kind), own, strings)
            .is_err()
        {
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

    /// Each name the unit's declarations own handed to `strings`
    /// ([`Intern::hand_name`]) in the order [`named`] walks them: the
    /// string the compile made for it as it went, `own`'s, else one made
    /// here where `making` makes it, else none.
    pub(crate) fn intern(
        self,
        making: Making,
        own: &[(Named, php_heap::Str)],
        strings: &mut dyn Intern,
    ) -> Result<(), Fatal> {
        let own: HashMap<Named, php_heap::Str> = own.iter().copied().collect();
        for (named, bytes) in self.walk() {
            let made = match (own.get(&named), making, named) {
                (Some(own), ..) => Some(*own),
                (None, Making::All | Making::Read, Named::Read(_))
                | (None, Making::All, Named::File | Named::Function(_) | Named::Class(_)) => {
                    let sizing = named.sizing_among(self.classes, &bytes);
                    Some(strings.intern_marked(&bytes, false, sizing)?)
                }
                (None, ..) => None,
            };
            strings.hand_name(made);
        }
        Ok(())
    }

    /// [`named`]: the file's, each function's, dynamic definition's,
    /// method's, hook's and class's, as [`intern_names`] interns them;
    /// the lowered keys; then the main op array's variables' and the
    /// names the declarations are read by.
    fn walk(self) -> impl Iterator<Item = (Named, Cow<'u, [u8]>)> {
        let functions = (self.functions.iter().enumerate())
            .map(|(at, (decl, _))| (FunctionAt::Top(at), &decl.name));
        let dynamic = (self.dynamic.iter().enumerate())
            .map(|(at, (decl, _))| (FunctionAt::Dynamic(at), &decl.name));
        let members = (self.classes.iter().enumerate()).flat_map(|(class, decl)| {
            // An enum's `cases`, `from` and `tryFrom` are php's internal
            // functions, named by its known strings.
            let methods = (php_unit::written_methods(decl).iter().enumerate())
                .map(move |(method, m)| (FunctionAt::Method { class, method }, &m.function.name));
            let hooks = (decl.properties.iter().enumerate())
                .filter_map(|(property, p)| Some((property, p.hooks.as_deref()?)))
                .flat_map(move |(property, hooks)| {
                    let get = hooks.get.iter().map(|m| (HookKind::Get, m));
                    get.chain(hooks.set.iter().map(|m| (HookKind::Set, m)))
                        .map(move |(kind, m)| {
                            let at = FunctionAt::Hook {
                                class,
                                property,
                                kind,
                            };
                            (at, &m.function.name)
                        })
                });
            methods.chain(hooks)
        });
        let functions =
            (functions.chain(dynamic).chain(members)).map(|(at, name)| (Named::Function(at), name));
        let classes =
            (self.classes.iter().enumerate()).map(|(at, class)| (Named::Class(at), &class.name));
        let names = (std::iter::once((Named::File, self.file))
            .chain(functions)
            .chain(classes))
        .map(|(named, name)| (named, Cow::Borrowed(&*name.0)));
        names.chain(self.keys()).chain(
            self.reads()
                .map(|(holder, name)| (Named::Read(holder), name)),
        )
    }

    /// The lowered key each declaration is bound under where it is not
    /// its name (`zend_string_tolower`): a function's, one a
    /// `DECLARE_FUNCTION` binds among them, a class's, which the
    /// `class_table` keeps however it was bound, and a method's, the
    /// class's `function_table` key. A closure has none.
    fn keys(self) -> impl Iterator<Item = (Named, Cow<'u, [u8]>)> {
        let functions = (self.functions.iter().enumerate())
            .map(|(at, (decl, _))| (Named::FunctionKey(at), &decl.name));
        let dynamic = (self.dynamic.iter().enumerate())
            .filter(|(_, (decl, _))| !decl.name.0.starts_with(b"{"))
            .map(|(at, (decl, _))| (Named::DefinitionKey(at), &decl.name));
        let classes =
            (self.classes.iter().enumerate()).map(|(at, class)| (Named::ClassKey(at), &class.name));
        let methods = (self.classes.iter().enumerate()).flat_map(|(class, decl)| {
            (php_unit::written_methods(decl).iter().enumerate())
                .map(move |(method, m)| (Named::MethodKey { class, method }, &m.function.name))
        });
        (functions.chain(dynamic).chain(classes).chain(methods))
            .filter(|(_, name)| name.0.iter().any(u8::is_ascii_uppercase))
            .map(|(named, name)| (named, Cow::Owned(name.0.to_ascii_lowercase())))
    }

    /// The names the declarations are read by, beside what holds each
    /// ([`named`]'s [`Named::Read`]).
    fn reads(self) -> impl Iterator<Item = (Holder, Cow<'u, [u8]>)> {
        let main = (self.main_vars.iter()).map(|var| (Holder::Main, Cow::Borrowed(&*var.0)));
        let bodies = (self.functions.iter().map(|(_, body)| (Holder::Unit, body)))
            .chain(
                (self.dynamic.iter().enumerate())
                    .map(|(at, (_, body))| (Holder::Dynamic(at), body)),
            )
            .chain(
                self.methods
                    .iter()
                    .map(|method| (Holder::Unit, &method.body)),
            )
            .chain(self.hooks.iter().map(|hook| (Holder::Unit, &hook.body)));
        let vars = bodies.flat_map(|(holder, body)| {
            (body.vars.iter()).map(move |var| (holder, Cow::Borrowed(&*var.0)))
        });
        let properties = self.classes.iter().flat_map(|class| {
            class.properties.iter().flat_map(move |property| {
                let mangled = mangling(property.visibility, &class.name.0)
                    .map(|prefix| Cow::Owned([prefix.as_slice(), &property.name.0].concat()));
                std::iter::once(Cow::Borrowed(&*property.name.0)).chain(mangled)
            })
        });
        // A class constant's and an enum case's, the `constants_table`
        // key `zend_compile_class_const_decl` and `zend_compile_enum_case`
        // intern (`zval_make_interned_string`).
        let constants = (self.classes.iter()).flat_map(|class| {
            (class.constants.iter()).map(|constant| Cow::Borrowed(&*constant.name.0))
        });
        let declared = properties.chain(constants).map(|name| (Holder::Unit, name));
        main.chain(vars).chain(declared)
    }
}

/// What [`Parts::intern`] makes of the names the compile made none of as
/// it went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Making {
    /// Every op array's and the file's, and those the declarations are
    /// read by: a file's, whose names opcache persists beside it.
    All,
    /// Those the declarations are read by: an `eval`'s, whose op arrays'
    /// names the engine makes as it notes the unit.
    Read,
    /// None: a refused `eval`'s, which hands over only what it made.
    None,
}

impl Making {
    /// What a compile of `kind` that ends in a unit makes.
    pub(crate) fn of(kind: SourceKind) -> Making {
        match kind {
            SourceKind::File => Making::All,
            SourceKind::Eval(_) => Making::Read,
        }
    }

    /// What a refused compile of `kind` that declared something makes.
    pub(crate) fn refused(kind: SourceKind) -> Making {
        match kind {
            SourceKind::File => Making::All,
            SourceKind::Eval(_) => Making::None,
        }
    }
}

/// What a name a compile hands its home names ([`Intern::hand_name`]), in
/// the order [`named`] walks them: one string a declaration owns, given
/// to what holds it ([`Named::holder`]) by what it names, never told by
/// its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Named {
    /// The unit's file's, `op_array->filename`.
    File,
    /// A function's, a closure's, a method's or a hook's,
    /// `function_name`.
    Function(FunctionAt),
    /// The class's at this place of [`Unit::classes`], `ce->name`.
    Class(usize),
    /// The lowered key the top-level function at this place of
    /// [`Unit::functions`] is bound under where lowering changes its
    /// name, the function table's: `zend_begin_func_decl`'s
    /// `zend_string_tolower`.
    FunctionKey(usize),
    /// The lowered key the dynamic definition at this place of
    /// [`Unit::dynamic`] is bound under where lowering changes its
    /// name, its `DECLARE_FUNCTION`'s constant.
    DefinitionKey(usize),
    /// The lowered key the class at this place is bound under where
    /// lowering changes its name, which php's compile interns
    /// (`zend_new_interned_string`).
    ClassKey(usize),
    /// The lowered key the method at `method` of the class at `class` is
    /// bound under where lowering changes its name, which
    /// `zend_begin_method_decl` interns.
    MethodKey { class: usize, method: usize },
    /// A name a declaration is read by: a variable's, a declared
    /// property's and a class constant's or enum case's.
    Read(Holder),
}

impl Named {
    /// What holds the name: the dynamic definition it is the name, the
    /// key or a variable of, the main op array a variable of, and the
    /// unit any other.
    #[must_use]
    pub fn holder(self) -> Holder {
        match self {
            Named::Function(FunctionAt::Dynamic(at)) | Named::DefinitionKey(at) => {
                Holder::Dynamic(at)
            }
            Named::Read(holder) => holder,
            Named::File
            | Named::Function(_)
            | Named::Class(_)
            | Named::FunctionKey(_)
            | Named::ClassKey(_)
            | Named::MethodKey { .. } => Holder::Unit,
        }
    }
}

/// Where a function's op array stands in its unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FunctionAt {
    /// The top-level function at this place of [`Unit::functions`].
    Top(usize),
    /// The dynamic definition at this place of [`Unit::dynamic`], a
    /// closure among them.
    Dynamic(usize),
    /// The method at `method` of the class at `class`.
    Method { class: usize, method: usize },
    /// The `kind` hook of the property at `property` of the class at
    /// `class`.
    Hook {
        class: usize,
        property: usize,
        kind: HookKind,
    },
}

impl FunctionAt {
    /// The size php lays the name of the function here, called `name`,
    /// at: `zend_string_finit`'s ([`Sizing::Finit`]) for a name php
    /// prints with `zend_strpprintf`, whose `smart_str_extract`
    /// php-8.5.12 trims to it, a closure's `{closure:…}`
    /// (`zend_begin_func_decl`, `Z/Zend/zend_compile.c:8379`), which no
    /// written name can begin as, and a property hook's `$name::get`
    /// (`zend_compile_property_hooks`, `:8782`); the scanner's,
    /// `zend_string_init`'s, for every other.
    #[must_use]
    pub fn sizing(self, name: &[u8]) -> Sizing {
        match self {
            FunctionAt::Hook { .. } => Sizing::Finit,
            FunctionAt::Dynamic(_) if name.starts_with(b"{") => Sizing::Finit,
            FunctionAt::Dynamic(_) | FunctionAt::Top(_) | FunctionAt::Method { .. } => {
                Sizing::Alloc
            }
        }
    }
}

impl Named {
    /// The size php lays the name at, called `name` in `unit`: a
    /// function's as [`FunctionAt::sizing`] says, an anonymous class's
    /// `class@anonymous…` at `zend_strpprintf`'s
    /// (`zend_generate_anon_class_name`, `:9219`), and every other at
    /// `zend_string_init`'s.
    #[must_use]
    pub fn sizing(self, unit: &Unit, name: &[u8]) -> Sizing {
        self.sizing_among(&unit.classes, name)
    }

    /// [`Named::sizing`] of a unit whose classes are `classes`.
    fn sizing_among(self, classes: &[ClassDecl], name: &[u8]) -> Sizing {
        match self {
            Named::Function(at) => at.sizing(name),
            Named::Class(at) => class_sizing(
                classes
                    .get(at)
                    .is_some_and(|class| class.flags.is_anonymous),
            ),
            _ => Sizing::Alloc,
        }
    }
}

/// The size php lays a class's name at: an anonymous one's at
/// `zend_strpprintf`'s, `zend_string_finit`'s, and a written one's at the
/// scanner's.
pub(crate) fn class_sizing(anonymous: bool) -> Sizing {
    if anonymous {
        Sizing::Finit
    } else {
        Sizing::Alloc
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
    Parts::of(unit).intern(Making::All, &[], strings)
}

/// Every name `unit`'s declarations own, in the order a compile hands
/// them to its home ([`Intern::hand_name`]), each beside what it names
/// ([`Named`]), so the home gives each to what holds it by its place
/// and never by its bytes: the file's, `op_array->filename`, each
/// function's, dynamic definition's, method's and hook's,
/// `function_name`, and each class's, `ce->name` ([`intern_names`]);
/// each lowered key a declaration is bound under where it is not its
/// name; then the names php's compile interns into whichever home it
/// compiles into, an `eval`'s among them. Those are each op array's
/// variables, the main one's first, which
/// `zend_try_compile_cv` interns (`zval_make_interned_string`) before
/// `lookup_cv` keeps them in `op_array->vars`, each declared
/// property's name with the key `zend_declare_typed_property` interns
/// as `prop_info->name`, mangled for a protected or a private one, and
/// each class constant's and enum case's, its `constants_table` key. A
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
///
/// A class constant's name is one string of the class's, which a
/// one-byte name, php's interned character, is not:
///
/// ```php
/// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
/// m('class W {} enum V { case A; }');
/// echo m('class Z1 { const ABCD = 1; }') - m('class Z2 { const A = 1; }'), "\n";
/// echo m('enum Z3 { case ABCD; }') - m('enum Z4 { case A; }'), "\n";
/// ```
/// ```expect regime=jit-off
/// 32
/// 32
/// ```
pub fn named(unit: &Unit) -> impl Iterator<Item = (Named, Cow<'_, [u8]>)> {
    Parts::of(unit).walk()
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
#[allow(
    clippy::needless_raw_string_hashes,
    reason = "expect-test writes every multi-line snapshot as r#\"…\"#, a dump without a quote as well"
)]
mod tests;
