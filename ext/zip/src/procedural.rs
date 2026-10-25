//! The procedural API php deprecated in 8.0: `zip_open` hands a `Zip
//! Directory` resource, `zip_read` a `Zip Entry` resource for each entry
//! in turn, each entry's data opened as it is read.

use php_ext::abi::{OrFalse, Path, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, type_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Resource, ResourceType, Value};

use crate::State;
use crate::archive::{Archive, Stat};
use crate::object::{basedir_refuses, expand};
use crate::read::Reader;

/// `zip_rsrc`: the archive, the index `zip_read` reaches next, and the
/// count it stops at.
pub(crate) struct Dir {
    za: Option<Box<Archive>>,
    index: usize,
    num_files: usize,
}

/// `zip_read_rsrc`: the entry opened for reading, its stat, and the id
/// of the directory resource it came from.
pub(crate) struct Entry {
    data: Option<Reader>,
    sb: Stat,
    dir: i64,
}

/// `le_zip_dir`, registered with `php_zip_free_dir` on first use.
fn dir_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().dir {
        return kind;
    }
    let kind = req
        .memory_mut()
        .resources()
        .register_type(b"Zip Directory", Some(free_dir));
    req.state::<State>().dir = Some(kind);
    kind
}

/// `le_zip_entry`.
fn entry_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().entry {
        return kind;
    }
    let kind = req
        .memory_mut()
        .resources()
        .register_type(b"Zip Entry", None);
    req.state::<State>().entry = Some(kind);
    kind
}

/// `php_zip_free_dir`: the archive closed, php's warning where that
/// fails.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the resource destructor's shape, which a destructor that throws needs"
)]
fn free_dir(req: &mut Request, res: Resource) -> Result<(), Thrown> {
    /// A close no callback is registered for, of an archive no stream
    /// reads.
    struct Alone(Box<Archive>);
    impl crate::write::Program for Alone {
        fn archive(&mut self) -> Option<&mut Archive> {
            Some(&mut self.0)
        }
        fn progress(&mut self, _: f64) {}
        fn cancel(&mut self) -> bool {
            false
        }
        fn in_use(&mut self) -> bool {
            false
        }
    }
    let Some(za) = res.native_mut::<Dir>(req).and_then(|d| d.za.take()) else {
        return Ok(());
    };
    let umask = req.environment().umask();
    if crate::write::close(&mut Alone(za), umask).is_err() {
        docref(req, Level::Warning, "Cannot destroy the zip context");
    }
    Ok(())
}

/// `zend_fetch_resource` of `kind`, php's `TypeError` for another.
fn fetch(
    req: &mut Request,
    res: Resource,
    kind: ResourceType,
    name: &str,
    native: &str,
) -> Result<Resource, Thrown> {
    if res.kind() != Some(kind) {
        let text = format!("{native}(): supplied resource is not a valid {name} resource");
        return Err(type_error(req, &text));
    }
    Ok(res)
}

/// `zip_open(string $filename)`: the directory resource, or libzip's
/// error code, or `false`.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-open-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->close();
/// $d = @zip_open('proc-open-witness.zip'); var_dump(get_resource_type($d), @zip_open('no-such.zip'));
/// try { @zip_open(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink('proc-open-witness.zip');
/// ```
/// ```expect
/// string(13) "Zip Directory"
/// int(9)
/// zip_open(): Argument #1 ($filename) must not be empty
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::open() instead"))]
fn zip_open(req: &mut Request, filename: Path) -> Result<Untyped<Owner<Value>>, Thrown> {
    let filename = filename.0.as_bytes().to_vec();
    if filename.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if basedir_refuses(req, &filename) {
        return Ok(Untyped(Owner::bool(false)));
    }
    let Some(resolved) = expand(req, &filename) else {
        docref(req, Level::Warning, "No such file or directory");
        return Ok(Untyped(Owner::bool(false)));
    };
    let host = {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&resolved))
    };
    match Archive::open(host, 0) {
        Ok(za) => {
            let num_files = za.num_entries();
            let kind = dir_type(req);
            let dir = Dir {
                za: Some(Box::new(za)),
                index: 0,
                num_files,
            };
            Ok(Untyped(Resource::new(req, kind, dir)?.into_value()))
        }
        Err(status) => Ok(Untyped(Owner::int(status.er.code()))),
    }
}

/// `zip_close($zip)`: the directory closed, the resource left closed.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-close-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->close();
/// $d = @zip_open('proc-close-witness.zip'); var_dump(@zip_close($d), get_resource_type($d));
/// try { @zip_close($d); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// unlink('proc-close-witness.zip');
/// ```
/// ```expect
/// NULL
/// string(7) "Unknown"
/// zip_close(): supplied resource is not a valid Zip Directory resource
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::close() instead"))]
fn zip_close(req: &mut Request, zip: Resource) -> Result<(), Thrown> {
    let kind = dir_type(req);
    let res = fetch(req, zip, kind, "Zip Directory", "zip_close")?;
    res.close(req);
    Ok(())
}

/// `zip_read($zip)`: the next entry, opened, or `false` past the last.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-read-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->addFromString('b', '22'); $z->close();
/// $d = @zip_open('proc-read-witness.zip');
/// while ($e = @zip_read($d)) echo get_resource_type($e), ' ', @zip_entry_name($e), "\n";
/// var_dump(@zip_read($d)); unlink('proc-read-witness.zip');
/// ```
/// ```expect
/// Zip Entry a
/// Zip Entry b
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::statIndex() instead"))]
fn zip_read(req: &mut Request, zip: Resource) -> Result<Untyped<Owner<Value>>, Thrown> {
    let kind = dir_type(req);
    let res = fetch(req, zip, kind, "Zip Directory", "zip_read")?;
    let id = res.id();
    let Some(dir) = res.native_mut::<Dir>(req) else {
        return Ok(Untyped(Owner::bool(false)));
    };
    if dir.index >= dir.num_files {
        return Ok(Untyped(Owner::bool(false)));
    }
    let index = dir.index;
    let Some(za) = dir.za.as_deref_mut() else {
        return Ok(Untyped(Owner::bool(false)));
    };
    let Ok(sb) = za.stat_index(index, 0) else {
        return Ok(Untyped(Owner::bool(false)));
    };
    let Ok(opened) = za.fopen(index, 0, None) else {
        return Ok(Untyped(Owner::bool(false)));
    };
    dir.index += 1;
    let kind = entry_type(req);
    let entry = Entry {
        data: Some(opened),
        sb,
        dir: id,
    };
    Ok(Untyped(Resource::new(req, kind, entry)?.into_value()))
}

/// `zip_entry_open($zip_dp, $zip_entry, string $mode = "rb")`: whether
/// the entry was opened.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-eopen-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->close();
/// $d = @zip_open('proc-eopen-witness.zip'); $e = @zip_read($d);
/// var_dump(@zip_entry_open($d, $e), @zip_entry_open($d, $e, 'r'));
/// try { @zip_entry_open($e, $d); } catch (TypeError $x) { echo $x->getMessage(), "\n"; }
/// unlink('proc-eopen-witness.zip');
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// zip_entry_open(): supplied resource is not a valid Zip Entry resource
/// ```
#[php_function(deprecated(since = "8.0"))]
fn zip_entry_open(
    req: &mut Request,
    zip_dp: Resource,
    zip_entry: Resource,
    #[php(default = "rb")] mode: PhpString,
) -> Result<bool, Thrown> {
    let _ = mode;
    let ekind = entry_type(req);
    let entry = fetch(req, zip_entry, ekind, "Zip Entry", "zip_entry_open")?;
    let dkind = dir_type(req);
    fetch(req, zip_dp, dkind, "Zip Directory", "zip_entry_open")?;
    Ok(entry.native::<Entry>(req).is_some_and(|e| e.data.is_some()))
}

/// `zip_entry_close($zip_entry)`.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-eclose-witness.zip', ZipArchive::CREATE); $z->addFromString('a', '1'); $z->close();
/// $d = @zip_open('proc-eclose-witness.zip'); $e = @zip_read($d);
/// var_dump(@zip_entry_close($e));
/// try { @zip_entry_close($e); } catch (TypeError $x) { echo $x->getMessage(), "\n"; }
/// unlink('proc-eclose-witness.zip');
/// ```
/// ```expect
/// bool(true)
/// zip_entry_close(): supplied resource is not a valid Zip Entry resource
/// ```
#[php_function(deprecated(since = "8.0"))]
fn zip_entry_close(req: &mut Request, zip_entry: Resource) -> Result<bool, Thrown> {
    let kind = entry_type(req);
    let entry = fetch(req, zip_entry, kind, "Zip Entry", "zip_entry_close")?;
    entry.close(req);
    Ok(true)
}

/// `zip_entry_read($zip_entry, int $len = 1024)`: up to `len` bytes, a
/// non-positive `len` read as 1024.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-eread-witness.zip', ZipArchive::CREATE); $z->addFromString('a', 'abcdef'); $z->close();
/// $d = @zip_open('proc-eread-witness.zip'); $e = @zip_read($d);
/// var_dump(@zip_entry_read($e, 2), @zip_entry_read($e, -1), @zip_entry_read($e));
/// unlink('proc-eread-witness.zip');
/// ```
/// ```expect
/// string(2) "ab"
/// string(4) "cdef"
/// string(0) ""
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::getFromIndex() instead"))]
fn zip_entry_read(
    req: &mut Request,
    zip_entry: Resource,
    #[php(default = 1024)] len: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let kind = entry_type(req);
    let entry = fetch(req, zip_entry, kind, "Zip Entry", "zip_entry_read")?;
    let len = if len <= 0 {
        1024
    } else {
        usize::try_from(len).unwrap_or(usize::MAX)
    };
    let dir = entry.native::<Entry>(req).map_or(0, |e| e.dir);
    let mut file = dir_data(req, dir).and_then(|d| d.stdio.take());
    let chunk = match entry.native_mut::<Entry>(req).and_then(|e| e.data.as_mut()) {
        Some(zf) => zf.read_through(len, &mut file),
        None => return Ok(OrFalse::False),
    };
    if let Some(d) = dir_data(req, dir) {
        d.stdio = file;
    }
    Ok(OrFalse::Value(PhpString::new(
        req,
        &chunk.unwrap_or_default(),
    )?))
}

/// The data of the archive directory resource `dir` holds, whose `FILE`
/// its entries are read through.
fn dir_data(req: &mut Request, dir: i64) -> Option<&mut crate::archive::Data> {
    req.memory()
        .resource(dir)?
        .native_mut::<Dir>(req)?
        .za
        .as_deref_mut()?
        .data
        .as_mut()
}

/// `php_zip_entry_get_info`: the entry's stat, while the directory it
/// came from stands.
fn info(req: &mut Request, zip_entry: Resource, native: &str) -> Result<Option<Stat>, Thrown> {
    let kind = entry_type(req);
    let entry = fetch(req, zip_entry, kind, "Zip Entry", native)?;
    let Some(e) = entry.native::<Entry>(req) else {
        return Ok(None);
    };
    if e.data.is_none() || req.memory().resource(e.dir).is_none() {
        return Ok(None);
    }
    Ok(Some(e.sb.clone()))
}

/// `zip_entry_name($zip_entry)`.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-ename-witness.zip', ZipArchive::CREATE); $z->addFromString('dir/a', '1'); $z->close();
/// $d = @zip_open('proc-ename-witness.zip'); $e = @zip_read($d);
/// var_dump(@zip_entry_name($e)); $d = null; var_dump(@zip_entry_name($e));
/// unlink('proc-ename-witness.zip');
/// ```
/// ```expect
/// string(5) "dir/a"
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::statIndex() instead"))]
fn zip_entry_name(
    req: &mut Request,
    zip_entry: Resource,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match info(req, zip_entry, "zip_entry_name")? {
        Some(sb) => Ok(OrFalse::Value(PhpString::new(
            req,
            php_ext::coerce::c_str(&sb.name),
        )?)),
        None => Ok(OrFalse::False),
    }
}

/// `zip_entry_compressedsize($zip_entry)`.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-csize-witness.zip', ZipArchive::CREATE); $z->addFromString('a', str_repeat('a', 100)); $z->close();
/// $d = @zip_open('proc-csize-witness.zip'); var_dump(@zip_entry_compressedsize(@zip_read($d)));
/// unlink('proc-csize-witness.zip');
/// ```
/// ```expect
/// int(6)
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::statIndex() instead"))]
fn zip_entry_compressedsize(
    req: &mut Request,
    zip_entry: Resource,
) -> Result<OrFalse<i64>, Thrown> {
    Ok(match info(req, zip_entry, "zip_entry_compressedsize")? {
        Some(sb) => OrFalse::Value(crate::c::long(sb.comp_size)),
        None => OrFalse::False,
    })
}

/// `zip_entry_filesize($zip_entry)`.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-fsize-witness.zip', ZipArchive::CREATE); $z->addFromString('a', str_repeat('a', 100)); $z->close();
/// $d = @zip_open('proc-fsize-witness.zip'); var_dump(@zip_entry_filesize(@zip_read($d)));
/// unlink('proc-fsize-witness.zip');
/// ```
/// ```expect
/// int(100)
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::statIndex() instead"))]
fn zip_entry_filesize(req: &mut Request, zip_entry: Resource) -> Result<OrFalse<i64>, Thrown> {
    Ok(match info(req, zip_entry, "zip_entry_filesize")? {
        Some(sb) => OrFalse::Value(crate::c::long(sb.size)),
        None => OrFalse::False,
    })
}

/// `zip_entry_compressionmethod($zip_entry)`: the method's name, or
/// `false` for one past implode.
///
/// ```php
/// $z = new ZipArchive; $z->open('proc-method-witness.zip', ZipArchive::CREATE);
/// $z->addFromString('s', 'x'); $z->addFromString('d', str_repeat('a', 100)); $z->addFromString('b', 'y'); $z->setCompressionName('b', ZipArchive::CM_BZIP2); $z->close();
/// $d = @zip_open('proc-method-witness.zip');
/// while ($e = @zip_read($d)) var_dump(@zip_entry_compressionmethod($e));
/// unlink('proc-method-witness.zip');
/// ```
/// ```expect
/// string(6) "stored"
/// string(8) "deflated"
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.0", message = "use ZipArchive::statIndex() instead"))]
fn zip_entry_compressionmethod(
    req: &mut Request,
    zip_entry: Resource,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(sb) = info(req, zip_entry, "zip_entry_compressionmethod")? else {
        return Ok(OrFalse::False);
    };
    let name: &[u8] = match sb.comp_method {
        0 => b"stored",
        1 => b"shrunk",
        2..=5 => b"reduced",
        6 => b"imploded",
        7 => b"tokenized",
        8 => b"deflated",
        9 => b"deflatedX",
        10 => b"implodedX",
        _ => return Ok(OrFalse::False),
    };
    Ok(OrFalse::Value(PhpString::new(req, name)?))
}
