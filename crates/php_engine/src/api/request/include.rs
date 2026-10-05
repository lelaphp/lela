//! The include area of a request: the primary file noted and
//! compiled, `include` and `require` in their four forms through
//! `zend_include_or_eval` and the stream layer, `eval()` and the strings
//! compiled for it, `stream_resolve_include_path()`, the warnings an open
//! that failed raises, and the list `get_included_files()` reads. Which
//! include is being opened, php's construct on the frame's opline, is
//! [`Include`]'s, whose field no file but this one reads.

use std::sync::Arc;

use php_compile::{CompileError, SourceKind};
use php_heap::Fatal;
use php_unit::Unit;

use crate::api::raising::{Level, Thrown};
use crate::exec::IncludeKind;

use super::{HandlePath, HostPath, Opened, Refused, Request, Unresolved, is_wrapper_url};

/// Which include is being opened, whose field no file but this one reads.
#[derive(Default)]
pub(super) struct Include {
    /// The include opening a file through a stream wrapper, and how many
    /// calls stood when it began: php's `get_active_function_name` names
    /// the construct (`include`, `require_once`) for what the stream layer
    /// raises under it, and a native a user-space wrapper serves from
    /// inside it is one call deeper.
    including: Option<(IncludeKind, usize)>,
}

impl Request {
    /// Whether the code being served is an include's open, which
    /// `php_verror` names by the construct (`include()`) as it names a
    /// native by the native.
    pub(crate) fn serving_include(&self) -> bool {
        self.include
            .including
            .is_some_and(|(_, depth)| depth == self.vm.call_depth())
    }

    /// The construct of the include being opened where the code being
    /// served is its open ([`Request::serving_include`]).
    pub(super) fn include_being_opened(&self) -> Option<IncludeKind> {
        self.include
            .including
            .filter(|_| self.serving_include())
            .map(|(kind, _)| kind)
    }

    /// `get_included_files()`: the paths, as `EG(included_files)` holds
    /// them, the primary file's first.
    #[must_use]
    pub fn included_files(&self) -> Vec<Vec<u8>> {
        self.included_files.paths()
    }

    /// The source an extension's `zend_compile_file` answers for `path`
    /// ([`crate::api::abi::ExtensionBuilder::compile_file`]), the first
    /// link to answer from the top of the chain; `None` where none
    /// replaces it.
    pub(crate) fn compile_file_source(&mut self, path: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
        let shared = Arc::clone(&self.shared);
        for hook in shared.supplied.compile_file.top_down() {
            if let Some(source) = hook(self, path)? {
                return Ok(Some(source));
            }
        }
        Ok(None)
    }

    /// `php_execute_script_ex` (`Z/main/main.c:2617`): the primary file
    /// into `EG(included_files)` under its expanded path before it runs,
    /// so a once form over the file being served answers `true`. A source
    /// the host handed over, `php -r` and stdin, has no path and is
    /// nowhere in the table, as php's `Standard input code` is not.
    pub(crate) fn note_primary_file(&mut self, script: &[u8]) {
        if let Some(real) = self.realpath(script) {
            // The memory limit's fatal is recorded and ends the request.
            let _ = self.note_included(real.as_os_str().as_encoded_bytes());
        }
    }

    /// `php_execute_script`'s `expand_filepath` of the primary file
    /// (`Z/main/main.c:2610-2619`), before `auto_prepend_file` runs: the
    /// file the request names made whole against its working directory
    /// with its links resolved, which is the name the unit compiles under
    /// and the page `php_statpage` stats, put in `EG(included_files)`;
    /// the name as given where it expands to nothing. A source the host
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
                && let Some(expanded) = self.expand_filepath(&script)
            {
                let _ = self.note_included(&expanded);
                self.description.script_name = Some(expanded);
            }
            return;
        }
        let script = script.unwrap_or_default();
        let expanded = self.expand_filepath(&script).unwrap_or(script);
        let _ = self.note_included(&expanded);
        self.description.script_name = Some(expanded);
    }

    /// `zend_include_or_eval` (`Z/Zend/zend_execute.c:5287`) for the four
    /// file constructs: the once forms resolve the path first
    /// ([`Request::resolve_path`]) and answer [`Opened::Already`] for one
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
        if written.contains(&0) {
            return self.failed_opening(kind, written);
        }
        let held = self.held_as_written(written);
        let resolved = match &held {
            Some(held) => Ok(held.path.clone()),
            None => self.resolve_path(written),
        };
        if kind.once()
            && held.is_none()
            && let Ok(path) = &resolved
        {
            self.file_once_key(written, path);
        }
        if kind.once()
            && let Ok(path) = &resolved
            && self.included_files.contains(path)
        {
            return Ok(Opened::Already);
        }
        let path = match resolved {
            Ok(path) if !is_wrapper_url(&path) => path,
            // php's `resolved_path = zend_string_copy(inc_filename)` for a
            // path it could not resolve, which a stream wrapper opens.
            Ok(path) => return self.include_stream(kind, written, &path),
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
            Err(_) if is_wrapper_url(written) => {
                if kind.once() && self.included_files.contains(written) {
                    return Ok(Opened::Already);
                }
                return self.include_stream(kind, written, written);
            }
            Err(Unresolved::Missing(reason)) => {
                // The plain files wrapper is handed the path as written,
                // and asks the guard of it before it opens.
                let reason = match self.include_guard(kind, written) {
                    Ok(_) => reason,
                    Err(refused) => refused.into(),
                };
                self.open_stream_failed(kind, written, &reason);
                return self.failed_opening(kind, written);
            }
        };
        if let Err(refused) = self.include_guard(kind, &path) {
            // The stream opens the resolved path, which its warning names.
            self.open_stream_failed(kind, &path, &refused.into());
            return self.failed_opening(kind, written);
        }
        if kind.once() {
            self.note_included(&path)?;
        }
        let outer = self.include.including.replace((kind, self.vm.call_depth()));
        let replaced = self.compile_file_source(&path);
        self.include.including = outer;
        if let Some(source) = replaced? {
            let shared = Arc::clone(&self.shared);
            let unit = match shared.compile_for(self, &source, &path, SourceKind::File) {
                Ok(unit) => unit,
                Err(refused) => {
                    return Err(self.refused_include(&refused, &path, SourceKind::File));
                }
            };
            self.note_included(&path)?;
            return Ok(Opened::Unit(unit));
        }
        let handle = if kind.once() {
            HandlePath::Opened
        } else {
            HandlePath::Filename
        };
        let cached = match held {
            Some(held) => Some(held.unit.clone()),
            None => self.cached_file(&path, Some(written), handle),
        };
        match cached {
            Some(Ok(unit)) => {
                self.note_included(&path)?;
                Ok(Opened::Unit(unit))
            }
            Some(Err(refused)) => Err(self.refused_include(&refused, &path, SourceKind::File)),
            // The file went between the stat that resolved it and the
            // read that would have compiled it.
            None => {
                let reason = std::io::Error::from(std::io::ErrorKind::NotFound);
                self.open_stream_failed(kind, written, &reason);
                self.failed_opening(kind, written)
            }
        }
    }

    /// The stream `php_stream_open_for_zend_ex` opens over a plain file
    /// an include or `spl_autoload` compiles, or looks up by its full
    /// path in opcache's ([`Request::cached_file`]): a `stream` resource under
    /// the next id, freed when `zend_destroy_file_handle` closes it
    /// before the file runs, so the id is spent and `EG(regular_list)`
    /// grows as php's does, here from its packed table of eight to a
    /// hash at id 8, 160 bytes more that the list keeps:
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
    /// A refusal has recorded the memory limit's fatal, which ends the
    /// request.
    pub(super) fn spend_include_stream(&mut self) {
        if let Some(hooks) = self.stream_hooks() {
            (hooks.spend)(self);
        }
    }

    /// `php_plain_files_stream_opener`'s guard over the path an include
    /// opens (`Z/main/streams/plain_wrapper.c:1265`), its warning named by
    /// the construct: the resolved path, or the path as written where
    /// none resolved, and the stream's warning naming the same.
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
    fn include_guard(&mut self, kind: IncludeKind, path: &[u8]) -> Result<HostPath, Refused> {
        let outer = self.include.including.replace((kind, self.vm.call_depth()));
        let guarded = self.check_open_basedir(path, true);
        self.include.including = outer;
        guarded
    }

    /// An include of a path a stream wrapper serves, `zend_stream_open`'s
    /// `php_stream_open_for_zend_ex` under `USE_PATH | REPORT_ERRORS |
    /// STREAM_OPEN_FOR_INCLUDE`: the stream opened as its wrapper opens it,
    /// the warnings raised under it naming the construct, read whole, and
    /// compiled as a file under the path, a unit of the request's own
    /// that nothing publishes, since opcache keeps plain files alone. The
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
    /// A read that fails, as one through a filter that answers
    /// `PSFS_ERR_FATAL` fails, is the file php could not open
    /// (`zend_stream_fixup`, [`crate::api::abi::StreamHooks::open`]).
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
        if kind.once() {
            self.note_included(path)?;
        }
        // Zend alone opens no wrapper's path (`zend_stream_open` with no
        // `zend_stream_open_function`).
        let Some(hooks) = self.stream_hooks() else {
            return self.failed_opening(kind, written);
        };
        let outer = self.include.including.replace((kind, self.vm.call_depth()));
        let read = (hooks.open)(self, path);
        self.include.including = outer;
        let read = read?;
        // `compile_file` reports the file it could not open or read only
        // where nothing is pending (`if (!EG(exception))`).
        if let Some(thrown) = self.stream_thrown.take() {
            return Err(thrown);
        }
        let Some(crate::api::abi::IncludeSource {
            source,
            opened_path,
        }) = read
        else {
            // A read a fatal ended is no file php reports: the bailout
            // jumps past `compile_file`'s message.
            if self.bailing_out() {
                return Ok(Opened::Failed);
            }
            return self.failed_opening(kind, written);
        };
        let path = opened_path.as_deref().unwrap_or(path);
        let shared = Arc::clone(&self.shared);
        let unit = match shared.compile_for(self, &source, path, SourceKind::File) {
            Ok(unit) => unit,
            Err(refused) => return Err(self.refused_include(&refused, path, SourceKind::File)),
        };
        self.note_included(path)?;
        Ok(Opened::Unit(unit))
    }

    /// `zend_include_or_eval`'s `ZEND_EVAL` arm
    /// (`Z/Zend/zend_execute.c:5354`): the string compiled as
    /// `compile_string` compiles it, from after an open tag and ending in
    /// `RETURN null` ([`SourceKind::Eval`]), under the name
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
    pub(crate) fn eval_string(&mut self, code: &[u8]) -> Result<Opened, Thrown> {
        if code.is_empty() {
            return Ok(Opened::Failed);
        }
        let mut name = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        name.extend_from_slice(format!("({}) : eval()'d code", self.current_line()).as_bytes());
        Ok(Opened::Unit(self.compile_eval(code, &name)?))
    }

    /// `zend_compile_string`, every string compiled as an `eval`
    /// compiles it: the links the bundle supplied
    /// ([`ExtensionBuilder::compile_string`]) from the top, each serving
    /// the one beneath where it will, and php's own `compile_string`
    /// beneath them all, from after an open tag ([`SourceKind::Eval`]).
    /// The `Err` is what a link threw; the unit, or the compiler's
    /// refusal for the caller to raise, is the `Ok`.
    ///
    /// [`ExtensionBuilder::compile_string`]: crate::api::abi::ExtensionBuilder::compile_string
    pub(crate) fn compile_string(
        &mut self,
        code: &[u8],
        name: &[u8],
    ) -> Result<Result<Arc<Unit>, CompileError>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let links = shared.supplied.compile_string.links();
        let top = crate::supplied::StringBeneath {
            links,
            source: code,
            name,
        };
        Ok(top.compile(self, code)?.0)
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
        if served.is_ok() && !self.bailing_out() {
            self.no_extensions = false;
        }
        served
    }

    /// php's own `compile_string`, beneath every link of the pointer:
    /// `source` compiled under `name` as the request's own unit.
    pub(crate) fn compile_string_own(
        &mut self,
        source: &[u8],
        name: &[u8],
    ) -> Result<Arc<Unit>, CompileError> {
        Arc::clone(&self.shared).compile_for(self, source, name, SourceKind::Eval)
    }

    /// [`Request::compile_string`] with the compiler's refusal raised as
    /// an included file's is, at `name`.
    pub(crate) fn compile_eval(&mut self, code: &[u8], name: &[u8]) -> Result<Arc<Unit>, Thrown> {
        self.compile_string(code, name)?
            .map_err(|refused| self.refused_include(&refused, name, SourceKind::Eval))
    }

    /// `zend_resolve_path`: each extension's override asked first, in
    /// bundle order ([`crate::api::abi::ExtensionBuilder::resolve_path`]),
    /// then the stream layer's `php_resolve_path` over the ini's
    /// `include_path` ([`crate::api::abi::StreamHooks::resolve_path`]), or,
    /// with no stream layer, Zend's own `VCWD_REALPATH`. The `Err` is the
    /// last attempt's reason, which the stream warning prints, or the
    /// throwable an override or a wrapper's `url_stat` raised.
    pub fn resolve_path(&mut self, written: &[u8]) -> Result<Vec<u8>, Unresolved> {
        let shared = Arc::clone(&self.shared);
        for hook in shared.supplied.resolve_path.top_down() {
            if let Some(path) = hook(self, written).map_err(Unresolved::Threw)? {
                return Ok(path);
            }
        }
        match self.stream_hooks() {
            Some(hooks) => (hooks.resolve_path)(self, written),
            // `zend_resolve_path_func`: `VCWD_REALPATH` and nothing else.
            None => self.realpath_file(written).map_err(Unresolved::Missing),
        }
    }

    /// The stream layer's hooks, as the supplied table resolved them
    /// ([`crate::api::abi::StreamHooks`]).
    pub(crate) fn stream_hooks(&self) -> Option<crate::api::abi::StreamHooks> {
        self.shared.supplied.stream_hooks.get()
    }

    /// `tsrm_realpath` of a path that is a regular file: the canonical
    /// path, against the request's working directory where the path is
    /// relative.
    pub fn realpath_file(&mut self, path: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        // `virtual_file_ex` sets `errno = ENOENT` for a path it could not
        // resolve.
        const ENOENT: i32 = 2;
        let real = self
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
    pub fn wrapper_warning(&mut self, function: &str, path: &[u8], text: &str) {
        let copy = self.heap_mut().block(path.len() + 1);
        let Ok(copy) = self.fatal_of(copy) else {
            return;
        };
        let origin = crate::api::raising::Origin::Function(function);
        crate::api::raising::verror(self, Level::Warning, origin, path, text.as_bytes());
        self.heap_mut().free_block(copy);
    }

    /// `php_stream_open_wrapper`'s warning under `REPORT_ERRORS`, the
    /// `Failed to open stream: %s` of `php_stream_display_wrapper_errors`
    /// with `strerror(errno)`, raised through `php_error_docref1` with
    /// the construct and the path as written as its origin.
    pub(super) fn open_stream_failed(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
        reason: &std::io::Error,
    ) {
        let reason = reason.to_string();
        let reason = reason
            .split_once(" (os error")
            .map_or(reason.as_str(), |(text, _)| text);
        let opener = self.opener(kind);
        self.wrapper_warning(
            &opener,
            written,
            &format!("Failed to open stream: {reason}"),
        );
    }

    /// `php_message_handler_for_zend`'s `ZMSG_FAILED_INCLUDE_FOPEN` and
    /// `ZMSG_FAILED_REQUIRE_FOPEN` (`Z/main/main.c:1746`): the warning
    /// and `false` for an `include`, the `Error` for a `require`, each
    /// naming the path as written and `include_path` as the ini has it.
    pub(super) fn failed_opening(
        &mut self,
        kind: IncludeKind,
        written: &[u8],
    ) -> Result<Opened, Thrown> {
        let include_path =
            String::from_utf8_lossy(self.ini_get(b"include_path").unwrap_or_default()).into_owned();
        let shown = String::from_utf8_lossy(written).into_owned();
        if kind.required() {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
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
    pub(super) fn refused_include(
        &mut self,
        refused: &CompileError,
        path: &[u8],
        kind: SourceKind,
    ) -> Thrown {
        if refused.stage == php_compile::Stage::Opening {
            // `open_file_for_scanning` refused before anything compiled:
            // `zend_error` places it where the request is executing.
            let file = self.current_file().map(<[u8]>::to_vec).unwrap_or_default();
            let line = self.current_line();
            self.end_in_compile_time(refused.level, &refused.message, &file, line);
            return Thrown::of(None);
        }
        let raised = refused.raised.iter();
        if kind == SourceKind::Eval {
            for raised in raised.clone() {
                self.note_error_in(raised.level, &raised.message, path, raised.line);
            }
        }
        if refused.level == Level::Parse || refused.thrown {
            let class = if refused.level == Level::Parse {
                "ParseError"
            } else {
                "CompileError"
            };
            let thrown =
                crate::api::raising::throw_at(self, class, &refused.message, path, refused.line);
            if kind == SourceKind::File {
                self.while_pending(&thrown, |req| {
                    for raised in raised {
                        req.note_error_in(raised.level, &raised.message, path, raised.line);
                    }
                });
            }
            return thrown;
        }
        let recording = self.begin_recording();
        if kind == SourceKind::File {
            for raised in raised {
                self.note_error_in(raised.level, &raised.message, path, raised.line);
            }
        }
        self.end_in_compile_time(refused.level, &refused.message, path, refused.line);
        self.emit_recorded(recording);
        Thrown::of(None)
    }
}
