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

use crate::ProgramFile;
use crate::api::abi::{Beneath, chain};
use crate::api::raising::{Level, Thrown};
use crate::exec::IncludeKind;
use crate::supplied::chain::{FileArgs, StringArgs};

use super::{HandlePath, HostPath, Opened, Refused, Request, Unresolved, is_wrapper_url};

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
}

/// A file one of the request's `zend_file_handle`s holds open past the
/// read that compiled it, with no `O_CLOEXEC`, as php's `fopen` and
/// `zend_stream_open` open it, so a process the request execs inherits
/// it (#2458, #2492): the program's file as its host hands it over
/// ([`ProgramFile`]), and `auto_prepend_file`'s, `auto_append_file`'s and
/// the built-in server's router's as `php_execute_script_ex` and
/// `php_cli_server_dispatch_router` keep theirs ([`HandlePath::Kept`]).
/// What holds it open is the [`std::fs::File`] alone, which nothing reads.
struct Held {
    _file: std::fs::File,
    until: Until,
}

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
    /// ([`ProgramFile::Listed`]).
    Ended,
}

impl Request {
    /// Whether the code being served is an include's open, which
    /// `php_verror` names by the construct (`include()`) as it names a
    /// native by the native.
    pub(crate) fn serving_include(&self) -> bool {
        self.include
            .including
            .is_some_and(|(_, depth)| depth == self.vm.depth())
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
    ) -> Result<Option<Result<Arc<Unit>, CompileError>>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let args = FileArgs::new(path, written, handle);
        let top = Beneath::<chain::CompileFile>::top(shared.supplied.compile_file.links(), args);
        Ok(top.serve(self)?.map(|compiled| compiled.0))
    }

    /// `php_execute_script_ex` (`Z/main/main.c:2617`): the primary file
    /// into `EG(included_files)` under its expanded path before it runs,
    /// so a once form over the file being served answers `true`. A source
    /// the host handed over, `php -r` and stdin, has no path and is
    /// nowhere in the table, as php's `Standard input code` is not.
    pub(crate) fn note_primary_file(&mut self, script: &[u8]) {
        if let Some(real) = self.realpath(script) {
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
            ProgramFile::Opened => Until::Served,
            ProgramFile::Unopened => return,
            _ if self.accelerator_holds(&script) => return,
            ProgramFile::Named => Until::Served,
            ProgramFile::Listed => Until::Ended,
        };
        self.hold_file(&script, until);
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
            self.include.held.push(Held { _file: file, until });
        }
    }

    /// The `zend_destroy_file_handle`s once the program has been served,
    /// before the request shuts down: the host's of the program's file,
    /// and `php_execute_script_ex`'s of `auto_prepend_file`,
    /// `auto_append_file` and the router; a program's file php-fpm leaves
    /// listed is closed with the request ([`ProgramFile::Listed`]).
    pub(crate) fn close_served_files(&mut self) {
        self.include.held.retain(|held| held.until == Until::Ended);
    }

    /// `php_execute_script`'s `expand_filepath` of the primary file
    /// (`Z/main/main.c:2610-2619`), before `auto_prepend_file` runs: the
    /// file the request names made whole against its working directory
    /// with its links resolved, which is the name the unit compiles under
    /// and the page `php_statpage` stats, and put in `EG(included_files)`
    /// there only where the host opened it as a `FILE *`, the CLI's
    /// [`ProgramFile::Opened`]; php-cgi's and php-fpm's
    /// `zend_file_handle` carries an `opened_path` and the built-in server's is a bare name, so the
    /// table takes theirs as it compiles, after the prepend file has been
    /// served ([`Request::note_compiled_program`]); the name as given
    /// where it expands to nothing. A source the host
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
                self.note_included(&expanded).ok();
                self.description.script_name = Some(expanded);
            }
            return;
        }
        let script = script.unwrap_or_default();
        let expanded = self.expand_filepath(&script).unwrap_or(script);
        if self.description.program_file == ProgramFile::Opened {
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
    pub(crate) fn note_compiled_program(&mut self) {
        if self.description.program_file == ProgramFile::Opened {
            return;
        }
        if let Some(script) = self.description.script_name.clone() {
            // The memory limit's fatal is recorded and ends the request.
            self.note_included(&script).ok();
        }
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
        // A unit the lookup by the key served is opcache's answer above
        // every link; any other file is compiled through them all, inside
        // the include, as `zend_include_or_eval` calls `zend_compile_file`.
        let cached = if let Some(held) = held {
            Some(held.unit.clone())
        } else {
            let outer = self.include.including.replace((kind, self.vm.depth()));
            let compiled = self.compile_file(&path, Some(written), handle);
            self.include.including = outer;
            compiled?
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
        let outer = self.include.including.replace((kind, self.vm.depth()));
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
    /// (`zend_stream_fixup`, [`crate::layer::StreamHooks::open`]).
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
        let outer = self.include.including.replace((kind, self.vm.depth()));
        let read = (hooks.open)(self, path);
        self.include.including = outer;
        let read = read?;
        // `compile_file` reports the file it could not open or read only
        // where nothing is pending (`if (!EG(exception))`).
        if let Some(thrown) = self.stream_thrown.take() {
            return Err(thrown);
        }
        let Some(crate::layer::IncludeSource {
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
    ) -> Result<Result<Arc<Unit>, CompileError>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let args = StringArgs::new(code, name, position);
        let top =
            Beneath::<chain::CompileString>::top(shared.supplied.compile_string.links(), args);
        Ok(top.serve(self)?.0)
    }

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
        &mut self,
        source: &[u8],
        name: &[u8],
        position: php_compile::syntax::Position,
    ) -> Result<(), Thrown> {
        let kind = SourceKind::Eval(position);
        let unit = match self.compile_string(source, name, position)? {
            Ok(unit) => unit,
            Err(refused) => return Err(self.refused_include(&refused, name, kind)),
        };
        if !crate::engine::note_compiled(self, &unit, kind) || self.bailing_out() {
            return Err(Thrown::of(self.take_pending_view()));
        }
        let standing = self.vm.depth();
        let callee = match self.caller_frame() {
            Some(frame) => self.push_code_frame(unit, frame, None),
            None => match self.push_frame(unit) {
                Ok(frame) => {
                    crate::observer::call(self, frame);
                    Ok(frame)
                }
                refused => refused,
            },
        };
        let callee = self.fatal_of(callee)?;
        let reenter = self.shared.loops.reenter;
        let served = reenter(self, callee);
        while self.vm.depth() > standing {
            let top = self.vm.stack.current();
            self.leave_frame(top);
        }
        self.fatal_of(served)?;
        let returned = std::mem::replace(&mut self.returned, crate::api::values::Value::null());
        self.release(returned);
        if self.is_pending() {
            return Err(Thrown::of(self.take_pending_view()));
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
    pub fn export_tree(&mut self, source: &[u8]) -> Result<Vec<u8>, Thrown> {
        let options = php_compile::Options {
            precision: self.precision(),
            short_open_tag: self.short_open_tag(),
            ..php_compile::Options::default()
        };
        let mut strings = php_heap::Strings::new();
        php_compile::export_source(source, &options, &mut strings)
            .map_err(|refused| self.refused_include(&refused, b"", SourceKind::EVAL))
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
    /// `source` compiled from `position` under `name` as the request's
    /// own unit.
    pub(crate) fn compile_string_own(
        &mut self,
        source: &[u8],
        name: &[u8],
        position: php_compile::syntax::Position,
    ) -> Result<Arc<Unit>, CompileError> {
        Arc::clone(&self.shared).compile_for(self, source, name, SourceKind::Eval(position))
    }

    /// [`Request::compile_string`] with the compiler's refusal raised as
    /// an included file's is, at `name`.
    pub(crate) fn compile_eval(&mut self, code: &[u8], name: &[u8]) -> Result<Arc<Unit>, Thrown> {
        self.compile_string(code, name, php_compile::syntax::Position::AfterOpenTag)?
            .map_err(|refused| self.refused_include(&refused, name, SourceKind::EVAL))
    }

    /// `zend_resolve_path`: the chain of the extensions' overrides
    /// entered from the top
    /// ([`crate::api::abi::ExtensionBuilder::resolve_path`]), with the
    /// engine's own resolution beneath every link
    /// ([`Request::resolve_path_own`]). The `Err` is the last attempt's
    /// reason, which the stream warning prints, or the throwable an
    /// override or a wrapper's `url_stat` raised.
    pub fn resolve_path(&mut self, written: &[u8]) -> Result<Vec<u8>, Unresolved> {
        let shared = Arc::clone(&self.shared);
        Beneath::<chain::ResolvePath>::top(shared.supplied.resolve_path.links(), written)
            .serve(self)
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
        if matches!(kind, SourceKind::Eval(_)) {
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
