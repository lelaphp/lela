//! The include area of a request: the primary file noted and
//! compiled, `include` and `require` in their four forms through
//! `zend_include_or_eval` and the stream layer, `eval()` and the strings
//! compiled for it, `stream_resolve_include_path()`, the warnings an open
//! that failed raises, and the list `get_included_files()` reads. Which
//! include is being opened, php's construct on the frame's opline, is
//! [`Include`]'s, whose field no file but this one reads.

use std::sync::Arc;

use php_compile::SourceKind;
use php_heap::Fatal;
use php_unit::PublishedUnit;

use crate::ProgramFile;
use crate::api::abi::{Beneath, chain};
use crate::api::raising::{Level, Thrown};
use crate::api::values::{ClassId, Owner, PhpString, Value};
use crate::exec::IncludeKind;
use crate::layer::{IncludeFile, IncludeReadHook};
use crate::supplied::chain::{FileArgs, StringArgs};
use crate::unit_cache::CompileRefused;

use super::{HandlePath, Opened, Request, Unresolved, is_wrapper_url};

/// The request's code facet (design §4.4): the code the request compiles
/// and serves beside its main file, includes and their resolution,
/// `eval`, a string served, the unit cache and preloading, and what it
/// serves at ticks, at shutdown and around calls, with the code a hook is
/// served as. [`Request::code`] reads it and [`Request::code_mut`]
/// changes it; either is the whole request, since nearly every operation
/// can serve PHP code.
pub struct Code<R>(pub(crate) R);

impl Request {
    /// The code facet, to read.
    #[must_use]
    pub fn code(&self) -> Code<&Request> {
        Code(self)
    }

    /// The code facet, to change.
    pub fn code_mut(&mut self) -> Code<&mut Request> {
        Code(self)
    }
}

/// Which include is being opened, and the files the request holds open,
/// whose fields no file but this one reads.
#[derive(Default)]
pub(super) struct Include {
    /// The include opening a file through a stream wrapper, and how many
    /// calls stood when it began: php's `get_active_function_name` names
    /// the construct (`include`, `require_once`) for what the stream layer
    /// raises under it, and a native a user-space wrapper serves from
    /// inside it is one call deeper.
    including: Option<(IncludeKind, usize)>,
    /// The files the request's `zend_file_handle`s hold open, in the
    /// order each was opened ([`Held`]).
    held: Vec<Held>,
    /// The include files listed so far ([`Listed`]), which names the next.
    listed: u64,
}

/// A file one of the request's `zend_file_handle`s holds open past the
/// read that compiled it, with no `O_CLOEXEC`, as php's `fopen` and
/// `zend_stream_open` open it, so a process the request execs inherits
/// it (#2458, #2492): the program's file as its host hands it over
/// ([`ProgramFile`]), and `auto_prepend_file`'s, `auto_append_file`'s and
/// the built-in server's router's as `php_execute_script_ex` and
/// `php_cli_server_dispatch_router` keep theirs ([`HandlePath::Kept`]);
/// and an include's stream from its read until its frame destroys it,
/// as `CG(open_files)` lists it ([`FileHandle`]).
struct Held {
    holder: Holder,
    until: Until,
}

/// What holds a [`Held`] file open.
enum Holder {
    /// A [`std::fs::File`] alone, which nothing reads.
    File { _file: std::fs::File },
    /// The stream layer's stream over the program, as
    /// `php_fopen_primary_script` opened it where opcache did not hold the
    /// file, with the `*opened_path` its opener set, which
    /// `EG(included_files)` holds the program under
    /// ([`StreamHooks::open_primary`](crate::layer::StreamHooks::open_primary)).
    Stream(PhpString),
    /// An include's file, listed where its frame put it ([`FileHandle`]).
    Include(Listed, IncludeFile),
}

/// An include's `zend_file_handle` as the frame that opened it holds it,
/// as `compile_filename`'s and `spl_autoload`'s C frames hold theirs: the
/// file [`StreamHooks::open`](crate::layer::StreamHooks::open) held, until
/// `open_file_for_scanning` lists it on `CG(open_files)` once
/// `zend_stream_fixup` has read it, and then its place there, which the
/// frame's `zend_destroy_file_handle` takes it off
/// ([`Request::close_stream`]). A fatal jumps past the destroy, so what it
/// leaves listed stands until `zend_stream_shutdown` closes it, oldest
/// first, before the request's resources ([`Request::close_listed_files`]).
/// The list holds the file, not the frame, so one a suspended fiber's
/// `spl_autoload` holds is closed there too, though no frame of that fiber
/// is left again:
///
/// ```php
/// class W { public $context; private $d = false; public $p;
///     function stream_open($p, $m, $o, &$op) { $this->p = $p; echo "open $p\n"; return true; }
///     function stream_read($n) { if ($this->d) return ''; $this->d = true;
///         return $this->p === 'ww://b' ? "<?php function strlen() {}" : "<?php echo 'ran {$this->p}', PHP_EOL; Fiber::suspend();"; }
///     function stream_eof() { return $this->d; }
///     function stream_stat() { return []; }
///     function stream_close() { echo "close {$this->p}\n"; }
///     function stream_set_option($o, $a, $b) { return false; } }
/// stream_wrapper_register('ww', 'W');
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// $a = new Fiber(fn () => spl_autoload('ww://A', '.inc'));
/// $a->start();
/// include 'ww://b';
/// ```
/// ```expect exit=255
/// open ww://a.inc
/// ran ww://a.inc
/// open ww://b
///
/// Fatal error: Cannot redeclare function strlen() in ww://b on line 1
/// shutdown
/// close ww://a.inc
/// close ww://b
/// ```
pub(super) enum FileHandle {
    Opened(IncludeFile),
    Listed(Listed),
}

/// An include file's place among the request's [`Held`] files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Listed(u64);

/// When the request closes a file it holds ([`Held`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Until {
    /// Once the program has been served: the host's
    /// `zend_destroy_file_handle` after `php_execute_script`, and
    /// `php_execute_script_ex`'s of `auto_prepend_file` and
    /// `auto_append_file` after the append file ran
    /// (`Z/main/main.c:2650-2656`), each before the shutdown functions.
    Served,
    /// As the request ends, after its shutdown functions
    /// ([`ProgramFile::Listed`], [`FileHandle`]).
    Ended,
}

impl Request {
    /// Whether the code being served is an include's open, which
    /// `php_verror` names by the construct (`include()`) as it names a
    /// native by the native: php reads the construct off a user frame's
    /// `INCLUDE_OR_EVAL` opline, so a native that compiles a file as a
    /// `require`, `phar_file_action`'s `zend_compile_file`, is named
    /// itself ([`Request::load_file`]).
    pub(crate) fn serving_include(&self) -> bool {
        self.include
            .including
            .is_some_and(|(_, depth)| depth == self.vm.depth())
            && !self.vm_serving_native()
    }

    /// The construct of the include being opened where the code being
    /// served is its open ([`Request::serving_include`]).
    pub(super) fn include_being_opened(&self) -> Option<IncludeKind> {
        self.include
            .including
            .filter(|_| self.serving_include())
            .map(|(kind, _)| kind)
    }

    /// `zend_compile_file` over the file at the full path `path`, named
    /// `written` where it was, through a `zend_file_handle` of the kind
    /// `handle` names: the links the bundle supplied
    /// ([`crate::api::abi::ExtensionBuilder::compile_file`]) entered from
    /// the top, and beneath them all the engine's cached compile
    /// ([`Request::cached_source`]). The `Err` is what a link threw; the
    /// unit or the compiler's refusal is the `Ok`, and nothing where the
    /// file was not there to read.
    ///
    /// What the compiler raises is raised at the included file's line,
    /// as php raises it inside `zend_include_or_eval`, whichever
    /// construct included it, and a once form serves the unit it compiled
    /// or nothing:
    ///
    /// ```php
    /// $f = __DIR__ . '/scope.inc';
    /// file_put_contents($f, "<?php\ndeclare(encoding='UTF-8');\nif (!function_exists('scoped')) { function scoped(\$a = 1, \$b) {} }\nreturn 1;\n");
    /// var_dump(include $f, require $f, include_once $f, require_once $f);
    /// unlink($f);
    /// ```
    /// ```expect
    ///
    /// Warning: declare(encoding=...) ignored because Zend multibyte feature is turned off by settings in %s on line 2
    ///
    /// Deprecated: scoped(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter in %s on line 3
    ///
    /// Warning: declare(encoding=...) ignored because Zend multibyte feature is turned off by settings in %s on line 2
    ///
    /// Deprecated: scoped(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter in %s on line 3
    /// int(1)
    /// int(1)
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn compile_file(
        &mut self,
        path: &[u8],
        written: Option<&[u8]>,
        handle: HandlePath,
    ) -> Result<Option<Result<PublishedUnit, CompileRefused>>, Thrown> {
        self.compile_through(FileArgs::new(path, written, handle))
    }

    /// [`Request::compile_file`] over the `zend_file_handle` `args`
    /// describe.
    fn compile_through(
        &mut self,
        args: FileArgs<'_>,
    ) -> Result<Option<Result<PublishedUnit, CompileRefused>>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let top = Beneath::<chain::CompileFile>::top(shared.supplied.compile_file.links(), args);
        let compiled = top.serve(self);
        // A delayed OOM the compile recorded is named at the site that
        // was executing as it compiled, the `include` php reads, before
        // the included file's frame stands above it.
        self.locate_oom();
        Ok(compiled?.map(|compiled| compiled.0))
    }

    /// `php_execute_script_ex` (`Z/main/main.c:2617`): the primary file
    /// into `EG(included_files)` under its expanded path before it runs,
    /// so a once form over the file being served answers `true`. A source
    /// the host handed over, `php -r` and stdin, has no path and is
    /// nowhere in the table, as php's `Standard input code` is not.
    pub(crate) fn note_primary_file(&mut self, script: &[u8]) {
        if let Some(real) = self.environment_mut().realpath(script) {
            // The memory limit's fatal is recorded and ends the request.
            self.note_included(real.as_os_str().as_encoded_bytes()).ok();
        }
    }

    /// The host's `zend_file_handle` of the program the request names,
    /// opened before `auto_prepend_file` is served, as the host opens it
    /// before `php_execute_script`, and with no `O_CLOEXEC`, as php's
    /// `fopen` and `zend_stream_open` open it, so a process the program
    /// execs holds it ([`ProgramFile`]); not opened where the host hands
    /// its name and opcache holds the file, which answers the open without
    /// making it, nor where the host hands it unopened, which its compile
    /// opens ([`ProgramFile::Unopened`]). An included file is closed once
    /// it has compiled, and is not held:
    ///
    /// ```php
    /// $inc = __DIR__ . '/held.inc'; file_put_contents($inc, '<?php ');
    /// include $inc;
    /// $listed = shell_exec('ls -iLd /dev/fd/* 2>/dev/null');
    /// $held = fn ($path) => preg_match_all('/^ *' . fileinode($path) . ' /m', $listed);
    /// var_dump($held(__FILE__), $held($inc));
    /// unlink($inc);
    /// ```
    /// ```expect process
    /// int(1)
    /// int(0)
    /// ```
    pub(crate) fn open_primary_file(&mut self) {
        let Some(script) = self.description.script_name.clone() else {
            return;
        };
        let program_file = self.description.program_file;
        let until = match program_file {
            ProgramFile::Opened => return self.hold_file(&script, Until::Served),
            ProgramFile::Unopened => return,
            _ if self.accelerator_holds(&script) => return,
            ProgramFile::Named => Until::Served,
            ProgramFile::Listed => Until::Ended,
        };
        let Some(hooks) = self.stream_hooks() else {
            return self.hold_file(&script, until);
        };
        match (hooks.open_primary)(self, &script) {
            Ok(Some(path)) => self.include.held.push(Held {
                holder: Holder::Stream(path),
                until,
            }),
            Ok(None) => {}
            Err(thrown) => self.leave_pending(&thrown),
        }
    }

    /// The `*opened_path` of the program's stream, where
    /// [`Request::open_primary_file`] opened one.
    pub(super) fn primary_opened_path(&self) -> Option<PhpString> {
        self.include.held.iter().find_map(|held| match held.holder {
            Holder::Stream(opened_path) => Some(opened_path),
            Holder::File { .. } | Holder::Include(..) => None,
        })
    }

    /// [`Beneath::read`](crate::api::abi::Beneath::read) over the file
    /// `args` name.
    pub(crate) fn read_file_source(&mut self, args: FileArgs<'_>) -> Option<Vec<u8>> {
        if let Some(source) = args.source {
            return Some(source.to_vec());
        }
        let source = crate::unit_cache::source_or_read(None, args.path)?.into_owned();
        if args.opens_stream() {
            let full = crate::unit_cache::full_path(args.path, &self.cwd);
            self.open_include_stream(&full, args.handle);
        }
        Some(source)
    }

    /// The file at the full path `path`, which a `zend_compile_file`
    /// link's stream holds open in the place of a `zend_file_handle` of
    /// the kind `handle` names, held as long as that `zend_file_handle`
    /// stands: until the program has been served for one php keeps
    /// ([`HandlePath::Kept`]), and not past the compile for any other.
    pub(crate) fn hold_opened(&mut self, path: &[u8], handle: HandlePath) {
        if handle == HandlePath::Kept {
            self.hold_file(path, Until::Served);
        }
    }

    /// The file at the full path `path` opened and held, inherited by a
    /// process the request execs, until `until` ([`Held`]); a file that
    /// does not open is not held, as the read that compiled it has
    /// answered for it.
    fn hold_file(&mut self, path: &[u8], until: Until) {
        let path = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path);
        if let Ok(file) = std::fs::File::open(path)
            && php_process::inherit_fd(std::os::fd::AsFd::as_fd(&file)).is_ok()
        {
            self.include.held.push(Held {
                holder: Holder::File { _file: file },
                until,
            });
        }
    }

    /// The `zend_destroy_file_handle`s once the program has been served,
    /// before the request shuts down: the host's of the program's file,
    /// and `php_execute_script_ex`'s of `auto_prepend_file`,
    /// `auto_append_file` and the router; a program's file php-fpm leaves
    /// listed is closed with the request ([`ProgramFile::Listed`]).
    pub(crate) fn close_served_files(&mut self) {
        self.close_held(Until::Served);
    }

    /// `shutdown_executor`'s `zend_stream_shutdown`, first in it
    /// (`Z/Zend/zend_execute_API.c:410`): `CG(open_files)` destroyed, so
    /// a program's file php-fpm left listed is closed and its
    /// `zend_file_handle`'s buffer, element and name freed ([`ProgramFile::Listed`]), after
    /// the shutdown functions and the destructors; and so is every
    /// include's file a fatal left listed ([`FileHandle`]), oldest first,
    /// before the request's resources would close them newest first. An
    /// error handler that includes a file whose compile is fatal leaves two:
    ///
    /// ```php
    /// class W { public $context; private $d = false; public $p;
    ///     function stream_open($p, $m, $o, &$op) { $this->p = $p; echo "open $p\n"; return true; }
    ///     function stream_read($n) { if ($this->d) return ''; $this->d = true;
    ///         return $this->p === 'ww://b' ? "<?php function strlen() {}" : "<?php function f(\$a = 1, \$b) {} echo 'ran a', PHP_EOL;"; }
    ///     function stream_eof() { return $this->d; }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close {$this->p}\n"; }
    ///     function stream_set_option($o, $a, $b) { return false; }
    ///     function __destruct() { echo "dtor {$this->p}\n"; } }
    /// stream_wrapper_register('ww', 'W');
    /// set_error_handler(function ($no, $str) { echo "handler: $str\n"; include 'ww://b'; return true; });
    /// register_shutdown_function(function () { echo "shutdown\n"; });
    /// $r = fopen('ww://r', 'r');
    /// include 'ww://a';
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// open ww://r
    /// open ww://a
    /// handler: f(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter
    /// open ww://b
    ///
    /// Fatal error: Cannot redeclare function strlen() in ww://b on line 1
    /// shutdown
    /// close ww://a
    /// close ww://b
    /// close ww://r
    /// ```
    pub(crate) fn close_listed_files(&mut self) {
        self.close_held(Until::Ended);
        // `zend_try` around `zend_stream_shutdown`.
        self.catch_bailout();
        self.destroy_primary_handle();
    }

    /// Every held file that closes by `until`, in the order each was
    /// opened, a stream through the layer, until a close is fatal: that
    /// `zend_bailout` leaves the walk, and a stream the rest held is left
    /// to the request's resources, which close it newest first:
    ///
    /// ```php
    /// class W { public $context; private $d = false; public $p;
    ///     function stream_open($p, $m, $o, &$op) { $this->p = $p; echo "open $p\n"; return true; }
    ///     function stream_read($n) { if ($this->d) return ''; $this->d = true;
    ///         return $this->p === 'ww://b' ? "<?php function strlen() {}" : "<?php function f(\$a = 1, \$b) {} echo 'ran a', PHP_EOL;"; }
    ///     function stream_eof() { return $this->d; }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close {$this->p}\n"; @trigger_error("Bail {$this->p}", E_USER_ERROR); }
    ///     function stream_set_option($o, $a, $b) { return false; } }
    /// stream_wrapper_register('ww', 'W');
    /// set_error_handler(function ($no, $str) { if ($no == E_USER_ERROR) return false; echo "handler: $str\n"; include 'ww://b'; return true; });
    /// $r = fopen('ww://r', 'r');
    /// include 'ww://a';
    /// ```
    /// ```expect exit=255
    /// open ww://r
    /// open ww://a
    /// handler: f(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter
    /// open ww://b
    ///
    /// Fatal error: Cannot redeclare function strlen() in ww://b on line 1
    /// close ww://a
    ///
    /// Fatal error: Cannot redeclare function strlen() in ww://b on line 1
    /// close ww://b
    ///
    /// Fatal error: Bail ww://b in %SCRATCH%/witness.php on line 8
    /// close ww://r
    ///
    /// Fatal error: Bail ww://r in %SCRATCH%/witness.php on line 8
    /// ```
    fn close_held(&mut self, until: Until) {
        let held = std::mem::take(&mut self.include.held);
        let entered = self.diagnostics().bailing_out();
        for held in held {
            if until == Until::Served && held.until == Until::Ended {
                self.include.held.push(held);
                continue;
            }
            let left = !entered && self.diagnostics().bailing_out();
            let Some(hooks) = self.stream_hooks().filter(|_| !left) else {
                continue;
            };
            match held.holder {
                Holder::File { .. } => {}
                Holder::Stream(_) => (hooks.close_primary)(self),
                Holder::Include(_, file) => (hooks.close_include)(self, file),
            }
        }
    }

    /// `php_execute_script`'s `expand_filepath` of the primary file
    /// (`Z/main/main.c:2610-2619`), before `auto_prepend_file` runs: the
    /// file the request names made whole against its working directory
    /// with its links resolved, which is the name the unit compiles under
    /// and the page `php_statpage` stats, and put in `EG(included_files)`
    /// there only where the host opened it as a `FILE *`, the CLI's
    /// ([`ProgramFile::noted_before_prepend`]); php-cgi's and php-fpm's
    /// `zend_file_handle` carries an `opened_path` and the built-in
    /// server's is a bare name, so the table takes theirs as it compiles,
    /// after the prepend file has been served
    /// ([`Request::note_compiled_program`]); the name as given where it
    /// expands to nothing. A source the host
    /// `served` under a name is the file php would have opened by it, its
    /// name expanded into the table and compiled under, so `__FILE__` and
    /// a diagnostic's file are the table's path (#1569); one named
    /// `Standard input code`, or nothing, is in no table.
    ///
    /// ```php
    /// var_dump(count(get_included_files()), __FILE__ === get_included_files()[0], __FILE__);
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// string(%d) "%SCRATCH%/witness.php"
    /// ```
    pub(crate) fn expand_primary_file(&mut self, served: bool) {
        let script = self.description.script_name.clone();
        if served {
            if let Some(script) = script.filter(|name| name != b"Standard input code")
                && let Some(expanded) = self.environment_mut().expand_filepath(&script)
            {
                self.note_included(&expanded).ok();
                self.description.script_name = Some(expanded);
            }
            return;
        }
        let script = script.unwrap_or_default();
        let expanded = self
            .environment_mut()
            .expand_filepath(&script)
            .unwrap_or(script);
        if self.description.program_file.noted_before_prepend() {
            self.note_included(&expanded).ok();
        }
        self.description.script_name = Some(expanded);
    }

    /// `zend_execute_script`'s `zend_hash_add_empty_element` of the
    /// `zend_file_handle`'s `opened_path` once the program has compiled
    /// (`Z/Zend/zend.c:1971-1974`): the program's file in
    /// `EG(included_files)` under its expanded name, after what
    /// `auto_prepend_file` included, for every host but the CLI, whose
    /// `FILE *` put it there before ([`Request::expand_primary_file`]).
    /// From the prepend file and from the program, `pre.php` and `t.php`
    /// each print the table's names, under the CLI and under the built-in
    /// server:
    ///
    /// ```php
    /// $d = realpath(sys_get_temp_dir()) . '/noted' . getmypid(); @mkdir($d);
    /// $list = '<?php echo basename(__FILE__), " ", implode(",", array_map("basename", get_included_files())), "\n";';
    /// file_put_contents("$d/pre.php", $list); file_put_contents("$d/t.php", $list);
    /// $p = proc_open([PHP_BINARY, '-n', '-d', "auto_prepend_file=$d/pre.php", "$d/t.php"], [1 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]); proc_close($p);
    /// $p = proc_open([PHP_BINARY, '-n', '-d', "auto_prepend_file=$d/pre.php", '-S', '127.0.0.1:0', '-t', $d], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $ready = [$pipes[2]]; $none = null;
    /// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// echo file_get_contents("http://127.0.0.1:$port/t.php");
    /// proc_terminate($p); proc_close($p);
    /// exec('rm -rf ' . escapeshellarg($d));
    /// ```
    /// ```expect process
    /// pre.php t.php,pre.php
    /// t.php t.php,pre.php
    /// pre.php pre.php
    /// t.php pre.php,t.php
    /// ```
    ///
    /// php-cgi's and php-fpm's `opened_path` is a string another place
    /// holds already, opcache's `script->filename` where opcache served
    /// the file and the stream's where `php_fopen_primary_script` opened
    /// it, so the table's key costs nothing more; the built-in server's
    /// is its compile's own.
    pub(crate) fn note_compiled_program(&mut self) {
        let program_file = self.description.program_file;
        if program_file.noted_before_prepend() {
            return;
        }
        let Some(script) = self.description.script_name.clone() else {
            return;
        };
        // The memory limit's fatal is recorded and ends the request.
        if matches!(program_file, ProgramFile::Named | ProgramFile::Listed) {
            let held = match self.primary_opened_path() {
                Some(opened_path) => {
                    self.addref(&Value::string(opened_path));
                    Ok(opened_path)
                }
                None => self.new_interned(&script).map(Owner::hand_on),
            };
            held.and_then(|held| self.note_included_held(held)).ok();
        } else {
            self.note_included(&script).ok();
        }
    }

    /// `zend_include_or_eval` (`Z/Zend/zend_execute.c:5287`) for the four
    /// file constructs: the once forms resolve the path first
    /// ([`Code::resolve_path`]) and answer [`Opened::Already`] for one
    /// the table has, then put the path in the table before the file is
    /// compiled, so a file whose compile is refused is in the table
    /// still; the other two compile first, `compile_filename`, and put
    /// the path in the table after (`Z/Zend/zend_language_scanner.l:709`).
    /// The file is compiled once and published through the unit cache
    /// (design §6.3), as opcache keeps it, and every request after
    /// reads the same unit; it is looked for first under the path as
    /// written, and served without the path resolved where the answer
    /// for it trusts the hit ([`Request::held_as_written`]).
    ///
    /// A path nothing opens is `zend_stream_open`'s warning, the stream
    /// layer's `Failed to open stream` with the construct and the path
    /// as written before it (`php_verror`, `Z/main/main.c:1096`), then
    /// `zend_message_dispatcher`'s (`Z/main/main.c:1746`): the warning
    /// `Failed opening '%s' for inclusion` and [`Opened::Failed`] under
    /// an `include`, php's `Error` `Failed opening required '%s'` under
    /// a `require`, each naming `include_path` as the ini has it. A
    /// path with a NUL byte in it is the second message alone.
    ///
    /// A file the compiler refuses is php's `ParseError`, thrown at the
    /// file and line the parser stopped at, which the includer may
    /// catch; what the compiler raised before it was refused is raised
    /// first. A refusal after the parse, `E_COMPILE_ERROR`, ends the
    /// request as it ends one for the primary file.
    pub(crate) fn include_file(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
    ) -> Result<Opened, Thrown> {
        let handle = if kind.once() {
            HandlePath::Opened
        } else {
            HandlePath::Filename
        };
        self.include_through(kind, written, handle)
    }

    /// `zend_execute_script(ZEND_REQUIRE, NULL, file)` over
    /// `auto_prepend_file` or `auto_append_file` (`Z/main/main.c:2638-2646`):
    /// required as [`Request::include_file`] requires a file, through a
    /// `zend_file_handle` that `php_execute_script_ex` destroys only once
    /// the program and the append file are served, so the file it opened is
    /// held open, and inherited by a process the request execs, until
    /// then; where the unit cache answers the file by the path as written
    /// it opens nothing, as opcache's lookup by its key opens nothing, and
    /// nothing is held ([`HandlePath::Kept`]):
    ///
    /// ```php
    /// $d = sys_get_temp_dir() . '/kept' . getmypid(); @mkdir($d);
    /// $lib = '<?php function held($f) { $l = shell_exec("ls -iLd /dev/fd/* 2>/dev/null"); return preg_match_all("/^ *" . fileinode($f) . " /m", $l); }';
    /// file_put_contents("$d/lib.php", $lib);
    /// file_put_contents("$d/pre.php", '<?php require __DIR__ . "/lib.php"; register_shutdown_function(fn () => print("shutdown " . held(__FILE__) . "\n")); echo "prepend ", held(__FILE__), "\n";');
    /// file_put_contents("$d/app.php", '<?php echo "append ", held(__DIR__ . "/pre.php"), held(__FILE__), "\n";');
    /// file_put_contents("$d/t.php", '<?php echo "program ", held(__DIR__ . "/pre.php"), "\n";');
    /// $p = proc_open([PHP_BINARY, '-n', '-d', "auto_prepend_file=$d/pre.php", '-d', "auto_append_file=$d/app.php", "$d/t.php"], [1 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]); proc_close($p);
    /// exec('rm -rf ' . escapeshellarg($d));
    /// ```
    /// ```expect process
    /// prepend 1
    /// program 1
    /// append 11
    /// shutdown 0
    /// ```
    pub(crate) fn require_kept(&mut self, written: &[u8]) -> Result<Opened, Thrown> {
        self.include_through(IncludeKind::Require, written, HandlePath::Kept)
    }

    /// [`Request::include_file`] through a `zend_file_handle` of the kind
    /// `handle` names.
    ///
    /// A file that resolves and then does not read compiles to nothing,
    /// and the include fails as one nothing opens; the reason the stream
    /// warning gives is #2766's:
    ///
    /// ```php
    /// $f = __DIR__ . '/locked.inc';
    /// file_put_contents($f, '<?php return 1;'); chmod($f, 0);
    /// var_dump(include $f, include_once $f);
    /// try { require $f; } catch (Error $e) { echo get_class($e), "\n"; }
    /// chmod($f, 0644); unlink($f);
    /// ```
    /// ```expect
    ///
    /// Warning: include(%s/locked.inc): Failed to open stream: %s in %s on line 4
    ///
    /// Warning: include(): Failed opening '%s/locked.inc' for inclusion (include_path='%s') in %s on line 4
    ///
    /// Warning: include_once(%s/locked.inc): Failed to open stream: %s in %s on line 4
    ///
    /// Warning: include_once(): Failed opening '%s/locked.inc' for inclusion (include_path='%s') in %s on line 4
    /// bool(false)
    /// bool(false)
    ///
    /// Warning: require(%s/locked.inc): Failed to open stream: %s in %s on line 5
    /// Error
    /// ```
    fn include_through(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
        handle: HandlePath,
    ) -> Result<Opened, Thrown> {
        if written.contains(&0) {
            return self.failed_opening(kind, written);
        }
        let mut held = self.held_as_written(written);
        let resolved = match &held {
            Some((held, _)) => Ok(held.path.clone()),
            None => self.code_mut().resolve_path(written),
        };
        if kind.once()
            && held.is_none()
            && let Ok(path) = &resolved
        {
            held = self.file_once_key(written, path);
        }
        if kind.once()
            && let Ok(path) = &resolved
            && self.included_files.contains(path)
        {
            return Ok(Opened::Already);
        }
        let path = match resolved {
            // A stream the cache holds is served as a file it holds is.
            Ok(path) if held.is_some() || !is_wrapper_url(&path) => path,
            // php's `resolved_path = zend_string_copy(inc_filename)` for a
            // path it could not resolve, which a stream wrapper opens.
            Ok(path) => return self.include_stream(kind, written, &path),
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
            Err(_) if is_wrapper_url(written) => {
                return self.include_stream(kind, written, written);
            }
            Err(Unresolved::Missing(reason)) => {
                // The plain files wrapper is handed the path as written,
                // and asks the guard of it before it opens.
                let refused = self.include_guard(kind, written, written);
                return refused.unwrap_or_else(|| self.not_opened(kind, written, &reason, written));
            }
        };
        // A file the lookup by the key found is opened by nothing here.
        if held.is_none()
            && let Some(failed) = self.include_guard(kind, &path, written)
        {
            return failed;
        }
        if kind.once() {
            // `zend_stream_open` before `zend_hash_add_empty_element(&EG(
            // included_files), ...)`: a file the cache does not hold is
            // opened first, and one that cannot be opened is not
            // recorded, so a later once form tries it again.
            if held.is_none()
                && let Err(reason) = readable(&path)
            {
                return self.not_opened(kind, written, &reason, written);
            }
            self.note_included(&path)?;
        }
        // A unit the lookup by the key found, fresh, is opcache's answer
        // above every link; any other file is compiled through them all,
        // inside the include, as `zend_include_or_eval` calls
        // `zend_compile_file`. A stale one, discarded, is opened as a miss
        // opens it: an include's through the path as written again, a once
        // form's at the path it put in the table.
        let served = held.map(|(held, how)| self.held_answer(&held, how, written, handle));
        let served = served.transpose()?;
        // What the opening names where it fails, `file_handle->filename`:
        // a once form's stale file's the path it put in the table.
        let named = if matches!(served, Some(None)) {
            &path[..]
        } else {
            written
        };
        let cached = match served {
            Some(Some(answer)) => Some(answer),
            Some(None) if !kind.once() => return self.include_through(kind, written, handle),
            stale => {
                if stale.is_some()
                    && let Some(failed) = self.include_guard(kind, &path, named)
                {
                    return failed;
                }
                let outer = self.include.including.replace((kind, self.vm.depth()));
                let compiled = self.compile_file(&path, Some(written), handle);
                self.include.including = outer;
                compiled?
            }
        };
        match cached {
            Some(Ok(unit)) => {
                self.note_compiled_file(&path, &unit)?;
                Ok(Opened::Unit(unit))
            }
            Some(Err(refused)) => Err(self.refused_include(&refused, &path, SourceKind::File)),
            // The file resolved and could not be read: gone since the
            // stat, or not readable. php's plain-files opener fails with
            // the system's `errno`, which its warning prints, so the
            // reason is what opening the path answers now.
            None => self.not_opened(kind, named, &unreadable(&path), named),
        }
    }

    /// The stream `php_stream_open_for_zend_ex` opens over the plain file
    /// at the full path `path` that an include or `spl_autoload` compiles,
    /// or looks up by its full path in opcache's ([`Request::cached_file`]):
    /// a `stream` resource under the next id, freed when
    /// `zend_destroy_file_handle` closes it before the file runs, so the
    /// id is spent and `EG(regular_list)` grows as php's does, here from
    /// its packed table of eight to a hash at id 8, 160 bytes more that
    /// the list keeps:
    ///
    /// ```php
    /// $f = realpath(__DIR__) . '/spent.inc'; file_put_contents($f, '<?php $x = 1;');
    /// include $f;
    /// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, "\n";
    /// function g($f) { $a = 1; include $f; return $a; }
    /// $m = memory_get_usage(); g($f); echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); g($f); echo memory_get_usage() - $m, " ";
    /// echo get_resource_id(fopen('php://memory', 'r')), "\n";
    /// unlink($f);
    /// ```
    /// ```expect
    /// 0 160 0
    /// 376 0 12
    /// ```
    ///
    /// Under opcache the first include compiles the file and spends the
    /// one id, and every hit after it, served by the path as written,
    /// opens nothing:
    ///
    /// ```php
    /// $f = realpath(__DIR__) . '/spent.inc'; file_put_contents($f, '<?php $x = 1;');
    /// include $f;
    /// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); include $f; echo memory_get_usage() - $m, "\n";
    /// echo get_resource_id(fopen('php://memory', 'r')), "\n";
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 0 0 0
    /// 7
    /// ```
    ///
    /// A `zend_file_handle` php keeps past the compile, `handle`
    /// [`HandlePath::Kept`], holds the file open until the program has
    /// been served ([`Request::require_kept`]).
    ///
    /// A refusal has recorded the memory limit's fatal, which ends the
    /// request.
    pub(super) fn open_include_stream(&mut self, path: &[u8], handle: HandlePath) {
        if let Some(hooks) = self.stream_hooks() {
            (hooks.spend)(self);
        }
        if handle == HandlePath::Kept {
            self.hold_file(path, Until::Served);
        }
    }

    /// `php_plain_files_stream_opener`'s guard over the path an include
    /// of `written` opens (`Z/main/streams/plain_wrapper.c:1265`): the
    /// include failed where it refuses the path, its warning named by the
    /// construct, the resolved path, or the path as written where none
    /// resolved, and the stream's warning naming the same; none where it
    /// lets the path be opened.
    ///
    /// ```php
    /// ini_set('open_basedir', __DIR__);
    /// var_dump(include '/etc/hosts');
    /// ```
    /// ```expect
    /// Warning: include(): open_basedir restriction in effect. File(%s) is not within the allowed path(s): (%s) in %s on line 3
    ///
    /// Warning: include(%s): Failed to open stream: Operation not permitted in %s on line 3
    ///
    /// Warning: include(): Failed opening '/etc/hosts' for inclusion (include_path='%s') in %s on line 3
    /// bool(false)
    /// ```
    ///
    /// A file opcache's lookup by the path as written serves with nothing
    /// due is opened by nothing, so nothing guards it:
    ///
    /// ```php
    /// ini_set('opcache.revalidate_freq', 60);
    /// $f = __DIR__ . '/guarded.inc'; file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// var_dump(include $f);
    /// ini_set('open_basedir', '/nonexistent');
    /// var_dump(include $f);
    /// ```
    /// ```expect regime=jit-off
    /// int(1)
    /// int(1)
    /// ```
    ///
    /// and nor does one whose stat is due, which `do_validate_timestamps`
    /// stats without opening it and finds unchanged
    /// ([`Request::held_answer`]):
    ///
    /// ```php
    /// $f = __DIR__ . '/g.inc'; file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// var_dump(include $f);
    /// ini_set('open_basedir', '/nonexistent');
    /// var_dump(include $f);
    /// ```
    /// ```expect regime=jit-off
    /// int(1)
    /// int(1)
    /// ```
    ///
    /// A once form's file the key found and its stat finds changed is
    /// opened after the path went in the table, as php's
    /// `zend_include_or_eval` puts it there before `persistent_compile_file`
    /// validates it, so a refused open leaves the file included:
    ///
    /// ```php
    /// $f = realpath(__DIR__) . '/e1.inc'; file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// $g = realpath(__DIR__) . '/e2.inc'; file_put_contents($g, '<?php return 1;'); touch($g, time() - 60);
    /// var_dump(opcache_compile_file($f), opcache_compile_file($g));
    /// file_put_contents($g, '<?php return 2;'); touch($g, time() - 30);
    /// ini_set('open_basedir', '/nonexistent');
    /// var_dump(include_once $f, @include_once $g, in_array($g, get_included_files()), require_once $g);
    /// ```
    /// ```expect regime=jit-off
    /// bool(true)
    /// bool(true)
    /// int(1)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// ```
    ///
    /// Its failure names that path, as `file_handle->filename` is, and
    /// not the path as written:
    ///
    /// ```php
    /// chdir(__DIR__);
    /// file_put_contents('r2.inc', '<?php return 1;'); touch('r2.inc', time() - 60);
    /// var_dump(opcache_compile_file('r2.inc'));
    /// file_put_contents('r2.inc', '<?php return 2;'); touch('r2.inc', time() - 30);
    /// ini_set('open_basedir', '/nonexistent');
    /// var_dump(include_once 'r2.inc');
    /// ```
    /// ```expect regime=jit-off
    /// bool(true)
    ///
    /// Warning: include_once(): open_basedir restriction in effect. File(%s/r2.inc) is not within the allowed path(s): (/nonexistent) in %s on line 7
    ///
    /// Warning: include_once(%s/r2.inc): Failed to open stream: Operation not permitted in %s on line 7
    ///
    /// Warning: include_once(): Failed opening '%s/r2.inc' for inclusion (include_path='%s') in %s on line 7
    /// bool(false)
    /// ```
    fn include_guard(
        &mut self,
        kind: IncludeKind,
        path: &[u8],
        written: &[u8],
    ) -> Option<Result<Opened, Thrown>> {
        let outer = self.include.including.replace((kind, self.vm.depth()));
        let guarded = self.environment_mut().check_open_basedir(path, true);
        self.include.including = outer;
        Some(self.not_opened(kind, path, &guarded.err()?.into(), written))
    }

    /// An include of a path a stream wrapper serves, `zend_stream_open`'s
    /// `php_stream_open_for_zend_ex` under `USE_PATH | REPORT_ERRORS |
    /// STREAM_OPEN_FOR_INCLUDE`: the stream opened as its wrapper opens it,
    /// the warnings raised under it naming the construct, read whole, and
    /// compiled as a file under the path, held by the unit cache where
    /// opcache's predicate admits the path, a `phar://` one
    /// ([`FileArgs::cached`]), and the request's own otherwise. The
    /// path goes in the included-files table as the construct puts a
    /// file's there, before the compile for the once forms and after it
    /// for the others.
    ///
    /// ```php
    /// class M { public $context; private $s;
    ///     function stream_open($p, $m, $o, &$op) { $this->s = '<?php echo __FILE__, " ", ' . strlen($p) . ', "\n"; return 7;'; return true; }
    ///     function stream_read($n) { $r = $this->s; $this->s = ''; return $r; }
    ///     function stream_eof() { return $this->s === ''; }
    ///     function stream_stat() { return []; }
    ///     function stream_set_option($o, $a, $b) { return false; }
    ///     function url_stat($p, $f) { return []; } }
    /// stream_wrapper_register('mm', 'M'); $b = get_resources();
    /// var_dump(include 'mm://one', include_once 'mm://two', include_once 'mm://two');
    /// echo count(array_diff_key(get_resources(), $b)), " left open\n";
    /// set_include_path('mm://dir:' . get_include_path());
    /// var_dump(include 'three');
    /// set_include_path('.');
    /// set_error_handler(function ($no, $msg) { echo $msg, "\n"; return true; });
    /// var_dump(include 'data:,x');
    /// ```
    /// ```expect
    /// mm://one 8
    /// mm://two 8
    /// int(7)
    /// int(7)
    /// bool(true)
    /// 0 left open
    /// mm://dir/three 14
    /// int(7)
    /// include(): data:// wrapper is disabled in the server configuration by allow_url_include=0
    /// include(data:,x): Failed to open stream: no suitable wrapper could be found
    /// include(): Failed opening 'data:,x' for inclusion (include_path='.')
    /// bool(false)
    /// ```
    ///
    /// A fatal while the file is read leaves its stream open, since
    /// `zend_bailout` jumps past `zend_destroy_file_handle`: the stream is
    /// closed with the request's resources, its `stream_close` served.
    ///
    /// ```php
    /// class L { public $context;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_set_option($o, $a, $b) { return false; }
    ///     function stream_stat() { return []; }
    ///     function stream_read($c) { function f() {} }
    ///     function stream_close() { echo "closed\n"; } }
    /// stream_wrapper_register('ll', 'L');
    /// function f() {}
    /// include 'll://x';
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare function f() (previously declared in %s:9) in %s on line 6
    /// closed
    /// ```
    ///
    /// The stream is closed once the compile is done, as
    /// `zend_destroy_file_handle` follows `zend_compile_file`: a file whose
    /// close throws is among the included files, and a `ParseError`
    /// stands while the stream closes, so `zend_call_function` refuses
    /// the wrapper's `stream_close`.
    ///
    /// ```php
    /// class W { public $context; function url_stat($p, $f) { return ['size' => 10]; } function stream_open($p, $m, $o, &$op) { return true; } function stream_read($n) { static $d = false; if ($d) return ''; $d = true; return "<?php echo 'inc'\n"; } function stream_eof() { return true; } function stream_stat() { return ['size'=>10]; } function stream_close() { echo "close\n"; throw new Exception("close"); } function stream_set_option($o, $a, $b) { return false; } }
    /// stream_wrapper_register('ww', 'W');
    /// try { include 'ww://x'; } catch (Throwable $e) { echo $e::class, ": ", $e->getMessage(), " <- [", $e->getPrevious()?->getMessage(), "]\n"; }
    /// echo "after\n";
    /// ```
    /// ```expect
    /// ParseError: syntax error, unexpected end of file <- []
    /// after
    /// ```
    ///
    /// ```php
    /// class W { public $context; static $src; private $d = false;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_read($n) { if ($this->d) return ''; $this->d = true; return self::$src; }
    ///     function stream_eof() { return $this->d; }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close\n"; }
    ///     function stream_set_option($o, $a, $b) { return false; } }
    /// class X extends W { function stream_close() { echo "close x\n"; throw new Exception('x'); } }
    /// stream_wrapper_register('ww', 'W'); stream_wrapper_register('xx', 'X');
    /// W::$src = "<?php echo 'ran', PHP_EOL;";
    /// include 'ww://a';
    /// W::$src = "<?php echo 'not run';";
    /// try { include 'xx://b'; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// W::$src = "<?php echo 'x'; )";
    /// try { require_once 'ww://c'; } catch (ParseError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(array_slice(get_included_files(), 1));
    /// ```
    /// ```expect
    /// close
    /// ran
    /// close x
    /// x
    /// Unmatched ')'
    /// array(3) {
    ///   [0]=>
    ///   string(6) "ww://a"
    ///   [1]=>
    ///   string(6) "xx://b"
    ///   [2]=>
    ///   string(6) "ww://c"
    /// }
    /// ```
    ///
    /// A read that threw closes the stream with its throwable standing,
    /// so the wrapper's `stream_close` is refused, and the destructor the
    /// close serves runs whole and chains its throwable onto the read's,
    /// as `zend_objects_destroy_object` does:
    ///
    /// ```php
    /// class W { public $context;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_read($n) { throw new Exception('read'); }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close\n"; }
    ///     function stream_set_option($o, $a, $b) { return false; }
    ///     function __destruct() { echo "dtor ", str_pad('a', 3, 'b'), "\n"; throw new Exception('dtor'); } }
    /// stream_wrapper_register('ww', 'W');
    /// try { include 'ww://x'; } catch (Exception $e) { echo $e->getMessage(), " <- ", $e->getPrevious()?->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// dtor abb
    /// dtor <- read
    /// ```
    ///
    /// What the compile raised is raised, and what it declares declared,
    /// before the stream closes, as `zend_compile_file` raises and declares
    /// inside the compile: a file whose close throws has its functions
    /// declared.
    ///
    /// ```php
    /// class W { public $context; static $src; private $d = false;
    ///     function stream_open($p, $m, $o, &$op) { return true; }
    ///     function stream_read($n) { if ($this->d) return ''; $this->d = true; return self::$src; }
    ///     function stream_eof() { return $this->d; }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close\n"; }
    ///     function stream_set_option($o, $a, $b) { return false; } }
    /// class X extends W { function stream_close() { echo "close x\n"; throw new Exception('x'); } }
    /// stream_wrapper_register('ww', 'W'); stream_wrapper_register('xx', 'X');
    /// W::$src = "<?php\nfunction f(\$a = 1, \$b) {}\necho 'ran', PHP_EOL;\n";
    /// include 'ww://a';
    /// W::$src = "<?php\nfunction g() {}\n";
    /// try { include 'xx://b'; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(function_exists('g'));
    /// ```
    /// ```expect
    /// Deprecated: f(): Optional parameter $a declared before required parameter $b is implicitly treated as a required parameter in ww://a on line 2
    /// close
    /// ran
    /// close x
    /// x
    /// bool(true)
    /// ```
    ///
    /// A once form opens the stream before it asks the table, by the
    /// `*opened_path` the opener set or the path, as `zend_include_or_eval`'s
    /// `ONCE` arm calls `zend_stream_open` first: a path the table holds is
    /// closed unread, and one that did not open is not put in it, so the
    /// next once form opens it again.
    ///
    /// ```php
    /// class W { public $context; private $d = false;
    ///     function stream_open($p, $m, $o, &$op) { echo "open $p\n"; if ($p === 'ww://b') $op = 'ww://a'; return $p !== 'ww://f'; }
    ///     function stream_read($n) { echo "read\n"; if ($this->d) return ''; $this->d = true; return "<?php echo 'ran ', __FILE__, PHP_EOL;"; }
    ///     function stream_eof() { return $this->d; }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close\n"; }
    ///     function stream_set_option($o, $a, $b) { return false; } }
    /// stream_wrapper_register('ww', 'W');
    /// include 'ww://a';
    /// var_dump(include_once 'ww://a', require_once 'ww://b');
    /// var_dump(@include_once 'ww://f', @include_once 'ww://f', count(get_included_files()));
    /// ```
    /// ```expect
    /// open ww://a
    /// read
    /// read
    /// close
    /// ran ww://a
    /// open ww://a
    /// close
    /// open ww://b
    /// close
    /// bool(true)
    /// bool(true)
    /// open ww://f
    /// open ww://f
    /// bool(false)
    /// bool(false)
    /// int(2)
    /// ```
    ///
    /// A read that fails, as one through a filter that answers
    /// `PSFS_ERR_FATAL` fails, is the file php could not open
    /// (`zend_stream_fixup`, [`crate::layer::StreamHooks::read_include`]).
    ///
    /// ```php
    /// class F extends php_user_filter { }
    /// stream_filter_register('f.fatal', F::class);
    /// var_dump(include 'php://filter/read=f.fatal/resource=' . __FILE__);
    /// ```
    /// ```expect
    /// Warning: include(): Unprocessed filter buckets remaining on input brigade in %s on line 4
    ///
    /// Warning: include(): Failed opening 'php://filter/read=f.fatal/resource=%s' for inclusion (include_path='%s') in %s on line 4
    /// bool(false)
    /// ```
    pub(crate) fn include_stream(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
        path: &[u8],
    ) -> Result<Opened, Thrown> {
        // Zend alone opens no wrapper's path (`zend_stream_open` with no
        // `zend_stream_open_function`).
        let Some(hooks) = self.stream_hooks() else {
            return self.failed_opening(kind, written);
        };
        let outer = self.include.including.replace((kind, self.vm.depth()));
        let opened = (hooks.open)(self, path, true);
        self.include.including = outer;
        let (file, named) = opened?.unzip();
        let mut file = file.map(FileHandle::Opened);
        let opened = file.as_mut().zip(named);
        let compiled = self.compile_stream(kind, written, opened, hooks.read_include);
        // What the close threw: the unit compiled before it is the
        // handler's `destroy_op_array` then.
        let closed = self.close_stream(hooks.close_include, file, compiled.as_ref().err());
        let Some(thrown) = closed else {
            return compiled;
        };
        if let Ok(Opened::Declared(unit)) = &compiled {
            self.release_unit_strings(Some(unit));
        }
        Err(thrown)
    }

    /// `zend_destroy_file_handle` over the file
    /// [`crate::layer::StreamHooks::open`] held, `file`, where it opened,
    /// `close`, once what came before it threw `thrown` or nothing: the
    /// throwable standing in `EG(exception)`, so the wrapper's
    /// `stream_close` is refused as `zend_call_function` refuses it and its
    /// destructor chains onto it, and otherwise what the close threw, which
    /// the layer kept. A fatal ending the request jumps past it: a file
    /// listed stays listed ([`FileHandle`]), and one not yet listed leaves
    /// its stream among the request's resources.
    pub(super) fn close_stream(
        &mut self,
        close: fn(&mut Request, IncludeFile),
        mut file: Option<FileHandle>,
        thrown: Option<&Thrown>,
    ) -> Option<Thrown> {
        let mut destroy = |req: &mut Request| {
            let file = file.take().filter(|_| !req.diagnostics().bailing_out());
            file.and_then(|file| req.unlist(file))
                .map(|file| close(req, file))
        };
        let Some(thrown) = thrown else {
            destroy(self);
            return self.stream_thrown.take();
        };
        if self.while_pending(thrown, &mut destroy).is_none() {
            destroy(self);
        }
        None
    }

    /// `open_file_for_scanning` over the file `file` holds:
    /// `zend_stream_fixup`, `read`, then the file put on `CG(open_files)`
    /// whether the read failed or not, as php lists it; a fatal in the
    /// read, a wrapper's callback's among them, jumps past the list and
    /// leaves the file's stream to the request's resources.
    fn scan(
        &mut self,
        file: &mut FileHandle,
        read: IncludeReadHook,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let FileHandle::Opened(opened) = file else {
            return Ok(None);
        };
        let source = read(self, opened)?;
        if self.diagnostics().bailing_out() {
            return Ok(source);
        }
        let listed = Listed(self.include.listed);
        self.include.listed += 1;
        if let FileHandle::Opened(opened) = std::mem::replace(file, FileHandle::Listed(listed)) {
            self.include.held.push(Held {
                holder: Holder::Include(listed, opened),
                until: Until::Ended,
            });
        }
        Ok(source)
    }

    /// `zend_destroy_file_handle`'s `zend_llist_del_element`: the file
    /// `file` holds, taken off `CG(open_files)` where it was listed.
    fn unlist(&mut self, file: FileHandle) -> Option<IncludeFile> {
        let listed = match file {
            FileHandle::Opened(file) => return Some(file),
            FileHandle::Listed(listed) => listed,
        };
        let held = &mut self.include.held;
        let at = held
            .iter()
            .position(|held| matches!(held.holder, Holder::Include(at, _) if at == listed))?;
        match held.remove(at).holder {
            Holder::Include(_, file) => Some(file),
            Holder::File { .. } | Holder::Stream(_) => None,
        }
    }

    /// [`Request::include_stream`] over the file the open held and the
    /// name it gave it, `opened`, before `zend_destroy_file_handle`: a once
    /// form's `EG(included_files)` asked by that name, `ZEND_FAKE_OP_ARRAY`
    /// for one it holds, which is read nothing of, and the name put in it
    /// before the compile otherwise; then `compile_file` over the file
    /// `read` whole and listed ([`FileHandle`]), compiled under the name,
    /// and its message where the open or the read failed.
    pub(super) fn compile_stream(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
        mut opened: Option<(&mut FileHandle, Vec<u8>)>,
        read: IncludeReadHook,
    ) -> Result<Opened, Thrown> {
        if kind.once()
            && let Some((_, named)) = &opened
        {
            if self.included_files.contains(named) {
                return Ok(Opened::Already);
            }
            self.note_included(named)?;
        }
        let source = match &mut opened {
            Some((file, _)) if self.stream_thrown.is_none() => {
                let outer = self.include.including.replace((kind, self.vm.depth()));
                let source = self.scan(file, read);
                self.include.including = outer;
                source?
            }
            _ => None,
        };
        // `compile_file` compiles nothing and reports the file it could not
        // open or read only where nothing is pending (`if (!EG(exception))`):
        // what the open or the read threw stands as the compile's.
        if let Some(thrown) = self.stream_thrown.take() {
            return Err(thrown);
        }
        let (Some((_, path)), Some(source)) = (opened, source) else {
            // A read a fatal ended is no file php reports: the bailout
            // jumps past `compile_file`'s message.
            if self.diagnostics().bailing_out() {
                return Ok(Opened::Failed);
            }
            return self.failed_opening(kind, written);
        };
        // Compiled through every `zend_compile_file` link, the stream's
        // bytes handed down as the stream php's handle holds.
        let args = FileArgs {
            source: Some(&source),
            ..FileArgs::new(&path, Some(written), HandlePath::Stream)
        };
        let outer = self.include.including.replace((kind, self.vm.depth()));
        let compiled = self.compile_through(args);
        self.include.including = outer;
        let unit = match compiled? {
            Some(Ok(unit)) => unit,
            Some(Err(refused)) => {
                return Err(self.refused_include(&refused, &path, SourceKind::File));
            }
            None => return self.failed_opening(kind, written),
        };
        self.note_compiled_file(&path, &unit)?;
        // `zend_compile_file` raises what it recorded and declares the
        // file's functions and classes before the handle is destroyed.
        if !crate::engine::note_compiled(self, &unit, SourceKind::File) {
            return Err(Thrown::of(self.take_pending_view()));
        }
        Ok(Opened::Declared(unit))
    }

    /// `zend_include_or_eval`'s `ZEND_EVAL` arm
    /// (`Z/Zend/zend_execute.c:5354`): the string compiled as
    /// `compile_string` compiles it, from after an open tag and ending in
    /// `RETURN null` ([`SourceKind::EVAL`]), under the name
    /// `zend_make_compiled_string_description` gives it, the file and
    /// line the `eval` stands at and `: eval()'d code`, which is what
    /// `__FILE__` folds to inside it and what a diagnostic raised in it
    /// names. The unit is this request's own, compiled into its memory
    /// and never published ([`Shared::compile_for`]). An empty string is
    /// `compile_string`'s `NULL` with nothing raised, which the handler
    /// answers `false` for ([`Opened::Failed`]); a string the compiler
    /// refuses is raised as an included file's refusal is, at the name
    /// the unit would have carried.
    ///
    /// ```php
    /// eval("echo __FILE__, ' ', __LINE__, \"\\n\";\necho \$nope;");
    /// ```
    /// ```expect
    /// %s(2) : eval()'d code 1
    ///
    /// Warning: Undefined variable $nope in %s(2) : eval()'d code on line 2
    /// ```
    ///
    /// A parse error in the string is the `ParseError` `zendparse`
    /// throws, at the string's name and the line it stopped on, which
    /// the caller may catch; one nothing catches ends the request as
    /// php's `Parse error`. A refusal after the parse is the
    /// `E_COMPILE_ERROR` that ends the request.
    ///
    /// ```php
    /// try { eval("\n1 +;"); } catch (ParseError $e) { echo get_class($e), ": ", $e->getMessage(), " in ", $e->getFile(), ":", $e->getLine(), "\n"; }
    /// eval('$a = ;');
    /// ```
    /// ```expect exit=255
    /// ParseError: syntax error, unexpected token ";" in %s(2) : eval()'d code:2
    ///
    /// Parse error: syntax error, unexpected token ";" in %s(3) : eval()'d code on line 1
    /// ```
    ///
    /// ```php
    /// eval('break;');
    /// ```
    /// ```expect exit=255
    /// Fatal error: 'break' not in the 'loop' or 'switch' context in %s(2) : eval()'d code on line 1
    /// ```
    ///
    /// The unit lives as long as what holds it: the frame while the
    /// string runs, and after it the functions, classes and closures
    /// the string declared, which the request keeps as it keeps a file's.
    ///
    /// ```php
    /// $f = eval('function g() { return "g"; } class K { const C = "k"; } return fn() => "closure";');
    /// var_dump(g(), K::C, $f(), function_exists('g'));
    /// ```
    /// ```expect
    /// string(1) "g"
    /// string(1) "k"
    /// string(7) "closure"
    /// bool(true)
    /// ```
    pub(crate) fn eval_string(
        &mut self,
        code: &[u8],
        (file, line): (&[u8], u32),
    ) -> Result<Opened, Thrown> {
        if code.is_empty() {
            return Ok(Opened::Failed);
        }
        let name = self.eval_name(file, line)?;
        match self.compile_eval(code, name.as_bytes()) {
            Ok(unit) => {
                self.name_unit(&unit, name);
                Ok(Opened::Unit(unit))
            }
            Err(thrown) => {
                self.release(crate::api::values::Value::string(name));
                Err(thrown)
            }
        }
    }

    /// `zend_make_compiled_string_description`'s `%s(%d) : %s` for an
    /// `eval` at `line` of `file`, the op array and the opline its
    /// `INCLUDE_OR_EVAL` stands at, which are what
    /// `zend_get_executed_filename` and `zend_get_executed_lineno` read
    /// there: a string of the request's, one allocation of its length
    /// with the file and the line written in place, which the unit
    /// compiled under it holds as its file's name
    /// ([`Request::name_unit`]). The `Err` is the memory limit.
    fn eval_name(&mut self, file: &[u8], line: u32) -> Result<PhpString, Thrown> {
        const TAIL: &[u8] = b") : eval()'d code";
        let line = php_heap::LongText::long(i64::from(line));
        let parts = [file, b"(", line.as_bytes(), TAIL];
        let len = parts.iter().map(|part| part.len()).sum();
        self.string_with(len, |mut into| {
            for part in parts {
                let Some((head, rest)) = into.split_at_mut_checked(part.len()) else {
                    break;
                };
                head.copy_from_slice(part);
                into = rest;
            }
        })
    }

    /// `zend_compile_string`, every string compiled as an `eval`
    /// compiles it: the links the bundle supplied
    /// ([`ExtensionBuilder::compile_string`]) from the top, each serving
    /// the one beneath where it will, and php's own `compile_string`
    /// beneath them all, its scanner begun where `position` says, after
    /// an open tag for an `eval` ([`SourceKind::EVAL`]). The `Err` is
    /// what a link threw; the unit, or the compiler's refusal for the
    /// caller to raise, is the `Ok`.
    ///
    /// [`ExtensionBuilder::compile_string`]: crate::api::abi::ExtensionBuilder::compile_string
    pub(crate) fn compile_string(
        &mut self,
        code: &[u8],
        name: &[u8],
        position: php_compile::syntax::Position,
    ) -> Result<Result<PublishedUnit, CompileRefused>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let args = StringArgs::new(code, name, position);
        let top =
            Beneath::<chain::CompileString>::top(shared.supplied.compile_string.links(), args);
        let compiled = top.serve(self);
        self.locate_oom();
        Ok(compiled?.0)
    }

    /// `f` over a string the host evaluates of its own, `-r`'s code, the
    /// shell's and a debugger's, compiled as `zend_eval_stringl` compiles
    /// it: `CG(compiler_options)` set to `ZEND_COMPILE_DEFAULT_FOR_EVAL`
    /// around the compile and put back after
    /// (`Z/Zend/zend_execute_API.c:1349-1352`), so its oplines carry no
    /// extended info whatever the engine's, and an `eval` the string
    /// serves compiles under the engine's again.
    pub(crate) fn host_string<T>(&mut self, f: impl FnOnce(&mut Request) -> T) -> T {
        let outer = std::mem::replace(&mut self.compiling_host_string, true);
        let compiled = f(self);
        self.compiling_host_string = outer;
        compiled
    }

    /// `served`, the serving of a string the host evaluates of its own
    /// once it compiled, as `zend_eval_stringl` serves it: with
    /// `EG(no_extensions)` set around its `zend_execute`, so no Zend
    /// extension is told of a statement it reaches, its own or a
    /// function's it calls (`Z/Zend/zend_execute_API.c:1357`, `:1382`).
    /// As php's, the word is cleared after, not put back, and a fatal,
    /// returned or bailing out, leaves it set, as `zend_bailout` leaves
    /// past the clearing.
    pub(crate) fn serve_host_string<T>(
        &mut self,
        served: impl FnOnce(&mut Request) -> Result<T, Fatal>,
    ) -> Result<T, Fatal> {
        self.no_extensions = true;
        let served = served(self);
        if served.is_ok() && !self.diagnostics().bailing_out() {
            self.no_extensions = false;
        }
        served
    }

    /// php's own `compile_string`, beneath every link of the pointer:
    /// `source` compiled from `position` under `name` as the request's
    /// own unit.
    pub(crate) fn compile_string_own(
        &mut self,
        source: &[u8],
        name: &[u8],
        position: php_compile::syntax::Position,
    ) -> Result<PublishedUnit, CompileRefused> {
        Arc::clone(&self.shared).compile_for(self, source, name, SourceKind::Eval(position))
    }

    /// [`Request::compile_string`] with the compiler's refusal raised as
    /// an included file's is, at `name`.
    pub(crate) fn compile_eval(
        &mut self,
        code: &[u8],
        name: &[u8],
    ) -> Result<PublishedUnit, Thrown> {
        self.compile_string(code, name, php_compile::syntax::Position::AfterOpenTag)?
            .map_err(|refused| self.refused_include(&refused, name, SourceKind::EVAL))
    }

    /// The `zend_resolve_path` beneath every override: the stream layer's
    /// `php_resolve_path` over the ini's `include_path`
    /// ([`crate::layer::StreamHooks::resolve_path`]), or, with no stream
    /// layer, Zend's own `VCWD_REALPATH`.
    pub(crate) fn resolve_path_own(&mut self, written: &[u8]) -> Result<Vec<u8>, Unresolved> {
        match self.stream_hooks() {
            Some(hooks) => (hooks.resolve_path)(self, written),
            // `zend_resolve_path_func`: `VCWD_REALPATH` and nothing else.
            None => self.realpath_file(written).map_err(Unresolved::Missing),
        }
    }

    /// The stream layer's hooks, as the supplied table resolved them
    /// ([`crate::layer::StreamHooks`]).
    pub(crate) fn stream_hooks(&self) -> Option<crate::layer::StreamHooks> {
        self.shared.supplied.stream_hooks.get()
    }

    /// `tsrm_realpath` of a path that is a regular file: the canonical
    /// path, against the request's working directory where the path is
    /// relative.
    pub(crate) fn realpath_file(&mut self, path: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        // `virtual_file_ex` sets `errno = ENOENT` for a path it could not
        // resolve.
        const ENOENT: i32 = 2;
        let real = self
            .environment_mut()
            .realpath(path)
            .ok_or_else(|| std::io::Error::from_raw_os_error(ENOENT))?;
        if !real.is_file() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        Ok(real.as_os_str().as_encoded_bytes().to_vec())
    }

    /// The warning `php_stream_display_wrapper_errors` raises with
    /// `php_error_docref1`, the path its `estrdup` (`php_strip_url_passwd`)
    /// standing across the raise as the parameter `function` names.
    pub(crate) fn wrapper_warning(&mut self, function: &str, path: &[u8], text: &str) {
        let copy = self.heap_mut().block(path.len() + 1);
        let Ok(copy) = self.fatal_of(copy) else {
            return;
        };
        let origin = crate::api::raising::Origin::Function(function);
        crate::api::raising::verror(self, Level::Warning, origin, path, text.as_bytes());
        self.heap_mut().free_block(copy);
    }

    /// The include of `written` failed where the stream it opens at
    /// `named` does not open: `php_stream_open_wrapper`'s warning under
    /// `REPORT_ERRORS`, the `Failed to open stream: %s` of
    /// `php_stream_display_wrapper_errors` with `strerror(errno)`, raised
    /// through `php_error_docref1` with the construct and `named` as its
    /// origin, then [`Request::failed_opening`].
    pub(super) fn not_opened(
        &mut self,
        kind: IncludeKind,
        named: &[u8],
        reason: &std::io::Error,
        written: &[u8],
    ) -> Result<Opened, Thrown> {
        let reason = reason.to_string();
        let reason = reason
            .split_once(" (os error")
            .map_or(reason.as_str(), |(text, _)| text);
        let opener = self.opener(kind);
        let named = crate::c_number::c_str(named);
        self.wrapper_warning(&opener, named, &format!("Failed to open stream: {reason}"));
        self.failed_opening(kind, written)
    }

    /// `php_message_handler_for_zend`'s `ZMSG_FAILED_INCLUDE_FOPEN` and
    /// `ZMSG_FAILED_REQUIRE_FOPEN` (`Z/main/main.c:1746`): the warning
    /// and `false` for an `include`, the `Error` for a `require`, each
    /// naming the path as written, up to a NUL byte in it as its
    /// `estrdup` reads it, and `include_path` as the ini has it.
    ///
    /// ```php
    /// set_include_path('.');
    /// var_dump(include "gone\0.php");
    /// try { require "gone\0.php"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Warning: include(): Failed opening 'gone' for inclusion (include_path='.') in %s on line 3
    /// bool(false)
    /// Failed opening required 'gone' (include_path='.')
    /// ```
    ///
    /// `opcache_compile_file` opens the path as C reads it, so its stream
    /// warns first:
    ///
    /// ```php
    /// set_include_path('.');
    /// var_dump(opcache_compile_file("gone\0.php"));
    /// ```
    /// ```expect regime=jit-off
    /// Warning: opcache_compile_file(gone): Failed to open stream: No such file or directory in %s on line 3
    ///
    /// Warning: opcache_compile_file(): Failed opening 'gone' for inclusion (include_path='.') in %s on line 3
    /// bool(false)
    /// ```
    pub(super) fn failed_opening(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
    ) -> Result<Opened, Thrown> {
        let include_path =
            String::from_utf8_lossy(self.ini().get(b"include_path").unwrap_or_default())
                .into_owned();
        let shown = String::from_utf8_lossy(crate::c_number::c_str(written)).into_owned();
        if kind.required() {
            return Err(crate::api::raising::error(
                self,
                &format!("Failed opening required '{shown}' (include_path='{include_path}')"),
            ));
        }
        let opener = self.opener(kind);
        crate::api::raising::warning(
            self,
            &format!(
                "{opener}(): Failed opening '{shown}' for inclusion (include_path='{include_path}')"
            ),
        );
        Ok(Opened::Failed)
    }

    /// What `php_verror` names a file's opening by: the construct where
    /// the program's own `include` opens it, and the native being served
    /// where one opens it as an include does, `opcache_compile_file`.
    fn opener(&self, kind: IncludeKind) -> String {
        if self.vm_serving_native() {
            self.active_native()
        } else {
            String::from(kind.name())
        }
    }

    /// What the compiler refused an included file or an `eval` with,
    /// raised as php raises it: the diagnostics before the refusal at the
    /// unit's own name, then a parse error as the `ParseError`
    /// `zendparse` throws, at the file and the line the parser stopped
    /// on, and a refusal after the parse as the `E_COMPILE_ERROR` that
    /// ends the request. A file's are what `compile_file` recorded, so
    /// they are emitted with the `ParseError` pending, which no user
    /// error handler is handed, or ahead of the fatal with the handler
    /// masked off; an `eval`'s were raised as it compiled, before either.
    ///
    /// ```php
    /// set_error_handler(function ($no, $str) { echo "handler: $str\n"; });
    /// $f = tempnam(sys_get_temp_dir(), 'w');
    /// file_put_contents($f, '<?php $a = (integer) 1; echo 1 +;');
    /// try { include $f; } catch (ParseError $e) { echo get_class($e), "\n"; }
    /// try { eval('$a = (integer) 1; echo 1 +;'); } catch (ParseError $e) { echo get_class($e), "\n"; }
    /// restore_error_handler();
    /// try { include $f; } catch (ParseError $e) { echo get_class($e), "\n"; }
    /// unlink($f);
    /// ```
    /// ```expect
    /// ParseError
    /// handler: Non-canonical cast (integer) is deprecated, use the (int) cast instead
    /// ParseError
    ///
    /// Deprecated: Non-canonical cast (integer) is deprecated, use the (int) cast instead in %s on line 1
    /// ParseError
    /// ```
    ///
    /// A file's compile error bails out of `compile_file` past its emit,
    /// so its record stands for the rest of the request; an `eval`'s
    /// leaves `compile_string`, which began none, so what a shutdown
    /// function raises after it is shown, and `error_get_last()` answers
    /// that rather than the fatal.
    ///
    /// ```php
    /// register_shutdown_function(function () {
    ///     echo $undefined, "after\n";
    ///     echo error_get_last()["message"], "\n";
    /// });
    /// eval('class A { public int $a = null; }');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Default value for property of type int may not be null. Use the nullable type ?int to allow null default value in %s : eval()'d code on line 1
    ///
    /// Warning: Undefined variable $undefined in %s on line %d
    /// after
    /// Undefined variable $undefined
    /// ```
    pub(super) fn refused_include(
        &mut self,
        refused: &CompileRefused,
        path: &[u8],
        kind: SourceKind,
    ) -> Thrown {
        let error = &refused.error;
        if error.stage == php_compile::Stage::Opening {
            // `open_file_for_scanning` refused before anything compiled:
            // `zend_error` places it where the request is executing.
            let file = self
                .frames()
                .current_file()
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            let line = self.frames().current_line();
            self.end_in_compile_time(error.level, &error.message, &file, line);
            return Thrown::of(None);
        }
        // The compile met php 8.5.12's hard memory limit, `_exit(137)`, or
        // its allocator's panic, `abort()`, where it asked, so nothing it
        // declared stands and nothing it raised is rendered.
        match error.raise {
            php_compile::Raise::HardLimit { limit, asked } => {
                self.terminate_at_hard_limit(limit, asked);
                return Thrown::of(None);
            }
            php_compile::Raise::Panic => {
                self.terminate_in_panic(&error.message);
                return Thrown::of(None);
            }
            php_compile::Raise::Fatal | php_compile::Raise::Thrown | php_compile::Raise::Unread => {
            }
        }
        let raised = error.raised.iter();
        if !error.bailed() {
            if matches!(kind, SourceKind::Eval(_)) {
                for raised in raised.clone() {
                    self.note_error_in(raised.level, raised.message.as_bytes(), path, raised.line);
                }
            }
            let class = if error.level == Level::Parse {
                ClassId::PARSE_ERROR
            } else {
                ClassId::COMPILE_ERROR
            };
            let thrown =
                crate::api::raising::throw_at(self, class, &error.message, path, error.line);
            if kind == SourceKind::File {
                self.while_pending(&thrown, |req| {
                    for raised in raised {
                        req.note_error_in(
                            raised.level,
                            raised.message.as_bytes(),
                            path,
                            raised.line,
                        );
                    }
                });
            }
            return thrown;
        }
        // `compile_file` records what the file's compile raises, its
        // fatal last; `compile_string` begins no record. What the compile
        // had declared stands, each in its turn among what it raised
        // ([`Request::declare_refused`]), unless one of them ended the
        // request.
        let recording = (kind == SourceKind::File).then(|| self.begin_recording());
        if let Some(noted) = self.declare_refused(refused, path) {
            for raised in error.raised.get(noted..).unwrap_or_default() {
                self.note_error_in(raised.level, raised.message.as_bytes(), path, raised.line);
            }
            self.end_in_compile_time(error.level, &error.message, path, error.line);
        }
        if let Some(recording) = recording {
            self.emit_recorded(recording);
        }
        Thrown::of(None)
    }
}

/// `EG(included_files)`: the path of every file the request compiled,
/// the primary one first, each the key of a row whose value is empty
/// (`zend_hash_add_empty_element` over `opened_path`), in the table
/// `init_executor` lays down in `EG` eight wide,
/// `zend_hash_init(&EG(included_files), 8, ...)`.
#[derive(Debug)]
pub(crate) struct IncludedFiles(php_heap::EmbeddedArr);

impl Default for IncludedFiles {
    fn default() -> IncludedFiles {
        IncludedFiles(php_heap::EmbeddedArr::new(8))
    }
}

impl IncludedFiles {
    /// `zend_hash_exists`.
    pub(crate) fn contains(&self, path: &[u8]) -> bool {
        self.0.arr().str_find(path).is_some()
    }

    /// The table's string for `path`, `zend_string_copy`'d by the
    /// caller, as an op array's `filename` is the string its file was
    /// compiled under; `None` for a path the request did not open.
    pub(crate) fn held(&self, path: &[u8]) -> Option<PhpString> {
        self.0.arr().str_key(path).map(PhpString::of)
    }

    /// The paths in the table's order, as `get_included_files()` walks
    /// it.
    fn paths(&self) -> Vec<Vec<u8>> {
        let rows = self.0.arr();
        (rows.iter())
            .filter_map(|(key, _)| match key {
                php_heap::Key::Str(path) => Some(path.bytes().to_vec()),
                php_heap::Key::Long(_) => None,
            })
            .collect()
    }
}

impl Request {
    /// `zend_hash_add_empty_element(&EG(included_files), opened_path)`:
    /// `path` put in the table where it is not there yet, its string
    /// one more allocation the table holds and the table grown as
    /// php's grows. A path the table has is left alone. The `Err` is the
    /// memory limit, which has ended the request.
    ///
    /// Each first include of a file keeps its path's string, and the one
    /// that finds eight paths in the table the doubled table too, as
    /// php's `memory_get_usage()` reads them; the primary file is the
    /// table's first where the host named one:
    ///
    /// ```php
    /// $d = realpath(__DIR__) . '/'; $d .= str_repeat('f', 120 - strlen($d));
    /// for ($i = 0; $i < 10; $i++) { file_put_contents("$d$i.inc", '<?php $x = 1;'); }
    /// for ($i = 0; count(get_included_files()) < 8; $i++) { include_once "$d$i.inc"; }
    /// for ($n = $i + 2; $i < $n; $i++) { $m = memory_get_usage(); include_once "$d$i.inc"; echo memory_get_usage() - $m, "\n"; }
    /// for ($i = 0; $i < 10; $i++) { unlink("$d$i.inc"); }
    /// ```
    /// ```expect
    /// 480
    /// 160
    /// ```
    pub(crate) fn note_included(&mut self, path: &[u8]) -> Result<(), Thrown> {
        if self.included_files.contains(path) {
            return Ok(());
        }
        let held = self.string(path)?;
        self.note_included_held(held)
    }

    /// `zend_hash_str_del(&EG(included_files), path)`: `path` taken out
    /// of the table and its string given up, as `phar_file_action` takes
    /// out the file whose compile made no op array. The row's value is
    /// the null every row holds, which owns nothing.
    pub(super) fn forget_included(&mut self, path: &[u8]) {
        let rows = self.included_files.0.arr();
        self.heap.array_str_take(rows, path);
    }

    /// The string a once form put in `EG(included_files)` for the file
    /// at `full` before compiling it, its `opened_path`, which names
    /// what the compile makes of it; none for any other compile.
    pub(crate) fn opened_once(&self, full: &[u8]) -> Option<PhpString> {
        let once = (self.include.including).is_some_and(|(kind, _)| kind.once());
        self.included_files.held(full).filter(|_| once)
    }

    /// [`Request::note_included`] for the file `path` that `unit` was
    /// compiled from or served as: `compile_filename`'s
    /// `zend_hash_add_empty_element(&EG(included_files), opened_path)`,
    /// whose `opened_path` is the string a request-only compile names its
    /// op arrays' `filename` by, so a unit the request compiled for itself
    /// lends the table its file's name and the path costs no string more;
    /// a path is the bytes of no other name a unit holds.
    fn note_compiled_file(&mut self, path: &[u8], unit: &PublishedUnit) -> Result<(), Thrown> {
        let own = (self.unit_strings).file_of(PublishedUnit::as_ptr(unit), path);
        match own.filter(|_| !self.included_files.contains(path)) {
            Some(own) => {
                self.addref(&Value::string(own));
                self.note_included_held(own)
            }
            None => self.note_included(path),
        }
    }

    /// [`Request::note_included`] over `held`, a string whose one owner
    /// the table takes: one another place holds already, so no string is
    /// made; given up where the table has its path.
    pub(crate) fn note_included_held(&mut self, held: PhpString) -> Result<(), Thrown> {
        let rows = self.included_files.0.arr();
        let added = if rows.str_find(held.as_bytes()).is_some() {
            Ok(())
        } else {
            let key = php_heap::Key::Str(held.heap());
            self.heap.array_add_new(rows, key, php_heap::Zval::null())
        };
        // The table took an owner of its own where it took the path.
        self.release(Value::string(held));
        self.fatal_of(added)
    }

    /// `shutdown_executor`'s frees of the two tables a debug build of php
    /// frees by name (`Z/Zend/zend_execute_API.c:496-502`): every
    /// `EG(symtable_cache)` kept, then `EG(included_files)`, each path
    /// string its row holds given up and the data freed. A release build
    /// leaves them to the heap's mass release, as php's fast shutdown
    /// leaves them.
    #[cfg(debug_assertions)]
    pub(super) fn free_hash_tables(&mut self) {
        self.free_symbol_tables();
        let rows = self.included_files.0.arr();
        for at in 0..rows.num_used() {
            self.heap.array_take_at(rows, at);
        }
        self.included_files.0.free_data(&mut self.heap);
    }
}

impl Code<&Request> {
    /// `get_included_files()`: the paths, as `EG(included_files)` holds
    /// them, the primary file's first.
    #[must_use]
    pub fn included_files(self) -> Vec<Vec<u8>> {
        self.0.included_files.paths()
    }
}

impl Code<&mut Request> {
    /// `compile_string(source, name, position)` and `zend_execute` of
    /// the op array it made: the string compiled through the
    /// `zend_compile_string` links ([`Request::compile_string`]) from
    /// where `position` begins the scanner, under `name`, which
    /// `__FILE__` folds to and a diagnostic raised in it names; then
    /// served as code at the top of a frame of its own over the
    /// variables of the native's caller, where `zend_execute` serves it
    /// with `EG(current_execute_data)` at the native. A parse error is
    /// the `ParseError` `zendparse` throws at `name`, a refusal after
    /// the parse the `E_COMPILE_ERROR` that ends the request, and what
    /// the code left uncaught or ended in is the `Err`. What the test
    /// extension's `zend_test_compile_string` serves, the witness there.
    pub fn serve_string(
        self,
        source: &[u8],
        name: &[u8],
        position: php_compile::syntax::Position,
    ) -> Result<(), Thrown> {
        let kind = SourceKind::Eval(position);
        let unit = match self.0.compile_string(source, name, position)? {
            Ok(unit) => unit,
            Err(refused) => return Err(self.0.refused_include(&refused, name, kind)),
        };
        if !crate::engine::note_compiled(self.0, &unit, kind) || self.0.diagnostics().bailing_out()
        {
            return Err(Thrown::of(self.0.take_pending_view()));
        }
        let standing = self.0.vm.depth();
        let from = (self.0.innermost_user_frame()).map_or(super::CodeFrom::Top(None), |frame| {
            super::CodeFrom::Nested(frame, None)
        });
        let callee = self.0.push_code_frame(unit, from);
        let callee = self.0.fatal_of(callee)?;
        crate::observer::call(self.0, callee);
        let reenter = self.0.loops().reenter;
        let served = reenter(self.0, callee);
        while self.0.vm.depth() > standing {
            let top = self.0.vm.stack.current();
            self.0.leave_frame(top);
        }
        self.0.fatal_of(served)?;
        let returned = std::mem::take(&mut self.0.returned);
        self.0.release(returned);
        if self.0.is_pending() {
            return Err(Thrown::of(self.0.take_pending_view()));
        }
        Ok(())
    }

    /// `zend_compile_string_to_ast` and `zend_ast_export("", ast, "")`
    /// (`Z/Zend/zend_language_scanner.l:604`, `Z/Zend/zend_ast.c:2954`):
    /// the source parsed from an open tag as a file is, and php's text
    /// of its tree, the text `assert()`'s description is written by,
    /// over the whole file. A parse error is the `ParseError` `zendparse`
    /// throws. What the test extension's `zend_test_compile_to_ast`
    /// answers, the witness there.
    pub fn export_tree(self, source: &[u8]) -> Result<Vec<u8>, Thrown> {
        let options = php_compile::Options {
            precision: self.0.ini().precision(),
            short_open_tag: self.0.short_open_tag(),
            ..php_compile::Options::default()
        };
        let mut strings = php_heap::Strings::new();
        php_compile::export_source(&mut self.0.compile_arena, source, &options, &mut strings)
            .map_err(|error| {
                let refused = CompileRefused {
                    error,
                    declared: None,
                };
                self.0.refused_include(&refused, b"", SourceKind::EVAL)
            })
    }

    /// `zend_resolve_path`: the chain of the extensions' overrides
    /// entered from the top
    /// ([`crate::api::abi::ExtensionBuilder::resolve_path`]), with the
    /// engine's own resolution beneath every link
    /// ([`Request::resolve_path_own`]). The `Err` is the last attempt's
    /// reason, which the stream warning prints, or the throwable an
    /// override or a wrapper's `url_stat` raised.
    pub fn resolve_path(self, written: &[u8]) -> Result<Vec<u8>, Unresolved> {
        let shared = Arc::clone(&self.0.shared);
        Beneath::<chain::ResolvePath>::top(shared.supplied.resolve_path.links(), written)
            .serve(self.0)
    }
}

/// Whether the file at `path` opens for reading, and the `errno` php's
/// plain-files opener reports where it does not: `Permission denied` for
/// a file whose permissions are 000, `No such file or directory` for one that went.
///
/// ```php
/// $f = __DIR__ . '/locked.inc';
/// file_put_contents($f, '<?php return 1;'); chmod($f, 0);
/// var_dump(@include $f, @include_once $f);
/// foreach (['require', 'require_once'] as $how) {
///     try { $how === 'require' ? require $f : require_once $f; } catch (Error $e) { echo get_class($e), "\n"; }
/// }
/// chmod($f, 0644); var_dump(include_once $f); unlink($f);
/// ```
/// ```expect exit=0
/// bool(false)
/// bool(false)
///
/// Warning: require(%s/locked.inc): Failed to open stream: Permission denied in %s on line 6
/// Error
///
/// Warning: require_once(%s/locked.inc): Failed to open stream: Permission denied in %s on line 6
/// Error
/// int(1)
/// ```
fn readable(path: &[u8]) -> Result<(), std::io::Error> {
    let path = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path);
    std::fs::File::open(path).map(drop)
}

/// Why the file at `path` cannot be read, as [`readable`] answers it; a
/// file that opens now went and came back, and reads as gone.
pub(super) fn unreadable(path: &[u8]) -> std::io::Error {
    readable(path)
        .err()
        .unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
}
