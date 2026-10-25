//! `PharFileInfo`: an `SplFileInfo` over one entry's `phar://` URL, the
//! entry laid in spl's `oth` slot as `phar_entry_object` lays it, with no
//! handler: a clone holds the same entry and adds nothing to its count,
//! and the entry is let go by `__destruct`, once per object.

use php_ext::abi::{Passed, Path as PathArg, This, True};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, throw_exception_ex};
use php_ext::request::{Request, Streams as _};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_spl::directory::{self as spl, Other};

use crate::archive::{
    self, ArchiveId, ENT_COMPRESSED_BZ2, ENT_COMPRESSED_GZ, ENT_COMPRESSION_MASK, ENT_PERM_MASK,
    Entry, archives, lossy, string,
};
use crate::entry::{self, Dir, Found, Target};
use crate::object::{SplFileInfo, construct_as, metadata_value, set_metadata};
use crate::open::{self, has_bz2, has_zlib, readonly};
use crate::path::{self, Create, Exec};
use crate::throwables::Throwables;
use crate::write;

/// `phar_entry_object.entry`: a row of the archive's manifest, or a
/// directory made up for a path entries stand under, which the object
/// owns.
pub(crate) enum Info {
    Slot {
        archive: ArchiveId,
        slot: usize,
    },
    TempDir {
        archive: ArchiveId,
        entry: Box<Entry>,
    },
}

impl Other for Info {
    fn dtor(self: Box<Self>, _req: &mut Request) {}

    fn clone_other(&self, _req: &mut Request) -> Box<dyn Other> {
        Box::new(match self {
            Info::Slot { archive, slot } => Info::Slot {
                archive: *archive,
                slot: *slot,
            },
            Info::TempDir { archive, entry } => Info::TempDir {
                archive: *archive,
                entry: Box::new(entry.shallow(&entry.filename)),
            },
        })
    }
}

/// `PHAR_ENTRY_OBJECT()`: the archive the entry is in, and its row, `None`
/// for a made-up directory; php's `BadMethodCallException` for an object
/// whose constructor never ran.
fn held(req: &mut Request, this: PhpObject) -> Result<(ArchiveId, Option<usize>), Thrown> {
    let found = spl::other_mut::<Info>(req, this).map(|info| match info {
        Info::Slot { archive, slot } => (*archive, Some(*slot)),
        Info::TempDir { archive, .. } => (*archive, None),
    });
    match found {
        Some(found) if entry_of(req, this).is_some() => Ok(found),
        _ => {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Cannot call method on an uninitialized PharFileInfo object",
            ))
        }
    }
}

/// The entry the object holds.
fn entry_of(req: &mut Request, this: PhpObject) -> Option<&mut Entry> {
    let slot = match spl::other_mut::<Info>(req, this)? {
        Info::Slot { archive, slot } => Some((*archive, *slot)),
        Info::TempDir { .. } => None,
    };
    match slot {
        Some((id, slot)) => archives(req).get_mut(id)?.manifest.at_mut(slot),
        None => match spl::other_mut::<Info>(req, this)? {
            Info::TempDir { entry, .. } => Some(entry),
            Info::Slot { .. } => None,
        },
    }
}

/// A field of the entry, read after [`held`] passed.
fn read<R: Default>(req: &mut Request, this: PhpObject, f: impl FnOnce(&Entry) -> R) -> R {
    entry_of(req, this).map(|e| f(e)).unwrap_or_default()
}

/// The archive's name and data bit, which the refusals read.
fn archive_facts(req: &mut Request, id: ArchiveId) -> (String, bool, bool) {
    archives(req)
        .get(id)
        .map_or((String::new(), false, false), |a| {
            (lossy(&a.fname).into_owned(), a.is_data, a.is_tar())
        })
}

/// The flush after a change, its error as `PharException`.
fn flushed(req: &mut Request, id: ArchiveId) -> Result<(), Thrown> {
    if let Some(a) = archives(req).get_mut(id) {
        a.is_modified = true;
    }
    match write::flush(req, id)? {
        Some(error) => {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.phar_exception, &error))
        }
        None => Ok(()),
    }
}

/// `class PharFileInfo extends SplFileInfo`.
///
/// ```php
/// $p = __DIR__ . '/pharfileinfo-witness.phar';
/// $files = ['a.txt' => 'hello', 'd/b.txt' => 'x'];
/// $m = ''; $body = '';
/// foreach ($files as $n => $c) { $m .= pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 0, strlen($c), crc32($c), 0x1b6, 0); $body .= $c; }
/// $m = pack('VnVV', count($files), 0x1110, 0x10000, 0) . pack('V', 0) . $m;
/// $data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $body;
/// file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
/// $i = new PharFileInfo("phar://$p/a.txt");
/// var_dump($i->getContent(), $i->getCRC32() === crc32('hello'), $i->isCRCChecked(), $i->getCompressedSize(), $i->hasMetadata(), $i->getMetadata(), $i->getPharFlags(), $i->isCompressed(), $i->isCompressed(Phar::GZ), $i->decompress());
/// $calls = [
///     'chmod' => fn() => $i->chmod(0755),
///     'compress' => fn() => $i->compress(Phar::GZ),
///     'setMetadata' => fn() => $i->setMetadata(1),
///     'delMetadata' => fn() => $i->delMetadata(),
/// ];
/// foreach ($calls as $name => $f) {
///     try { $f(); echo "$name: ok\n"; } catch (Exception $e) { echo "$name: ", get_class($e), ': ', str_replace($p, 'P', $e->getMessage()), "\n"; }
/// }
/// $d = new PharFileInfo("phar://$p/d");
/// try { $d->getContent(); } catch (BadMethodCallException $e) { echo str_replace($p, 'P', $e->getMessage()), "\n"; }
/// try { $d->getCRC32(); } catch (BadMethodCallException $e) { echo $e->getMessage(), "\n"; }
/// try { new PharFileInfo('x.phar'); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// $c = clone $i; var_dump($c->getContent());
/// unset($i, $d, $c); unlink($p);
/// ```
/// ```expect
/// string(5) "hello"
/// bool(true)
/// bool(true)
/// int(5)
/// bool(false)
/// NULL
/// int(0)
/// bool(false)
/// bool(false)
/// bool(true)
/// chmod: PharException: Cannot modify permissions for file "a.txt" in phar "P", write operations are prohibited
/// compress: BadMethodCallException: Phar is readonly, cannot change compression
/// setMetadata: BadMethodCallException: Write operations disabled by the php.ini setting phar.readonly
/// delMetadata: BadMethodCallException: Write operations disabled by the php.ini setting phar.readonly
/// phar error: Cannot retrieve contents, "d" in phar "P" is a directory
/// Phar entry is a directory, does not have a CRC
/// 'x.phar' is not a valid phar archive URL (must have at least phar://filename.phar)
/// string(5) "hello"
/// ```
#[php_class(extends(SplFileInfo))]
pub(crate) struct PharFileInfo;

#[php_class]
impl PharFileInfo {
    /// `PharFileInfo::__construct(string $filename)`: the entry found in
    /// its archive and held, then `SplFileInfo::__construct($filename)`.
    fn __construct(req: &mut Request, this: This<Self>, filename: PathArg) -> Result<(), Thrown> {
        let this = this.object();
        if spl::other_mut::<Info>(req, this).is_some() {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Cannot call constructor twice",
            ));
        }
        let fname = filename.0.as_bytes().to_vec();
        let split = if fname.len() < 7 || !fname.starts_with(b"phar://") {
            None
        } else {
            path::split_fname(req, &fname, Exec::Either, Create::No)?.ok()
        };
        let Some(split) = split else {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.runtime_exception,
                &format!(
                    "'{}' is not a valid phar archive URL (must have at least phar://filename.phar)",
                    lossy(&fname)
                ),
            ));
        };
        let id = match open::open_from_filename(req, &split.arch, None, true)? {
            Ok(id) => id,
            Err(error) => {
                let tail = error.map(|e| format!(": {e}")).unwrap_or_default();
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.runtime_exception,
                    &format!("Cannot open phar file '{}'{tail}", lossy(&fname)),
                ));
            }
        };
        let info = match entry::find(req, id, &split.entry, Dir::Allow, true)? {
            Ok(Found::Slot(slot)) => {
                if let Some(a) = archives(req).get_mut(id) {
                    a.refcount += 1;
                    if let Some(e) = a.manifest.at_mut(slot) {
                        e.fileinfo_lock_count += 1;
                    }
                }
                Info::Slot { archive: id, slot }
            }
            Ok(Found::TempDir(entry)) => Info::TempDir { archive: id, entry },
            Err(error) => {
                let tail = error.map(|e| format!(", {e}")).unwrap_or_default();
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.runtime_exception,
                    &format!(
                        "Cannot access phar file entry '{}' in archive '{}'{tail}",
                        lossy(&split.entry),
                        lossy(&split.arch)
                    ),
                ));
            }
        };
        spl::set_other(req, this, Box::new(info));
        let url = string(req, &fname)?;
        let constructed = construct_as(req, b"SplFileInfo", this, &[*url]);
        url.give_up(req);
        constructed
    }

    /// `PharFileInfo::__destruct()`: the entry let go, and the archive
    /// with it.
    fn __destruct(req: &mut Request, this: This<Self>) {
        let this = this.object();
        let held = spl::other_mut::<Info>(req, this).map(|info| match info {
            Info::Slot { archive, slot } => Some((*archive, *slot)),
            Info::TempDir { .. } => None,
        });
        let Some(held) = held else {
            return;
        };
        drop(spl::take_other(req, this));
        if let Some((id, slot)) = held {
            if let Some(e) = archives(req)
                .get_mut(id)
                .and_then(|a| a.manifest.at_mut(slot))
            {
                e.fileinfo_lock_count = e.fileinfo_lock_count.saturating_sub(1);
            }
            archive::delref(req, id);
        }
    }

    /// `chmod(int $perms): void`: the permission bits set, the archive
    /// written.
    #[php(tentative)]
    fn chmod(req: &mut Request, this: This<Self>, perms: i64) -> Result<(), Thrown> {
        let this = this.object();
        let (id, _) = held(req, this)?;
        let (fname, is_data, _) = archive_facts(req, id);
        let (name, temp) = read(req, this, |e| {
            (lossy(&e.filename).into_owned(), e.is_temp_dir)
        });
        if temp {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                &format!(
                    "Phar entry \"{name}\" is a temporary directory (not an actual entry in the archive), cannot chmod"
                ),
            ));
        }
        if readonly(req) && !is_data {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.phar_exception,
                &format!(
                    "Cannot modify permissions for file \"{name}\" in phar \"{fname}\", write operations are prohibited"
                ),
            ));
        }
        if let Some(e) = entry_of(req, this) {
            e.flags &= !ENT_PERM_MASK;
            e.flags |= u32::try_from(perms & 0o777).unwrap_or(0);
            e.old_flags = e.flags;
            e.is_modified = true;
        }
        req.clear_stat_cache();
        flushed(req, id)
    }

    /// `compress(int $compression): true`.
    #[php(tentative)]
    fn compress(req: &mut Request, this: This<Self>, compression: i64) -> Result<True, Thrown> {
        let this = this.object();
        let (id, slot) = held(req, this)?;
        let (fname, is_data, is_tar) = archive_facts(req, id);
        let bad = |req: &mut Request, m: &str| {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.bad_method_call_exception, m))
        };
        if is_tar {
            return bad(
                req,
                "Cannot compress with Gzip compression, not possible with tar-based phar archives",
            );
        }
        let (is_dir, deleted, flags, name) = read(req, this, |e| {
            (
                e.is_dir,
                e.is_deleted,
                e.flags,
                lossy(&e.filename).into_owned(),
            )
        });
        if is_dir {
            return bad(req, "Phar entry is a directory, cannot set compression");
        }
        if readonly(req) && !is_data {
            return bad(req, "Phar is readonly, cannot change compression");
        }
        if deleted {
            return bad(req, "Cannot compress deleted file");
        }
        let (target, other, other_name, lib_name, has_other, has_target) = match compression {
            0x1000 => (
                ENT_COMPRESSED_GZ,
                ENT_COMPRESSED_BZ2,
                "bzip2",
                "gzip",
                has_bz2(req),
                has_zlib(req),
            ),
            0x2000 => (
                ENT_COMPRESSED_BZ2,
                ENT_COMPRESSED_GZ,
                "gzip",
                "bzip2",
                has_zlib(req),
                has_bz2(req),
            ),
            _ => return bad(req, "Unknown compression type specified"),
        };
        if flags & target != 0 {
            return Ok(True);
        }
        if flags & other != 0 {
            if !has_other {
                let ext = if other == ENT_COMPRESSED_BZ2 {
                    "bz2"
                } else {
                    "zlib"
                };
                return bad(
                    req,
                    &format!(
                        "Cannot compress with {lib_name} compression, file is already compressed with {other_name} compression and {ext} extension is not enabled, cannot decompress"
                    ),
                );
            }
            if let Some(slot) = slot
                && let Err(error) = entry::open_entry_fp(req, id, slot, true)?
            {
                let other_kind = if other == ENT_COMPRESSED_BZ2 {
                    "bzip2"
                } else {
                    "gzip"
                };
                return bad(
                    req,
                    &format!(
                        "phar error: Cannot decompress {other_kind}-compressed file \"{name}\" in phar \"{fname}\" in order to compress with {lib_name}: {error}"
                    ),
                );
            }
        }
        if !has_target {
            let message = if target == ENT_COMPRESSED_GZ {
                "Cannot compress with gzip compression, zlib extension is not enabled"
            } else {
                "Cannot compress with bzip2 compression, bz2 extension is not enabled"
            };
            return bad(req, message);
        }
        if let Some(e) = entry_of(req, this) {
            e.old_flags = e.flags;
            e.flags = (e.flags & !ENT_COMPRESSION_MASK) | target;
            e.is_modified = true;
        }
        flushed(req, id)?;
        Ok(True)
    }

    /// `decompress(): true`.
    #[php(tentative)]
    fn decompress(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        let this = this.object();
        let (id, slot) = held(req, this)?;
        let (fname, is_data, _) = archive_facts(req, id);
        let bad = |req: &mut Request, m: &str| {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.bad_method_call_exception, m))
        };
        let (is_dir, deleted, flags, name) = read(req, this, |e| {
            (
                e.is_dir,
                e.is_deleted,
                e.flags,
                lossy(&e.filename).into_owned(),
            )
        });
        if is_dir {
            return bad(req, "Phar entry is a directory, cannot set compression");
        }
        if flags & ENT_COMPRESSION_MASK == 0 {
            return Ok(True);
        }
        if readonly(req) && !is_data {
            return bad(req, "Phar is readonly, cannot decompress");
        }
        if deleted {
            return bad(req, "Cannot compress deleted file");
        }
        if flags & ENT_COMPRESSED_GZ != 0 && !has_zlib(req) {
            return bad(
                req,
                "Cannot decompress Gzip-compressed file, zlib extension is not enabled",
            );
        }
        if flags & ENT_COMPRESSED_BZ2 != 0 && !has_bz2(req) {
            return bad(
                req,
                "Cannot decompress Bzip2-compressed file, bz2 extension is not enabled",
            );
        }
        let kind = match flags & ENT_COMPRESSION_MASK {
            ENT_COMPRESSED_GZ => "gzip",
            ENT_COMPRESSED_BZ2 => "bz2",
            _ => {
                return bad(
                    req,
                    "Cannot decompress file compressed with unknown compression type",
                );
            }
        };
        if let Some(slot) = slot
            && let Err(error) = entry::open_entry_fp(req, id, slot, true)?
        {
            return bad(
                req,
                &format!(
                    "Phar error: Cannot decompress {kind}-compressed file \"{name}\" in phar \"{fname}\": {error}"
                ),
            );
        }
        if let Some(e) = entry_of(req, this) {
            e.old_flags = e.flags;
            e.flags &= !ENT_COMPRESSION_MASK;
            e.is_modified = true;
        }
        flushed(req, id)?;
        Ok(True)
    }

    /// `delMetadata(): true`.
    #[php(tentative)]
    fn delMetadata(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        let this = this.object();
        let (id, _) = held(req, this)?;
        let (_, is_data, _) = archive_facts(req, id);
        if readonly(req) && !is_data {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        let (temp, has) = read(req, this, |e| (e.is_temp_dir, e.metadata.has_data()));
        if temp {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Phar entry is a temporary directory (not an actual entry in the archive), cannot delete metadata",
            ));
        }
        if !has {
            return Ok(True);
        }
        let mut old = entry_of(req, this).map(|e| {
            e.is_modified = true;
            std::mem::take(&mut e.metadata)
        });
        if let Some(old) = old.as_mut() {
            old.free(req);
        }
        flushed(req, id)?;
        Ok(True)
    }

    /// `getCompressedSize(): int`.
    #[php(tentative)]
    fn getCompressedSize(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = this.object();
        held(req, this)?;
        Ok(read(req, this, |e| i64::from(e.compressed_filesize)))
    }

    /// `getCRC32(): int`.
    #[php(tentative)]
    fn getCRC32(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = this.object();
        held(req, this)?;
        let (is_dir, checked, crc) = read(req, this, |e| (e.is_dir, e.is_crc_checked, e.crc32));
        if is_dir {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Phar entry is a directory, does not have a CRC",
            ));
        }
        if !checked {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Phar entry was not CRC checked",
            ));
        }
        Ok(i64::from(crc))
    }

    /// `getContent(): string`: the whole entry, a link's target read
    /// where it is one.
    #[php(tentative)]
    fn getContent(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = this.object();
        let (id, slot) = held(req, this)?;
        let (fname, _, _) = archive_facts(req, id);
        let (is_dir, name) = read(req, this, |e| (e.is_dir, lossy(&e.filename).into_owned()));
        let bad = |req: &mut Request, m: &str| {
            let ce = Throwables::of(req)?;
            Err(throw_exception_ex(req, ce.bad_method_call_exception, m))
        };
        if is_dir {
            return bad(
                req,
                &format!(
                    "phar error: Cannot retrieve contents, \"{name}\" in phar \"{fname}\" is a directory"
                ),
            );
        }
        let Some(slot) = slot else {
            return bad(
                req,
                &format!("phar error: Cannot retrieve contents of \"{name}\" in phar \"{fname}\""),
            );
        };
        let link = entry::link_source(req, id, slot).unwrap_or(slot);
        if let Err(error) = entry::open_entry_fp(req, id, link, false)? {
            return bad(
                req,
                &format!(
                    "phar error: Cannot retrieve contents, \"{name}\" in phar \"{fname}\": {error}"
                ),
            );
        }
        let target = Target::Slot(link);
        let Some(efp) = entry::efp(req, id, &target)? else {
            return bad(
                req,
                &format!("phar error: Cannot retrieve contents of \"{name}\" in phar \"{fname}\""),
            );
        };
        let (offset, size) = archives(req)
            .get(id)
            .and_then(|a| a.manifest.at(link))
            .map_or((0, 0), |e| (e.offset, e.uncompressed_filesize as usize));
        let bytes = entry::read_efp(req, id, &target, &efp, offset, size)?;
        PhpString::new(req, &bytes)
    }

    /// `getMetadata(array $unserializeOptions = []): mixed`.
    #[php(tentative)]
    fn getMetadata(
        req: &mut Request,
        this: This<Self>,
        #[php(default = [])] unserializeOptions: Passed<PhpArray>,
    ) -> Result<Owner<Value>, Thrown> {
        let this = this.object();
        held(req, this)?;
        let Some(mut meta) = entry_of(req, this).map(|e| std::mem::take(&mut e.metadata)) else {
            return Ok(Owner::null());
        };
        let value = metadata_value(
            req,
            &mut meta,
            unserializeOptions,
            "PharFileInfo::getMetadata",
        );
        match entry_of(req, this) {
            Some(e) => e.metadata = meta,
            None => meta.free(req),
        }
        value
    }

    /// `getPharFlags(): int`: the flags beside the permission and
    /// compression bits.
    #[php(tentative)]
    fn getPharFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let this = this.object();
        held(req, this)?;
        Ok(read(req, this, |e| {
            i64::from(e.flags & !(ENT_PERM_MASK | ENT_COMPRESSION_MASK))
        }))
    }

    /// `hasMetadata(): bool`.
    #[php(tentative)]
    fn hasMetadata(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        held(req, this)?;
        Ok(read(req, this, |e| e.metadata.has_data()))
    }

    /// `isCompressed(?int $compression = null): bool`.
    #[php(tentative)]
    fn isCompressed(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] compression: Option<i64>,
    ) -> Result<bool, Thrown> {
        let this = this.object();
        held(req, this)?;
        let flags = read(req, this, |e| e.flags);
        match compression {
            None | Some(9_021_976) => Ok(flags & ENT_COMPRESSION_MASK != 0),
            Some(0x1000) => Ok(flags & ENT_COMPRESSED_GZ != 0),
            Some(0x2000) => Ok(flags & ENT_COMPRESSED_BZ2 != 0),
            Some(_) => {
                let ce = Throwables::of(req)?;
                Err(throw_exception_ex(
                    req,
                    ce.bad_method_call_exception,
                    "Unknown compression type specified",
                ))
            }
        }
    }

    /// `isCRCChecked(): bool`.
    #[php(tentative)]
    fn isCRCChecked(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        held(req, this)?;
        Ok(read(req, this, |e| e.is_crc_checked))
    }

    /// `setMetadata(mixed $metadata): void`.
    #[php(tentative)]
    fn setMetadata(req: &mut Request, this: This<Self>, metadata: Value) -> Result<(), Thrown> {
        let this = this.object();
        let (id, _) = held(req, this)?;
        let (_, is_data, _) = archive_facts(req, id);
        if readonly(req) && !is_data {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        if read(req, this, |e| e.is_temp_dir) {
            let ce = Throwables::of(req)?;
            return Err(throw_exception_ex(
                req,
                ce.bad_method_call_exception,
                "Phar entry is a temporary directory (not an actual entry in the archive), cannot set metadata",
            ));
        }
        let Some(mut meta) = entry_of(req, this).map(|e| std::mem::take(&mut e.metadata)) else {
            return Ok(());
        };
        let set = set_metadata(req, &mut meta, &metadata);
        if let Some(e) = entry_of(req, this) {
            e.metadata = meta;
            if set.is_ok() {
                e.is_modified = true;
            }
        }
        set?;
        flushed(req, id)
    }
}
