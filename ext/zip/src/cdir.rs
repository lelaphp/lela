//! libzip 1.12.1's `zip_open.c` past the stat, re-derived: the end record
//! found as `_zip_find_central_dir` finds it, the last of the archive's
//! tail that reads as one (`find_eocd`, `_zip_read_eocd`,
//! `_zip_read_eocd64`, `check_eocd`), the central directory it names
//! read as `_zip_read_cdir` reads it, each record through
//! `_zip_dirent_read`, and under `ZIP_CHECKCONS` every local header held
//! to its central one (`_zip_checkcons`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::archive::{
    CHECKCONS, Dirent, EM_AES_128, EM_AES_192, EM_AES_256, EM_NONE, EM_TRAD_PKWARE, EM_UNKNOWN,
    GPBF_DATA_DESCRIPTOR, GPBF_ENCODING_UTF_8, GPBF_ENCRYPTED, GPBF_STRONG_ENCRYPTION, find_ef,
    strip_internal, u16_at, u32_at, u64_at,
};
use crate::dostime::DosTime;
use crate::error::{Detail, Er, Status};
use crate::strings::{Enc, FL_ENC_UTF_8, ZStr};

/// `EOCDLEN`, `EOCD64LOCLEN`, `EOCD64LEN`, `CDENTRYSIZE`, `LENTRYSIZE`.
const EOCDLEN: usize = 22;
const EOCD64LOCLEN: usize = 20;
const EOCD64LEN: usize = 56;
const CDENTRYSIZE: usize = 46;
const LENTRYSIZE: usize = 30;
/// `CDBUFSIZE`: `MAXCOMLEN + EOCDLEN + EOCD64LOCLEN`, the tail
/// `_zip_find_central_dir` reads.
pub(crate) const CDBUFSIZE: u64 = (MAXCOMLEN + EOCDLEN + EOCD64LOCLEN) as u64;
/// `MAXCOMLEN`.
const MAXCOMLEN: usize = 65_536;
/// `MAX_DATA_DESCRIPTOR_LENGTH`.
const MAX_DATA_DESCRIPTOR_LENGTH: usize = 24;
/// `EFBIG`, the `errno` of a seek past `ZIP_INT64_MAX`.
const EFBIG: i32 = libc::EFBIG;
const INT64_MAX: u64 = u64::MAX >> 1;

/// The central directory `zip_open` read: its records, where it starts,
/// its bytes as read and the archive's comment.
pub(crate) struct Cdir {
    pub(crate) entries: Vec<Dirent>,
    pub(crate) offset: u64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) comment: Vec<u8>,
    /// Whether a Zip64 end record gave it, which sizes the records after
    /// the data `_zip_checkcons` reads.
    pub(crate) is_zip64: bool,
}

/// `zip_cdir_t` before its entries: what an end record says.
struct End {
    this_disk: u32,
    eocd_disk: u32,
    disk_entries: u64,
    num_entries: u64,
    size: u64,
    offset: u64,
    eocd_offset: u64,
    is_zip64: bool,
}

/// What `_zip_read_cdir` made of one end record: passed over for an
/// earlier one, with the error it set if it set one, or taken, read or
/// failed.
enum Candidate {
    Passed(Option<Status>),
    Taken(Result<Cdir, Status>),
}

/// `cdir_status_t`, what `_zip_read_eocd64` returns.
enum Eocd64 {
    Ok,
    Invalid(Status),
    NotFound,
}

/// `_zip_find_central_dir`: the central directory of the `len` bytes of
/// `file`, opened under `flags`, or libzip's error. Each end record of the
/// tail is tried from the last back, and the first `_zip_read_cdir`
/// takes is the archive's, read or refused.
///
/// A central directory that stops short of its end record is refused
/// under `ZIP_CHECKCONS` alone (`check_eocd`, libzip 1.12.1):
///
/// ```php
/// $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0, 0, 0, 0, 0x81a40000, 0) . 'a';
/// $b = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'ahello';
/// file_put_contents('gap-witness.zip', $b . $cd . 'GAP' . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($b), 0));
/// $z = new ZipArchive; var_dump($z->open('gap-witness.zip'), $z->numFiles); $z->close();
/// var_dump($z->open('gap-witness.zip', ZipArchive::CHECKCONS)); unlink('gap-witness.zip');
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// int(21)
/// ```
pub(crate) fn find(file: &mut File, len: u64, flags: u32) -> Result<Cdir, Status> {
    if len < EOCDLEN as u64 {
        return Err(Status::of(Er::Nozip, 0));
    }
    let buflen = len.min(CDBUFSIZE);
    let buf_offset = len - buflen;
    let buf = read_at(file, buf_offset, usize::try_from(buflen).unwrap_or(0))?;
    if buf.len() < usize::try_from(buflen).unwrap_or(0) {
        return Err(Status::of(Er::Eof, 0));
    }
    let mut error = Status::of(Er::Nozip, 0);
    let mut last = None;
    while let Some(at) = find_eocd(&buf, last) {
        last = Some(at);
        match read_cdir(file, &buf, buf_offset, at, flags) {
            Candidate::Passed(None) => {}
            Candidate::Passed(Some(e)) => error = e,
            Candidate::Taken(read) => {
                let cd = read?;
                if flags & CHECKCONS != 0 {
                    return checkcons(file, cd);
                }
                return Ok(cd);
            }
        }
    }
    Err(error)
}

/// `find_eocd`: the last `PK\5\6` of the tail before `last`.
fn find_eocd(buf: &[u8], last: Option<usize>) -> Option<usize> {
    let start = match last {
        None => buf.len().checked_sub(4)?,
        Some(0) => return None,
        Some(l) => l - 1,
    };
    (0..=start)
        .rev()
        .find(|&p| buf[p..].starts_with(b"PK\x05\x06"))
}

/// `_zip_read_cdir`, the end record at `at` of the tail read from
/// `buf_offset`.
fn read_cdir(file: &mut File, buf: &[u8], buf_offset: u64, at: usize, flags: u32) -> Candidate {
    use Candidate::{Passed, Taken};
    let Some(eocd) = buf.get(at..at + EOCDLEN) else {
        return Passed(Some(Status::bare(Detail::EocdLengthInvalid)));
    };
    let field16 = |k| u16_at(eocd, k).unwrap_or(0);
    let field32 = |k| u32_at(eocd, k).unwrap_or(0);
    let mut cd = End {
        this_disk: u32::from(field16(4)),
        eocd_disk: u32::from(field16(6)),
        disk_entries: u64::from(field16(8)),
        num_entries: u64::from(field16(10)),
        size: u64::from(field32(12)),
        offset: u64::from(field32(16)),
        eocd_offset: buf_offset + at as u64,
        is_zip64: false,
    };
    let eocd64_found = at >= EOCD64LOCLEN && buf[at - EOCD64LOCLEN..].starts_with(b"PK\x06\x07");
    if eocd64_found {
        match eocd64(&mut cd, file, buf, buf_offset, at - EOCD64LOCLEN, flags) {
            Eocd64::Ok => {}
            Eocd64::Invalid(e) => return Taken(Err(e)),
            Eocd64::NotFound => return Passed(None),
        }
    }
    if (cd.eocd_disk != 0 || cd.this_disk != 0)
        && !eocd64_found
        && cd.eocd_disk != cd.this_disk
        && (cd.this_disk < cd.eocd_disk || cd.size <= cd.eocd_offset)
    {
        return Passed(None);
    }
    if !eocd64_found {
        let empty = cd.this_disk == 0
            && cd.eocd_disk == 0
            && cd.eocd_offset == 0
            && cd.offset == 0
            && cd.num_entries == 0;
        if !empty && !check_magic(cd.offset, buf, buf_offset, file, *b"PK\x01\x02") {
            return Passed(None);
        }
    }
    if let Err(e) = check_eocd(&cd, flags) {
        return Taken(Err(e));
    }
    Taken(read_entries(file, buf, buf_offset, at, &cd, flags))
}

/// `_zip_read_cdir` past `check_eocd`: the comment, and the records of the
/// directory `cd` names.
fn read_entries(
    file: &mut File,
    buf: &[u8],
    buf_offset: u64,
    at: usize,
    cd: &End,
    flags: u32,
) -> Result<Cdir, Status> {
    let mut comment_len = usize::from(u16_at(buf, at + 20).unwrap_or(0));
    if cd.offset.saturating_add(cd.size) > buf_offset + at as u64 {
        return Err(Status::bare(Detail::CdirOverlapsEocd));
    }
    let tail = &buf[at + EOCDLEN..];
    if (comment_len != 0 || flags & CHECKCONS != 0) && tail.len() != comment_len {
        if flags & CHECKCONS != 0 {
            return Err(Status::bare(Detail::CommentLengthInvalid));
        }
        comment_len = comment_len.min(tail.len());
    }
    let comment = tail[..comment_len].to_vec();
    let size = usize::try_from(cd.size).map_err(|_| Status::of(Er::Memory, 0))?;
    let (mut src, origin) = if cd.offset >= buf_offset {
        let start = usize::try_from(cd.offset - buf_offset).unwrap_or(usize::MAX);
        let Some(data) = buf.get(start..start.saturating_add(size)) else {
            return Err(Status::bare(Detail::CdirLengthInvalid));
        };
        (data.to_vec(), Origin::Directory)
    } else {
        (read_at(file, cd.offset, size)?, Origin::File)
    };
    // The file past the directory, read a record's most at a time where
    // the records are read from it, as libzip reads each from its source.
    let file_end = buf_offset + buf.len() as u64;
    let mut entries = Vec::with_capacity(
        usize::try_from(cd.num_entries.min(cd.size / CDENTRYSIZE as u64)).unwrap_or(0),
    );
    let mut nentry = cd.num_entries;
    let mut left = cd.size;
    let mut pos = 0usize;
    while left > 0 {
        let i = entries.len() as u64;
        let mut grown = false;
        if i == nentry {
            // InfoZIP's count of entries modulo 0x10000, read on where
            // another record follows and no EOCD64 gave the count.
            if cd.is_zip64 || left < CDENTRYSIZE as u64 {
                break;
            }
            nentry += 0x10000;
            grown = true;
        }
        let index = entries.len();
        if let Origin::File = origin {
            let want = pos + CDENTRYSIZE + 3 * usize::from(u16::MAX);
            let have = cd.offset + src.len() as u64;
            if src.len() < want && have < file_end {
                let more =
                    usize::try_from(file_end - have).map_or(want, |m| m.min(want - src.len()));
                src.extend(read_at(file, have, more)?);
            }
        }
        let read = dirent(
            &src[pos.min(src.len())..],
            false,
            cd.is_zip64,
            flags & CHECKCONS != 0,
            origin,
        )
        .and_then(|mut r| {
            winzip_aes(&mut r.de, r.ef)?;
            Ok(r)
        })
        .map_err(|e| {
            if grown && e.zip == Er::Nozip as i32 {
                Status::incons(Detail::CdirEntryInvalid, Some(index))
            } else {
                e.at_entry(index)
            }
        })?;
        pos += read.used;
        left = left.wrapping_sub(read.used as u64);
        entries.push(read.de);
    }
    if entries.len() as u64 != nentry || left > 0 {
        return Err(Status::bare(Detail::CdirWrongEntriesCount));
    }
    if flags & CHECKCONS != 0 && pos != size {
        return Err(Status::bare(Detail::CdirLengthInvalid));
    }
    Ok(Cdir {
        entries,
        offset: cd.offset,
        bytes: src[..pos.min(src.len())].to_vec(),
        comment,
        is_zip64: cd.is_zip64,
    })
}

/// `_zip_read_eocd64`: the Zip64 end record the locator at `loc` names,
/// over what the end record `cd` says.
///
/// Under `ZIP_CHECKCONS` the record must reach its locator, the size it
/// gives itself read as libzip 1.12.1 reads it:
///
/// ```php
/// $cd = "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0, 0, 0, 0, 0x81a40000, 0) . 'a';
/// $b = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'ahello';
/// foreach ([[44, ''], [44, 'HOLE'], [60, ''], [48, 'HOLE']] as [$size, $hole]) {
///     $e64 = strlen($b . $cd);
///     $z64 = "PK\6\6" . pack('P', $size) . pack('vvVV', 45, 45, 0, 0) . pack('PPPP', 1, 1, strlen($cd), strlen($b)) . $hole . "PK\6\7" . pack('VPV', 0, $e64, 1);
///     file_put_contents('eocd64-witness.zip', $b . $cd . $z64 . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($cd), strlen($b), 0));
///     $z = new ZipArchive; echo var_export($z->open('eocd64-witness.zip'), true), ' ';
///     $z->close(); echo var_export($z->open('eocd64-witness.zip', ZipArchive::CHECKCONS), true), "\n";
/// }
/// unlink('eocd64-witness.zip');
/// ```
/// ```expect
/// true true
/// true 21
/// true 21
/// true true
/// ```
fn eocd64(
    cd: &mut End,
    file: &mut File,
    buf: &[u8],
    buf_offset: u64,
    loc: usize,
    flags: u32,
) -> Eocd64 {
    let eocdloc_offset = buf_offset + loc as u64;
    let eocd_disk = u32_at(buf, loc + 4).unwrap_or(0);
    let eocd_offset = u64_at(buf, loc + 8).unwrap_or(0);
    let num_disks = u32_at(buf, loc + 16).unwrap_or(0);
    if !check_magic(eocd_offset, buf, buf_offset, file, *b"PK\x06\x06") {
        return Eocd64::NotFound;
    }
    if num_disks != 1 {
        return Eocd64::Invalid(Status::of(Er::Multidisk, 0));
    }
    if eocd_offset > INT64_MAX {
        return Eocd64::Invalid(Status::of(Er::Seek, EFBIG));
    }
    if eocd_offset + EOCD64LEN as u64 > eocdloc_offset {
        return Eocd64::Invalid(Status::bare(Detail::Eocd64OverlapsEocd));
    }
    let record = if eocd_offset >= buf_offset
        && eocd_offset + EOCD64LEN as u64 <= buf_offset + buf.len() as u64
    {
        let start = usize::try_from(eocd_offset - buf_offset).unwrap_or(0);
        buf[start..start + EOCD64LEN].to_vec()
    } else {
        match read_at(file, eocd_offset, EOCD64LEN) {
            Ok(r) if r.len() == EOCD64LEN => r,
            Ok(_) => return Eocd64::Invalid(Status::of(Er::Eof, 0)),
            Err(e) => return Eocd64::Invalid(e),
        }
    };
    let at64 = |k| u64_at(&record, k).unwrap_or(0);
    let at32 = |k| u32_at(&record, k).unwrap_or(0);
    if !record.starts_with(b"PK\x06\x06") {
        return Eocd64::Invalid(Status::bare(Detail::Eocd64WrongMagic));
    }
    let own = at64(4);
    if flags & CHECKCONS != 0
        && own
            .checked_add(eocd_offset + 12)
            .is_none_or(|end| end != eocdloc_offset)
    {
        return Eocd64::Invalid(Status::bare(Detail::Eocd64OverlapsEocd));
    }
    let this_disk = at32(16);
    if at32(20) != eocd_disk {
        return Eocd64::Invalid(Status::bare(Detail::Eocd64LocatorMismatch));
    }
    let i = at64(24);
    let nentry = at64(32);
    if nentry != i {
        return Eocd64::Invalid(Status::of(Er::Multidisk, 0));
    }
    let size = at64(40);
    let offset = at64(48);
    if offset > INT64_MAX || offset.checked_add(size).is_none() {
        return Eocd64::Invalid(Status::of(Er::Seek, EFBIG));
    }
    if nentry > size / CDENTRYSIZE as u64 {
        return Eocd64::Invalid(Status::bare(Detail::CdirInvalid));
    }
    if (cd.size != 0xffff_ffff && cd.size != size)
        || (cd.offset != 0xffff_ffff && cd.offset != offset)
        || (cd.num_entries != 0xffff && cd.num_entries != nentry)
        || (cd.disk_entries != 0xffff && cd.disk_entries != i)
        || (cd.this_disk != 0xffff && cd.this_disk != this_disk)
        || (cd.eocd_disk != 0xffff && cd.eocd_disk != eocd_disk)
    {
        return Eocd64::Invalid(Status::bare(Detail::Eocd64Mismatch));
    }
    *cd = End {
        this_disk,
        eocd_disk,
        disk_entries: i,
        num_entries: nentry,
        size,
        offset,
        eocd_offset,
        is_zip64: true,
    };
    Eocd64::Ok
}

/// `check_eocd`: one disk, a directory whose end can be sought to, and
/// under `ZIP_CHECKCONS` one that ends where its end record starts.
fn check_eocd(cd: &End, flags: u32) -> Result<(), Status> {
    if cd.disk_entries != cd.num_entries || cd.this_disk != 0 || cd.eocd_disk != 0 {
        return Err(Status::of(Er::Multidisk, 0));
    }
    let Some(end) = cd.offset.checked_add(cd.size) else {
        return Err(Status::of(Er::Seek, EFBIG));
    };
    if flags & CHECKCONS != 0 && end != cd.eocd_offset {
        return Err(Status::bare(Detail::CdirOverlapsEocd));
    }
    Ok(())
}

/// `check_magic`: whether `magic` stands at `offset`, read from the tail
/// where it holds the offset and from the file otherwise.
fn check_magic(offset: u64, buf: &[u8], buf_offset: u64, file: &mut File, magic: [u8; 4]) -> bool {
    if buf_offset <= offset {
        usize::try_from(offset - buf_offset)
            .ok()
            .and_then(|at| buf.get(at..at.checked_add(4)?))
            == Some(&magic[..])
    } else {
        read_at(file, offset, 4).is_ok_and(|r| r == magic)
    }
}

/// `_zip_checkcons`: every entry's local header within the archive before
/// its central directory, read, and matching its central record.
fn checkcons(file: &mut File, mut cd: Cdir) -> Result<Cdir, Status> {
    let is_zip64 = cd.is_zip64;
    let first = cd.entries.first().map_or(0, |e| e.offset);
    let (mut min, mut max) = (first, first);
    for (i, e) in cd.entries.iter_mut().enumerate() {
        min = min.min(e.offset);
        if min > cd.offset {
            return Err(Status::of(Er::Nozip, 0));
        }
        let tail = e.filename.as_ref().map_or(0, |f| f.raw.len() as u64) + LENTRYSIZE as u64;
        let Some(end) = e
            .offset
            .checked_add(tail)
            .and_then(|s| s.checked_add(e.comp_size))
        else {
            return Err(Status::of(Er::Nozip, 0));
        };
        max = max.max(end);
        if max > cd.offset {
            return Err(Status::of(Er::Nozip, 0));
        }
        let local = local_header(file, e, is_zip64).map_err(|s| s.at_entry(i))?;
        if !headers_match(e, &local) {
            return Err(Status::incons(Detail::EntryHeaderMismatch, Some(i)));
        }
        e.extra_local = local.extra_local;
        if let Some(detail) = check_consistency(e) {
            return Err(Status::incons(detail, Some(i)));
        }
    }
    Ok(cd)
}

/// `_zip_dirent_read` of the local header of `central`, the `PK\x07\x08`
/// record after its data read where its bits say one follows.
fn local_header(file: &mut File, central: &Dirent, is_zip64: bool) -> Result<Dirent, Status> {
    let mut head = read_at(file, central.offset, LENTRYSIZE)?;
    if let (Some(n), Some(m)) = (u16_at(&head, 26), u16_at(&head, 28)) {
        let variable = usize::from(n) + usize::from(m);
        head.extend(read_at(file, central.offset + LENTRYSIZE as u64, variable)?);
    }
    let mut read = dirent(&head, true, is_zip64, true, Origin::File)?;
    let de = &mut read.de;
    if de.bitflags & GPBF_DATA_DESCRIPTOR != 0 {
        let at = central.offset + read.used as u64 + central.comp_size;
        let desc = read_at(file, at, MAX_DATA_DESCRIPTOR_LENGTH)?;
        if desc.len() < MAX_DATA_DESCRIPTOR_LENGTH {
            return Err(Status::of(Er::Eof, 0));
        }
        let mut k = if desc.starts_with(b"PK\x07\x08") {
            4
        } else {
            0
        };
        let crc = u32_at(&desc, k).unwrap_or(0);
        k += 4;
        let mut size = || {
            let v = if is_zip64 {
                u64_at(&desc, k)
            } else {
                u32_at(&desc, k).map(u64::from)
            };
            k += if is_zip64 { 8 } else { 4 };
            v
        };
        let (Some(comp_size), Some(uncomp_size)) = (size(), size()) else {
            return Err(Status::of(Er::Internal, 0));
        };
        if (de.crc != 0 && de.crc != crc)
            || (de.comp_size != 0 && de.comp_size != comp_size)
            || (de.uncomp_size != 0 && de.uncomp_size != uncomp_size)
        {
            return Err(Status::bare(Detail::DataDescriptorMismatch));
        }
        de.crc = crc;
        de.comp_size = comp_size;
        de.uncomp_size = uncomp_size;
    }
    winzip_aes(&mut read.de, read.ef)?;
    Ok(read.de)
}

/// `_zip_headercomp`: a local header that matches its central record,
/// its sizes and CRC zero or matching where a record follows the data.
fn headers_match(central: &Dirent, local: &Dirent) -> bool {
    if central.version_needed < local.version_needed
        || central.comp_method != local.comp_method
        || central.last_mod != local.last_mod
        || central.filename.as_ref().map(|f| &f.raw) != local.filename.as_ref().map(|f| &f.raw)
    {
        return false;
    }
    if central.crc != local.crc
        || central.comp_size != local.comp_size
        || central.uncomp_size != local.uncomp_size
    {
        if local.bitflags & GPBF_DATA_DESCRIPTOR == 0 {
            return false;
        }
        if (local.crc != 0 && central.crc != local.crc)
            || (local.comp_size != 0 && central.comp_size != local.comp_size)
            || (local.uncomp_size != 0 && central.uncomp_size != local.uncomp_size)
        {
            return false;
        }
    }
    true
}

/// `zip_dirent_check_consistency`: a stored entry's compressed size is its
/// size and the header of its encryption.
fn check_consistency(de: &Dirent) -> Option<Detail> {
    if de.comp_method != 0 {
        return None;
    }
    let header = match de.encryption_method {
        EM_NONE => 0,
        EM_TRAD_PKWARE => 12,
        EM_AES_128 => 20,
        EM_AES_192 => 24,
        EM_AES_256 => 28,
        _ => return None,
    };
    (de.uncomp_size.checked_add(header) != Some(de.comp_size)).then_some(Detail::StoredSizeMismatch)
}

/// Where `_zip_dirent_read` reads a record from: the central directory
/// read whole, where a short record is `ZIP_ER_NOZIP` or an overflow, or
/// the file, where it is `ZIP_ER_EOF`.
#[derive(Clone, Copy)]
enum Origin {
    Directory,
    File,
}

/// One header `_zip_dirent_read` read: the record, its raw extra fields
/// and the bytes it took.
struct Header<'a> {
    de: Dirent,
    ef: &'a [u8],
    used: usize,
}

/// `_zip_dirent_read` up to its `WinZip` AES field: a central record, or a
/// local header with `local`, read from `origin`. An inconsistency is set
/// without its entry, which the caller adds.
///
/// A size Zip64 holds needs its extra field where the archive has a Zip64
/// end record (libzip 1.12.1), under `ZIP_CHECKCONS` or not:
///
/// ```php
/// $cd = fn ($ef) => "PK\1\2" . pack('vvvvvvVVVvvvvvVV', 0x314, 20, 0, 0, 0, 0x21, crc32('hello'), 5, 0xffffffff, 1, strlen($ef), 0, 0, 0, 0x81a40000, 0) . 'a' . $ef;
/// $b = "PK\3\4" . pack('vvvvvVVVvv', 20, 0, 0, 0, 0x21, crc32('hello'), 5, 5, 1, 0) . 'ahello';
/// foreach (['', pack('vvP', 1, 8, 5), pack('vvV', 1, 4, 5)] as $ef) {
///     $c = $cd($ef); $e64 = strlen($b . $c); $seen = [];
///     $z64 = "PK\6\6" . pack('P', 44) . pack('vvVV', 45, 45, 0, 0) . pack('PPPP', 1, 1, strlen($c), strlen($b)) . "PK\6\7" . pack('VPV', 0, $e64, 1);
///     foreach ([$z64, ''] as $end) {
///         file_put_contents('zip64-witness.zip', $b . $c . $end . "PK\5\6" . pack('vvvvVVv', 0, 0, 1, 1, strlen($c), strlen($b), 0));
///         $z = new ZipArchive; $r = $z->open('zip64-witness.zip');
///         $seen[] = $r === true ? 'size ' . $z->statIndex(0)['size'] : $r;
///         if ($r === true) $z->close();
///     }
///     echo implode(', ', $seen), "\n";
/// }
/// unlink('zip64-witness.zip');
/// ```
/// ```expect
/// 21, size 4294967295
/// size 5, size 5
/// 21, 21
/// ```
fn dirent(
    b: &[u8],
    local: bool,
    is_zip64: bool,
    check: bool,
    origin: Origin,
) -> Result<Header<'_>, Status> {
    let short = || {
        Status::of(
            match origin {
                Origin::Directory => Er::Nozip,
                Origin::File => Er::Eof,
            },
            0,
        )
    };
    let size = if local { LENTRYSIZE } else { CDENTRYSIZE };
    if b.len() < size {
        return Err(short());
    }
    let magic: &[u8; 4] = if local { b"PK\x03\x04" } else { b"PK\x01\x02" };
    if !b.starts_with(magic) {
        return Err(Status::of(Er::Nozip, 0));
    }
    // The fixed fields after the magic, which the local header has
    // without `version made by` and stops before the comment's length.
    let k = if local { 4 } else { 6 };
    let f16 = |at| u16_at(b, at).unwrap_or(0);
    let f32 = |at| u32_at(b, at).unwrap_or(0);
    let bitflags = f16(k + 2);
    let nlen = usize::from(f16(k + 22));
    let elen = usize::from(f16(k + 24));
    let clen = if local { 0 } else { usize::from(f16(32)) };
    let mut de = Dirent {
        version_madeby: if local { 0 } else { f16(4) },
        version_needed: f16(k),
        bitflags,
        comp_method: i32::from(f16(k + 4)),
        last_mod: DosTime {
            time: f16(k + 6),
            date: f16(k + 8),
        },
        crc: f32(k + 10),
        comp_size: u64::from(f32(k + 14)),
        uncomp_size: u64::from(f32(k + 18)),
        disk_number: if local { 0 } else { u32::from(f16(34)) },
        int_attrib: if local { 0 } else { f16(36) },
        ext_attrib: if local { 0 } else { f32(38) },
        offset: if local { 0 } else { u64::from(f32(42)) },
        encryption_method: if bitflags & GPBF_ENCRYPTED == 0 {
            EM_NONE
        } else if bitflags & GPBF_STRONG_ENCRYPTION != 0 {
            EM_UNKNOWN
        } else {
            EM_TRAD_PKWARE
        },
        extra_local: None,
        ..Dirent::default()
    };
    let used = size + nlen + elen + clen;
    let Some(variable) = b.get(size..used) else {
        return Err(match origin {
            Origin::Directory => Status::bare(Detail::VariableSizeOverflow),
            Origin::File => short(),
        });
    };
    let (name, rest) = variable.split_at(nlen);
    let (ef, comment) = rest.split_at(elen);
    let utf8 = bitflags & GPBF_ENCODING_UTF_8 != 0;
    let string = |raw: &[u8], invalid| match ZStr::new(raw, if utf8 { FL_ENC_UTF_8 } else { 0 }) {
        Ok(s) => Ok(s),
        Err(()) => Err(Status::bare(invalid)),
    };
    de.filename = string(name, Detail::InvalidUtf8InFilename)?;
    check_extra_fields(ef)?;
    de.comment = string(comment, Detail::InvalidUtf8InComment)?;
    de.filename = utf8_field(ef, 0x7075, de.filename, check, Detail::Utf8FilenameMismatch)?;
    if check && de.filename.as_ref().is_some_and(|f| f.raw.contains(&0)) {
        return Err(Status::bare(Detail::NulInFilename));
    }
    if !local {
        de.comment = utf8_field(ef, 0x6375, de.comment, check, Detail::Utf8CommentMismatch)?;
    }
    if de.uncomp_size == 0xffff_ffff || de.comp_size == 0xffff_ffff || de.offset == 0xffff_ffff {
        match find_ef(ef, 0x0001) {
            Some(z64) => zip64_field(&mut de, z64, local)?,
            None if is_zip64 => return Err(Status::bare(Detail::MissingZip64Ef)),
            None => {}
        }
    }
    if de.offset > INT64_MAX {
        return Err(Status::of(Er::Seek, EFBIG));
    }
    if local {
        de.extra_local = Some(strip_internal(ef));
    } else {
        de.extra_central = strip_internal(ef);
    }
    Ok(Header { de, ef, used })
}

/// `_zip_ef_parse`'s refusals: a field longer than what is left, and
/// bytes after the last field other than fewer than four NULs.
pub(crate) fn check_extra_fields(ef: &[u8]) -> Result<(), Status> {
    let mut at = 0;
    while ef.len() - at >= 4 {
        let len = usize::from(u16_at(ef, at + 2).unwrap_or(0));
        if ef.len() - at - 4 < len {
            return Err(Status::bare(Detail::InvalidEfLength));
        }
        at += 4 + len;
    }
    if ef[at..].iter().any(|&b| b != 0) {
        return Err(Status::bare(Detail::EfTrailingGarbage));
    }
    Ok(())
}

/// `_zip_dirent_process_ef_utf_8`: the string of the Info-ZIP UTF-8
/// field `id` where its CRC is that of `s`, `s` otherwise, and the
/// inconsistency `mismatch` where `check` refuses an ASCII one that
/// differs.
fn utf8_field(
    ef: &[u8],
    id: u16,
    s: Option<ZStr>,
    check: bool,
    mismatch: Detail,
) -> Result<Option<ZStr>, Status> {
    let Some(f) = find_ef(ef, id).filter(|f| f.len() >= 5 && f[0] == 1) else {
        return Ok(s);
    };
    let crc = crc32fast::hash(s.as_ref().map_or(&[][..], |s| &s.raw));
    if u32_at(f, 1) != Some(crc) {
        return Ok(s);
    }
    let Ok(Some(found)) = ZStr::new(&f[5..], FL_ENC_UTF_8) else {
        return Ok(s);
    };
    if check && s.as_ref().map(|s| &s.raw) != Some(&found.raw) && found.raw.is_ascii() {
        // `_zip_dirent_read` refuses the mismatch of a string it has; no
        // string stays none.
        return if s.is_some() {
            Err(Status::bare(mismatch))
        } else {
            Ok(None)
        };
    }
    Ok(Some(ZStr {
        known: Some(Enc::Utf8Known),
        ..found
    }))
}

/// `zip_dirent_process_ef_zip64`: the sizes and offset the Zip64 field
/// holds for those its record marks, the local header's two sizes always
/// there, and a field of any other length refused unless it is the whole
/// 24 or 28 bytes and agrees.
fn zip64_field(de: &mut Dirent, z64: &[u8], local: bool) -> Result<(), Status> {
    let mut at = 0usize;
    let mut ok = true;
    let mut next = |width: usize| {
        let v = z64
            .get(at..at + width)
            .map(|b| b.iter().rev().fold(0u64, |v, &x| (v << 8) | u64::from(x)));
        ok &= v.is_some();
        at += width;
        v.unwrap_or(0)
    };
    if de.uncomp_size == 0xffff_ffff {
        de.uncomp_size = next(8);
    } else if local {
        next(8);
    }
    if de.comp_size == 0xffff_ffff {
        de.comp_size = next(8);
    } else if local {
        next(8);
    }
    if !local {
        if de.offset == 0xffff_ffff {
            de.offset = next(8);
        }
        if de.disk_number == 0xffff {
            de.disk_number = u32::try_from(next(4)).unwrap_or(0);
        }
    }
    if ok && at == z64.len() {
        return Ok(());
    }
    let agrees = match z64.len() {
        28 => u32_at(z64, 24) == Some(de.disk_number) && head24(de, z64),
        24 => head24(de, z64),
        _ => false,
    };
    if agrees {
        Ok(())
    } else {
        Err(Status::bare(Detail::InvalidZip64Ef))
    }
}

/// The first 24 bytes of a whole Zip64 field: the size, compressed size
/// and offset the record holds.
fn head24(de: &Dirent, z64: &[u8]) -> bool {
    u64_at(z64, 0) == Some(de.uncomp_size)
        && u64_at(z64, 8) == Some(de.comp_size)
        && u64_at(z64, 16) == Some(de.offset)
}

/// `_zip_dirent_process_winzip_aes`: a `WinZip` AES entry's method,
/// strength and CRC validity, from its field.
fn winzip_aes(de: &mut Dirent, ef: &[u8]) -> Result<(), Status> {
    if de.comp_method != 99 {
        return Ok(());
    }
    let Some(aes) = find_ef(ef, 0x9901).filter(|f| f.len() >= 7) else {
        return Err(Status::bare(Detail::InvalidWinzipaesEf));
    };
    let crc_valid = match u16_at(aes, 0) {
        Some(1) => true,
        Some(2) => false,
        _ => return Err(Status::of(Er::Encrnotsupp, 0)),
    };
    if &aes[2..4] != b"AE" {
        return Err(Status::of(Er::Encrnotsupp, 0));
    }
    let method = match aes[4] {
        1 => EM_AES_128,
        2 => EM_AES_192,
        3 => EM_AES_256,
        _ => return Err(Status::of(Er::Encrnotsupp, 0)),
    };
    if aes.len() != 7 {
        return Err(Status::bare(Detail::InvalidWinzipaesEf));
    }
    de.crc_valid = crc_valid;
    de.encryption_method = method;
    de.comp_method = i32::from(u16_at(aes, 5).unwrap_or(0));
    Ok(())
}

/// Up to `n` bytes of `file` from `offset`, fewer at its end: the reads
/// `zip_open` makes through its source, a seek or read that fails being
/// `ZIP_ER_SEEK` or `ZIP_ER_READ` with the `errno`.
fn read_at(file: &mut File, offset: u64, n: usize) -> Result<Vec<u8>, Status> {
    let sys = |e: &std::io::Error| e.raw_os_error().unwrap_or(0);
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Status::of(Er::Seek, sys(&e)))?;
    let mut out = Vec::with_capacity(n);
    file.by_ref()
        .take(n as u64)
        .read_to_end(&mut out)
        .map_err(|e| Status::of(Er::Read, sys(&e)))?;
    Ok(out)
}
