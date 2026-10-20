//! `class ZipArchive` (`php_zip.c`): the object over one open archive,
//! its six properties read through `zip_prop_handlers`, and its methods.

use php_ext::coerce::c_str;
use std::path::PathBuf;

use php_ext::abi::{
    Callable, DebugInfo, ObjectHandlers, OrFalse, Path, PropertyPlace, PropertyRead, PropertyTest,
    Ref, Refusals, Std, This, Union, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::call;
use php_ext::coerce::{truthy, value_name};
use php_ext::raising::{
    Level, Thrown, arg_value_error, docref, leave_pending, throw, while_pending,
};
use php_ext::request::{Request, Streams as _};
use php_ext::values::walk;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Resource, Value, ValueRef};

use crate::archive::{self as za, Archive, Source};
use crate::c;
use crate::error::{Done, Er, Status};
use crate::read::Stdio;
use crate::write::Hooks;

/// A callable kept past the call that registered it, `zend_fcc_dup`.
pub(crate) type Kept = Box<Owner<Callable>>;

/// `ze_zip_object.last_id`, `-1` until an add.
pub(crate) struct LastId(pub(crate) i64);

impl Default for LastId {
    fn default() -> LastId {
        LastId(-1)
    }
}

/// `ze_zip_object`: the archive while open, its resolved name, the last
/// index added, the error the last close left, and the callbacks.
#[derive(Default)]
pub(crate) struct Intern {
    pub(crate) za: Option<Box<Archive>>,
    pub(crate) filename: Option<Vec<u8>>,
    pub(crate) last_id: LastId,
    pub(crate) err: Status,
    pub(crate) progress: Option<Kept>,
    pub(crate) cancel: Option<Kept>,
    /// The archive's number, which its entries' streams read by.
    pub(crate) id: u64,
}

/// The archive numbered `id` discarded: its entries' streams read php's
/// `Containing zip archive was closed` from here.
fn discarded(req: &mut Request, id: u64) {
    let state = req.state::<crate::State>();
    state.closed.insert(id);
    state.files.remove(&id);
}

/// Archive `id`'s stdio `FILE`, out of the request for one read.
pub(crate) fn file(req: &mut Request, id: u64) -> Option<Stdio> {
    req.state::<crate::State>().files.remove(&id)
}

/// Archive `id`'s stdio `FILE` back.
pub(crate) fn file_back(req: &mut Request, id: u64, file: Option<Stdio>) {
    if let Some(f) = file {
        req.state::<crate::State>().files.insert(id, f);
    }
}

/// `op` on the object's archive with the archive's `FILE` lent to it.
///
/// One `FILE` serves the archive and every stream over its entries, so
/// a stream reads what another entry's opening left in its buffer:
///
/// ```php
/// $z = new ZipArchive; $z->open('file-witness.zip', ZipArchive::CREATE|ZipArchive::OVERWRITE);
/// foreach (['a', 'b'] as $n) { $z->addFromString($n, str_repeat($n, 20000)); $z->setCompressionName($n, ZipArchive::CM_STORE); }
/// $z->close(); $z->open('file-witness.zip'); $s = $z->getStream('a'); $t = $z->getStream('b');
/// $h = fopen('file-witness.zip', 'r+'); ftruncate($h, 1000); fclose($h);
/// var_dump(strlen(fread($s, 8192)), fread($t, 8192), strlen($z->getFromName('a', 100))); $z->close(); unlink('file-witness.zip');
/// ```
/// ```expect
///
/// Warning: fread(): Zip stream error: Premature end of file in %s on line %d
/// int(969)
/// bool(false)
/// int(100)
/// ```
pub(crate) fn with_file<T>(
    req: &mut Request,
    object: PhpObject,
    op: impl FnOnce(&mut Archive) -> T,
) -> Option<T> {
    let id = object.native::<Intern>(req).map_or(0, |i| i.id);
    let mut f = file(req, id);
    let za = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.za.as_deref_mut());
    let out = za.map(|za| {
        za.lend(&mut f);
        let out = op(za);
        za.lend(&mut f);
        out
    });
    file_back(req, id, f);
    out
}

/// `zip_prop_handlers`: the six properties, each read from the object.
#[derive(Clone, Copy)]
enum Prop {
    LastId,
    Status,
    StatusSys,
    NumFiles,
    Filename,
    Comment,
}

impl Prop {
    const ALL: [(&'static [u8], Prop); 6] = [
        (b"lastId", Prop::LastId),
        (b"status", Prop::Status),
        (b"statusSys", Prop::StatusSys),
        (b"numFiles", Prop::NumFiles),
        (b"filename", Prop::Filename),
        (b"comment", Prop::Comment),
    ];

    fn of(name: &[u8]) -> Option<Prop> {
        Prop::ALL.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
    }

    /// `php_zip_property_reader`.
    fn read(self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
        let Some(i) = object.native::<Intern>(req) else {
            return Ok(Owner::null());
        };
        let status = i.za.as_ref().map_or(i.err, |za| za.error);
        let scalar = match self {
            Prop::LastId => Value::int(i.last_id.0),
            Prop::Status => Value::int(i64::from(status.zip)),
            Prop::StatusSys => Value::int(i64::from(status.sys)),
            Prop::NumFiles => {
                let n = i.za.as_ref().map_or(0, |za| za.num_entries());
                Value::int(i64::try_from(n).unwrap_or(i64::MAX))
            }
            Prop::Filename => {
                let bytes = i.filename.clone().unwrap_or_default();
                return Ok(PhpString::new(req, &bytes)?.into_value());
            }
            Prop::Comment => {
                let bytes = i.za.as_ref().map(|za| za.archive_comment(0));
                return Ok(PhpString::new(req, &bytes.unwrap_or_default())?.into_value());
            }
        };
        Ok(Owner::take(req, scalar))
    }
}

/// `zip_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `php_zip_object_free_storage`: an open archive closed, its changes
    /// written, and php's warning where that fails. What a progress or
    /// cancel callback threw meanwhile is left pending, for the release
    /// that freed the object to raise.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/witness-free-' . getmypid() . '.zip';
    /// $z = new ZipArchive;
    /// $z->open($f, ZipArchive::CREATE | ZipArchive::OVERWRITE);
    /// $z->addFromString('a.txt', 'hello');
    /// $z->registerProgressCallback(0.5, function ($r) { echo "progress\n"; throw new Exception("progress"); });
    /// try { $z = null; echo "after\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// @unlink($f);
    /// ```
    /// ```expect
    /// progress
    /// caught progress
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        if let Some((mut archive, id)) = object
            .native_mut::<Intern>(req)
            .and_then(|i| Some((i.za.take()?, i.id)))
        {
            let (result, thrown) = close_with_hooks(req, object, &mut archive);
            discarded(req, id);
            if result.is_err() {
                let text = archive.error.text(req);
                let text = format!("Cannot destroy the zip context: {text}");
                match &thrown {
                    Some(t) => {
                        while_pending(req, t, |req| docref(req, Level::Warning, &text));
                    }
                    None => docref(req, Level::Warning, &text),
                }
            }
            if let Some(thrown) = thrown {
                leave_pending(req, thrown);
            }
        }
        let kept = object
            .native_mut::<Intern>(req)
            .map(|i| (i.progress.take(), i.cancel.take()));
        if let Some((p, c)) = kept {
            for k in [p, c].into_iter().flatten() {
                k.give_up(req);
            }
        }
        Std.free_obj(req, object);
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        match Prop::of(name) {
            Some(p) => p.read(req, object),
            None => Std.read_property(req, object, name, read),
        }
    }

    /// `php_zip_write_property`: the six refused.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if Prop::of(name.as_bytes()).is_none() {
            return Std.write_property(req, object, name, value);
        }
        let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
        name.give_up(req);
        value.give_up(req);
        let class = String::from_utf8_lossy(&Std.get_class_name(req, object)).into_owned();
        let message = format!("Cannot write read-only property {class}::${shown}");
        Err(throw(req, "Error", &message))
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        match Prop::of(name) {
            Some(_) => Ok(PropertyPlace::Handlers),
            None => Std.get_property_ptr_ptr(req, object, name),
        }
    }

    /// `php_zip_has_property`.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let Some(p) = Prop::of(name) else {
            return Std.has_property(req, object, name, test);
        };
        if test == PropertyTest::Exists {
            return Ok(true);
        }
        let value = p.read(req, object)?;
        let answer = match test {
            PropertyTest::NotEmpty => truthy(req, &value),
            _ => Ok(!matches!(value.as_ref(), ValueRef::Null)),
        };
        value.give_up(req);
        answer
    }

    /// `php_zip_get_properties`: each of the six written into the table
    /// as it reads now, then the table. `unserialize()` reads it first,
    /// so what it writes under one of the six lands in the table's row
    /// held to no type, and the property reads on from the archive.
    ///
    /// ```php
    /// $z = unserialize('O:10:"ZipArchive":2:{s:8:"filename";a:1:{i:0;i:7;}s:6:"status";s:1:"x";}');
    /// var_dump($z->filename, $z->status);
    /// echo serialize($z), "\n";
    /// ```
    /// ```expect
    /// string(0) ""
    /// int(0)
    /// O:10:"ZipArchive":6:{s:6:"lastId";i:-1;s:6:"status";i:0;s:9:"statusSys";i:0;s:8:"numFiles";i:0;s:8:"filename";s:0:"";s:7:"comment";s:0:"";}
    /// ```
    fn get_properties(&self, req: &mut Request, object: PhpObject) -> Result<walk::Table, Thrown> {
        refresh(req, object)?;
        Std.get_properties(req, object)
    }

    /// `zend_std_get_debug_info` over the table `get_properties` writes.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        refresh(req, object)?;
        Std.get_debug_info(req, object)
    }

    /// `php_zip_get_properties` stands in `get_properties`, so a
    /// `foreach` walks the six it writes, then a subclass's own, and
    /// `spl_array_set_array` refuses the object.
    ///
    /// ```php
    /// class Z extends ZipArchive { public $x = 1; }
    /// foreach (new Z as $k => $v) { echo $k, "=", var_export($v, true), "\n"; }
    /// ```
    /// ```expect
    /// lastId=-1
    /// status=0
    /// statusSys=0
    /// numFiles=0
    /// filename=''
    /// comment=''
    /// x=1
    /// ```
    fn properties_overloaded(&self) -> bool {
        true
    }
}

/// The six written into the property table as they read now, with
/// `zend_hash_update`, which puts a plain value where the table held
/// php's `IS_INDIRECT` to the slot ([`PhpObject::update_row`]):
/// `unserialize()` writes those rows with no type to hold to, and a
/// subclass's own properties stay slots.
///
/// ```php
/// class Z extends ZipArchive { public $x = 1; }
/// var_dump(unserialize('O:1:"Z":1:{s:8:"filename";a:0:{}}'));
/// $z = new Z; var_dump($z); $z->x = 5; $z->numFiles;
/// var_dump($z, get_object_vars($z));
/// ```
/// ```expect
/// object(Z)#1 (7) {
///   ["lastId"]=>
///   int(-1)
///   ["status"]=>
///   int(0)
///   ["statusSys"]=>
///   int(0)
///   ["numFiles"]=>
///   int(0)
///   ["filename"]=>
///   string(0) ""
///   ["comment"]=>
///   string(0) ""
///   ["x"]=>
///   int(1)
/// }
/// object(Z)#1 (7) {
///   ["lastId"]=>
///   int(-1)
///   ["status"]=>
///   int(0)
///   ["statusSys"]=>
///   int(0)
///   ["numFiles"]=>
///   int(0)
///   ["filename"]=>
///   string(0) ""
///   ["comment"]=>
///   string(0) ""
///   ["x"]=>
///   int(1)
/// }
/// object(Z)#1 (7) {
///   ["lastId"]=>
///   int(-1)
///   ["status"]=>
///   int(0)
///   ["statusSys"]=>
///   int(0)
///   ["numFiles"]=>
///   int(0)
///   ["filename"]=>
///   string(0) ""
///   ["comment"]=>
///   string(0) ""
///   ["x"]=>
///   int(5)
/// }
/// array(7) {
///   ["lastId"]=>
///   int(-1)
///   ["status"]=>
///   int(0)
///   ["statusSys"]=>
///   int(0)
///   ["numFiles"]=>
///   int(0)
///   ["filename"]=>
///   string(0) ""
///   ["comment"]=>
///   string(0) ""
///   ["x"]=>
///   int(5)
/// }
/// ```
fn refresh(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    for (name, p) in Prop::ALL {
        let value = p.read(req, object)?;
        let key = PhpString::new(req, name)?;
        let updated = object.update_row(req, &Key::Str(*key), value);
        key.give_up(req);
        updated?;
    }
    Ok(())
}

/// The callbacks a close serves: the progress and cancel callables, and
/// the first throwable either left.
struct PhpHooks<'r> {
    req: &'r mut Request,
    progress: Option<Callable>,
    cancel: Option<Callable>,
    thrown: Option<Thrown>,
}

impl Hooks for PhpHooks<'_> {
    /// `php_zip_progress_callback`.
    fn progress(&mut self, current: f64) {
        let (Some(cb), None) = (&self.progress, &self.thrown) else {
            return;
        };
        match call(self.req, cb, &[Value::float(current)]) {
            Ok(v) => v.give_up(self.req),
            Err(t) => self.thrown = Some(t),
        }
    }

    /// `php_zip_cancel_callback`: a throw cancels, and so does a value
    /// that is no integer, after php's `TypeError`.
    fn cancel(&mut self) -> bool {
        if self.thrown.is_some() {
            return true;
        }
        let Some(cb) = &self.cancel else {
            return false;
        };
        let v = match call(self.req, cb, &[]) {
            Ok(v) => v,
            Err(t) => {
                self.thrown = Some(t);
                return true;
            }
        };
        let long = match v.deref().as_ref() {
            ValueRef::Int(n) => Some(n),
            ValueRef::Bool(b) => Some(i64::from(b)),
            ValueRef::Null => Some(0),
            ValueRef::Float(f) if f.is_finite() => Some(c::float_long(f)),
            ValueRef::Str(s) => std::str::from_utf8(s.as_bytes())
                .ok()
                .and_then(|t| t.trim().parse::<i64>().ok()),
            _ => None,
        };
        let Some(long) = long else {
            let name = value_name(self.req, &v);
            v.give_up(self.req);
            let message = format!(
                "Return value of callback provided to ZipArchive::registerCancelCallback() must be of type int, {name} returned"
            );
            self.thrown = Some(throw(self.req, "TypeError", &message));
            return true;
        };
        v.give_up(self.req);
        c::i32(long) != 0
    }
}

/// `zip_close` with the object's callbacks served, and the throwable
/// one of them left.
fn close_with_hooks(
    req: &mut Request,
    object: PhpObject,
    archive: &mut Archive,
) -> (Result<(), ()>, Option<Thrown>) {
    let (progress, cancel) = object.native::<Intern>(req).map_or((None, None), |i| {
        (
            i.progress.as_deref().map(|k| (**k).clone()),
            i.cancel.as_deref().map(|k| (**k).clone()),
        )
    });
    let umask = req.environment().umask();
    let mut file = file(req, object.native::<Intern>(req).map_or(0, |i| i.id));
    archive.lend(&mut file);
    let mut hooks = PhpHooks {
        req,
        progress,
        cancel,
        thrown: None,
    };
    let result = archive.close(&mut hooks, umask).map_err(|_| ());
    (result, hooks.thrown)
}

/// `zend_value_error("Invalid or uninitialized Zip object")`.
fn invalid(req: &mut Request) -> Thrown {
    throw(req, "ValueError", "Invalid or uninitialized Zip object")
}

/// `ZIP_FROM_OBJECT`: the open archive, or php's `ValueError`.
macro_rules! archive {
    ($req:expr, $this:expr) => {{
        let object = $this.object();
        if object.native::<Intern>($req).is_none_or(|i| i.za.is_none()) {
            return Err(invalid($req));
        }
        match object
            .native_mut::<Intern>($req)
            .and_then(|i| i.za.as_deref_mut())
        {
            Some(za) => za,
            None => return Err(invalid($req)),
        }
    }};
}

/// A `zend_long` index as libzip's `zip_uint64_t`: a negative one past
/// every entry.
fn index(i: i64) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

/// The engine's `PhpString` of `bytes`.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, bytes)
}

/// `RETURN_SB`.
fn stat_array(req: &mut Request, st: &za::Stat) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    let name = string(req, &st.name)?;
    let rows: [(&[u8], Value); 8] = [
        (b"name", Value::string(*name)),
        (b"index", Value::int(c::long(st.index))),
        (b"crc", Value::int(i64::from(st.crc))),
        (b"size", Value::int(c::long(st.size))),
        (b"mtime", Value::int(st.mtime)),
        (b"comp_size", Value::int(c::long(st.comp_size))),
        (b"comp_method", Value::int(i64::from(st.comp_method))),
        (
            b"encryption_method",
            Value::int(i64::from(st.encryption_method)),
        ),
    ];
    for (k, v) in rows {
        out.set_str(req, k, Owner::take(req, v))?;
    }
    name.give_up(req);
    Ok(out)
}

/// `expand_filepath`: the path against the working directory, or `None`.
pub(crate) fn expand(req: &mut Request, path: &[u8]) -> Option<Vec<u8>> {
    let cwd = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    req.environment_mut()
        .virtual_file_ex(&cwd, path, php_ext::request::Resolve::FilePath)
}

/// `ZIP_OPENBASEDIR_CHECKPATH`: `true` where `open_basedir` refuses.
pub(crate) fn basedir_refuses(req: &mut Request, path: &[u8]) -> bool {
    req.environment_mut()
        .check_open_basedir(path, true)
        .is_err()
}

fn host(path: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(path))
}

/// `php_zip_add_file`: `filename` added as `entry`, or as the data of
/// `replace`, its window `start` and `len` long; `lastId` the new index.
#[expect(clippy::too_many_arguments, reason = "php_zip_add_file's own eight")]
fn add_file(
    req: &mut Request,
    object: PhpObject,
    filename: &[u8],
    entry: &[u8],
    start: i64,
    len: i64,
    replace: Option<i64>,
    flags: i64,
) -> R<bool> {
    if basedir_refuses(req, filename) {
        return Ok(false);
    }
    let Some(resolved) = expand(req, filename) else {
        docref(req, Level::Warning, "No such file or directory");
        return Ok(false);
    };
    if req.url_stat_quiet(&resolved, false)?.found().is_none() {
        docref(req, Level::Warning, "No such file or directory");
        return Ok(false);
    }
    let mut flags = c::u32(flags);
    let mut file = None;
    if flags & OPEN_FILE_NOW != 0 {
        let Ok(f) = std::fs::File::open(host(&resolved)) else {
            return Ok(false);
        };
        flags ^= OPEN_FILE_NOW;
        file = Some(f);
    }
    let source = Source::file(host(&resolved), file, c::u64(start), len);
    let Some(za) = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.za.as_deref_mut())
    else {
        return Ok(false);
    };
    let source = match source {
        Ok(Some(s)) => s,
        Ok(None) => return Ok(false),
        Err(er) => {
            za.error.set(er, 0);
            return Ok(false);
        }
    };
    let Some(idx) = replace else {
        return Ok(added(req, object, |za| {
            za.file_replace(None, Some(c_str(entry)), source, flags)
        }));
    };
    if index(idx) >= za.num_entries() {
        za.error.set(Er::Inval, 0);
        return Ok(false);
    }
    let done = za
        .file_replace(Some(index(idx)), None, source, flags)
        .is_ok();
    if done {
        za.error = Status::default();
    }
    Ok(done)
}

/// `ZIP_FL_OPEN_FILE_NOW`, php's own bit past libzip's.
const OPEN_FILE_NOW: u32 = 1 << 30;

/// An add: its index kept as `lastId` and the error cleared, or `-1`
/// kept.
fn added(
    req: &mut Request,
    object: PhpObject,
    add: impl FnOnce(&mut Archive) -> Done<usize>,
) -> bool {
    let Some(i) = object.native_mut::<Intern>(req) else {
        return false;
    };
    let Some(za) = i.za.as_deref_mut() else {
        return false;
    };
    let result = add(za);
    if result.is_ok() {
        za.error = Status::default();
    }
    i.last_id.0 = result.map_or(-1, |n| c::long(n as u64));
    result.is_ok()
}

/// The request, the receiver, and the results the methods answer with.
type Rq = Request;
type Z = This<ZipArchive>;
type R<T> = Result<T, Thrown>;
type Str = R<OrFalse<Owner<PhpString>>>;
type Arr = R<OrFalse<Owner<PhpArray>>>;
type Res = R<Untyped<OrFalse<Owner<Resource>>>>;

/// `ZIP_FROM_OBJECT`, a name `zend_argument_must_not_be_empty_error`
/// refuses, and `zip_name_locate`: the archive and the index, or `$none`.
macro_rules! named {
    ($req:expr, $this:expr, $name:expr, $none:expr) => {{
        archive!($req, $this);
        if $name.is_empty() {
            return Err(arg_value_error($req, 1, "not be empty"));
        }
        let za = archive!($req, $this);
        match za.name_locate(c_str($name.as_bytes()), 0) {
            Ok(idx) => (za, idx),
            Err(_) => return Ok($none),
        }
    }};
}

/// `ZIP_FROM_OBJECT` and `PHP_ZIP_STAT_INDEX`: the archive and the
/// index, or `$none` where `zip_stat_index` fails.
macro_rules! indexed {
    ($req:expr, $this:expr, $index:expr, $none:expr) => {{
        let za = archive!($req, $this);
        if za.stat_index(index($index), 0).is_err() {
            return Ok($none);
        }
        (za, index($index))
    }};
}

/// `class ZipArchive implements Countable`.
///
/// ```php
/// $z = new ZipArchive;
/// var_dump($z, count(get_object_vars($z)), isset($z->status), empty($z->numFiles), isset($z->nope));
/// try { $z->status = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $z->lastId++; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone $z; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class Mine extends ZipArchive { public $x = 1; }
/// try { $m = new Mine; $m->comment = 'c'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump((array) new Mine);
/// ```
/// ```expect
/// object(ZipArchive)#1 (6) {
///   ["lastId"]=>
///   int(-1)
///   ["status"]=>
///   int(0)
///   ["statusSys"]=>
///   int(0)
///   ["numFiles"]=>
///   int(0)
///   ["filename"]=>
///   string(0) ""
///   ["comment"]=>
///   string(0) ""
/// }
/// int(6)
/// bool(true)
/// bool(true)
/// bool(false)
/// Cannot write read-only property ZipArchive::$status
/// Cannot write read-only property ZipArchive::$lastId
/// Trying to clone an uncloneable object of class ZipArchive
/// Cannot write read-only property Mine::$comment
/// array(7) {
///   ["lastId"]=>
///   int(-1)
///   ["status"]=>
///   int(0)
///   ["statusSys"]=>
///   int(0)
///   ["numFiles"]=>
///   int(0)
///   ["filename"]=>
///   string(0) ""
///   ["comment"]=>
///   string(0) ""
///   ["x"]=>
///   int(1)
/// }
/// ```
#[php_class(size = 176, payload = Intern, handlers = HANDLERS, implements(crate::Countable))]
pub(crate) struct ZipArchive;

#[php_class]
impl ZipArchive {
    const CREATE: i64 = 1;
    const EXCL: i64 = 2;
    const CHECKCONS: i64 = 4;
    const OVERWRITE: i64 = 8;
    const RDONLY: i64 = 16;
    const FL_NOCASE: i64 = 1;
    const FL_NODIR: i64 = 2;
    const FL_COMPRESSED: i64 = 4;
    const FL_UNCHANGED: i64 = 8;
    #[php(deprecated(since = "8.3"))]
    const FL_RECOMPRESS: i64 = 16;
    const FL_ENCRYPTED: i64 = 32;
    const FL_OVERWRITE: i64 = 8192;
    const FL_LOCAL: i64 = 256;
    const FL_CENTRAL: i64 = 512;
    const FL_ENC_GUESS: i64 = 0;
    const FL_ENC_RAW: i64 = 64;
    const FL_ENC_STRICT: i64 = 128;
    const FL_ENC_UTF_8: i64 = 2048;
    const FL_ENC_CP437: i64 = 4096;
    const FL_OPEN_FILE_NOW: i64 = 1_073_741_824;
    const CM_DEFAULT: i64 = -1;
    const CM_STORE: i64 = 0;
    const CM_SHRINK: i64 = 1;
    const CM_REDUCE_1: i64 = 2;
    const CM_REDUCE_2: i64 = 3;
    const CM_REDUCE_3: i64 = 4;
    const CM_REDUCE_4: i64 = 5;
    const CM_IMPLODE: i64 = 6;
    const CM_DEFLATE: i64 = 8;
    const CM_DEFLATE64: i64 = 9;
    const CM_PKWARE_IMPLODE: i64 = 10;
    const CM_BZIP2: i64 = 12;
    const CM_LZMA: i64 = 14;
    const CM_LZMA2: i64 = 33;
    const CM_ZSTD: i64 = 93;
    const CM_XZ: i64 = 95;
    const CM_TERSE: i64 = 18;
    const CM_LZ77: i64 = 19;
    const CM_WAVPACK: i64 = 97;
    const CM_PPMD: i64 = 98;
    const ER_OK: i64 = 0;
    const ER_MULTIDISK: i64 = 1;
    const ER_RENAME: i64 = 2;
    const ER_CLOSE: i64 = 3;
    const ER_SEEK: i64 = 4;
    const ER_READ: i64 = 5;
    const ER_WRITE: i64 = 6;
    const ER_CRC: i64 = 7;
    const ER_ZIPCLOSED: i64 = 8;
    const ER_NOENT: i64 = 9;
    const ER_EXISTS: i64 = 10;
    const ER_OPEN: i64 = 11;
    const ER_TMPOPEN: i64 = 12;
    const ER_ZLIB: i64 = 13;
    const ER_MEMORY: i64 = 14;
    const ER_CHANGED: i64 = 15;
    const ER_COMPNOTSUPP: i64 = 16;
    const ER_EOF: i64 = 17;
    const ER_INVAL: i64 = 18;
    const ER_NOZIP: i64 = 19;
    const ER_INTERNAL: i64 = 20;
    const ER_INCONS: i64 = 21;
    const ER_REMOVE: i64 = 22;
    const ER_DELETED: i64 = 23;
    const ER_ENCRNOTSUPP: i64 = 24;
    const ER_RDONLY: i64 = 25;
    const ER_NOPASSWD: i64 = 26;
    const ER_WRONGPASSWD: i64 = 27;
    const ER_OPNOTSUPP: i64 = 28;
    const ER_INUSE: i64 = 29;
    const ER_TELL: i64 = 30;
    const ER_COMPRESSED_DATA: i64 = 31;
    const ER_CANCELLED: i64 = 32;
    const ER_DATA_LENGTH: i64 = 33;
    const ER_NOT_ALLOWED: i64 = 34;
    const ER_TRUNCATED_ZIP: i64 = 35;
    const AFL_RDONLY: i64 = 2;
    const AFL_IS_TORRENTZIP: i64 = 4;
    const AFL_WANT_TORRENTZIP: i64 = 8;
    const AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE: i64 = 16;
    const OPSYS_DOS: i64 = 0;
    const OPSYS_AMIGA: i64 = 1;
    const OPSYS_OPENVMS: i64 = 2;
    const OPSYS_UNIX: i64 = 3;
    const OPSYS_VM_CMS: i64 = 4;
    const OPSYS_ATARI_ST: i64 = 5;
    const OPSYS_OS_2: i64 = 6;
    const OPSYS_MACINTOSH: i64 = 7;
    const OPSYS_Z_SYSTEM: i64 = 8;
    const OPSYS_CPM: i64 = 9;
    const OPSYS_WINDOWS_NTFS: i64 = 10;
    const OPSYS_MVS: i64 = 11;
    const OPSYS_VSE: i64 = 12;
    const OPSYS_ACORN_RISC: i64 = 13;
    const OPSYS_VFAT: i64 = 14;
    const OPSYS_ALTERNATE_MVS: i64 = 15;
    const OPSYS_BEOS: i64 = 16;
    const OPSYS_TANDEM: i64 = 17;
    const OPSYS_OS_400: i64 = 18;
    const OPSYS_OS_X: i64 = 19;
    const OPSYS_DEFAULT: i64 = 3;
    const EM_NONE: i64 = 0;
    const EM_TRAD_PKWARE: i64 = 1;
    const EM_AES_128: i64 = 257;
    const EM_AES_192: i64 = 258;
    const EM_AES_256: i64 = 259;
    const EM_UNKNOWN: i64 = 65535;
    const LIBZIP_VERSION: &str = crate::LIBZIP_LIBRARY;
    const LENGTH_TO_END: i64 = 0;
    const LENGTH_UNCHECKED: i64 = -2;

    #[php(property)]
    const lastId: i64;
    #[php(property)]
    const status: i64;
    #[php(property)]
    const statusSys: i64;
    #[php(property)]
    const numFiles: i64;
    #[php(property)]
    const filename: PhpString;
    #[php(property)]
    const comment: PhpString;

    /// `ZipArchive::open()`: `true`, or libzip's error code; an archive
    /// open already closed first, its changes written, and an empty file
    /// taken as a new archive after php's deprecation.
    ///
    /// ```php
    /// $z = new ZipArchive;
    /// var_dump($z->open('open-witness.zip'), $z->open('open-witness.zip', ZipArchive::CREATE), $z->filename === getcwd() . '/open-witness.zip');
    /// var_dump($z->addFromString('a', 'b'), $z->open('open-witness.zip', ZipArchive::EXCL), $z->open('open-witness.zip', ZipArchive::RDONLY | ZipArchive::OVERWRITE));
    /// var_dump($z->open('open-witness.zip', ZipArchive::CHECKCONS), $z->numFiles);
    /// file_put_contents('open-witness.txt', 'not a zip');
    /// touch('open-witness-empty.zip');
    /// var_dump($z->open('open-witness.txt'), $z->open('.'), $z->open('open-witness-empty.zip'), $z->numFiles);
    /// try { $z->open(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// unlink('open-witness.zip'); unlink('open-witness.txt'); unlink('open-witness-empty.zip');
    /// ```
    /// ```expect
    /// int(9)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// int(10)
    /// int(25)
    /// bool(true)
    /// int(1)
    ///
    /// Deprecated: ZipArchive::open(): Using empty file as ZipArchive is deprecated in %s on line 8
    /// int(19)
    /// int(28)
    /// bool(true)
    /// int(0)
    /// ZipArchive::open(): Argument #1 ($filename) must not be empty
    /// ```
    #[php(tentative)]
    fn open(
        req: &mut Rq,
        this: Z,
        filename: Path,
        #[php(default = 0)] flags: i64,
    ) -> R<Union<bool, i64>> {
        let filename = filename.0.as_bytes().to_vec();
        if filename.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        if basedir_refuses(req, &filename) {
            return Ok(Union::A(false));
        }
        let Some(resolved) = expand(req, &filename) else {
            docref(req, Level::Warning, "No such file or directory");
            return Ok(Union::A(false));
        };
        let object = this.object();
        if let Some((mut old, id)) = object
            .native_mut::<Intern>(req)
            .and_then(|i| Some((i.za.take()?, i.id)))
        {
            let (result, thrown) = close_with_hooks(req, object, &mut old);
            discarded(req, id);
            if result.is_err() {
                docref(req, Level::Warning, "Empty string as source");
            }
            if let Some(t) = thrown {
                return Err(t);
            }
            if result.is_err() {
                return Ok(Union::A(false));
            }
        }
        if let Some(i) = object.native_mut::<Intern>(req) {
            i.filename = None;
        }
        let mut flags = flags;
        if flags & 0x18 == 0 && std::fs::metadata(host(&resolved)).is_ok_and(|m| m.len() == 0) {
            docref(
                req,
                Level::Deprecated,
                "Using empty file as ZipArchive is deprecated",
            );
            flags |= 8;
        }
        let opened = match u32::try_from(flags) {
            Ok(flags) => Archive::open(host(&resolved), flags),
            Err(_) => Err(Status::of(Er::Inval, 0)),
        };
        let mut archive = match opened {
            Ok(archive) => archive,
            Err(status) => return Ok(Union::B(i64::from(status.zip))),
        };
        let state = req.state::<crate::State>();
        state.archives += 1;
        let id = state.archives;
        if let Some(f) = archive.data.as_mut().and_then(|d| d.stdio.take()) {
            state.files.insert(id, f);
        }
        if let Some(i) = object.native_mut::<Intern>(req) {
            i.za = Some(Box::new(archive));
            i.filename = Some(resolved);
            i.id = id;
        }
        Ok(Union::A(true))
    }

    /// `ZipArchive::setPassword()`: the password encrypted entries are
    /// read with; an empty one refused. The archive is one the oracle wrote,
    /// `s` under PKWARE with `pw`, since a header written at each close lets `no`
    /// through its check byte about once in a hundred.
    ///
    /// ```php
    /// file_put_contents('password-witness.zip', base64_decode('UEsDBBQACQAAAPgTQ13l6KJcEgAAAAYAAAABAAAAc8euH2s2oYKwPfvmL6NjIhq7OlBLBwjl6KJcEgAAAAYAAABQSwECPwMUAAkAAAD4E0Nd5eiiXBIAAAAGAAAAAQAAAAAAAAAAAAAAtoEAAAAAc1BLBQYAAAAAAQABAC8AAABBAAAAAAA='));
    /// $z = new ZipArchive; $z->open('password-witness.zip');
    /// var_dump($z->getFromName('s'), $z->getStatusString(), $z->setPassword(''), $z->setPassword('no'), $z->getFromName('s'), $z->getStatusString());
    /// var_dump($z->setPassword('pw'), $z->getFromName('s'), $z->statName('s')['encryption_method']);
    /// $z->close(); unlink('password-witness.zip');
    /// try { $z->setPassword('x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(false)
    /// string(20) "No password provided"
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// string(23) "Wrong password provided"
    /// bool(true)
    /// string(6) "secret"
    /// int(1)
    /// Invalid or uninitialized Zip object
    /// ```
    #[php(tentative)]
    fn setPassword(req: &mut Rq, this: Z, #[php(sensitive)] password: PhpString) -> R<bool> {
        let za = archive!(req, this);
        let password = c_str(password.as_bytes());
        if password.is_empty() {
            return Ok(false);
        }
        za.default_password = Some(password.to_vec());
        Ok(true)
    }

    /// `ZipArchive::close()`: the changes written, php's warning and the
    /// error kept where that fails, and the archive closed either way.
    ///
    /// ```php
    /// $z = new ZipArchive;
    /// var_dump($z->open('close-witness.zip', ZipArchive::CREATE), $z->close(), file_exists('close-witness.zip'));
    /// $z->open('close-witness.zip', ZipArchive::CREATE); $z->addEmptyDir('d');
    /// var_dump($z->close(), file_exists('close-witness.zip'), $z->status, $z->filename);
    /// $z->open('close-witness.zip'); $z->deleteName('d/');
    /// var_dump($z->close(), file_exists('close-witness.zip'));
    /// $z->open('no-such-dir/close-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'b');
    /// var_dump($z->close(), $z->status, $z->statusSys, $z->getStatusString());
    /// try { $z->close(); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// int(0)
    /// string(0) ""
    /// bool(true)
    /// bool(false)
    ///
    /// Warning: ZipArchive::close(): Failure to create temporary file: No such file or directory in %s on line 9
    /// bool(false)
    /// int(12)
    /// int(2)
    /// string(59) "Failure to create temporary file: No such file or directory"
    /// Invalid or uninitialized Zip object
    /// ```
    #[php(tentative)]
    fn close(req: &mut Rq, this: Z) -> R<bool> {
        let object = this.object();
        archive!(req, this);
        let Some(mut za) = object.native_mut::<Intern>(req).and_then(|i| i.za.take()) else {
            return Err(invalid(req));
        };
        let (result, thrown) = close_with_hooks(req, object, &mut za);
        discarded(req, object.native::<Intern>(req).map_or(0, |i| i.id));
        let mut err = Status::default();
        if result.is_err() {
            let text = za.error.text(req);
            match &thrown {
                Some(t) => {
                    while_pending(req, t, |req| docref(req, Level::Warning, &text));
                }
                None => docref(req, Level::Warning, &text),
            }
            err = za.error;
        }
        drop(za);
        req.clear_stat_cache();
        let kept = object.native_mut::<Intern>(req).map(|i| {
            i.err = err;
            i.filename = None;
            (i.progress.take(), i.cancel.take())
        });
        for k in kept.into_iter().flat_map(|(p, c)| [p, c]).flatten() {
            k.give_up(req);
        }
        match thrown {
            Some(t) => Err(t),
            None => Ok(result.is_ok()),
        }
    }

    /// `ZipArchive::count()`, `numFiles`: deleted entries counted until
    /// the close.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('count-witness.zip', ZipArchive::CREATE);
    /// $z->addFromString('a', '1'); $z->addFromString('b', '2'); $z->deleteName('a');
    /// var_dump(count($z), $z->count(), $z->numFiles);
    /// $z->close(); $z->open('count-witness.zip'); var_dump(count($z)); $z->close(); unlink('count-witness.zip');
    /// try { count($z); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(2)
    /// int(2)
    /// int(2)
    /// int(1)
    /// Invalid or uninitialized Zip object
    /// ```
    #[php(tentative)]
    fn count(req: &mut Rq, this: Z) -> R<i64> {
        let za = archive!(req, this);
        Ok(c::long(za.num_entries() as u64))
    }

    /// `ZipArchive::getStatusString()`: `zip_error_strerror` of the
    /// archive's error, or of the one its close left.
    ///
    /// ```php
    /// $z = new ZipArchive; var_dump($z->getStatusString());
    /// $z->open('status-witness.zip', ZipArchive::CREATE);
    /// $z->deleteIndex(5); var_dump($z->getStatusString(), $z->status);
    /// $z->locateName('nope'); var_dump($z->getStatusString());
    /// $z->addFromString('a', ''); $z->addFromString('a', '', 0); var_dump($z->getStatusString());
    /// $z->close(); unlink('status-witness.zip');
    /// ```
    /// ```expect
    /// string(8) "No error"
    /// string(16) "Invalid argument"
    /// int(18)
    /// string(12) "No such file"
    /// string(19) "File already exists"
    /// ```
    #[php(tentative)]
    fn getStatusString(req: &mut Rq, this: Z) -> R<Owner<PhpString>> {
        let status = this
            .object()
            .native::<Intern>(req)
            .map_or(Status::default(), |i| {
                i.za.as_ref().map_or(i.err, |za| za.error)
            });
        let text = status.text(req);
        PhpString::new(req, text.as_bytes())
    }

    /// `ZipArchive::clearError()`: the archive's error, or the one its
    /// close left, cleared.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('clear-witness.zip', ZipArchive::CREATE);
    /// $z->deleteIndex(9); var_dump($z->status); $z->clearError(); var_dump($z->status);
    /// $z->close(); $z->clearError(); var_dump($z->status);
    /// ```
    /// ```expect
    /// int(18)
    /// int(0)
    /// int(0)
    /// ```
    fn clearError(req: &mut Rq, this: Z) {
        if let Some(i) = this.object().native_mut::<Intern>(req) {
            match &mut i.za {
                Some(za) => za.error = Status::default(),
                None => i.err = Status::default(),
            }
        }
    }

    /// `ZipArchive::addEmptyDir()`: a directory entry, `/` appended where
    /// it is missing.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('dir-witness.zip', ZipArchive::CREATE);
    /// var_dump($z->addEmptyDir('a'), $z->lastId, $z->addEmptyDir('a/'), $z->lastId, $z->getStatusString(), $z->addEmptyDir(''));
    /// var_dump($z->getNameIndex(0), $z->addEmptyDir('b', ZipArchive::FL_OVERWRITE), $z->addEmptyDir('a', ZipArchive::FL_OVERWRITE), $z->lastId);
    /// $z->getExternalAttributesIndex(0, $o, $a); printf("%d %o\n", $o, $a >> 16);
    /// $z->close(); unlink('dir-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// int(0)
    /// bool(false)
    /// int(-1)
    /// string(19) "File already exists"
    /// bool(false)
    /// string(2) "a/"
    /// bool(true)
    /// bool(true)
    /// int(0)
    /// 3 40777
    /// ```
    #[php(tentative)]
    fn addEmptyDir(
        req: &mut Rq,
        this: Z,
        dirname: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        archive!(req, this);
        if dirname.is_empty() {
            return Ok(false);
        }
        let name = c_str(dirname.as_bytes()).to_vec();
        Ok(added(req, this.object(), |za| {
            za.dir_add(&name, c::u32(flags))
        }))
    }

    /// `ZipArchive::addFromString()`: a string as an entry's data, an
    /// entry of that name replaced under `FL_OVERWRITE`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('string-witness.zip', ZipArchive::CREATE);
    /// var_dump($z->addFromString('a.txt', 'one'), $z->addFromString('a.txt', 'two'), $z->lastId, $z->addFromString('a.txt', 'x', 0), $z->lastId);
    /// var_dump($z->addFromString("n\0ul", 'z'), $z->getNameIndex(1), $z->addFromString('big', str_repeat('ab', 5000)));
    /// $z->close(); $z->open('string-witness.zip');
    /// foreach ([0, 2] as $i) { $s = $z->statIndex($i); echo "$s[name] $s[size] $s[comp_size] $s[comp_method]\n"; }
    /// var_dump($z->getFromName('a.txt'), $z->getFromName('n'));
    /// $z->close(); unlink('string-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// int(0)
    /// bool(false)
    /// int(-1)
    /// bool(true)
    /// string(1) "n"
    /// bool(true)
    /// a.txt 3 3 0
    /// big 10000 28 8
    /// string(3) "two"
    /// string(1) "z"
    /// ```
    #[php(tentative)]
    fn addFromString(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        content: PhpString,
        #[php(default = ZipArchive::FL_OVERWRITE)] flags: i64,
    ) -> R<bool> {
        archive!(req, this);
        let (name, data) = (c_str(name.as_bytes()).to_vec(), content.as_bytes().to_vec());
        let source = Source::buffer(data);
        Ok(added(req, this.object(), |za| {
            za.file_replace(None, Some(&name), source, c::u32(flags))
        }))
    }

    /// `ZipArchive::addFile()`: a file read at the close, or at once under
    /// `FL_OPEN_FILE_NOW`, a window of it where `start` or `length` asks.
    ///
    /// ```php
    /// file_put_contents('addfile-witness.txt', 'abcdefghij');
    /// $z = new ZipArchive; $z->open('addfile-witness.zip', ZipArchive::CREATE);
    /// var_dump($z->addFile('addfile-witness.txt'), $z->addFile('addfile-witness.txt', 'mid', 2, 3), $z->addFile('addfile-witness.txt', 'now', 0, 0, ZipArchive::FL_OPEN_FILE_NOW));
    /// var_dump($z->addFile('addfile-witness.txt', 'bad', 8, 5), $z->getStatusString(), $z->lastId);
    /// var_dump($z->addFile('no-such-file.txt'));
    /// try { $z->addFile(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// unlink('addfile-witness.txt');
    /// var_dump($z->close(), $z->getStatusString(), file_exists('addfile-witness.zip'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// string(16) "Invalid argument"
    /// int(2)
    ///
    /// Warning: ZipArchive::addFile(): No such file or directory in %s on line 6
    /// bool(false)
    /// ZipArchive::addFile(): Argument #1 ($filepath) must not be empty
    ///
    /// Warning: ZipArchive::close(): Can't open file: No such file or directory in %s on line 9
    /// bool(false)
    /// string(42) "Can't open file: No such file or directory"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn addFile(
        req: &mut Rq,
        this: Z,
        filepath: Path,
        #[php(default = "")] entryname: PhpString,
        #[php(default = 0)] start: i64,
        #[php(default = ZipArchive::LENGTH_TO_END)] length: i64,
        #[php(default = ZipArchive::FL_OVERWRITE)] flags: i64,
    ) -> R<bool> {
        let filename = filepath.0.as_bytes().to_vec();
        if filename.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        let entry = if entryname.is_empty() {
            filename.clone()
        } else {
            entryname.as_bytes().to_vec()
        };
        add_file(
            req,
            this.object(),
            &filename,
            &entry,
            start,
            length,
            None,
            flags,
        )
    }

    /// `ZipArchive::replaceFile()`: a file as the data of the entry at
    /// `index`.
    ///
    /// ```php
    /// file_put_contents('replace-witness.txt', 'new data');
    /// $z = new ZipArchive; $z->open('replace-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'old');
    /// var_dump($z->replaceFile('replace-witness.txt', 0, 4), $z->replaceFile('replace-witness.txt', 3), $z->getStatusString(), $z->lastId);
    /// try { $z->replaceFile('replace-witness.txt', -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); $z->open('replace-witness.zip'); var_dump($z->getFromIndex(0));
    /// $z->close(); unlink('replace-witness.zip'); unlink('replace-witness.txt');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// string(16) "Invalid argument"
    /// int(0)
    /// ZipArchive::replaceFile(): Argument #2 ($index) must be greater than or equal to 0
    /// string(4) "data"
    /// ```
    #[php(tentative)]
    fn replaceFile(
        req: &mut Rq,
        this: Z,
        filepath: Path,
        index: i64,
        #[php(default = 0)] start: i64,
        #[php(default = ZipArchive::LENGTH_TO_END)] length: i64,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let filename = filepath.0.as_bytes().to_vec();
        if filename.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        if index < 0 {
            return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
        }
        add_file(
            req,
            this.object(),
            &filename,
            b"",
            start,
            length,
            Some(index),
            flags,
        )
    }

    /// `ZipArchive::addGlob()`: each file a glob matches, named as the
    /// options shape it.
    ///
    /// ```php
    /// @mkdir('glob-witness'); file_put_contents('glob-witness/a.txt', 'a'); file_put_contents('glob-witness/b.txt', 'b'); file_put_contents('glob-witness/c.php', 'c');
    /// $z = new ZipArchive; $z->open('glob-witness.zip', ZipArchive::CREATE);
    /// var_dump($z->addGlob('glob-witness/*.txt', 0, ['remove_path' => 'glob-witness', 'add_path' => 'in/', 'comp_method' => ZipArchive::CM_STORE]));
    /// var_dump($z->addGlob('glob-witness/*.php', 0, ['remove_all_path' => true]), $z->addGlob('glob-witness/*.none'), $z->addGlob('*', -1));
    /// try { $z->addGlob('*', 0, ['add_path' => '']); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { $z->addGlob('*', 0, ['flags' => 'x']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// for ($i = 0; $i < $z->numFiles; $i++) { echo $z->getNameIndex($i), ' ', $z->statIndex($i)['comp_method'], "\n"; }
    /// $z->close(); unlink('glob-witness.zip'); array_map('unlink', glob('glob-witness/*')); rmdir('glob-witness');
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(18) "glob-witness/a.txt"
    ///   [1]=>
    ///   string(18) "glob-witness/b.txt"
    /// }
    ///
    /// Warning: ZipArchive::addGlob(): At least one of the passed flags is invalid or not supported on this platform in %s on line 5
    /// array(1) {
    ///   [0]=>
    ///   string(18) "glob-witness/c.php"
    /// }
    /// array(0) {
    /// }
    /// bool(false)
    /// Option "add_path" must not be empty
    /// Option "flags" must be of type int, string given
    /// in/a.txt 0
    /// in/b.txt 0
    /// c.php 0
    /// ```
    #[php(tentative)]
    fn addGlob(
        req: &mut Rq,
        this: Z,
        pattern: Path,
        #[php(default = 0)] flags: i64,
        #[php(default = [])] options: PhpArray,
    ) -> Arr {
        let kind = crate::pattern::Kind::Glob(flags);
        crate::pattern::add_from_pattern(req, this.object(), pattern.0.as_bytes(), &kind, options)
    }

    /// `ZipArchive::addPattern()`: each regular file of a directory whose
    /// name the expression matches, in name order.
    ///
    /// ```php
    /// @mkdir('pattern-witness'); foreach (['b.txt', 'a.txt', 'c.log'] as $f) file_put_contents("pattern-witness/$f", $f); @mkdir('pattern-witness/d.txt');
    /// $z = new ZipArchive; $z->open('pattern-witness.zip', ZipArchive::CREATE);
    /// var_dump($z->addPattern('/\.txt$/', 'pattern-witness', ['remove_all_path' => true]), $z->addPattern('/x/', 'pattern-witness'));
    /// var_dump(@$z->addPattern('/(/', 'pattern-witness'), $z->addPattern('/x/', 'no-such-dir'));
    /// for ($i = 0; $i < $z->numFiles; $i++) echo $z->getNameIndex($i), "\n";
    /// $z->close(); unlink('pattern-witness.zip'); array_map('unlink', glob('pattern-witness/*.*[gt]')); rmdir('pattern-witness/d.txt'); rmdir('pattern-witness');
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(21) "pattern-witness/a.txt"
    ///   [1]=>
    ///   string(21) "pattern-witness/b.txt"
    /// }
    /// array(2) {
    ///   [0]=>
    ///   string(21) "pattern-witness/a.txt"
    ///   [1]=>
    ///   string(21) "pattern-witness/b.txt"
    /// }
    ///
    /// Warning: ZipArchive::addPattern(no-such-dir): Failed to open directory: No such file or directory in %s on line 5
    /// bool(false)
    /// bool(false)
    /// a.txt
    /// b.txt
    /// pattern-witness/a.txt
    /// pattern-witness/b.txt
    ///
    /// Warning: unlink(pattern-witness/d.txt): Operation not permitted in %s on line 7
    /// ```
    #[php(tentative)]
    fn addPattern(
        req: &mut Rq,
        this: Z,
        pattern: Path,
        #[php(default = ".")] path: PhpString,
        #[php(default = [])] options: PhpArray,
    ) -> Arr {
        let kind = crate::pattern::Kind::Pcre(path.as_bytes().to_vec());
        crate::pattern::add_from_pattern(req, this.object(), pattern.0.as_bytes(), &kind, options)
    }

    /// `ZipArchive::renameIndex()`: a file stays a file and a directory a
    /// directory, and a name another entry holds is refused.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('rename-witness.zip', ZipArchive::CREATE);
    /// $z->addFromString('a', '1'); $z->addFromString('b', '2'); $z->addEmptyDir('d');
    /// var_dump($z->renameIndex(0, 'c'), $z->getNameIndex(0), $z->renameIndex(0, 'b'), $z->getStatusString(), $z->renameIndex(0, 'x/'), $z->renameIndex(2, 'e/'));
    /// var_dump($z->renameIndex(-1, 'z'), $z->renameIndex(9, 'z'));
    /// try { $z->renameIndex(0, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); unlink('rename-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "c"
    /// bool(false)
    /// string(19) "File already exists"
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ZipArchive::renameIndex(): Argument #2 ($new_name) must not be empty
    /// ```
    #[php(tentative)]
    fn renameIndex(req: &mut Rq, this: Z, index: i64, new_name: PhpString) -> R<bool> {
        if index < 0 {
            return Ok(false);
        }
        archive!(req, this);
        if new_name.is_empty() {
            return Err(arg_value_error(req, 2, "not be empty"));
        }
        let za = archive!(req, this);
        Ok(za
            .rename(self::index(index), c_str(new_name.as_bytes()))
            .is_ok())
    }

    /// `ZipArchive::renameName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('renamen-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->renameName('a', 'b'), $z->renameName('a', 'c'), $z->getStatusString(), $z->locateName('b'));
    /// foreach ([['', 'x'], ['b', '']] as [$from, $to]) { try { $z->renameName($from, $to); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
    /// $z->close(); unlink('renamen-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// string(12) "No such file"
    /// int(0)
    /// ZipArchive::renameName(): Argument #1 ($name) must not be empty
    /// ZipArchive::renameName(): Argument #2 ($new_name) must not be empty
    /// ```
    #[php(tentative)]
    fn renameName(req: &mut Rq, this: Z, name: PhpString, new_name: PhpString) -> R<bool> {
        archive!(req, this);
        if new_name.is_empty() {
            return Err(arg_value_error(req, 2, "not be empty"));
        }
        let (za, idx) = named!(req, this, name, false);
        Ok(za.rename(idx, c_str(new_name.as_bytes())).is_ok())
    }

    /// `ZipArchive::setArchiveComment()`: a comment not UTF-8 nor ASCII
    /// refused, as libzip refuses CP437.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('acomment-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setArchiveComment('hello'), $z->comment, $z->setArchiveComment("\xff"), $z->getStatusString(), $z->setArchiveComment("gr\xc3\xbc\xc3\x9f"));
    /// try { $z->setArchiveComment(str_repeat('x', 65536)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); $z->open('acomment-witness.zip'); var_dump($z->getArchiveComment()); $z->close(); unlink('acomment-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(5) "hello"
    /// bool(false)
    /// string(16) "Invalid argument"
    /// bool(true)
    /// ZipArchive::setArchiveComment(): Argument #1 ($comment) must be less than 65535 bytes
    /// string(6) "grüß"
    /// ```
    #[php(tentative)]
    fn setArchiveComment(req: &mut Rq, this: Z, comment: PhpString) -> R<bool> {
        archive!(req, this);
        if comment.len() > 0xffff {
            return Err(arg_value_error(req, 1, "be less than 65535 bytes"));
        }
        let za = archive!(req, this);
        Ok(za.set_archive_comment(comment.as_bytes()).is_ok())
    }

    /// `ZipArchive::getArchiveComment()`: the changed comment, or under
    /// `FL_UNCHANGED` the one the file has.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('gcomment-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->setArchiveComment('one'); $z->close();
    /// $z->open('gcomment-witness.zip'); $z->setArchiveComment('two');
    /// var_dump($z->getArchiveComment(), $z->getArchiveComment(ZipArchive::FL_UNCHANGED), $z->unchangeArchive(), $z->getArchiveComment());
    /// $z->close(); unlink('gcomment-witness.zip');
    /// ```
    /// ```expect
    /// string(3) "two"
    /// string(3) "one"
    /// bool(true)
    /// string(3) "one"
    /// ```
    #[php(tentative)]
    fn getArchiveComment(req: &mut Rq, this: Z, #[php(default = 0)] flags: i64) -> Str {
        let comment = archive!(req, this).archive_comment(c::u32(flags));
        Ok(OrFalse::Value(PhpString::new(req, &comment)?))
    }

    /// `ZipArchive::setArchiveFlag()`: `AFL_IS_TORRENTZIP` and every bit of
    /// an archive opened read-only refused.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('aflag-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setArchiveFlag(ZipArchive::AFL_IS_TORRENTZIP, 1), $z->setArchiveFlag(ZipArchive::AFL_RDONLY, 1), $z->getStatusString());
    /// var_dump($z->setArchiveFlag(ZipArchive::AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE, 1), $z->getArchiveFlag(ZipArchive::AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE));
    /// $z->close(); unlink('aflag-witness.zip');
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(22) "Entry has been changed"
    /// bool(true)
    /// int(1)
    /// ```
    fn setArchiveFlag(req: &mut Rq, this: Z, flag: i64, value: i64) -> R<bool> {
        let za = archive!(req, this);
        Ok(za
            .set_archive_flag(c::u32(flag), c::i32(value) != 0)
            .is_ok())
    }

    /// `ZipArchive::getArchiveFlag()`: `1` or `0`, the flags as opened under
    /// `FL_UNCHANGED`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('gflag-witness.zip', ZipArchive::CREATE);
    /// $z->setArchiveFlag(ZipArchive::AFL_WANT_TORRENTZIP, 1);
    /// var_dump($z->getArchiveFlag(ZipArchive::AFL_WANT_TORRENTZIP), $z->getArchiveFlag(ZipArchive::AFL_WANT_TORRENTZIP, ZipArchive::FL_UNCHANGED), $z->getArchiveFlag(ZipArchive::AFL_RDONLY));
    /// $z->close();
    /// ```
    /// ```expect
    /// int(1)
    /// int(0)
    /// int(0)
    /// ```
    fn getArchiveFlag(req: &mut Rq, this: Z, flag: i64, #[php(default = 0)] flags: i64) -> R<i64> {
        let za = archive!(req, this);
        Ok(i64::from(za.archive_flag(c::u32(flag), c::u32(flags))))
    }

    /// `ZipArchive::setCommentIndex()`: an empty comment removes the one
    /// the entry has.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('ccomment-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setCommentIndex(0, 'note'), $z->getCommentIndex(0), $z->setCommentIndex(0, ''), $z->getCommentIndex(0), $z->setCommentIndex(4, 'x'));
    /// try { $z->setCommentIndex(0, str_repeat('x', 65536)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->setCommentIndex(0, 'kept'); $z->close(); $z->open('ccomment-witness.zip'); var_dump($z->getCommentIndex(0)); $z->close(); unlink('ccomment-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(4) "note"
    /// bool(true)
    /// string(0) ""
    /// bool(false)
    /// ZipArchive::setCommentIndex(): Argument #2 ($comment) must be less than 65535 bytes
    /// string(4) "kept"
    /// ```
    #[php(tentative)]
    fn setCommentIndex(req: &mut Rq, this: Z, index: i64, comment: PhpString) -> R<bool> {
        archive!(req, this);
        if comment.len() > 0xffff {
            return Err(arg_value_error(req, 2, "be less than 65535 bytes"));
        }
        let (za, idx) = indexed!(req, this, index, false);
        Ok(set_file_comment(za, idx, comment.as_bytes()))
    }

    /// `ZipArchive::setCommentName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('ncomment-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setCommentName('a', 'note'), $z->getCommentName('a'), $z->setCommentName('b', 'x'));
    /// try { $z->setCommentName('', 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); unlink('ncomment-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(4) "note"
    /// bool(false)
    /// ZipArchive::setCommentName(): Argument #1 ($name) must not be empty
    /// ```
    #[php(tentative)]
    fn setCommentName(req: &mut Rq, this: Z, name: PhpString, comment: PhpString) -> R<bool> {
        if name.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        archive!(req, this);
        if comment.len() > 0xffff {
            return Err(arg_value_error(req, 2, "be less than 65535 bytes"));
        }
        let (za, idx) = named!(req, this, name, false);
        Ok(set_file_comment(za, idx, comment.as_bytes()))
    }

    /// `ZipArchive::setMtimeIndex()`: the time the close writes, as an
    /// MS-DOS time in two-second steps.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('mtime-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setMtimeIndex(0, 1500000001), $z->statIndex(0)['mtime'], $z->setMtimeIndex(3, 0));
    /// $z->close(); $z->open('mtime-witness.zip'); var_dump($z->statIndex(0)['mtime']); $z->close(); unlink('mtime-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1500000001)
    /// bool(false)
    /// int(1500000000)
    /// ```
    #[php(tentative)]
    fn setMtimeIndex(
        req: &mut Rq,
        this: Z,
        index: i64,
        timestamp: i64,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let _ = flags;
        Ok(archive!(req, this)
            .set_mtime(self::index(index), timestamp)
            .is_ok())
    }

    /// `ZipArchive::setMtimeName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('mtimen-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setMtimeName('a', 1234567890), $z->statName('a')['mtime'], $z->setMtimeName('b', 1));
    /// try { $z->setMtimeName('', 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); unlink('mtimen-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// int(1234567890)
    /// bool(false)
    /// ZipArchive::setMtimeName(): Argument #1 ($name) must not be empty
    /// ```
    #[php(tentative)]
    fn setMtimeName(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        timestamp: i64,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let _ = flags;
        let (za, idx) = named!(req, this, name, false);
        Ok(za.set_mtime(idx, timestamp).is_ok())
    }

    /// `ZipArchive::getCommentIndex()`: the comment as UTF-8, or raw under
    /// `FL_ENC_RAW`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('gccomment-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->setCommentIndex(0, "\x80\x81"); $z->close();
    /// $z->open('gccomment-witness.zip');
    /// var_dump(bin2hex($z->getCommentIndex(0)), bin2hex($z->getCommentIndex(0, ZipArchive::FL_ENC_RAW)), $z->getCommentIndex(1));
    /// $z->close(); unlink('gccomment-witness.zip');
    /// ```
    /// ```expect
    /// string(8) "c387c3bc"
    /// string(4) "8081"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getCommentIndex(req: &mut Rq, this: Z, index: i64, #[php(default = 0)] flags: i64) -> Str {
        let (za, idx) = indexed!(req, this, index, OrFalse::False);
        let comment = za.file_comment(idx, c::u32(flags)).unwrap_or_default();
        Ok(OrFalse::Value(PhpString::new(req, &comment)?))
    }

    /// `ZipArchive::getCommentName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('gncomment-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->getCommentName('a'), $z->setCommentName('a', 'c'), $z->getCommentName('a'), $z->getCommentName('b'));
    /// try { $z->getCommentName(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); unlink('gncomment-witness.zip');
    /// ```
    /// ```expect
    /// string(0) ""
    /// bool(true)
    /// string(1) "c"
    /// bool(false)
    /// ZipArchive::getCommentName(): Argument #1 ($name) must not be empty
    /// ```
    #[php(tentative)]
    fn getCommentName(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> Str {
        let (za, idx) = named!(req, this, name, OrFalse::False);
        let comment = za.file_comment(idx, c::u32(flags)).unwrap_or_default();
        Ok(OrFalse::Value(PhpString::new(req, &comment)?))
    }

    /// `ZipArchive::deleteIndex()`: the entry gone at the close, its index
    /// kept until then.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('delete-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->addFromString('b', '2'); $z->close();
    /// $z->open('delete-witness.zip');
    /// var_dump($z->deleteIndex(0), $z->deleteIndex(0), $z->getStatusString(), $z->deleteIndex(-1), $z->deleteIndex(7), $z->statIndex(0), $z->getStatusString());
    /// $z->close(); $z->open('delete-witness.zip'); var_dump($z->numFiles, $z->getNameIndex(0)); $z->close(); unlink('delete-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// string(22) "Entry has been deleted"
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// string(22) "Entry has been deleted"
    /// int(1)
    /// string(1) "b"
    /// ```
    #[php(tentative)]
    fn deleteIndex(req: &mut Rq, this: Z, index: i64) -> R<bool> {
        let za = archive!(req, this);
        Ok(index >= 0 && za.delete(self::index(index)).is_ok())
    }

    /// `ZipArchive::deleteName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('deleten-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->deleteName('a'), $z->deleteName('a'), $z->getStatusString(), $z->deleteName(''), $z->numFiles);
    /// $z->close(); var_dump(file_exists('deleten-witness.zip'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// string(12) "No such file"
    /// bool(false)
    /// int(1)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn deleteName(req: &mut Rq, this: Z, name: PhpString) -> R<bool> {
        let za = archive!(req, this);
        if name.is_empty() {
            return Ok(false);
        }
        let Ok(idx) = za.name_locate(c_str(name.as_bytes()), 0) else {
            return Ok(false);
        };
        Ok(za.stat_index(idx, 0).is_ok() && za.delete(idx).is_ok())
    }

    /// `ZipArchive::statName()`: the entry's `zip_stat`, its name matched
    /// as the flags ask.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('stat-witness.zip', ZipArchive::CREATE); $z->addFromString('dir/File.txt', str_repeat('z', 100)); $z->setMtimeName('dir/File.txt', 1600000000); $z->close();
    /// $z->open('stat-witness.zip');
    /// var_dump($z->statName('dir/File.txt'), $z->statName('file.txt', ZipArchive::FL_NOCASE | ZipArchive::FL_NODIR)['index'], $z->statName('nope'));
    /// try { $z->statName(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); unlink('stat-witness.zip');
    /// ```
    /// ```expect
    /// array(8) {
    ///   ["name"]=>
    ///   string(12) "dir/File.txt"
    ///   ["index"]=>
    ///   int(0)
    ///   ["crc"]=>
    ///   int(2913972901)
    ///   ["size"]=>
    ///   int(100)
    ///   ["mtime"]=>
    ///   int(1600000000)
    ///   ["comp_size"]=>
    ///   int(6)
    ///   ["comp_method"]=>
    ///   int(8)
    ///   ["encryption_method"]=>
    ///   int(0)
    /// }
    /// int(0)
    /// bool(false)
    /// ZipArchive::statName(): Argument #1 ($name) must not be empty
    /// ```
    #[php(tentative)]
    fn statName(req: &mut Rq, this: Z, name: Path, #[php(default = 0)] flags: i64) -> Arr {
        archive!(req, this);
        let name = name.0.as_bytes().to_vec();
        if name.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        let (za, flags) = (archive!(req, this), c::u32(flags));
        match za
            .name_locate(&name, flags)
            .and_then(|idx| za.stat_index(idx, flags))
        {
            Ok(st) => Ok(OrFalse::Value(stat_array(req, &st)?)),
            Err(_) => Ok(OrFalse::False),
        }
    }

    /// `ZipArchive::statIndex()`: an added entry's size and time, its
    /// CRC and compressed size known once written.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('stati-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'hello');
    /// $s = $z->statIndex(0); var_dump($s['crc'], $s['size'], $s['comp_size'], $s['comp_method'], $s['mtime'] > 0, $z->statIndex(1));
    /// $z->close(); $z->open('stati-witness.zip'); $s = $z->statIndex(0); var_dump($s['crc'], $s['comp_size']); $z->close(); unlink('stati-witness.zip');
    /// ```
    /// ```expect
    /// int(0)
    /// int(5)
    /// int(5)
    /// int(0)
    /// bool(true)
    /// bool(false)
    /// int(907060870)
    /// int(5)
    /// ```
    #[php(tentative)]
    fn statIndex(req: &mut Rq, this: Z, index: i64, #[php(default = 0)] flags: i64) -> Arr {
        match archive!(req, this).stat_index(self::index(index), c::u32(flags)) {
            Ok(st) => Ok(OrFalse::Value(stat_array(req, &st)?)),
            Err(_) => Ok(OrFalse::False),
        }
    }

    /// `ZipArchive::locateName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('locate-witness.zip', ZipArchive::CREATE); $z->addFromString('Dir/A.txt', '1'); $z->addFromString('b', '2');
    /// var_dump($z->locateName('b'), $z->locateName('a.txt', ZipArchive::FL_NOCASE | ZipArchive::FL_NODIR), $z->locateName('A.txt'), $z->getStatusString(), $z->locateName(''));
    /// $z->close(); unlink('locate-witness.zip');
    /// ```
    /// ```expect
    /// int(1)
    /// int(0)
    /// bool(false)
    /// string(12) "No such file"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn locateName(
        req: &mut Rq,
        this: Z,
        name: Path,
        #[php(default = 0)] flags: i64,
    ) -> R<OrFalse<i64>> {
        let za = archive!(req, this);
        let name = name.0.as_bytes();
        Ok(match za.name_locate(name, c::u32(flags)) {
            Ok(idx) if !name.is_empty() => OrFalse::Value(c::long(idx as u64)),
            _ => OrFalse::False,
        })
    }

    /// `ZipArchive::getNameIndex()`: the name, a CP437 one as UTF-8 unless
    /// `FL_ENC_RAW` asks for its bytes.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('name-witness.zip', ZipArchive::CREATE); $z->addFromString("caf\x82", '1'); $z->close();
    /// $z->open('name-witness.zip');
    /// var_dump($z->getNameIndex(0), bin2hex($z->getNameIndex(0, ZipArchive::FL_ENC_RAW)), $z->getNameIndex(1), $z->getNameIndex(-1));
    /// $z->close(); unlink('name-witness.zip');
    /// ```
    /// ```expect
    /// string(5) "café"
    /// string(8) "63616682"
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getNameIndex(req: &mut Rq, this: Z, index: i64, #[php(default = 0)] flags: i64) -> Str {
        let za = archive!(req, this);
        match za.get_name(self::index(i64::from(c::i32(index))), c::u32(flags)) {
            Ok(name) => Ok(OrFalse::Value(PhpString::new(req, c_str(&name))?)),
            Err(_) => Ok(OrFalse::False),
        }
    }

    /// `ZipArchive::unchangeArchive()`: the comment and the archive flags
    /// as the archive was opened.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('unarchive-witness.zip', ZipArchive::CREATE);
    /// $z->setArchiveComment('c'); $z->setArchiveFlag(ZipArchive::AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE, 1);
    /// var_dump($z->unchangeArchive(), $z->comment, $z->getArchiveFlag(ZipArchive::AFL_CREATE_OR_KEEP_FILE_FOR_EMPTY_ARCHIVE));
    /// $z->close(); var_dump(file_exists('unarchive-witness.zip'));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(0) ""
    /// int(0)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn unchangeArchive(req: &mut Rq, this: Z) -> R<bool> {
        archive!(req, this).unchange_archive();
        Ok(true)
    }

    /// `ZipArchive::unchangeAll()`: every entry as the file has it, added
    /// ones left as unnamed slots.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('unall-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->close();
    /// $z->open('unall-witness.zip'); $z->renameIndex(0, 'b'); $z->addFromString('c', '2'); $z->setArchiveComment('x');
    /// var_dump($z->unchangeAll(), $z->getNameIndex(0), $z->numFiles, $z->getNameIndex(1), $z->comment);
    /// $z->close(); unlink('unall-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "a"
    /// int(2)
    /// bool(false)
    /// string(0) ""
    /// ```
    #[php(tentative)]
    fn unchangeAll(req: &mut Rq, this: Z) -> R<bool> {
        Ok(archive!(req, this).unchange_all().is_ok())
    }

    /// `ZipArchive::unchangeIndex()`: a renamed entry back to its name,
    /// unless another holds it now.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('unindex-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->addFromString('b', '2'); $z->close();
    /// $z->open('unindex-witness.zip'); $z->renameIndex(0, 'x'); $z->deleteIndex(1);
    /// var_dump($z->unchangeIndex(1), $z->getNameIndex(1), $z->renameIndex(1, 'a'), $z->unchangeIndex(0), $z->getStatusString(), $z->unchangeIndex(-1));
    /// $z->close(); unlink('unindex-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "b"
    /// bool(true)
    /// bool(false)
    /// string(19) "File already exists"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn unchangeIndex(req: &mut Rq, this: Z, index: i64) -> R<bool> {
        let za = archive!(req, this);
        Ok(index >= 0 && za.unchange_one(self::index(index), false).is_ok())
    }

    /// `ZipArchive::unchangeName()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('unname-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->close();
    /// $z->open('unname-witness.zip'); $z->setCommentName('a', 'c');
    /// var_dump($z->unchangeName('a'), $z->getCommentName('a'), $z->unchangeName('b'), $z->unchangeName(''));
    /// $z->close(); unlink('unname-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// string(0) ""
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn unchangeName(req: &mut Rq, this: Z, name: PhpString) -> R<bool> {
        let za = archive!(req, this);
        if name.is_empty() {
            return Ok(false);
        }
        let Ok(idx) = za.name_locate(c_str(name.as_bytes()), 0) else {
            return Ok(false);
        };
        Ok(za.stat_index(idx, 0).is_ok() && za.unchange_one(idx, false).is_ok())
    }

    /// `ZipArchive::extractTo()`: every entry, one, or a list of them,
    /// under the destination, each name made relative.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('extract-witness.zip', ZipArchive::CREATE);
    /// $z->addFromString('../up.txt', 'u'); $z->addFromString('d/e/f.txt', 'f'); $z->addEmptyDir('g'); $z->close();
    /// $z->open('extract-witness.zip');
    /// var_dump($z->extractTo('extract-witness'), file_get_contents('extract-witness/up.txt'), file_get_contents('extract-witness/d/e/f.txt'), is_dir('extract-witness/g'));
    /// var_dump($z->extractTo('extract-witness/one', 'd/e/f.txt'), $z->extractTo('extract-witness/two', ['nope']), $z->extractTo('extract-witness/two', []), $z->extractTo(''));
    /// $z->close(); unlink('extract-witness.zip');
    /// foreach (['up.txt', 'd/e/f.txt', 'one/d/e/f.txt'] as $f) unlink("extract-witness/$f");
    /// foreach (['d/e', 'd', 'g', 'one/d/e', 'one/d', 'one', 'two', ''] as $d) rmdir("extract-witness/$d");
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "u"
    /// string(1) "f"
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn extractTo(
        req: &mut Rq,
        this: Z,
        pathto: Path,
        #[php(default = null)] files: Option<Union<PhpArray, PhpString>>,
    ) -> R<bool> {
        archive!(req, this);
        crate::extract::extract_to(req, this.object(), pathto.0.as_bytes(), files)
    }

    /// `ZipArchive::getFromName()`: the first `len` bytes, all of them
    /// under `0`, `""` for an empty entry.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('fromname-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'abcdef'); $z->addFromString('e', ''); $z->close();
    /// $z->open('fromname-witness.zip');
    /// var_dump($z->getFromName('a'), $z->getFromName('a', 2), $z->getFromName('e'), $z->getFromName('A', 0, ZipArchive::FL_NOCASE), $z->getFromName('b'));
    /// $z->close(); unlink('fromname-witness.zip');
    /// ```
    /// ```expect
    /// string(6) "abcdef"
    /// string(2) "ab"
    /// string(0) ""
    /// string(6) "abcdef"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getFromName(
        req: &mut Rq,
        this: Z,
        name: Path,
        #[php(default = 0)] len: i64,
        #[php(default = 0)] flags: i64,
    ) -> Str {
        let name = name.0.as_bytes().to_vec();
        get_from(req, this, &Union::B(name), len, flags)
    }

    /// `ZipArchive::getFromIndex()`: an entry's data, compressed under
    /// `FL_COMPRESSED`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('fromindex-witness.zip', ZipArchive::CREATE); $z->addFromString('a', str_repeat('xy', 50)); $z->close();
    /// $z->open('fromindex-witness.zip');
    /// var_dump(strlen($z->getFromIndex(0)), bin2hex($z->getFromIndex(0, 0, ZipArchive::FL_COMPRESSED)), $z->getFromIndex(1));
    /// $z->close(); unlink('fromindex-witness.zip');
    /// ```
    /// ```expect
    /// int(100)
    /// string(14) "aba8aca0390400"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getFromIndex(
        req: &mut Rq,
        this: Z,
        index: i64,
        #[php(default = 0)] len: i64,
        #[php(default = 0)] flags: i64,
    ) -> Str {
        get_from(req, this, &Union::A(index), len, flags)
    }

    /// `ZipArchive::getStreamIndex()`: a read stream over the entry,
    /// seekable where it is stored.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('streamindex-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'hello world'); $z->setCompressionIndex(0, ZipArchive::CM_STORE); $z->close();
    /// $z->open('streamindex-witness.zip'); $s = $z->getStreamIndex(0);
    /// var_dump(fseek($s, 6), fread($s, 5), ftell($s), feof($s), fwrite($s, 'x'), $z->getStreamIndex(3));
    /// fclose($s); $z->close(); unlink('streamindex-witness.zip');
    /// ```
    /// ```expect
    /// int(0)
    /// string(5) "world"
    /// int(11)
    /// bool(true)
    /// int(1)
    /// bool(false)
    /// ```
    fn getStreamIndex(req: &mut Rq, this: Z, index: i64, #[php(default = 0)] flags: i64) -> Res {
        get_stream(req, this, &Union::A(index), flags)
    }

    /// `ZipArchive::getStreamName()`: an entry's stream, reading php's
    /// error once the archive closes under it.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('streamname-witness.zip', ZipArchive::CREATE); $z->addFromString('a', str_repeat('q', 99)); $z->close();
    /// $z->open('streamname-witness.zip'); $s = $z->getStreamName('a');
    /// var_dump(stream_get_meta_data($s)['seekable'], strlen(stream_get_contents($s)));
    /// $t = $z->getStreamName('a'); $z->close(); var_dump(stream_get_contents($t)); unlink('streamname-witness.zip');
    /// ```
    /// ```expect
    /// bool(false)
    /// int(99)
    ///
    /// Warning: stream_get_contents(): Zip stream error: Containing zip archive was closed in %s on line 5
    /// string(0) ""
    /// ```
    fn getStreamName(req: &mut Rq, this: Z, name: Path, #[php(default = 0)] flags: i64) -> Res {
        get_stream(req, this, &Union::B(name.0.as_bytes().to_vec()), flags)
    }

    /// `ZipArchive::getStream()`: `getStreamName()` without flags.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('stream-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'x'); $z->close();
    /// $z->open('stream-witness.zip'); var_dump(fread($z->getStream('a'), 9), $z->getStream('b'));
    /// try { $z->getStream(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close(); unlink('stream-witness.zip');
    /// ```
    /// ```expect
    /// string(1) "x"
    /// bool(false)
    /// ZipArchive::getStream(): Argument #1 ($name) must not be empty
    /// ```
    fn getStream(req: &mut Rq, this: Z, name: Path) -> Res {
        get_stream(req, this, &Union::B(name.0.as_bytes().to_vec()), 0)
    }

    /// `ZipArchive::setExternalAttributesName()`: the system and the
    /// attributes the central record carries.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('setattr-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setExternalAttributesName('a', ZipArchive::OPSYS_DOS, 0x20), $z->setExternalAttributesName('b', 0, 0));
    /// $z->close(); $z->open('setattr-witness.zip'); var_dump($z->getExternalAttributesName('a', $o, $a), $o, $a); $z->close(); unlink('setattr-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// int(0)
    /// int(32)
    /// ```
    #[php(tentative)]
    fn setExternalAttributesName(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        opsys: i64,
        attr: i64,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let _ = flags;
        let (za, idx) = named!(req, this, name, false);
        Ok(za
            .set_external_attributes(idx, c::u8(opsys), c::u32(attr))
            .is_ok())
    }

    /// `ZipArchive::setExternalAttributesIndex()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('setattri-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->setExternalAttributesIndex(0, ZipArchive::OPSYS_UNIX, 0100600 << 16), $z->setExternalAttributesIndex(2, 3, 0));
    /// $z->getExternalAttributesIndex(0, $o, $a); printf("%d %o\n", $o, $a >> 16); $z->close(); unlink('setattri-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// 3 100600
    /// ```
    #[php(tentative)]
    fn setExternalAttributesIndex(
        req: &mut Rq,
        this: Z,
        index: i64,
        opsys: i64,
        attr: i64,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let _ = flags;
        let (za, idx) = indexed!(req, this, index, false);
        Ok(za
            .set_external_attributes(idx, c::u8(opsys), c::u32(attr))
            .is_ok())
    }

    /// `ZipArchive::getExternalAttributesName()`: a new entry's are libzip's
    /// default, a Unix `0666` file.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('getattr-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->getExternalAttributesName('a', $o, $a), $o, decoct($a >> 16), $z->getExternalAttributesName('b', $o, $a));
    /// try { $z->getExternalAttributesName('', $o, $a); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $z->close();
    /// ```
    /// ```expect
    /// bool(true)
    /// int(3)
    /// string(6) "100666"
    /// bool(false)
    /// ZipArchive::getExternalAttributesName(): Argument #1 ($name) must not be empty
    /// ```
    #[php(tentative)]
    fn getExternalAttributesName(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        opsys: Ref<Untyped<Value>>,
        attr: Ref<Untyped<Value>>,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let (za, idx) = named!(req, this, name, false);
        let found = za.external_attributes(idx, c::u32(flags));
        attributes(req, found, &opsys, &attr)
    }

    /// `ZipArchive::getExternalAttributesIndex()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('getattri-witness.zip', ZipArchive::CREATE); $z->addEmptyDir('d');
    /// var_dump($z->getExternalAttributesIndex(0, $o, $a), $o, decoct($a >> 16), $z->getExternalAttributesIndex(1, $o, $a));
    /// $z->close();
    /// ```
    /// ```expect
    /// bool(true)
    /// int(3)
    /// string(5) "40777"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn getExternalAttributesIndex(
        req: &mut Rq,
        this: Z,
        index: i64,
        opsys: Ref<Untyped<Value>>,
        attr: Ref<Untyped<Value>>,
        #[php(default = 0)] flags: i64,
    ) -> R<bool> {
        let (za, idx) = indexed!(req, this, index, false);
        let found = za.external_attributes(idx, c::u32(flags));
        attributes(req, found, &opsys, &attr)
    }

    /// `ZipArchive::setCompressionName()`: the method the close writes the
    /// entry in; one this build cannot write refused.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('compname-witness.zip', ZipArchive::CREATE);
    /// foreach (['s' => ZipArchive::CM_STORE, 'd' => ZipArchive::CM_DEFLATE, 'b' => ZipArchive::CM_BZIP2, 'x' => ZipArchive::CM_XZ, 'l' => ZipArchive::CM_LZMA, 'z' => ZipArchive::CM_ZSTD] as $n => $m) { $z->addFromString($n, str_repeat("$n ", 200)); var_dump($z->setCompressionName($n, $m)); }
    /// var_dump($z->setCompressionName('s', ZipArchive::CM_PPMD), $z->getStatusString(), $z->setCompressionName('q', 0));
    /// $z->close(); $z->open('compname-witness.zip');
    /// for ($i = 0; $i < $z->numFiles; $i++) { $s = $z->statIndex($i); echo $s['name'], ' ', $s['comp_method'], ' ', $z->getFromIndex($i) === str_repeat("$s[name] ", 200) ? 'ok' : 'bad', "\n"; }
    /// $z->close(); unlink('compname-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// string(32) "Compression method not supported"
    /// bool(false)
    /// s 0 ok
    /// d 8 ok
    /// b 12 ok
    /// x 95 ok
    /// l 14 ok
    /// z 93 ok
    /// ```
    #[php(tentative)]
    fn setCompressionName(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        method: i64,
        #[php(default = 0)] compflags: i64,
    ) -> R<bool> {
        let (za, idx) = named!(req, this, name, false);
        Ok(za
            .set_compression(idx, c::i32(method), c::u32(compflags))
            .is_ok())
    }

    /// `ZipArchive::setCompressionIndex()`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('compindex-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'aaaaaaaaaa');
    /// var_dump($z->setCompressionIndex(0, ZipArchive::CM_DEFLATE, 1), $z->setCompressionIndex(5, 0), $z->getStatusString());
    /// $z->close(); $z->open('compindex-witness.zip'); var_dump($z->statIndex(0)['comp_method']); $z->close(); unlink('compindex-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// string(16) "Invalid argument"
    /// int(8)
    /// ```
    #[php(tentative)]
    fn setCompressionIndex(
        req: &mut Rq,
        this: Z,
        index: i64,
        method: i64,
        #[php(default = 0)] compflags: i64,
    ) -> R<bool> {
        let za = archive!(req, this);
        Ok(za
            .set_compression(self::index(index), c::i32(method), c::u32(compflags))
            .is_ok())
    }

    /// `ZipArchive::setEncryptionName()`: traditional PKWARE or `WinZip` AES,
    /// with the entry's password or the archive's.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('encname-witness.zip', ZipArchive::CREATE);
    /// foreach (['t' => ZipArchive::EM_TRAD_PKWARE, 'a' => ZipArchive::EM_AES_128, 'b' => ZipArchive::EM_AES_192, 'c' => ZipArchive::EM_AES_256] as $n => $m) { $z->addFromString($n, "data $n"); var_dump($z->setEncryptionName($n, $m, 'pw')); }
    /// var_dump($z->setEncryptionName('t', 99, 'pw'), $z->getStatusString(), $z->setEncryptionName('q', 1));
    /// $z->close(); $z->open('encname-witness.zip');
    /// for ($i = 0; $i < 4; $i++) echo $z->statIndex($i)['encryption_method'], ' ', var_export($z->getFromIndex($i), true), "\n";
    /// echo $z->getStatusString(), "\n"; $z->setPassword('pw');
    /// for ($i = 0; $i < 4; $i++) echo $z->getFromIndex($i), "\n";
    /// $z->close(); unlink('encname-witness.zip');
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// string(31) "Encryption method not supported"
    /// bool(false)
    /// 0 'data t'
    /// 257 false
    /// 258 false
    /// 259 false
    /// No password provided
    /// data t
    /// data a
    /// data b
    /// data c
    /// ```
    #[php(tentative)]
    fn setEncryptionName(
        req: &mut Rq,
        this: Z,
        name: PhpString,
        method: i64,
        #[php(sensitive, default = null)] password: Option<PhpString>,
    ) -> R<bool> {
        let (_, idx) = named!(req, this, name, false);
        Ok(set_encryption(req, this, idx, method, password))
    }

    /// `ZipArchive::setEncryptionIndex()`: `EM_NONE` writes the entry in
    /// the clear.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('encindex-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'clear'); $z->setPassword('pw');
    /// var_dump($z->setEncryptionIndex(0, ZipArchive::EM_AES_256), $z->setEncryptionIndex(0, ZipArchive::EM_NONE), $z->setEncryptionIndex(4, 0));
    /// $z->close(); $z->open('encindex-witness.zip'); var_dump($z->statIndex(0)['encryption_method'], $z->getFromIndex(0)); $z->close(); unlink('encindex-witness.zip');
    /// ```
    /// ```expect
    ///
    /// Warning: ZipArchive::setEncryptionIndex(): password reset failed in %s on line 3
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// string(5) "clear"
    /// ```
    #[php(tentative)]
    fn setEncryptionIndex(
        req: &mut Rq,
        this: Z,
        index: i64,
        method: i64,
        #[php(sensitive, default = null)] password: Option<PhpString>,
    ) -> R<bool> {
        archive!(req, this);
        Ok(set_encryption(
            req,
            this,
            self::index(index),
            method,
            password,
        ))
    }

    /// `ZipArchive::registerProgressCallback()`: called with how far the
    /// close is, from `0.0` to `1.0`.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('progress-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->addFromString('b', '2');
    /// var_dump($z->registerProgressCallback(0.4, function ($r) { echo "at $r\n"; }));
    /// var_dump($z->close()); unlink('progress-witness.zip');
    /// try { $z->registerProgressCallback(0.1, 'strlen'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// at 0
    /// at 0.5
    /// at 1
    /// bool(true)
    /// Invalid or uninitialized Zip object
    /// ```
    ///
    /// The callables are kept as `zend_fcc_dup` keeps them, so a
    /// `Class::method` string resolved with `$this` in scope keeps the
    /// object it names until the close gives the callables up:
    ///
    /// ```php
    /// class A { function __destruct(){ echo "dtor\n"; } function p($r){ echo "at $r\n"; } function c(){ echo "asked\n"; return 0; }
    ///   function reg($z){ $z->registerProgressCallback(0.5, 'A::p'); $z->registerCancelCallback('A::c'); } }
    /// $z = new ZipArchive; $z->open('kept-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// (new A)->reg($z);
    /// echo "registered\n";
    /// var_dump($z->close()); unlink('kept-witness.zip');
    /// ```
    /// ```expect
    /// registered
    /// at 0
    /// asked
    /// asked
    /// at 1
    /// asked
    /// dtor
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn registerProgressCallback(req: &mut Rq, this: Z, rate: f64, callback: Callable) -> R<bool> {
        archive!(req, this);
        let kept = Box::new(Owner::take(req, callback));
        let old = match this.object().native_mut::<Intern>(req) {
            Some(i) => {
                if let Some(za) = &mut i.za {
                    za.progress = Some(rate);
                }
                i.progress.replace(kept)
            }
            None => Some(kept),
        };
        if let Some(old) = old {
            old.give_up(req);
        }
        Ok(true)
    }

    /// `ZipArchive::registerCancelCallback()`: a non-zero answer cancels
    /// the close, which writes nothing.
    ///
    /// ```php
    /// $z = new ZipArchive; $z->open('cancel-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1');
    /// var_dump($z->registerCancelCallback(function () { static $n = 0; echo "asked\n"; return $n++ ? 1 : 0; }));
    /// var_dump($z->close(), $z->getStatusString(), file_exists('cancel-witness.zip'));
    /// ```
    /// ```expect
    /// bool(true)
    /// asked
    /// asked
    ///
    /// Warning: ZipArchive::close(): Operation cancelled in %s on line 4
    /// bool(false)
    /// string(19) "Operation cancelled"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn registerCancelCallback(req: &mut Rq, this: Z, callback: Callable) -> R<bool> {
        archive!(req, this);
        let kept = Box::new(Owner::take(req, callback));
        let old = match this.object().native_mut::<Intern>(req) {
            Some(i) => {
                if let Some(za) = &mut i.za {
                    za.cancel = true;
                }
                i.cancel.replace(kept)
            }
            None => Some(kept),
        };
        if let Some(old) = old {
            old.give_up(req);
        }
        Ok(true)
    }

    /// `ZipArchive::isCompressionMethodSupported()`: libzip's set, the
    /// oracle's build's.
    ///
    /// ```php
    /// foreach ([-1, 0, 1, 8, 9, 12, 14, 33, 93, 95, 98, 2**32 + 8] as $m) echo $m, ' ', var_export(ZipArchive::isCompressionMethodSupported($m), true), ' ', var_export(ZipArchive::isCompressionMethodSupported($m, false), true), "\n";
    /// ```
    /// ```expect
    /// -1 true true
    /// 0 true true
    /// 1 false false
    /// 8 true true
    /// 9 false false
    /// 12 true true
    /// 14 true true
    /// 33 false false
    /// 93 true true
    /// 95 true true
    /// 98 false false
    /// 4294967304 true true
    /// ```
    fn isCompressionMethodSupported(method: i64, #[php(default = true)] enc: bool) -> bool {
        let _ = enc;
        za::compression_supported(i64::from(c::i32(method)))
    }

    /// `ZipArchive::isEncryptionMethodSupported()`.
    ///
    /// ```php
    /// foreach ([0, 1, 2, 257, 258, 259, 260, 0x6601, 65535, 65536 + 1] as $m) echo $m, ' ', var_export(ZipArchive::isEncryptionMethodSupported($m), true), "\n";
    /// ```
    /// ```expect
    /// 0 true
    /// 1 true
    /// 2 false
    /// 257 true
    /// 258 true
    /// 259 true
    /// 260 false
    /// 26113 false
    /// 65535 false
    /// 65537 true
    /// ```
    fn isEncryptionMethodSupported(method: i64, #[php(default = true)] enc: bool) -> bool {
        let _ = enc;
        za::encryption_supported(i64::from(c::u16(method)))
    }
}

/// `ZEND_TRY_ASSIGN_REF_LONG` of the system and the attributes, or
/// `false` where there are none.
fn attributes(
    req: &mut Rq,
    found: Option<(u8, u32)>,
    opsys: &Ref<Untyped<Value>>,
    attr: &Ref<Untyped<Value>>,
) -> R<bool> {
    let Some((os, at)) = found else {
        return Ok(false);
    };
    opsys.set(req, Untyped(Value::int(i64::from(os))))?;
    attr.set(req, Untyped(Value::int(i64::from(at))))?;
    Ok(true)
}

/// `php_zip_set_file_comment`: an unchanged comment left alone, an empty
/// one removed.
fn set_file_comment(za: &mut Archive, idx: usize, comment: &[u8]) -> bool {
    za.file_comment(idx, crate::strings::FL_ENC_RAW).as_deref() == Some(comment)
        || za.set_file_comment(idx, comment, 0).is_ok()
}

/// `setEncryptionName`'s and `setEncryptionIndex`'s body: the password
/// reset, then the method set, php's warning where the reset fails.
fn set_encryption(
    req: &mut Rq,
    this: Z,
    idx: usize,
    method: i64,
    password: Option<PhpString>,
) -> bool {
    let password = password.map(|p| c_str(p.as_bytes()).to_vec());
    let Some(za) = this
        .object()
        .native_mut::<Intern>(req)
        .and_then(|i| i.za.as_deref_mut())
    else {
        return false;
    };
    if za.set_encryption(idx, 0, None).is_err() {
        docref(req, Level::Warning, "password reset failed");
        return false;
    }
    za.set_encryption(idx, c::u16(method), password.as_deref())
        .is_ok()
}

/// The entry `which` names, by index or by name, and its stat: `None`
/// where either fails.
fn locate(za: &mut Archive, which: &Union<i64, Vec<u8>>, flags: u32) -> Option<za::Stat> {
    match which {
        Union::A(index) => za.stat_index(self::index(*index), flags).ok(),
        Union::B(name) => za
            .name_locate(name, flags)
            .and_then(|idx| za.stat_index(idx, flags))
            .ok(),
    }
}

/// `php_zip_get_from`: `len` bytes of the entry, all of it under `0`.
fn get_from(req: &mut Rq, this: Z, which: &Union<i64, Vec<u8>>, len: i64, flags: i64) -> Str {
    let za = archive!(req, this);
    if matches!(which, Union::B(name) if name.is_empty()) {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let flags = c::u32(flags);
    let Some(st) = locate(za, which, flags) else {
        return Ok(OrFalse::False);
    };
    if st.size < 1 {
        return Ok(OrFalse::Value(PhpString::new(req, b"")?));
    }
    let len = usize::try_from(if len < 1 { st.size } else { c::u64(len) }).unwrap_or(usize::MAX);
    let idx = usize::try_from(st.index).unwrap_or(usize::MAX);
    let data = with_file(req, this.object(), |za| {
        let mut zf = za.fopen(idx, flags, None).ok()?;
        Some(za.read_entry(&mut zf, len).unwrap_or_default())
    });
    let Some(Some(data)) = data else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(PhpString::new(req, &data)?))
}

/// `php_zip_get_stream`: the entry as a read stream, its name the
/// stream's `orig_path`.
fn get_stream(req: &mut Rq, this: Z, which: &Union<i64, Vec<u8>>, flags: i64) -> Res {
    let za = archive!(req, this);
    if matches!(which, Union::B(name) if name.is_empty()) {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let flags = c::u32(flags);
    let Some(st) = locate(za, which, flags) else {
        return Ok(Untyped(OrFalse::False));
    };
    let idx = usize::try_from(st.index).unwrap_or(usize::MAX);
    let Some(Ok(zf)) = with_file(req, this.object(), |za| za.fopen(idx, flags, None)) else {
        return Ok(Untyped(OrFalse::False));
    };
    let id = this.object().native::<Intern>(req).map_or(0, |i| i.id);
    let stream = crate::stream::open_entry(req, zf, Some((&st.name, id)), b"rb", None, None)?;
    Ok(Untyped(stream.map_or(OrFalse::False, |s| {
        OrFalse::Value(s.into_resource())
    })))
}

/// `php_zip_add_file` as `php_zip_add_from_pattern` calls it: the whole
/// file, under `entry`.
pub(crate) fn add_file_for_pattern(
    req: &mut Rq,
    object: PhpObject,
    file: &[u8],
    entry: &[u8],
    flags: i64,
) -> R<bool> {
    add_file(req, object, file, entry, 0, 0, None, flags)
}
