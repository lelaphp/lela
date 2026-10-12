//! The one shape of every chained kind (#1945): php's saved pointer,
//! which each supplier overwrites and keeps as the one beneath, and
//! calls before its own work, after it, around it or not at all, as it
//! chooses. Each link is handed [`Beneath`], every link under it and,
//! beneath them all, php's own function of the kind, and the chain is
//! entered from the top, the last supplier's link first. A link that
//! serves the one beneath does so where its work is to stand, and one
//! that does not has answered for every link under it.
//!
//! A kind is one of the markers here, each saying what a link
//! is handed besides the ones beneath, what the chain answers, and what
//! php's own function is ([`ChainKind`]); the kinds are the engine's, and
//! no extension adds one.

use php_heap::FrameRef;
use php_unit::Opcode as Op;

use crate::api::abi::{
    CompileFileHook, CompileStringHook, ErrorHook, GcCollectHook, InterruptHook, OpcodeHandler,
    PostStartupHook, ResolvePathHook, ThrownHook, UploadHook,
};
use crate::api::raising::{Level, Thrown};
use crate::api::request::{HandlePath, Request, Unresolved};
use crate::api::values::{PhpObject, PhpString};
use crate::observer::Opcodes;

use super::{Bail, Compiled};

/// A chained kind: what [`Beneath`] serves, and what the chain answers
/// where it is entered, each link's answer and php's own beneath them.
/// Implemented by the markers of [`crate::api::abi::chain`] alone; a kind
/// of another crate's is refused, its engine half being unnameable:
///
/// ```compile_fail,E0277
/// enum Mine {}
/// impl php_engine::api::abi::ChainKind for Mine {
///     type Answer = ();
/// }
/// ```
pub trait ChainKind: sealed::Kind {
    /// What the chain answers.
    type Answer;
}

pub(crate) mod sealed {
    use super::{Beneath, ChainKind};
    use crate::api::request::Request;

    /// The engine's half of a kind, which no other crate names: the
    /// link a supplier hands, what each link is handed besides the ones
    /// beneath, how a link is called, and php's own function beneath
    /// every link.
    pub trait Kind: Sized + 'static {
        /// One supplier's link, as its builder took it.
        type Link: Copy;
        /// What each link is handed besides the ones beneath.
        type Args<'a>: Copy;

        /// `link` served, handed the ones beneath it.
        fn link(link: Self::Link, req: &mut Request, beneath: Beneath<'_, Self>) -> Self::Answer
        where
            Self: ChainKind;

        /// php's own function of the kind, beneath every link.
        fn own(req: &mut Request, args: Self::Args<'_>) -> Self::Answer
        where
            Self: ChainKind;
    }
}

/// What a link holds of the pointer it saved: every link beneath it
/// and, beneath them all, php's own function of the kind, with what the
/// link was handed. [`Beneath::serve`] serves the one beneath; a link
/// that never does has answered in place of every one under it.
pub struct Beneath<'a, K: ChainKind> {
    links: &'a [K::Link],
    args: K::Args<'a>,
}

impl<K: ChainKind> Clone for Beneath<'_, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: ChainKind> Copy for Beneath<'_, K> {}

impl<'a, K: ChainKind> Beneath<'a, K> {
    /// The chain of `links`, the first supplier's first, entered from
    /// the top with `args`.
    pub(crate) fn top(links: &'a [K::Link], args: K::Args<'a>) -> Beneath<'a, K> {
        Beneath { links, args }
    }

    /// The one beneath served: the next link down, handed the ones
    /// beneath it, or php's own function of the kind.
    pub fn serve(self, req: &mut Request) -> K::Answer {
        match self.links.split_last() {
            Some((link, links)) => K::link(
                *link,
                req,
                Beneath {
                    links,
                    args: self.args,
                },
            ),
            None => K::own(req, self.args),
        }
    }
}

/// `zend_interrupt_function` (`Z/Zend/zend.c:89`), which
/// pcntl's link serves before the one it saved
/// (`Z/ext/pcntl/pcntl.c:1922-1927`): nothing of php's stands beneath
/// the links. What a link answers is left pending where the loop stands.
pub enum Interrupt {}

/// `zend_compile_file` (`Z/Zend/zend_compile.c:87`): the compiled file,
/// with beneath every link the engine's cached compile of it, the unit
/// cache's lookup and compile ([`Request::cached_source`]), in which
/// opcache's admission decides as the one question the cache asks
/// (ADR-0005, #2457). A link wraps it and may post-process what it
/// answers, as phpdbg's `phpdbg_compile_file` keeps a file's source, or
/// hands the ones beneath a source of its own, as phar's
/// `phar_compile_file` hands `phar_orig_compile_file` an archive's stub
/// (`Z/ext/phar/phar.c:3248-3310`). The answer is `None` where the file
/// was not there to read, php's `NULL` op array with nothing thrown.
pub enum CompileFile {}

/// `zend_compile_string` (`Z/Zend/zend_compile.c:88`), beneath every
/// link php's own `compile_string`.
pub enum CompileString {}

/// `zend_error_cb` (`Z/Zend/zend.c:90`), beneath every link php's own
/// `php_error_cb`, which stores, logs and displays the diagnostic.
pub enum Error {}

/// The `gc_collect_cycles` pointer (`Z/Zend/zend_gc.c:254`), beneath
/// every link the heap's own collection, `zend_gc_collect_cycles`
/// (design §5.12).
pub enum GcCollect {}

/// `zend_throw_exception_hook` (`Z/Zend/zend_exceptions.c:60`): nothing
/// of php's stands beneath the links.
pub enum Throw {}

/// `zend_resolve_path`, beneath every link the stream layer's
/// `php_resolve_path` over the ini's `include_path`, or, with no stream
/// layer, Zend's own `VCWD_REALPATH`.
pub enum ResolvePath {}

/// `zend_post_startup_cb`: nothing of php's stands beneath the links,
/// so the chain answers [`PostStartup::Serve`](crate::api::abi::PostStartup::Serve)
/// where no link answered otherwise.
pub enum PostStartup {}

/// `php_rfc1867_callback`: nothing of php's stands beneath the links,
/// so the chain answers php's `SUCCESS` where no link failed.
pub enum Upload {}

/// `zend_user_opcode_handlers`, one handler per opcode, which a link
/// set over another's saves and calls after its own work, as xdebug's
/// `xdebug_set_opcode_handler` does: the links beneath are those that
/// declared the opcode, and nothing of php's stands beneath them, the
/// opcode's own handler running after the chain whatever it did.
pub enum Opcode {}

impl ChainKind for Interrupt {
    type Answer = Result<(), Thrown>;
}

impl sealed::Kind for Interrupt {
    type Link = InterruptHook;
    type Args<'a> = ();

    fn link(
        link: InterruptHook,
        req: &mut Request,
        beneath: Beneath<'_, Self>,
    ) -> <Self as ChainKind>::Answer {
        link(req, beneath)
    }

    fn own(_: &mut Request, (): ()) -> <Self as ChainKind>::Answer {
        Ok(())
    }
}

/// What a link of `zend_compile_file` is handed, the `zend_file_handle`
/// as the engine holds it: the file's full path, the name it was written
/// by, which opcache files a key of, the kind of `zend_file_handle` it is
/// compiled through, the source a link above handed down in place of the
/// file's own, and the file the stream that link opened holds.
#[derive(Clone, Copy)]
pub struct FileArgs<'a> {
    pub(crate) path: &'a [u8],
    pub(crate) written: Option<&'a [u8]>,
    pub(crate) handle: HandlePath,
    pub(crate) source: Option<&'a [u8]>,
    pub(crate) opened: Option<&'a [u8]>,
}

impl<'a> FileArgs<'a> {
    pub(crate) fn new(path: &'a [u8], written: Option<&'a [u8]>, handle: HandlePath) -> Self {
        FileArgs {
            path,
            written,
            handle,
            source: None,
            opened: None,
        }
    }

    /// Whether the compile opens the file as a stream, as php's
    /// `zend_stream_fixup` opens a `zend_file_handle` that holds no stream
    /// yet: not the program the host opened, nor a wrapper's stream, nor
    /// a file a link handed a source of, read from a stream of its own.
    pub(crate) fn opens_stream(&self) -> bool {
        self.source.is_none() && !matches!(self.handle, HandlePath::Host | HandlePath::Stream)
    }
}

impl ChainKind for CompileFile {
    type Answer = Result<Option<Compiled>, Thrown>;
}

impl sealed::Kind for CompileFile {
    type Link = CompileFileHook;
    type Args<'a> = FileArgs<'a>;

    fn link(
        link: CompileFileHook,
        req: &mut Request,
        beneath: Beneath<'_, Self>,
    ) -> <Self as ChainKind>::Answer {
        link(req, beneath)
    }

    fn own(req: &mut Request, args: FileArgs<'_>) -> <Self as ChainKind>::Answer {
        if let Some(opened) = args.opened {
            req.hold_opened(opened, args.handle);
        }
        Ok(req.cached_source(args).map(Compiled))
    }
}

impl<'a> Beneath<'a, CompileFile> {
    /// The full path of the file about to be compiled.
    #[must_use]
    pub fn path(&self) -> &'a [u8] {
        self.args.path
    }

    /// The ones beneath handed `source` to compile under the file's path
    /// in place of the file's own, as a link calls the pointer it saved
    /// with a `zend_file_handle` of its own over the same name: the unit
    /// cache files what it compiles under the file, as opcache's
    /// `persistent_compile_file` caches what the one it saved compiled.
    #[must_use]
    pub fn with_source<'b>(self, source: &'b [u8]) -> Beneath<'b, CompileFile>
    where
        'a: 'b,
    {
        Beneath {
            links: self.links,
            args: FileArgs {
                source: Some(source),
                ..self.args
            },
        }
    }

    /// `zend_stream_fixup` over the `zend_file_handle`: the bytes the file
    /// compiles from, the source a link above handed down or the file's
    /// own, read as the compile beneath would read them, with the stream
    /// it would open opened (and a kept one held), as phpdbg's
    /// `phpdbg_compile_file` copies them before it serves the one it
    /// saved; none where the file does not read. A link that hands them on
    /// with [`Beneath::with_source`] has the ones beneath compile those
    /// bytes, opening nothing more.
    #[must_use]
    pub fn read(&self, req: &mut Request) -> Option<Vec<u8>> {
        req.read_file_source(self.args)
    }

    /// The ones beneath told that the stream the link put in the
    /// `zend_file_handle`'s place holds the file at the full path `path`
    /// open, as phar's stub stream over an uncompressed archive holds the
    /// archive: a `zend_file_handle` php destroys only once the program
    /// has been served, `auto_prepend_file`'s, `auto_append_file`'s or the built-in
    /// server's router's and program's, holds it until then, inherited by
    /// a process the request execs ([`HandlePath::Kept`], #2561).
    #[must_use]
    pub fn opened<'b>(self, path: &'b [u8]) -> Beneath<'b, CompileFile>
    where
        'a: 'b,
    {
        Beneath {
            links: self.links,
            args: FileArgs {
                opened: Some(path),
                ..self.args
            },
        }
    }
}

/// What a link of `zend_compile_string` is handed: the string, the name
/// it compiles under, and where the scanner begins.
#[derive(Clone, Copy)]
pub struct StringArgs<'a> {
    source: &'a [u8],
    name: &'a [u8],
    /// Where `compile_string`'s scanner begins, after an open tag for an
    /// `eval`.
    position: php_compile::syntax::Position,
}

impl<'a> StringArgs<'a> {
    pub(crate) fn new(
        source: &'a [u8],
        name: &'a [u8],
        position: php_compile::syntax::Position,
    ) -> StringArgs<'a> {
        StringArgs {
            source,
            name,
            position,
        }
    }
}

impl ChainKind for CompileString {
    type Answer = Result<Compiled, Thrown>;
}

impl sealed::Kind for CompileString {
    type Link = CompileStringHook;
    type Args<'a> = StringArgs<'a>;

    fn link(
        link: CompileStringHook,
        req: &mut Request,
        beneath: Beneath<'_, Self>,
    ) -> <Self as ChainKind>::Answer {
        link(req, beneath)
    }

    fn own(req: &mut Request, args: StringArgs<'_>) -> <Self as ChainKind>::Answer {
        Ok(Compiled(req.compile_string_own(
            args.source,
            args.name,
            args.position,
        )))
    }
}

impl<'a> Beneath<'a, CompileString> {
    /// The string the link was handed.
    #[must_use]
    pub fn source(&self) -> &'a [u8] {
        self.args.source
    }

    /// The name the string compiles under, `eval()'d code` after the
    /// file and line of the `eval`, or the CLI's `Command line code`.
    #[must_use]
    pub fn name(&self) -> &'a [u8] {
        self.args.name
    }

    /// The ones beneath handed `source` in place of the string, a
    /// string the link made, as php's links call the pointer they saved
    /// with one of their own.
    #[must_use]
    pub fn with_source<'b>(self, source: &'b [u8]) -> Beneath<'b, CompileString>
    where
        'a: 'b,
    {
        Beneath {
            links: self.links,
            args: StringArgs {
                source,
                name: self.args.name,
                position: self.args.position,
            },
        }
    }
}

/// What a link of `zend_error_cb` is handed: the diagnostic, and whether
/// `php_error_cb` bails out of a level it cannot recover from.
#[derive(Clone, Copy)]
pub struct ErrorArgs<'a> {
    pub(crate) level: Level,
    pub(crate) bail: Bail,
    pub(crate) string: PhpString,
    pub(crate) message: &'a str,
    pub(crate) file: &'a [u8],
    pub(crate) line: u32,
}

impl ChainKind for Error {
    type Answer = ();
}

impl sealed::Kind for Error {
    type Link = ErrorHook;
    type Args<'a> = ErrorArgs<'a>;

    fn link(link: ErrorHook, req: &mut Request, beneath: Beneath<'_, Self>) {
        link(req, beneath);
    }

    fn own(req: &mut Request, args: ErrorArgs<'_>) {
        req.php_error_cb(args);
    }
}

impl<'a> Beneath<'a, Error> {
    /// The level the diagnostic was raised at.
    #[must_use]
    pub fn level(&self) -> Level {
        self.args.level
    }

    /// The message, its bytes as raised.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        self.args.string.as_bytes()
    }

    /// The file the diagnostic names, `Unknown` where nothing executes.
    #[must_use]
    pub fn file(&self) -> &'a [u8] {
        self.args.file
    }

    /// The line the diagnostic names.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.args.line
    }

    /// Whether `php_error_cb` bails out of the diagnostic once it has
    /// served it (`Z/main/main.c:1503-1543`): a level of
    /// `E_FATAL_ERRORS` without php's `E_DONT_BAIL`, which
    /// `zend_exception_error` sets on the uncaught throwable it renders.
    /// It is where the `zend_catch` of a link that serves the one beneath
    /// inside a `zend_try` runs, as soap's error handler puts the
    /// response's status back there.
    #[must_use]
    pub fn bails(&self) -> bool {
        self.args.level.bit() & crate::exec::E_FATAL_ERRORS != 0 && self.args.bail == Bail::Out
    }
}

impl ChainKind for GcCollect {
    type Answer = i64;
}

impl sealed::Kind for GcCollect {
    type Link = GcCollectHook;
    type Args<'a> = ();

    fn link(link: GcCollectHook, req: &mut Request, beneath: Beneath<'_, Self>) -> i64 {
        link(req, beneath)
    }

    fn own(req: &mut Request, (): ()) -> i64 {
        req.gc_collect_own()
    }
}

impl ChainKind for Throw {
    type Answer = ();
}

impl sealed::Kind for Throw {
    type Link = ThrownHook;
    type Args<'a> = PhpObject;

    fn link(link: ThrownHook, req: &mut Request, beneath: Beneath<'_, Self>) {
        link(req, beneath);
    }

    fn own(_: &mut Request, _: PhpObject) {}
}

impl Beneath<'_, Throw> {
    /// The throwable thrown, standing pending.
    #[must_use]
    pub fn throwable(&self) -> PhpObject {
        self.args
    }
}

impl ChainKind for ResolvePath {
    type Answer = Result<Vec<u8>, Unresolved>;
}

impl sealed::Kind for ResolvePath {
    type Link = ResolvePathHook;
    type Args<'a> = &'a [u8];

    fn link(
        link: ResolvePathHook,
        req: &mut Request,
        beneath: Beneath<'_, Self>,
    ) -> <Self as ChainKind>::Answer {
        link(req, beneath)
    }

    fn own(req: &mut Request, written: &[u8]) -> <Self as ChainKind>::Answer {
        req.resolve_path_own(written)
    }
}

impl<'a> Beneath<'a, ResolvePath> {
    /// The path as an include or an open along the `include_path` wrote
    /// it.
    #[must_use]
    pub fn path(&self) -> &'a [u8] {
        self.args
    }
}

impl ChainKind for PostStartup {
    type Answer = crate::api::abi::PostStartup;
}

impl sealed::Kind for PostStartup {
    type Link = PostStartupHook;
    type Args<'a> = ();

    fn link(
        link: PostStartupHook,
        req: &mut Request,
        beneath: Beneath<'_, Self>,
    ) -> <Self as ChainKind>::Answer {
        link(req, beneath)
    }

    fn own(_: &mut Request, (): ()) -> <Self as ChainKind>::Answer {
        crate::api::abi::PostStartup::Serve
    }
}

impl ChainKind for Upload {
    type Answer = Result<bool, Thrown>;
}

impl sealed::Kind for Upload {
    type Link = UploadHook;
    type Args<'a> = crate::api::request::Upload<'a>;

    fn link(
        link: UploadHook,
        req: &mut Request,
        beneath: Beneath<'_, Self>,
    ) -> <Self as ChainKind>::Answer {
        link(req, beneath)
    }

    fn own(_: &mut Request, _: crate::api::request::Upload<'_>) -> <Self as ChainKind>::Answer {
        Ok(true)
    }
}

impl<'a> Beneath<'a, Upload> {
    /// The step of the body's reading.
    #[must_use]
    pub fn event(&self) -> crate::api::request::Upload<'a> {
        self.args
    }
}

/// One link of the opcodes chain as the engine started it: the opcodes
/// the extension declared, and its handler.
#[derive(Debug, Clone, Copy)]
pub struct OpcodeLink {
    pub(crate) declared: Opcodes,
    pub(crate) handler: OpcodeHandler,
}

/// What a link of the opcodes chain is handed: the frame standing at
/// the opline, and its opcode.
#[derive(Clone, Copy)]
pub struct OpcodeArgs {
    pub(crate) frame: FrameRef,
    pub(crate) opcode: Op,
}

impl ChainKind for Opcode {
    type Answer = ();
}

impl sealed::Kind for Opcode {
    type Link = OpcodeLink;
    type Args<'a> = OpcodeArgs;

    /// The link told where it declared the opcode, and passed over to
    /// the ones beneath where it did not, as php's handler for one
    /// opcode saves only that opcode's.
    fn link(link: OpcodeLink, req: &mut Request, beneath: Beneath<'_, Self>) {
        if link.declared.contains(beneath.args.opcode) {
            crate::observer::told(link.handler, req, beneath);
        } else {
            beneath.serve(req);
        }
    }

    fn own(_: &mut Request, _: OpcodeArgs) {}
}

impl Beneath<'_, Opcode> {
    /// The opcode's name as php names it,
    /// `zend_get_opcode_name(EX(opline)->opcode)`.
    #[must_use]
    pub fn opcode(&self) -> &'static str {
        self.args.opcode.name()
    }

    /// The frame standing at the opline.
    pub(crate) fn frame(&self) -> FrameRef {
        self.args.frame
    }
}
