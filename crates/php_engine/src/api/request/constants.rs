//! The global constants of a request: `EG(zend_constants)` past what
//! the engine registered, `define()` and `const`, the constants an
//! extension or the host registers, `PHP_SAPI`, the `STDIN`, `STDOUT`
//! and `STDERR` handles the host registers with the host's file
//! descriptors beneath them, a constant's attributes and deprecation,
//! `__COMPILER_HALT_OFFSET__`, and the lookup that reads them all. The
//! request's rows and `PHP_SAPI` are [`Constants`]'s, whose fields no
//! file but this one reads.

use std::collections::HashMap;

use php_link::NameHasher;
use php_unit::decl::Attribute;

use crate::api::raising::Thrown;
use crate::api::values::{Emalloc, Owner, PhpArray, PhpString, Value, adopted};

use super::Symbols;
use super::{ExtensionId, Request, Standard};

/// The name `__halt_compiler()` registers its file's offset under.
const HALT_OFFSET: &[u8] = b"__COMPILER_HALT_OFFSET__";

/// `zend_mangle_property_name("__COMPILER_HALT_OFFSET__", file)`: a NUL,
/// the name, a NUL and the file's, the key one file's halt offset is
/// registered under.
pub(crate) fn halt_offset_key(file: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(HALT_OFFSET.len() + 2 + file.len());
    key.push(0);
    key.extend_from_slice(HALT_OFFSET);
    key.push(0);
    key.extend_from_slice(file);
    key
}

/// Who a row the request added to `EG(zend_constants)` belongs to,
/// php's `ZEND_CONSTANT_MODULE_NUMBER` beside `zend_constant.filename`:
/// the host, which registers under `Core` at request start; an
/// extension that registers one while the request runs; or the program,
/// `PHP_USER_CONSTANT`, with the file `zend_register_constant` read off
/// `zend_get_executed_filename_ex` where one was executing.
enum ConstantOwner {
    Core,
    Extension(ExtensionId),
    /// `c->filename`, `zend_get_executed_filename_ex()` with one more
    /// owner ([`Request::executed_file_string`]): an `eval`'s name string
    /// among them, which the row keeps past the `eval`.
    User(Option<Owner<PhpString>>),
}

/// `sizeof(zend_constant)`: the value, the name, the attributes and the
/// file, which `zend_register_constant` `pemalloc`s for every constant a
/// request registers (`zend_hash_add_constant`,
/// `Z/Zend/zend_constants.c`).
const ZEND_CONSTANT: usize = 40;

/// One row a request added to `EG(zend_constants)`: its owner, its name
/// as `zend_register_constant` keys it, its value, and
/// `zend_constant::attributes`, what a `DECLARE_ATTRIBUTED_CONST` hung
/// on it, with `CONST_DEPRECATED` decoded from them where it was
/// registered.
struct ConstantRow {
    /// php's `zend_constant` on the request's heap, which the row is in
    /// the engine's own shape; none where the memory limit refused it,
    /// whose fatal ends the request.
    room: Option<Emalloc>,
    /// `c->name`, released with the row: a host's or an extension's
    /// name through `zend_string_init_interned` ([`Request::interned`]),
    /// and for `define()`'s and `const`'s one more owner of the
    /// program's string (`zend_string_copy`), the one a declaration
    /// holds for the bytes where any does ([`Request::declared_string`]);
    /// none where the memory limit refused it.
    named: Option<Owner<PhpString>>,
    owner: ConstantOwner,
    /// The key `EG(zend_constants)` holds the row under: the name with
    /// its namespace lowered, which every lookup compares against.
    name: Vec<u8>,
    /// `c->name`, the name as it was given, which a listing shows.
    given: Vec<u8>,
    value: Value,
    attributes: Vec<Attribute>,
    deprecated: bool,
}

impl ConstantRow {
    fn new(
        room: Option<Emalloc>,
        named: Option<Owner<PhpString>>,
        owner: ConstantOwner,
        name: Vec<u8>,
        value: Value,
    ) -> ConstantRow {
        ConstantRow {
            room,
            named,
            owner,
            given: name.clone(),
            name,
            value,
            attributes: Vec::new(),
            deprecated: false,
        }
    }
}

/// The request's rows of `EG(zend_constants)` and `PHP_SAPI`, whose
/// fields no file but this one reads.
/// Where a constant the table holds stands, what a `FETCH_CONSTANT`'s
/// cache slot keeps once its first pass found it, as php's
/// `CACHE_PTR(opline->extended_value, c)` keeps the `zend_constant`
/// (#2438): `PHP_SAPI`, a row of the engine's declared table, or a row this
/// request added, beside the generation of the request's rows it was
/// found in, so a slot written before phpdbg's `run <file` replaced
/// `STDIN` reads as unprimed after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstantAt {
    Sapi,
    Declared(u32),
    Request { at: u32, generation: u32 },
}

pub(super) struct Constants {
    /// The rows this request added to `EG(zend_constants)`, in the order
    /// it added them, each under the extension it is registered by:
    /// `Core` for what the host registers at request start
    /// ([`crate::engine::FileHandles`]), `None` for what `define()` and
    /// `const` wrote, php's `PHP_USER_CONSTANT`, with the file it was
    /// defined in.
    rows: Vec<ConstantRow>,
    /// Each row's position by its name, `EG(zend_constants)`' hash.
    index: HashMap<Box<[u8]>, u32, NameHasher>,
    /// How many times rows were taken out, which moves those after them.
    generation: u32,
    /// `PHP_SAPI`, `sapi_module.name` as `php_module_startup` registers
    /// it: the host's, so the request's, the engine's interned string
    /// for the name the description carries.
    sapi_constant: Value,
}

impl Default for Constants {
    fn default() -> Constants {
        Constants {
            rows: Vec::new(),
            index: HashMap::with_hasher(NameHasher),
            generation: 0,
            sapi_constant: Value::null(),
        }
    }
}

impl Constants {
    /// `row` at the end, under its name.
    fn push(&mut self, row: ConstantRow) {
        if let Ok(at) = u32::try_from(self.rows.len()) {
            self.index.insert(row.name.clone().into_boxed_slice(), at);
        }
        self.rows.push(row);
    }

    /// The row under `name` beside its position.
    fn find(&self, name: &[u8]) -> Option<(u32, &ConstantRow)> {
        let at = *self.index.get(name)?;
        Some((at, self.rows.get(usize::try_from(at).ok()?)?))
    }

    /// The constant the request holds under `name`, its value and whether
    /// `CONST_DEPRECATED` marks it: a row it added, or `PHP_SAPI`, which
    /// php registers at startup (`CONST_PERSISTENT|CONST_NO_FILE_CACHE`)
    /// and the engine holds per request, since the host names it. The
    /// engine's declared constants are not among them.
    fn request(&self, name: &[u8]) -> Option<(Value, bool)> {
        if name == b"PHP_SAPI" {
            return Some((self.sapi_constant, false));
        }
        let (_, row) = self.find(name)?;
        Some((row.value, row.deprecated))
    }

    /// The constant the request added under `name` as a compile of its
    /// own reads `EG(zend_constants)` past the persistent table
    /// ([`php_compile::RequestConstants`]). `PHP_SAPI` is not among them:
    /// php registers it `CONST_PERSISTENT`, so every compile, a file the
    /// cache keeps among them, substitutes it from the persistent table
    /// ([`php_compile::Persistent::host`]).
    pub(super) fn defined(&self, name: &[u8]) -> Option<php_compile::Defined> {
        let (_, row) = self.find(name)?;
        Some(php_compile::Defined {
            value: row.value.to_zval(),
            deprecated: row.deprecated,
        })
    }

    /// The row under `name` taken out, those after it moved down and the
    /// index laid again, a new generation of positions.
    fn remove(&mut self, name: &[u8]) -> Option<ConstantRow> {
        let (at, _) = self.find(name)?;
        let row = self.rows.remove(usize::try_from(at).ok()?);
        self.index.clear();
        for (at, row) in (0u32..).zip(&self.rows) {
            self.index.insert(row.name.clone().into_boxed_slice(), at);
        }
        self.generation = self.generation.wrapping_add(1);
        Some(row)
    }
}

impl Request {
    /// The values of the rows this request added to `EG(zend_constants)`
    /// given up, a debug build's teardown, as `zend_shutdown_executor_values`
    /// destroys the request's constants before the object store
    /// (`Z/Zend/zend_execute_API.c:286-307`), since a constant may hold an
    /// object. A release build leaves them to the heap's mass release.
    #[cfg(debug_assertions)]
    pub(super) fn free_constants(&mut self) {
        self.constants.index.clear();
        for row in std::mem::take(&mut self.constants.rows).into_iter().rev() {
            self.free_row(row);
        }
    }

    /// A row out of `EG(zend_constants)`: its value given up and its
    /// `zend_constant` freed, `free_zend_constant`.
    fn free_row(&mut self, row: ConstantRow) {
        self.release(row.value);
        if let Some(room) = row.room {
            room.free(self);
        }
        let file = match row.owner {
            ConstantOwner::User(file) => file,
            ConstantOwner::Core | ConstantOwner::Extension(_) => None,
        };
        for string in [row.named, file].into_iter().flatten() {
            string.give_up(self);
        }
    }

    /// `zend_register_constant`'s `pemalloc(sizeof(zend_constant))` for a
    /// row the request adds: what `define()` costs the request, and what
    /// a host's or an extension's constant registered while it runs costs
    /// beside its name:
    ///
    /// ```php
    /// $m = memory_get_usage(); define('ABCDEF', 1); const GHIJKL = 2; echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 80
    /// ```
    fn constant_room(&mut self) -> Option<Emalloc> {
        Emalloc::new(self, ZEND_CONSTANT).ok()
    }

    /// The name of a constant a host or an extension registers while the
    /// request runs, `zend_string_init_interned` ([`Request::interned`]):
    /// a string of the request's own before its first file compiles, as
    /// `do_cli` and `cli_register_file_handles` register theirs, and the
    /// engine's where its table holds the name after. The memory limit's
    /// fatal is recorded and ends the request.
    fn constant_name(&mut self, name: &[u8]) -> Option<Owner<PhpString>> {
        self.interned(name).ok()
    }

    /// `PHP_SAPI` as `php_module_startup` registers it: the engine's
    /// interned string for the host's name, where the engine has one.
    pub(super) fn register_sapi_constant(&mut self) {
        if let Some(name) = self
            .shared
            .sapi_constant(self.description.sapi_name.as_bytes())
        {
            self.constants.sapi_constant = name;
        }
    }

    /// What [`constants`](Self::constants) makes, its one owner not yet named.
    pub(crate) fn constants_made(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = self.array()?;
        let constants: Vec<(Vec<u8>, Value)> = self
            .constants
            .rows
            .iter()
            .filter(|row| matches!(row.owner, ConstantOwner::User(_)))
            .map(|row| (row.given.clone(), row.value))
            .collect();
        for (name, value) in constants {
            let key = PhpString::new(self, &name)?.into_key();
            let value = Owner::take(self, value);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `zend_register_constant` for a constant the program defines,
    /// `define()` and `const`: `name` is the key php writes, the
    /// namespace lowered. A sink: the row takes the value's owner, and
    /// keeps the file executing when it was written,
    /// `zend_register_constant`'s `filename`. Where the name stands
    /// already, or is `__COMPILER_HALT_OFFSET__`, `true`, `false` or
    /// `null`, `false`: php's warning is raised and the value given up
    /// after it, so a destructor that release serves runs after the
    /// warning, as `zval_ptr_dtor_nogc` runs it there.
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; } }
    /// const A = new D(1);
    /// const A = new D(2);
    /// var_dump(define('B', new D(3)), define('B', new D(4)), define('TRUE', new D(5)));
    /// echo "end\n";
    /// ```
    /// ```expect
    /// Warning: Constant A already defined, this will be an error in PHP 9 in %s on line 4
    /// dtor 2
    ///
    /// Warning: Constant B already defined, this will be an error in PHP 9 in %s on line 5
    /// dtor 4
    ///
    /// Warning: Constant TRUE already defined, this will be an error in PHP 9 in %s on line 5
    /// dtor 5
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// end
    /// dtor 1
    /// dtor 3
    /// ```
    pub(crate) fn define_constant(&mut self, name: &[u8], value: Owner<Value>) -> bool {
        self.define_attributed_constant(name, value, Vec::new())
    }

    /// The name and the file a constant the program defines is kept
    /// with are strings the row holds an owner of, as
    /// `zend_register_constant` copies `c->name` and
    /// `zend_get_executed_filename_ex()`: a constant an `eval` defines
    /// keeps the `eval`'s name string past it (the file under the
    /// recipe's `TMPDIR`, so that name has one length):
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/w3259-constants.inc';
    /// file_put_contents($f, <<<'PHP'
    /// <?php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W { function __toString() { return ""; } }');
    /// m('$q0 = 1;');
    /// foreach ([
    ///     'class R { const B = 1; } class Rlonger { const Blonger = 2; } const X1 = "y"; const X2 = 3; const X3 = 4; const Xlongname4 = 5;',
    ///     'const X5 = 3;',
    ///     'define("X6", 3);',
    ///     '$q = 1;',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 1144  class R { const B = 1; } class Rlonger { const Blonger = 2; } const X1 = "y"; const X2 = 3; const X3 = 4; const Xlongname4 = 5;
    /// 184  const X5 = 3;
    /// 184  define("X6", 3);
    /// 0  $q = 1;
    /// ```
    ///
    /// `ZEND_DECLARE_ATTRIBUTED_CONST`'s `zend_register_constant` and
    /// `zend_constant_add_attributes`: [`Request::define_constant`] with
    /// the attributes the declaration carries hung on the row, and
    /// `CONST_DEPRECATED` raised from them. `false`, nothing hung, the
    /// warning raised and the value given up, where the name stands
    /// already.
    pub(crate) fn define_attributed_constant(
        &mut self,
        name: &[u8],
        value: Owner<Value>,
        attributes: Vec<Attribute>,
    ) -> bool {
        // The row keeps the name as given.
        let given = name;
        let key = Request::constant_key(given);
        let name = key.as_slice();
        let special = name == HALT_OFFSET
            || name.eq_ignore_ascii_case(b"true")
            || name.eq_ignore_ascii_case(b"false")
            || name.eq_ignore_ascii_case(b"null");
        if special || self.constant_named(name).is_some() {
            self.refuse_constant(name, value);
            return false;
        }
        // `zend_get_executed_filename_ex`: the innermost frame's file,
        // none where nothing is executing.
        let file = self
            .innermost_frame()
            .and_then(crate::exec::FrameAt::code)
            .map(|code| code.filename.0.clone());
        let deprecated = crate::attributes::Flags::of(&attributes).deprecated;
        let room = self.constant_room();
        let named = self.declared_string(given).ok();
        let file = file.and_then(|file| self.executed_file_string(&file).ok());
        self.constants.push(ConstantRow {
            room,
            named,
            owner: ConstantOwner::User(file),
            name: name.to_vec(),
            given: given.to_vec(),
            value: value.hand_on(),
            attributes,
            deprecated,
        });
        true
    }

    /// `zend_register_constant` refusing `name`: php's warning, the name
    /// up to a NUL in it as `%s` prints `ZSTR_VAL(name)`, and then the
    /// value given up, `zval_ptr_dtor_nogc`, so a destructor it serves
    /// runs after the warning.
    fn refuse_constant(&mut self, name: &[u8], value: Owner<Value>) {
        let shown = crate::c_number::c_str(name);
        crate::api::raising::warning(
            self,
            &format!(
                "Constant {} already defined, this will be an error in PHP 9",
                String::from_utf8_lossy(shown)
            ),
        );
        value.give_up(self);
    }

    /// `zend_register_constant` at request start for a constant the
    /// host registers under `Core`, as `do_cli` registers
    /// `PHP_CLI_PROCESS_TITLE` and `cli_register_file_handles` the three
    /// streams: after every extension's own, so it stands last in
    /// `Core`'s group and before what the program defines. A sink: the
    /// row takes the value's owner.
    pub(crate) fn register_host_constant(&mut self, name: &'static [u8], value: Owner<Value>) {
        let named = self.constant_name(name);
        let room = self.constant_room();
        self.constants.push(ConstantRow::new(
            room,
            named,
            ConstantOwner::Core,
            name.to_vec(),
            value.hand_on(),
        ));
    }

    /// `PHP_CLI_PROCESS_TITLE`, which `do_cli` registers after
    /// `php_request_startup` as whether `cli_set_process_title` can set
    /// the title, for a host that registers it
    /// ([`crate::engine::ProcessTitle`]), then
    /// `cli_register_file_handles` (`Z/sapi/cli/php_cli.c:519`):
    /// `php://stdin` opened `rb`, `php://stdout` and `php://stderr` `wb`
    /// by the stream layer ([`crate::layer::StreamHooks::standard_handles`]),
    /// and the three constants on them, each holding its stream's one
    /// owner. Where one of the three does not open, php closes the others
    /// and registers none, which no host here reaches. A no-op for a host
    /// that asked for no handles ([`crate::engine::FileHandles`]), and
    /// with no stream layer the title alone.
    ///
    /// ```php
    /// var_dump(is_resource(STDIN), get_resource_type(STDOUT), defined('PHP_CLI_PROCESS_TITLE'));
    /// var_dump(STDIN, STDOUT, STDERR, fwrite(STDERR, "x"));
    /// var_dump(array_slice(array_keys(get_defined_constants(true)['Core']), -5));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(6) "stream"
    /// bool(true)
    /// resource(1) of type (stream)
    /// resource(2) of type (stream)
    /// resource(3) of type (stream)
    /// int(1)
    /// array(5) {
    ///   [0]=>
    ///   string(10) "PHP_BINARY"
    ///   [1]=>
    ///   string(21) "PHP_CLI_PROCESS_TITLE"
    ///   [2]=>
    ///   string(5) "STDIN"
    ///   [3]=>
    ///   string(6) "STDOUT"
    ///   [4]=>
    ///   string(6) "STDERR"
    /// }
    /// ```
    /// ```stderr
    /// x
    /// ```
    pub(crate) fn register_file_handles(&mut self, stdin_replaced: bool) -> Result<(), Thrown> {
        if self.description.file_handles.is_none() {
            return Ok(());
        }
        let titled = match self.process_title() {
            crate::engine::ProcessTitle::Unregistered => None,
            crate::engine::ProcessTitle::Unsaved => Some(false),
            crate::engine::ProcessTitle::Saved => Some(true),
        };
        if let Some(titled) = titled {
            self.register_host_constant(b"PHP_CLI_PROCESS_TITLE", Owner::bool(titled));
        }
        let Some(hooks) = self.stream_hooks() else {
            return Ok(());
        };
        let Some([stdin, stdout, stderr]) = (hooks.standard_handles)(self, stdin_replaced)? else {
            return Ok(());
        };
        self.register_host_constant(b"STDIN", stdin);
        self.register_host_constant(b"STDOUT", stdout);
        self.register_host_constant(b"STDERR", stderr);
        Ok(())
    }

    /// `phpdbg_register_file_handles` once `run <file` has opened its
    /// `stdin_file` (`Z/sapi/phpdbg/phpdbg.c:879-924`, `:1097`): the
    /// host's fd 0 is `stdin` from here, and the three constants are
    /// deleted, their streams with them, and registered again on streams
    /// opened anew.
    pub(crate) fn reregister_file_handles(
        &mut self,
        stdin: std::os::fd::OwnedFd,
    ) -> Result<(), Thrown> {
        let Some(handles) = &mut self.description.file_handles else {
            return Ok(());
        };
        handles.set_stdin(stdin);
        for name in [&b"STDIN"[..], b"STDOUT", b"STDERR"] {
            if let Some(row) = self.constants.remove(name) {
                self.free_row(row);
            }
        }
        self.register_file_handles(true)
    }

    /// `ZEND_CONSTANT_FLAGS(c) & CONST_DEPRECATED` of the constant the
    /// table holds under `name`, and whether a program defined it,
    /// `PHP_USER_CONSTANT`: what a fetch reads before it words a
    /// deprecation. None for a name no table has.
    pub(crate) fn constant_deprecation(&self, name: &[u8]) -> Option<(bool, bool)> {
        if let Some((_, row)) = self.constants.find(name) {
            let user = matches!(row.owner, ConstantOwner::User(_));
            return Some((row.deprecated, user));
        }
        self.shared
            .constant_deprecated(name)
            .map(|deprecated| (deprecated, false))
    }

    /// `zend_get_constant_str_impl` (`Z/Zend/zend_constants.c:262`), what
    /// `constant()` and a constant expression look a name up by: the
    /// table, then `__COMPILER_HALT_OFFSET__` as the executing file
    /// registered it (`zend_get_halt_offset_constant`), which the
    /// `FETCH_CONSTANT` handler never looks for.
    pub(crate) fn constant_str(&self, name: &[u8]) -> Option<Value> {
        self.constant_named(name).or_else(|| {
            let file = self
                .frames()
                .current_file()
                .filter(|_| name == HALT_OFFSET)?;
            self.constant_named(&halt_offset_key(file))
        })
    }

    /// `zend_register_constant`'s key for `name`, which every lookup of
    /// a namespaced constant compares against: the namespace, the bytes
    /// before the last `\`, lowered, and the name itself as written.
    pub(crate) fn constant_key(name: &[u8]) -> Vec<u8> {
        let mut key = name.to_vec();
        if let Some(slash) = key.iter().rposition(|b| *b == b'\\') {
            key[..slash].make_ascii_lowercase();
        }
        key
    }

    pub(crate) fn constant_named(&self, name: &[u8]) -> Option<Value> {
        (self.constants.request(name).map(|(value, _)| value))
            .or_else(|| self.shared.constant(name))
    }

    /// The constant the table holds under `name` as `FETCH_CONSTANT`
    /// finds it: where it stands, its value, and whether it is
    /// `CONST_DEPRECATED`, which php caches no slot for.
    pub(crate) fn constant_found(&self, name: &[u8]) -> Option<(ConstantAt, Value, bool)> {
        if name == b"PHP_SAPI" {
            return Some((ConstantAt::Sapi, self.constants.sapi_constant, false));
        }
        if let Some((at, row)) = self.constants.find(name) {
            let generation = self.constants.generation;
            return Some((
                ConstantAt::Request { at, generation },
                row.value,
                row.deprecated,
            ));
        }
        let at = self.shared.constant_index(name)?;
        let value = self.shared.constant_at(at)?;
        let deprecated = self.shared.constant_deprecated(name).unwrap_or(false);
        Some((
            ConstantAt::Declared(u32::try_from(at).ok()?),
            value,
            deprecated,
        ))
    }

    /// The value of the constant a slot found, none where the request's
    /// rows moved since.
    #[inline]
    pub(crate) fn constant_at(&self, at: ConstantAt) -> Option<Value> {
        match at {
            ConstantAt::Sapi => Some(self.constants.sapi_constant),
            ConstantAt::Declared(at) => self.shared.constant_at(usize::try_from(at).ok()?),
            ConstantAt::Request { at, generation } => {
                if generation != self.constants.generation {
                    return None;
                }
                Some(self.constants.rows.get(usize::try_from(at).ok()?)?.value)
            }
        }
    }

    /// The fd the host handed under one of its standard streams
    /// ([`crate::engine::FileHandles`]), `None` where it handed none.
    #[must_use]
    pub(crate) fn host_fd(&self, standard: Standard) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.description.file_handles.as_ref()?.fd(standard)
    }

    /// Whether the host's standard fds are the process's own, as php's
    /// CLI registers them ([`crate::engine::FileHandles::process`]).
    #[must_use]
    pub(crate) fn host_fds_are_process(&self) -> bool {
        self.description
            .file_handles
            .as_ref()
            .is_some_and(crate::engine::FileHandles::is_process)
    }

    /// `fclose()` reaching the stream a standard stream's constant holds:
    /// the host's fd under it closed, where this request is the only one
    /// holding it ([`crate::engine::FileHandles`]).
    pub(crate) fn close_host_fd(&mut self, standard: Standard) {
        if let Some(handles) = &mut self.description.file_handles {
            handles.close(standard as usize);
        }
    }
}

impl<'r> Symbols<&'r Request> {
    /// `EG(zend_constants)` in its order, each row the extension it was
    /// registered under, `ZEND_CONSTANT_MODULE_NUMBER`, by name, none for
    /// one the program defined, `PHP_USER_CONSTANT`: the engine's own
    /// under `Core`, then each extension's in bundle order, then what
    /// `define()` and `const` wrote. A value is a view, no owner taken;
    /// a declared constant whose value the engine did not publish is no
    /// row. What `get_defined_constants(true)` groups and phpdbg's `info
    /// constants` walks for the program's own.
    #[must_use]
    pub fn constant_table(self) -> Vec<(Option<&'r str>, &'r [u8], Value)> {
        let decls = self.0.shared.declared().constants.declared.rows();
        let modules =
            (self.0.shared.constant_modules.iter()).map(|&at| self.0.extensions().name(at));
        let mut rows: Vec<(Option<&str>, &[u8], Value)> =
            Vec::with_capacity(decls.len() + self.0.constants.rows.len() + 1);
        for ((at, decl), module) in decls.iter().enumerate().zip(modules) {
            // `PHP_SAPI` stands where php registers it, before
            // `PHP_BINARY`, and is the request's.
            if decl.name.0.as_ref() == b"PHP_BINARY" {
                rows.push((Some("Core"), b"PHP_SAPI", self.0.constants.sapi_constant));
            }
            if let Some(value) = self.0.shared.constant_at(at) {
                rows.push((module, &decl.name.0, value));
            }
        }
        rows.extend(self.0.constants.rows.iter().map(|row| {
            let module = match &row.owner {
                ConstantOwner::Core => Some("Core"),
                ConstantOwner::Extension(extension) => self.0.extensions().name(*extension),
                ConstantOwner::User(_) => None,
            };
            (module, row.given.as_slice(), row.value)
        }));
        rows
    }

    /// `zend_constant.filename`: the file a constant the program
    /// defined was defined in, and none for one an extension or the
    /// engine registered, php's `NULL` for a persistent constant.
    ///
    /// ```php
    /// define('D', 1); const C = 2;
    /// var_dump((new ReflectionConstant('D'))->getFileName() === __FILE__, (new ReflectionConstant('C'))->getFileName() === __FILE__, (new ReflectionConstant('PHP_VERSION'))->getFileName());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    #[must_use]
    pub fn constant_file(self, name: &[u8]) -> Option<&'r [u8]> {
        self.0
            .constants
            .find(name)
            .and_then(|(_, row)| match &row.owner {
                ConstantOwner::User(file) => file.as_ref().map(|file| file.as_bytes()),
                ConstantOwner::Core | ConstantOwner::Extension(_) => None,
            })
    }

    /// `zend_constant::attributes`: the attributes the declaration of
    /// the global constant `name` carries, what an extension declared or
    /// a program's `DECLARE_ATTRIBUTED_CONST` hung on it, or an extension
    /// registered it with while the request runs
    /// ([`Symbols::register_constant`]), and the empty list for a name no
    /// table has. The key is the one
    /// `zend_register_constant` writes, the namespace lowered. What
    /// `ReflectionConstant::getAttributes()` reads, and what the
    /// deprecation a read raises is worded from
    /// ([`crate::api::raising::deprecated_constant`]).
    ///
    /// ```php
    /// #[MyAttribute(1)]
    /// const C = 1;
    /// define('D', 2);
    /// var_dump(count((new ReflectionConstant('C'))->getAttributes()),
    ///     count((new ReflectionConstant('D'))->getAttributes()),
    ///     count((new ReflectionConstant('E_STRICT'))->getAttributes()));
    /// ```
    /// ```expect
    /// int(1)
    /// int(0)
    /// int(1)
    /// ```
    #[must_use]
    pub fn constant_attributes(self, name: &[u8]) -> &'r [Attribute] {
        if let Some((_, row)) = self.0.constants.find(name) {
            return &row.attributes;
        }
        self.0
            .shared
            .declared()
            .constants
            .declared
            .find(name)
            .map_or(&[], |(_, decl)| decl.attributes.as_slice())
    }

    /// `ZEND_CONSTANT_MODULE_NUMBER(c)` of the constant the table holds
    /// under `name`: the extension that registered it, `Core` for the
    /// engine's own and a host's, and none for one a program defined
    /// (`PHP_USER_CONSTANT`) or a name no table has. One lookup, as
    /// `ReflectionConstant::getExtension` reads the number off the
    /// constant:
    ///
    /// ```php
    /// define('MINE', 1);
    /// foreach (['E_ALL', 'PHP_SAPI', 'PHP_VERSION', 'PREG_SPLIT_NO_EMPTY', 'JSON_ERROR_NONE', 'STDIN', 'MINE'] as $name) {
    ///     $c = new ReflectionConstant($name);
    ///     var_dump($c->getExtensionName(), $c->getExtension()?->getName());
    /// }
    /// ```
    /// ```expect capture
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "pcre"
    /// string(4) "pcre"
    /// string(4) "json"
    /// string(4) "json"
    /// string(4) "Core"
    /// string(4) "Core"
    /// bool(false)
    /// NULL
    /// ```
    #[must_use]
    pub fn constant_extension(self, name: &[u8]) -> Option<ExtensionId> {
        if name == b"PHP_SAPI" {
            return Some(ExtensionId::CORE);
        }
        if let Some((_, row)) = self.0.constants.find(name) {
            return match row.owner {
                ConstantOwner::Core => Some(ExtensionId::CORE),
                ConstantOwner::Extension(extension) => Some(extension),
                ConstantOwner::User(_) => None,
            };
        }
        self.0.shared.constant_module(name)
    }
}

impl Symbols<&mut Request> {
    /// `get_defined_constants()`: what `define()` declared in this
    /// request, each name a string of the request's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn constants(self) -> Result<Owner<PhpArray>, Thrown> {
        let made = self.0.constants_made();
        adopted(made)
    }

    /// `REGISTER_STRINGL_CONSTANT` and its kin while a request runs:
    /// `name` registered under `extension`, php's `module_number`,
    /// carrying `attributes` as `zend_add_attribute` puts them on the
    /// constant, a `#[\Deprecated]` among them php's `CONST_DEPRECATED`,
    /// which a read raises. Where the request has registered the name
    /// already, by an extension or by `define()`, the row keeps its
    /// owner and attributes and takes `value`, the old value released,
    /// which is what an extension writes through the `zval` that
    /// `zend_get_constant_str` finds. A sink: the row takes the value's
    /// owner; `false`, php's warning raised and the owner given up after
    /// it, as `zend_register_constant` refuses a name, for one the engine
    /// or an extension declared as it started, which no request writes. The
    /// session's `SID` is the witness, in `ext/session`.
    pub fn register_constant(
        self,
        extension: ExtensionId,
        name: &[u8],
        value: Owner<Value>,
        attributes: Vec<Attribute>,
    ) -> bool {
        if let Some(row) = self
            .0
            .constants
            .rows
            .iter_mut()
            .find(|row| row.name == name)
        {
            let old = std::mem::replace(&mut row.value, value.hand_on());
            self.0.release(old);
            return true;
        }
        if name == b"PHP_SAPI" || self.0.shared.constant(name).is_some() {
            self.0.refuse_constant(name, value);
            return false;
        }
        let deprecated = crate::attributes::Flags::of(&attributes).deprecated;
        let named = self.0.constant_name(name);
        let room = self.0.constant_room();
        self.0.constants.push(ConstantRow {
            room,
            named,
            owner: ConstantOwner::Extension(extension),
            name: name.to_vec(),
            given: name.to_vec(),
            value: value.hand_on(),
            attributes,
            deprecated,
        });
        true
    }
}
