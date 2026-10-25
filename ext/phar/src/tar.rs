//! `tar.c`: a tar archive read (`phar_parse_tarfile`), opened or created
//! as one (`phar_open_or_create_tar`), and written (`phar_tar_flush`),
//! its magic `.phar/` entries carrying the alias, the stub, the
//! `metadata` and the signature.

use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::archive::{
    self, Archive, ArchiveId, Content, ENT_PERM_DEF_FILE, ENT_PERM_MASK, Entry, Format, Metadata,
    TAR_DIR, TAR_FILE, TAR_LINK, TAR_SYMLINK, archives, lossy,
};
use crate::entry;
use crate::open::{self, Failed, Opened, require_hash};
use crate::path;
use crate::write;

/// `TAR_GLOBAL_HDR` and `TAR_FILE_HDR`, a pax header's type bytes.
const TAR_GLOBAL_HDR: u8 = b'g';
const TAR_FILE_HDR: u8 = b'x';
/// `TAR_CHAR`, `TAR_BLOCK` and `TAR_FIFO`, the device and fifo type bytes.
const TAR_CHAR: u8 = b'3';
const TAR_BLOCK: u8 = b'4';
const TAR_FIFO: u8 = b'6';
/// `TAR_LONGLINK` and `TAR_LONGNAME`, GNU's long link and long name records.
const TAR_LONGLINK: u8 = b'K';
const TAR_LONGNAME: u8 = b'L';

/// The fields of `tar_header` as offsets into its 512 bytes.
const NAME: std::ops::Range<usize> = 0..100;
const MODE: std::ops::Range<usize> = 100..108;
const SIZE: std::ops::Range<usize> = 124..136;
const MTIME: std::ops::Range<usize> = 136..148;
const CHECKSUM: std::ops::Range<usize> = 148..156;
const TYPEFLAG: usize = 156;
const LINKNAME: std::ops::Range<usize> = 157..257;
const MAGIC: std::ops::Range<usize> = 257..263;
const VERSION: std::ops::Range<usize> = 263..265;
const PREFIX: std::ops::Range<usize> = 345..500;
/// `sizeof(old_tar_header)`, the fields before `magic`.
const OLD_HEADER: usize = 257;

/// The stub a new executable tar archive carries.
const NEWSTUB: &[u8] = b"<?php // tar-based phar archive stub file\n__HALT_COMPILER();";

/// `phar_tar_number`: an octal field, leading spaces skipped, read to its
/// first byte that is not a digit.
fn number(field: &[u8]) -> u32 {
    let mut num: u32 = 0;
    let digits = field.iter().skip_while(|b| **b == b' ');
    for b in digits.take_while(|b| (b'0'..=b'7').contains(*b)) {
        num = num.wrapping_mul(8).wrapping_add(u32::from(b - b'0'));
    }
    num
}

/// `phar_tar_size`: the size field as php 8.5.12 reads it (GHSA-j3wh-g957-2m85),
/// leading spaces skipped, octal digits, then only spaces and NULs to the
/// field's end; `None` for GNU's base-256 form, any other byte, and a size
/// whose block rounding would pass `u32::MAX`.
fn size(field: &[u8]) -> Option<u32> {
    let mut i = field.iter().take_while(|b| **b == b' ').count();
    if field.get(i).is_some_and(|b| b & 0x80 != 0) {
        return None;
    }
    let mut num: u64 = 0;
    for b in field[i..].iter().take_while(|b| (b'0'..=b'7').contains(*b)) {
        num = num * 8 + u64::from(b - b'0');
        i += 1;
    }
    i += field[i..]
        .iter()
        .take_while(|b| **b == b' ' || **b == 0)
        .count();
    if i != field.len() {
        return None;
    }
    u32::try_from(num).ok().filter(|n| *n <= u32::MAX - 511)
}

/// `phar_tar_type_has_data`: whether a header's type byte is followed by
/// its data blocks, which every type but links, devices, directories and
/// fifos is.
fn type_has_data(typeflag: u8) -> bool {
    !matches!(
        typeflag,
        TAR_LINK | TAR_SYMLINK | TAR_CHAR | TAR_BLOCK | TAR_DIR | TAR_FIFO
    )
}

/// `phar_tar_octal`: `val` in `len` octal digits; the digits all `7`
/// where it overflows, which is the `Err`.
fn octal(field: &mut [u8], val: u32, len: usize) -> Result<(), ()> {
    let mut val = val;
    for at in (0..len).rev() {
        field[at] = b'0' + (val & 7) as u8;
        val >>= 3;
    }
    if val == 0 {
        return Ok(());
    }
    field[..len].fill(b'7');
    Err(())
}

/// `phar_tar_checksum`: the bytes summed.
fn checksum(buf: &[u8]) -> u32 {
    buf.iter().map(|b| u32::from(*b)).sum()
}

/// A byte field read to its first NUL, at most its length.
fn strn(field: &[u8]) -> &[u8] {
    php_ext::coerce::c_str(field)
}

/// `phar_is_tar`: the first block's checksum holds, or the name says
/// `.tar` and the archive is taken for a damaged one; a block that opens
/// with `<?php` is a stub, not a tar.
pub(crate) fn is_tar(buf: &[u8], fname: &[u8]) -> bool {
    if buf.len() < 512 || buf.starts_with(b"<?php") {
        return false;
    }
    let sum = number(&buf[CHECKSUM]);
    let mut header = buf[..512].to_vec();
    header[CHECKSUM].fill(b' ');
    if sum == checksum(&header) {
        return true;
    }
    let base = match fname.iter().rposition(|b| *b == b'/') {
        Some(slash) => &fname[slash..],
        None => fname,
    };
    path::find(base, b".tar").is_some_and(|at| matches!(base.get(at + 4), None | Some(b'.')))
}

/// `phar_open_or_create_tar`: the archive opened or created, which must
/// be a tar one where it was there already.
pub(crate) fn open_or_create(
    req: &mut Request,
    fname: &[u8],
    alias: Option<&[u8]>,
    is_data: bool,
    report: bool,
) -> Result<Opened, Thrown> {
    let id = match open::create_or_parse(req, fname, alias, is_data, report)? {
        Ok(id) => id,
        Err(error) => return Ok(Err(error)),
    };
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(Err(None));
    };
    a.is_data = is_data;
    if a.is_tar() {
        return Ok(Ok(id));
    }
    if a.is_brandnew {
        a.format = Format::Tar;
        return Ok(Ok(id));
    }
    Ok(Err(Some(format!(
        "phar tar error: \"{}\" already exists as a regular phar and must be deleted from disk prior to creating as a tar-based phar",
        lossy(fname)
    ))))
}

/// What a tar parse refuses: php's message with the archive's name.
type Refused = Result<Opened, Thrown>;

/// The cursor over the archive's bytes that `php_stream_tell` and
/// `php_stream_read` are in `phar_parse_tarfile`.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    /// `php_stream_read(fp, buf, len)`: at most `len` bytes from here.
    fn read(&mut self, len: usize) -> &[u8] {
        let start = self.at.min(self.bytes.len());
        let end = start.saturating_add(len).min(self.bytes.len());
        self.at = end;
        &self.bytes[start..end]
    }

    /// One 512-byte header, or `None` where fewer are left.
    fn header(&mut self) -> Option<[u8; 512]> {
        let got = self.read(512);
        <[u8; 512]>::try_from(got).ok()
    }
}

/// `phar_tar_process_metadata`: a magic `metadata` file's contents laid on
/// the archive or on the entry it names; `false` where they cannot be
/// read or the owner has some already.
fn process_metadata(archive: &mut Archive, name: &[u8], contents: Option<&[u8]>) -> bool {
    let Some(contents) = contents else {
        return false;
    };
    let meta = open::lazy_metadata(contents);
    if name == b".phar/.metadata.bin" {
        if archive.metadata.has_data() {
            return false;
        }
        archive.metadata = meta;
        return true;
    }
    let (head, tail) = (b".phar/.metadata/".len(), b"/.metadata.bin".len());
    if name.len() > head + tail
        && let Some(owner) = archive.manifest.get_mut(&name[head..name.len() - tail])
    {
        if owner.metadata.has_data() {
            return false;
        }
        owner.metadata = meta;
        return true;
    }
    if let Some(own) = archive.manifest.get_mut(name) {
        own.metadata = meta;
    }
    true
}

/// `phar_parse_tarfile`: every header read to the end of the archive or
/// the two zero blocks, the magic entries taken up, the signature
/// verified, and the archive registered under its name and alias. Each
/// header's size is read as php 8.5.12's `phar_tar_size` reads it, a GNU
/// long link is refused (GHSA-j3wh-g957-2m85), and the data of every type
/// that carries any is skipped, so no header hides inside a member's data.
///
/// ```php
/// function member($name, $size, $type = '0', $data = '') {
///     $h = pack('a100a8a8a8a12a12a8a1a100a6a2a247', $name, '0000644', '', '', $size, '00000000000', '        ', $type, '', 'ustar', '00', '');
///     $h = substr_replace($h, sprintf('%06o', array_sum(array_map('ord', str_split($h)))) . "\0 ", 148, 8);
///     return $h . str_pad($data, (strlen($data) + 511) & ~511, "\0");
/// }
/// $t = __DIR__ . '/tar-size-witness.tar';
/// $tars = [
///     member('a.txt', '37777777777'),
///     member('a.txt', "\x80\0\0\0\0\0\0\0\0\0\x02\0"),
///     member('a.txt', '1x'),
///     member('a.txt', '00000000000', 'K'),
///     member('carrier', '00000001000', '7', member('hidden', '00000000000')),
/// ];
/// foreach ($tars as $tar) {
///     file_put_contents($t, $tar . str_repeat("\0", 1024));
///     try {
///         foreach (new PharData($t) as $f) echo $f->getFilename(), "\n";
///     } catch (UnexpectedValueException $e) {
///         echo substr(strstr($e->getMessage(), '" is'), 2), "\n";
///     }
/// }
/// unlink($t);
/// ```
/// ```expect
/// is a corrupted tar file (invalid entry size)
/// is a corrupted tar file (invalid entry size)
/// is a corrupted tar file (invalid entry size)
/// is a tar file with an unsupported GNU long link entry
/// carrier
/// ```
#[allow(clippy::too_many_lines)] // php's one function, its refusals in its order
pub(crate) fn parse(
    req: &mut Request,
    bytes: Vec<u8>,
    fname: &[u8],
    alias: Option<&[u8]>,
    compression: u32,
) -> Refused {
    let name = lossy(fname).into_owned();
    let fail = |text: String| Ok(Err(Some(text)));
    let truncated = || {
        fail(format!(
            "phar error: \"{name}\" is a corrupted tar file (truncated)"
        ))
    };
    let total = bytes.len();
    let mut cur = Cursor {
        bytes: &bytes,
        at: 0,
    };
    let Some(mut buf) = cur.header() else {
        return fail(format!(
            "phar error: \"{name}\" is not a tar file or is truncated"
        ));
    };
    let mut old = &buf[MAGIC][..5] != b"ustar";
    let mut phar = Archive::new(b"", Format::Tar);
    phar.flags = compression;
    let mut actual_alias: Option<Vec<u8>> = None;
    let mut longlink: Option<Vec<u8>> = None;
    loop {
        let pos = cur.at;
        let sum1 = number(&buf[CHECKSUM]);
        if sum1 == 0 && checksum(&buf) == 0 {
            break;
        }
        buf[CHECKSUM].fill(b' ');
        let mut sum2 = checksum(&buf[..if old { OLD_HEADER } else { 512 }]);
        if old && sum2 != sum1 {
            let sum3 = checksum(&buf);
            if sum3 == sum1 {
                sum2 = sum3;
                old = false;
            }
        }
        let typeflag = buf[TYPEFLAG];
        let Some(size) = size(&buf[SIZE]) else {
            return fail(format!(
                "phar error: \"{name}\" is a corrupted tar file (invalid entry size)"
            ));
        };
        if typeflag == TAR_LONGLINK {
            return fail(format!(
                "phar error: \"{name}\" is a tar file with an unsupported GNU long link entry"
            ));
        }
        let skip = if !old && (typeflag == TAR_GLOBAL_HDR || typeflag == TAR_FILE_HDR) {
            Some(round(size))
        } else {
            None
        };
        if skip.is_none()
            && (old || buf[PREFIX][0] == 0)
            && strn(&buf[NAME]) == b".phar/signature.bin"
        {
            if size > 511 {
                return fail(format!(
                    "phar error: tar-based phar \"{name}\" has signature that is larger than 511 bytes, cannot process"
                ));
            }
            let curloc = cur.at;
            let sig = cur.read(size as usize).to_vec();
            if sig.len() != size as usize || sig.len() <= 8 {
                return fail(format!(
                    "phar error: tar-based phar \"{name}\" signature cannot be read"
                ));
            }
            phar.sig_flags = open::le32(&sig, 0);
            let end = cur.at - size as usize - 512;
            match crate::signature::verify(req, &bytes, end, phar.sig_flags, &sig[8..], fname)? {
                Ok(hex) => phar.signature = Some(hex),
                Err(error) => {
                    return fail(format!(
                        "phar error: tar-based phar \"{name}\" signature cannot be verified: {error}"
                    ));
                }
            }
            cur.at = curloc + 512;
            if (typeflag == 0 || typeflag == TAR_FILE) && size > 0 {
                cur.at += 512;
                if cur.at > total {
                    return truncated();
                }
            }
            let Some(next) = cur.header() else {
                return truncated();
            };
            if number(&next[CHECKSUM]) == 0 && checksum(&next) == 0 {
                break;
            }
            return fail(format!(
                "phar error: \"{name}\" has entries after signature, invalid phar"
            ));
        }
        if let Some(skip) = skip {
            cur.at += skip as usize;
            if cur.at > total {
                return truncated();
            }
        } else {
            if longlink.is_none() && typeflag == TAR_LONGNAME {
                if size == 0 || size as usize > total {
                    return fail(format!(
                        "phar error: \"{name}\" is a corrupted tar file (invalid entry size)"
                    ));
                }
                let long = cur.read(size as usize).to_vec();
                if long.len() != size as usize {
                    return truncated();
                }
                longlink = Some(long);
                cur.at += (round(size) - size) as usize;
                if cur.at > total {
                    return truncated();
                }
                let Some(next) = cur.header() else {
                    return truncated();
                };
                buf = next;
                continue;
            }
            let filename = match longlink.take() {
                Some(long) => long,
                None if !old && buf[PREFIX][0] != 0 => {
                    let mut name = strn(&buf[PREFIX]).to_vec();
                    name.push(b'/');
                    name.extend_from_slice(strn(&buf[NAME]));
                    if name.last() == Some(&b'/') {
                        name.pop();
                    }
                    name
                }
                None => {
                    let mut name = strn(&buf[NAME]).to_vec();
                    if name.last() == Some(&b'/') {
                        name.pop();
                    }
                    name
                }
            };
            phar.add_virtual_dirs(&filename);
            if sum1 != sum2 {
                return fail(format!(
                    "phar error: \"{name}\" is a corrupted tar file (checksum mismatch of file \"{}\")",
                    lossy(&filename)
                ));
            }
            let mode = number(&buf[MODE]);
            let mut e = Entry::new(&filename);
            e.uncompressed_filesize = size;
            e.compressed_filesize = size;
            e.is_crc_checked = true;
            e.tar_type = if old && typeflag == 0 {
                TAR_FILE
            } else {
                typeflag
            };
            e.offset = i64::try_from(pos).unwrap_or(i64::MAX);
            e.offset_abs = e.offset;
            e.content = Content::Fp;
            e.flags = mode & ENT_PERM_MASK;
            e.timestamp = number(&buf[MTIME]);
            if old && e.tar_type == TAR_FILE && mode & 0o170_000 == 0o040_000 {
                e.tar_type = TAR_DIR;
            }
            e.is_dir = e.tar_type == TAR_DIR;
            let linkname = strn(&buf[LINKNAME]).to_vec();
            if e.tar_type == TAR_LINK {
                if !phar.manifest.contains(&linkname) {
                    return fail(format!(
                        "phar error: \"{name}\" is a corrupted tar file - hard link to non-existent file \"{}\"",
                        lossy(&linkname)
                    ));
                }
                e.link = Some(linkname);
            } else if e.tar_type == TAR_SYMLINK {
                e.link = Some(linkname);
            }
            e.set_inode(b"", 1, false);
            // `zend_hash_update_mem`: an entry of the same name is
            // replaced, and the manifest's destructor frees it.
            if let (_, Some(replaced)) = phar.manifest.update(&filename, e) {
                replaced.free(req);
            }
            if filename.starts_with(b".phar/.metadata") {
                let at = cur.at;
                let contents = cur.read(size as usize).to_vec();
                cur.at = at;
                let read = (contents.len() == size as usize).then_some(contents.as_slice());
                if !process_metadata(&mut phar, &filename, read) {
                    return fail(format!(
                        "phar error: tar-based phar \"{name}\" has invalid metadata in magic file \"{}\"",
                        lossy(&filename)
                    ));
                }
            }
            if actual_alias.is_none() && filename == b".phar/alias.txt" {
                if size > 511 {
                    return fail(format!(
                        "phar error: tar-based phar \"{name}\" has alias that is larger than 511 bytes, cannot process"
                    ));
                }
                let at = cur.at;
                let read = cur.read(size as usize).to_vec();
                if read.len() != size as usize {
                    return fail(format!(
                        "phar error: Unable to read alias from tar-based phar \"{name}\""
                    ));
                }
                if !path::valid_alias(&read) {
                    let mut shown = read.clone();
                    if shown.len() > 50 {
                        shown.truncate(50);
                        shown.extend_from_slice(b"...");
                    }
                    return fail(format!(
                        "phar error: invalid alias \"{}\" in tar-based phar \"{name}\"",
                        lossy(strn(&shown))
                    ));
                }
                actual_alias = Some(read);
                cur.at = pos.max(at.min(pos));
            }
            let padded = round(size);
            if type_has_data(typeflag) && padded > 0 {
                cur.at += padded as usize;
                if cur.at > total {
                    return truncated();
                }
            }
        }
        if cur.at == total {
            break;
        }
        let Some(next) = cur.header() else {
            return truncated();
        };
        buf = next;
    }
    phar.is_data = !phar.manifest.contains(b".phar/stub.php");
    if !phar.is_data && require_hash(req) && phar.signature.is_none() {
        phar.release_all(req);
        return fail(format!(
            "tar-based phar \"{name}\" does not have a signature"
        ));
    }
    phar.fname = fname.to_vec();
    phar.fp = Some(archive::Fp::opened(bytes, phar.flags));
    phar.find_ext();
    phar.version = archive::API_VERSION.to_string();
    if archives(req).by_fname(fname).is_some() {
        phar.release_all(req);
        return fail(format!(
            "phar error: Unable to add tar-based phar \"{name}\" to phar registry"
        ));
    }
    let registered_alias = actual_alias
        .clone()
        .or_else(|| alias.filter(|a| !a.is_empty()).map(<[u8]>::to_vec));
    phar.is_temporary_alias = actual_alias.is_none();
    phar.alias = Some(registered_alias.clone().unwrap_or_else(|| fname.to_vec()));
    let id = archives(req).insert(phar);
    archives(req).fname_map.add(fname, id).ok();
    if let Some(alias) = registered_alias {
        if let Some(holder) = archives(req).by_alias(&alias)
            && !archive::free_alias(req, holder)
        {
            archive::fname_map_del(req, fname);
            return fail(format!(
                "phar error: Unable to add tar-based phar \"{name}\", alias is already in use"
            ));
        }
        archives(req).alias_map.add(&alias, id).ok();
    }
    Ok(Ok(id))
}

/// A size rounded up to the 512-byte block, `(size+511)&~511`.
fn round(size: u32) -> u32 {
    size.wrapping_add(511) & !511
}

/// What `phar_tar_writeheaders_int` writes an entry from: its name, the
/// fields its header carries, and its contents.
struct Member<'a> {
    filename: &'a [u8],
    flags: u32,
    size: u32,
    timestamp: u32,
    tar_type: u8,
    link: Option<&'a [u8]>,
}

/// `phar_tar_writeheaders_int`'s header and contents: the ustar header,
/// a long name split at a `/` into `prefix` and `name`, the contents and
/// their padding; the header's checksum, which php keeps as the entry's
/// `crc32`, or php's message.
fn write_member(
    out: &mut Vec<u8>,
    m: &Member<'_>,
    contents: &[u8],
    fname: &str,
) -> Result<u32, String> {
    let shown = lossy(m.filename).into_owned();
    let too_long = || {
        format!(
            "tar-based phar \"{fname}\" cannot be created, filename \"{shown}\" is too long for tar file format"
        )
    };
    let mut header = [0_u8; 512];
    let name = m.filename;
    if name.len() > 100 {
        if name.len() > 256 {
            return Err(too_long());
        }
        let from = name.len() - 101;
        let Some(boundary) = name[from..]
            .iter()
            .position(|b| *b == b'/')
            .map(|at| from + at)
        else {
            return Err(too_long());
        };
        if boundary > 155 {
            return Err(too_long());
        }
        header[PREFIX.start..PREFIX.start + boundary].copy_from_slice(&name[..boundary]);
        let rest = &name[boundary + 1..];
        header[NAME.start..NAME.start + rest.len()].copy_from_slice(rest);
    } else {
        header[NAME.start..NAME.start + name.len()].copy_from_slice(name);
    }
    octal(&mut header[MODE], m.flags & ENT_PERM_MASK, 7).ok();
    if octal(&mut header[SIZE], m.size, 11).is_err() {
        return Err(format!(
            "tar-based phar \"{fname}\" cannot be created, filename \"{shown}\" is too large for tar file format"
        ));
    }
    if octal(&mut header[MTIME], m.timestamp, 11).is_err() {
        return Err(format!(
            "tar-based phar \"{fname}\" cannot be created, file modification time of file \"{shown}\" is too large for tar file format"
        ));
    }
    header[TYPEFLAG] = m.tar_type;
    if let Some(link) = m.link {
        if link.len() >= 100 {
            return Err(format!(
                "tar-based phar \"{fname}\" cannot be created, link \"{}\" is too long for format",
                lossy(link)
            ));
        }
        header[LINKNAME.start..LINKNAME.start + link.len()].copy_from_slice(link);
    }
    header[MAGIC.start..MAGIC.start + 5].copy_from_slice(b"ustar");
    header[VERSION].copy_from_slice(b"00");
    header[CHECKSUM].fill(b' ');
    let sum = checksum(&header);
    if octal(&mut header[CHECKSUM], sum, 7).is_err() {
        return Err(format!(
            "tar-based phar \"{fname}\" cannot be created, checksum of file \"{shown}\" is too large for tar file format"
        ));
    }
    out.extend_from_slice(&header);
    if m.size != 0 {
        out.extend_from_slice(contents);
        out.resize(out.len() + (round(m.size) - m.size) as usize, 0);
    }
    Ok(sum)
}

/// `phar_tar_setmetadata`: a magic file made to hold `meta`, serialised,
/// as its own contents.
fn set_metadata(
    req: &mut Request,
    id: ArchiveId,
    slot: usize,
    meta: &Metadata,
) -> Result<(), Thrown> {
    let mut copy = meta.copied(req);
    let text = match write::serialized(req, &mut copy) {
        Ok(text) => text,
        Err(thrown) => {
            copy.free(req);
            return Err(thrown);
        }
    };
    let Some(e) = archives(req)
        .get_mut(id)
        .and_then(|a| a.manifest.at_mut(slot))
    else {
        copy.free(req);
        return Ok(());
    };
    let mut old = std::mem::replace(&mut e.metadata, copy);
    let text = text.unwrap_or_default();
    let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
    e.uncompressed_filesize = len;
    e.compressed_filesize = len;
    e.content = Content::Mod(text);
    e.is_modified = true;
    e.offset = 0;
    e.offset_abs = 0;
    old.free(req);
    Ok(())
}

/// A magic `metadata` file added to the manifest, every other field zero.
fn add_magic(id: ArchiveId, req: &mut Request, name: &[u8]) -> Option<usize> {
    let a = archives(req).get_mut(id)?;
    let mut e = Entry::new(name);
    e.tar_type = TAR_FILE;
    a.manifest.add(name, e).ok()
}

/// The archive's own `metadata`, borrowed out and back.
fn with_archive_metadata(req: &mut Request, id: ArchiveId, slot: usize) -> Result<(), Thrown> {
    let Some(mut meta) = archives(req)
        .get_mut(id)
        .map(|a| std::mem::take(&mut a.metadata))
    else {
        return Ok(());
    };
    let set = set_metadata(req, id, slot, &meta);
    match archives(req).get_mut(id) {
        Some(a) => a.metadata = meta,
        None => meta.free(req),
    }
    set
}

/// `phar_tar_setupmetadata` over the manifest: each changed entry's
/// `metadata` in its magic file, a file whose entry has none removed, an
/// orphan removed, and the archive's own rewritten.
fn setup_metadata(req: &mut Request, id: ArchiveId) -> Result<(), Thrown> {
    let mut at = 0;
    loop {
        let Some(a) = archives(req).get(id) else {
            return Ok(());
        };
        let slots = a.manifest.slots();
        let Some(&slot) = slots.get(at) else {
            return Ok(());
        };
        at += 1;
        let Some(e) = a.manifest.at(slot) else {
            continue;
        };
        let name = e.filename.clone();
        if name.starts_with(b".phar/.metadata") {
            if name == b".phar/.metadata.bin" {
                with_archive_metadata(req, id, slot)?;
                continue;
            }
            let (head, tail) = (b".phar/.metadata/".len(), b"/.metadata.bin".len());
            if name.len() > head + tail && !a.manifest.contains(&name[head..name.len() - tail]) {
                if let Some(gone) = archives(req)
                    .get_mut(id)
                    .and_then(|a| a.manifest.remove_at(slot))
                {
                    gone.free(req);
                }
                at -= 1;
            }
            continue;
        }
        if !e.is_modified {
            continue;
        }
        let lookfor = [&b".phar/.metadata/"[..], &name, b"/.metadata.bin"].concat();
        if !e.metadata.has_data() {
            let removed = archives(req).get_mut(id).and_then(|a| {
                let gone = a.manifest.slot(&lookfor)?;
                a.manifest.remove_at(gone).map(|e| (gone, e))
            });
            if let Some((gone, e)) = removed {
                e.free(req);
                if gone < slot {
                    at -= 1;
                }
            }
            continue;
        }
        let target = match archives(req)
            .get(id)
            .and_then(|a| a.manifest.slot(&lookfor))
        {
            Some(target) => target,
            None => match add_magic(id, req, &lookfor) {
                Some(target) => target,
                None => continue,
            },
        };
        let Some(mut meta) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
            .map(|e| std::mem::take(&mut e.metadata))
        else {
            continue;
        };
        let set = set_metadata(req, id, target, &meta);
        match archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        {
            Some(e) => e.metadata = meta,
            None => meta.free(req),
        }
        set?;
    }
}

/// `phar_tar_flush`: the magic alias, stub and `metadata` files set, each
/// entry's header and contents written, the signature last for an
/// executable archive or one given an algorithm, and two zero blocks.
#[allow(clippy::too_many_lines)] // php's one function, its steps in its order
pub(crate) fn flush(
    req: &mut Request,
    id: ArchiveId,
    user_stub: Option<&[u8]>,
    is_default_stub: bool,
) -> Result<Failed, Thrown> {
    let Some(a) = archives(req).get_mut(id) else {
        return Ok(None);
    };
    let name = lossy(&a.fname).into_owned();
    let timestamp = write::now();
    let magic = |filename: &[u8], contents: Vec<u8>, compressed: u32| {
        let mut e = Entry::new(filename);
        e.flags = ENT_PERM_DEF_FILE;
        e.timestamp = timestamp;
        e.is_modified = true;
        e.is_crc_checked = true;
        e.tar_type = TAR_FILE;
        e.uncompressed_filesize = u32::try_from(contents.len()).unwrap_or(u32::MAX);
        e.compressed_filesize = compressed;
        e.content = Content::Mod(contents);
        e
    };
    if !a.is_data {
        let mut dropped = Vec::new();
        match a
            .alias
            .clone()
            .filter(|alias| !a.is_temporary_alias && !alias.is_empty())
        {
            Some(alias) => {
                let (_, old) = a
                    .manifest
                    .update(b".phar/alias.txt", magic(b".phar/alias.txt", alias, 0));
                dropped.extend(old);
            }
            None => dropped.extend(a.manifest.remove(b".phar/alias.txt")),
        }
        if let Some(stub) = user_stub.filter(|_| !is_default_stub) {
            const HALT: &[u8] = b"__HALT_COMPILER();";
            let Some(pos) = stub
                .windows(HALT.len())
                .position(|w| w.eq_ignore_ascii_case(HALT))
            else {
                return Ok(Some(format!("illegal stub for tar-based phar \"{name}\"")));
            };
            let contents = [&stub[..pos + HALT.len()], b" ?>\r\n"].concat();
            let (_, old) = a
                .manifest
                .update(b".phar/stub.php", magic(b".phar/stub.php", contents, 0));
            dropped.extend(old);
        } else {
            let e = magic(b".phar/stub.php", NEWSTUB.to_vec(), NEWSTUB.len() as u32);
            if is_default_stub {
                let (_, old) = a.manifest.update(b".phar/stub.php", e);
                dropped.extend(old);
            } else if !a.manifest.contains(b".phar/stub.php") {
                a.manifest.add(b".phar/stub.php", e).ok();
            }
        }
        for e in dropped {
            e.free(req);
        }
    }
    let has_meta = archives(req).get(id).is_some_and(|a| a.metadata.has_data());
    if has_meta {
        let slot = match archives(req)
            .get(id)
            .and_then(|a| a.manifest.slot(b".phar/.metadata.bin"))
        {
            Some(slot) => Some(slot),
            None => add_magic(id, req, b".phar/.metadata.bin"),
        };
        if let Some(slot) = slot {
            with_archive_metadata(req, id, slot)?;
        }
    }
    setup_metadata(req, id)?;
    let mut newfile: Vec<u8> = Vec::new();
    let slots = archives(req)
        .get(id)
        .map(|a| a.manifest.slots())
        .unwrap_or_default();
    for slot in slots {
        let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
            continue;
        };
        if e.is_mounted {
            continue;
        }
        if e.is_deleted {
            if e.can_remove()
                && let Some(gone) = archives(req)
                    .get_mut(id)
                    .and_then(|a| a.manifest.remove_at(slot))
            {
                gone.free(req);
            }
            continue;
        }
        let filename = e.filename.clone();
        if let Some(a) = archives(req).get_mut(id) {
            a.add_virtual_dirs(&filename);
        }
        let Some(e) = archives(req).get(id).and_then(|a| a.manifest.at(slot)) else {
            continue;
        };
        let (flags, size, timestamp, tar_type, link) = (
            e.flags,
            e.uncompressed_filesize,
            e.timestamp,
            e.tar_type,
            e.link.clone(),
        );
        let contents = if size == 0 {
            Vec::new()
        } else {
            if let Err(error) = entry::open_entry_fp(req, id, slot, false)? {
                return Ok(Some(error));
            }
            entry::contents(req, id, slot)?
        };
        let header_offset = i64::try_from(newfile.len()).unwrap_or(i64::MAX);
        let member = Member {
            filename: &filename,
            flags,
            size,
            timestamp,
            tar_type,
            link: link.as_deref(),
        };
        let sum = match write_member(&mut newfile, &member, &contents, &name) {
            Ok(sum) => sum,
            Err(error) => return Ok(Some(error)),
        };
        let Some(e) = archives(req)
            .get_mut(id)
            .and_then(|a| a.manifest.at_mut(slot))
        else {
            continue;
        };
        e.crc32 = sum;
        e.header_offset = header_offset;
        e.is_modified = false;
        e.content = Content::Fp;
        e.offset = header_offset + 512;
        e.offset_abs = e.offset;
    }
    let Some(a) = archives(req).get(id) else {
        return Ok(None);
    };
    if !a.is_data || a.sig_flags != 0 {
        let fname = a.fname.clone();
        let mut sig_flags = a.sig_flags;
        let signature = match crate::signature::create(req, &mut sig_flags, &newfile, &fname)? {
            Ok(signature) => signature,
            Err(error) => {
                return Ok(Some(format!(
                    "phar error: unable to write signature to tar-based phar: {error}"
                )));
            }
        };
        if let Some(a) = archives(req).get_mut(id) {
            a.sig_flags = sig_flags;
            a.signature = Some(crate::signature::hex(&signature));
        }
        let mut sigbuf = sig_flags.to_le_bytes().to_vec();
        sigbuf.extend_from_slice(
            &u32::try_from(signature.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        sigbuf.extend_from_slice(&signature);
        let member = Member {
            filename: b".phar/signature.bin",
            flags: ENT_PERM_DEF_FILE,
            size: u32::try_from(sigbuf.len()).unwrap_or(u32::MAX),
            timestamp,
            tar_type: TAR_FILE,
            link: None,
        };
        if let Err(error) = write_member(&mut newfile, &member, &sigbuf, &name) {
            return Ok(Some(error));
        }
    }
    newfile.resize(newfile.len() + 1024, 0);
    if let Some(a) = archives(req).get_mut(id) {
        a.ufp = None;
        a.is_brandnew = false;
    }
    write::write_out(req, id, newfile)
}
