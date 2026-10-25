//! libzip's error state re-derived (`zip_error_t`, `zip_err_str.c`, which
//! libzip's build generates from `zip.h`'s comments, and
//! `zip_error_strerror.c`): a code, `ZipArchive::$status`, and the number
//! beside it, `$statusSys`, read as an `errno` for the codes whose kind
//! is the system's.

use php_ext::request::Request;

/// `ZIP_ER_*`, each its `ZipArchive::ER_*`: the codes this crate sets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub(crate) enum Er {
    Multidisk = 1,
    Rename = 2,
    Seek = 4,
    Read = 5,
    Write = 6,
    Crc = 7,
    Zipclosed = 8,
    Noent = 9,
    Exists = 10,
    Open = 11,
    Tmpopen = 12,
    Zlib = 13,
    Memory = 14,
    Changed = 15,
    Compnotsupp = 16,
    Eof = 17,
    Inval = 18,
    Nozip = 19,
    Internal = 20,
    Incons = 21,
    Remove = 22,
    Deleted = 23,
    Encrnotsupp = 24,
    Rdonly = 25,
    Nopasswd = 26,
    Wrongpasswd = 27,
    Opnotsupp = 28,
    Inuse = 29,
    CompressedData = 31,
    Cancelled = 32,
    DataLength = 33,
    NotAllowed = 34,
    TruncatedZip = 35,
    EfTooLarge = 36,
}

/// `_zip_err_str`, as libzip's build generates it from `zip.h`'s comments:
/// each code's sentence after its kind, `S` where the system half is an
/// `errno`, `Z` a zlib code, `L` one of libzip's details, `N` none.
const STR: &str = "N No error|N Multi-disk zip archives not supported|S Renaming temporary file failed|S Closing zip archive failed|S Seek error|S Read error|S Write error|N CRC error|N Containing zip archive was closed|N No such file|N File already exists|S Can't open file|S Failure to create temporary file|Z Zlib error|N Malloc failure|N Entry has been changed|N Compression method not supported|N Premature end of file|N Invalid argument|N Not a zip archive|N Internal error|L Zip archive inconsistent|S Can't remove file|N Entry has been deleted|N Encryption method not supported|N Read-only archive|N No password provided|N Wrong password provided|N Operation not supported|N Resource still in use|S Tell error|N Compressed data invalid|N Operation cancelled|N Unexpected length of data|N Not allowed in torrentzip|N Possibly truncated or corrupted zip archive|N Extra fields too large";

/// `zip_error_t`: the code and its system half.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Status {
    pub(crate) zip: i32,
    pub(crate) sys: i32,
}

impl Status {
    pub(crate) fn of(er: Er, sys: i32) -> Status {
        Status {
            zip: er as i32,
            sys,
        }
    }

    /// `zip_error_set`.
    pub(crate) fn set(&mut self, er: Er, sys: i32) {
        *self = Status::of(er, sys);
    }

    /// `zip_error_set(ZIP_ER_INCONS, MAKE_DETAIL_WITH_INDEX(detail,
    /// entry))`: an inconsistency libzip names, at the entry it names or,
    /// with `None`, at `MAX_DETAIL_INDEX`, which names none.
    pub(crate) fn incons(detail: Detail, entry: Option<usize>) -> Status {
        let index = entry
            .and_then(|i| i32::try_from(i).ok())
            .map_or(MAX_DETAIL_INDEX, |i| i.min(MAX_DETAIL_INDEX));
        Status::of(Er::Incons, (index << 8) | detail as i32)
    }

    /// `zip_error_set(ZIP_ER_INCONS, detail)`, the detail with no index
    /// made, which libzip's text reads as at entry 0.
    pub(crate) fn bare(detail: Detail) -> Status {
        Status::of(Er::Incons, detail as i32)
    }

    /// `ADD_INDEX_TO_DETAIL(detail, entry)` on an inconsistency, which
    /// `_zip_read_cdir` and `_zip_checkcons` make of what
    /// `_zip_dirent_read` set; any other code as it stands.
    pub(crate) fn at_entry(self, entry: usize) -> Status {
        if self.zip != Er::Incons as i32 {
            return self;
        }
        let index = i32::try_from(entry).map_or(MAX_DETAIL_INDEX, |i| i.min(MAX_DETAIL_INDEX));
        Status::of(Er::Incons, (index << 8) | (self.sys & 0xff))
    }

    /// `zip_error_strerror`: the sentence, then `: ` and the `errno`'s
    /// text, zlib's `zError`, or libzip's detail, for a code whose kind
    /// has one.
    pub(crate) fn text(self, req: &Request) -> String {
        let entry = usize::try_from(self.zip)
            .ok()
            .and_then(|i| STR.split('|').nth(i));
        let Some((kind, sentence)) = entry.and_then(|e| e.split_once(' ')) else {
            return format!("Unknown error {}", self.zip);
        };
        match kind {
            "S" => format!("{sentence}: {}", req.environment().os_error_text(self.sys)),
            "Z" => format!("{sentence}: {}", z_error(self.sys)),
            "L" => match detail_text(self.sys) {
                Some(detail) => format!("{sentence}: {detail}"),
                None => sentence.to_owned(),
            },
            _ => sentence.to_owned(),
        }
    }
}

/// `ZIP_ER_DETAIL_*`: the details of `ZIP_ER_INCONS` this crate sets, by
/// libzip's number.
#[derive(Clone, Copy)]
pub(crate) enum Detail {
    CdirOverlapsEocd = 1,
    CommentLengthInvalid = 2,
    CdirLengthInvalid = 3,
    CdirEntryInvalid = 4,
    CdirWrongEntriesCount = 5,
    EntryHeaderMismatch = 6,
    EocdLengthInvalid = 7,
    Eocd64OverlapsEocd = 8,
    Eocd64WrongMagic = 9,
    Eocd64Mismatch = 10,
    CdirInvalid = 11,
    VariableSizeOverflow = 12,
    InvalidUtf8InFilename = 13,
    InvalidUtf8InComment = 14,
    InvalidZip64Ef = 15,
    InvalidWinzipaesEf = 16,
    EfTrailingGarbage = 17,
    InvalidEfLength = 18,
    InvalidFileLength = 19,
    StoredSizeMismatch = 20,
    DataDescriptorMismatch = 21,
    Eocd64LocatorMismatch = 22,
    Utf8FilenameMismatch = 23,
    Utf8CommentMismatch = 24,
    NulInFilename = 26,
    MissingZip64Ef = 27,
}

/// `MAX_DETAIL_INDEX`, the index of a detail that names no entry.
const MAX_DETAIL_INDEX: i32 = 0x7f_ffff;

/// `_zip_err_details`, as libzip's build generates it from `zipint.h`'s
/// comments: each detail's type, `G` for the archive and `E` for an
/// entry, and its description.
const DETAILS: &str = "G no detail|G central directory overlaps EOCD, or there is space between them|G archive comment length incorrect|G central directory length invalid|E central header invalid|G central directory count of entries is incorrect|E local and central headers do not match|G wrong EOCD length|G EOCD64 overlaps EOCD, or there is space between them|G EOCD64 magic incorrect|G EOCD64 and EOCD do not match|G invalid value in central directory|E variable size fields overflow header|E invalid UTF-8 in filename|E invalid UTF-8 in comment|E invalid Zip64 extra field|E invalid WinZip AES extra field|E garbage at end of extra fields|E extra field length is invalid|E file length in header doesn't match actual file length|E compressed and uncompressed sizes don't match for stored file|E local header and data descriptor do not match|G EOCD64 and EOCD64 locator do not match|E UTF-8 filename is ASCII and doesn't match filename|E UTF-8 comment is ASCII and doesn't match comment|G garbage at end of compressed data|E NUL byte in file name|E missing Zip64 extra field";

/// The `ZIP_ET_LIBZIP` half of `zip_error_strerror`: no text for detail
/// 0, `invalid detail error N` past the table, `entry I: ` before an
/// entry's detail that names one, the description otherwise.
fn detail_text(sys: i32) -> Option<String> {
    let error = sys & 0xff;
    let index = (sys >> 8) & MAX_DETAIL_INDEX;
    if error == 0 {
        return None;
    }
    let found = usize::try_from(error)
        .ok()
        .and_then(|e| DETAILS.split('|').nth(e));
    let Some((kind, description)) = found.and_then(|d| d.split_once(' ')) else {
        return Some(format!("invalid detail error {error}"));
    };
    Some(if kind == "E" && index < MAX_DETAIL_INDEX {
        format!("entry {index}: {description}")
    } else {
        description.to_owned()
    })
}

/// zlib's `zError`: the message for a zlib return code.
fn z_error(code: i32) -> &'static str {
    match code {
        2 => "need dictionary",
        1 => "stream end",
        -1 => "file error",
        -2 => "stream error",
        -3 => "data error",
        -4 => "insufficient memory",
        -5 => "buffer error",
        -6 => "incompatible version",
        _ => "",
    }
}

/// A libzip call that failed, its error already kept in the archive's
/// [`Status`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Failed;

pub(crate) type Done<T = ()> = Result<T, Failed>;
