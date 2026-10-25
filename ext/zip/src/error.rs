//! libzip's error state re-derived (`zip_error_t`, `zip_err_str.c`, which
//! libzip's build generates from `zip.h`'s comments, and
//! `zip_error_strerror.c`): a code, `ZipArchive::$status`, and its system
//! half, `$statusSys`, an `errno`, a zlib code or one of libzip's details
//! as the code's kind reads it. Both are typed until they are read as
//! php's two integers or as `getStatusString()`'s text.

use php_ext::request::Request;

/// `ZIP_ER_*`, each its `ZipArchive::ER_*`: the codes this crate sets.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
#[repr(i32)]
pub(crate) enum Er {
    #[default]
    Ok = 0,
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

/// `zip_error_system_type`: what a code's system half is.
#[derive(Clone, Copy)]
enum System {
    /// `ZIP_ET_NONE`: nothing.
    None,
    /// `ZIP_ET_SYS`: an `errno`.
    Errno,
    /// `ZIP_ET_ZLIB`: zlib's return code.
    Zlib,
    /// `ZIP_ET_LIBZIP`: one of libzip's details.
    Libzip,
}

impl Er {
    /// The code as `ZipArchive::$status` and `ZipArchive::open()` give it.
    pub(crate) fn code(self) -> i64 {
        i64::from(self as i32)
    }

    /// `_zip_err_str`'s row, as libzip's build generates it from `zip.h`'s
    /// comments: the kind of the code's system half and its sentence.
    fn row(self) -> (System, &'static str) {
        match self {
            Er::Ok => (System::None, "No error"),
            Er::Multidisk => (System::None, "Multi-disk zip archives not supported"),
            Er::Rename => (System::Errno, "Renaming temporary file failed"),
            Er::Seek => (System::Errno, "Seek error"),
            Er::Read => (System::Errno, "Read error"),
            Er::Write => (System::Errno, "Write error"),
            Er::Crc => (System::None, "CRC error"),
            Er::Zipclosed => (System::None, "Containing zip archive was closed"),
            Er::Noent => (System::None, "No such file"),
            Er::Exists => (System::None, "File already exists"),
            Er::Open => (System::Errno, "Can't open file"),
            Er::Tmpopen => (System::Errno, "Failure to create temporary file"),
            Er::Zlib => (System::Zlib, "Zlib error"),
            Er::Memory => (System::None, "Malloc failure"),
            Er::Changed => (System::None, "Entry has been changed"),
            Er::Compnotsupp => (System::None, "Compression method not supported"),
            Er::Eof => (System::None, "Premature end of file"),
            Er::Inval => (System::None, "Invalid argument"),
            Er::Nozip => (System::None, "Not a zip archive"),
            Er::Internal => (System::None, "Internal error"),
            Er::Incons => (System::Libzip, "Zip archive inconsistent"),
            Er::Remove => (System::Errno, "Can't remove file"),
            Er::Deleted => (System::None, "Entry has been deleted"),
            Er::Encrnotsupp => (System::None, "Encryption method not supported"),
            Er::Rdonly => (System::None, "Read-only archive"),
            Er::Nopasswd => (System::None, "No password provided"),
            Er::Wrongpasswd => (System::None, "Wrong password provided"),
            Er::Opnotsupp => (System::None, "Operation not supported"),
            Er::Inuse => (System::None, "Resource still in use"),
            Er::CompressedData => (System::None, "Compressed data invalid"),
            Er::Cancelled => (System::None, "Operation cancelled"),
            Er::DataLength => (System::None, "Unexpected length of data"),
            Er::NotAllowed => (System::None, "Not allowed in torrentzip"),
            Er::TruncatedZip => (System::None, "Possibly truncated or corrupted zip archive"),
            Er::EfTooLarge => (System::None, "Extra fields too large"),
        }
    }
}

/// The system half of a libzip error, `zip_error_t`'s `sys_err` decoded.
/// `None` is libzip's 0, which a kind that reads a number reads as 0.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum Sys {
    #[default]
    None,
    /// An `errno`.
    Errno(i32),
    /// zlib's return code.
    Zlib(i32),
    /// One of libzip's details of `ZIP_ER_INCONS`, and where.
    Detail(Detail, At),
}

/// The entry a detail names: `MAKE_DETAIL_WITH_INDEX`'s index, or
/// `MAX_DETAIL_INDEX`, which names none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum At {
    Entry(u32),
    Unnamed,
}

impl At {
    /// Entry `i`, or none where `i` reaches `MAX_DETAIL_INDEX`.
    fn entry(i: usize) -> At {
        u32::try_from(i)
            .ok()
            .filter(|i| *i < MAX_DETAIL_INDEX)
            .map_or(At::Unnamed, At::Entry)
    }

    /// The index as `MAKE_DETAIL_WITH_INDEX` packs it.
    fn index(self) -> u32 {
        match self {
            At::Entry(i) => i,
            At::Unnamed => MAX_DETAIL_INDEX,
        }
    }
}

/// `zip_error_t`: the code and its system half.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Status {
    pub(crate) er: Er,
    /// Private, so a status is built by the constructors below, each
    /// giving its code the half its kind reads.
    sys: Sys,
}

impl Status {
    /// Code `er` with nothing beside it.
    pub(crate) fn new(er: Er) -> Status {
        Status { er, sys: Sys::None }
    }

    /// Code `er` beside `errno`.
    pub(crate) fn errno(er: Er, errno: i32) -> Status {
        Status {
            er,
            sys: Sys::Errno(errno),
        }
    }

    /// Code `er` beside the `errno` of `e`.
    pub(crate) fn os(er: Er, e: &std::io::Error) -> Status {
        Status::errno(er, e.raw_os_error().unwrap_or(0))
    }

    /// `ZIP_ER_ZLIB` beside zlib's return `code`.
    pub(crate) fn zlib(code: i32) -> Status {
        Status {
            er: Er::Zlib,
            sys: Sys::Zlib(code),
        }
    }

    /// `zip_error_set(error, er, 0)`.
    pub(crate) fn set(&mut self, er: Er) {
        *self = Status::new(er);
    }

    /// `zip_error_set(ZIP_ER_INCONS, MAKE_DETAIL_WITH_INDEX(detail,
    /// entry))`: an inconsistency libzip names, at the entry it names or,
    /// with `None`, at `MAX_DETAIL_INDEX`, which names none.
    pub(crate) fn incons(detail: Detail, entry: Option<usize>) -> Status {
        Status {
            er: Er::Incons,
            sys: Sys::Detail(detail, entry.map_or(At::Unnamed, At::entry)),
        }
    }

    /// `zip_error_set(ZIP_ER_INCONS, detail)`, the detail with no index
    /// made, which libzip's text reads as at entry 0.
    pub(crate) fn bare(detail: Detail) -> Status {
        Status {
            er: Er::Incons,
            sys: Sys::Detail(detail, At::Entry(0)),
        }
    }

    /// `ADD_INDEX_TO_DETAIL(detail, entry)` on an inconsistency, which
    /// `_zip_read_cdir` and `_zip_checkcons` make of what
    /// `_zip_dirent_read` set; any other error as it stands.
    pub(crate) fn at_entry(self, entry: usize) -> Status {
        match self.sys {
            Sys::Detail(detail, _) => Status {
                sys: Sys::Detail(detail, At::entry(entry)),
                ..self
            },
            _ => self,
        }
    }

    /// `zip_error_code_system`: the system half as `ZipArchive::$statusSys`
    /// gives it.
    pub(crate) fn sys_code(self) -> i64 {
        i64::from(match self.sys {
            Sys::None => 0,
            Sys::Errno(n) | Sys::Zlib(n) => n,
            Sys::Detail(detail, at) => {
                let packed = (at.index() << 8) | detail as u32;
                i32::try_from(packed).unwrap_or(i32::MAX)
            }
        })
    }

    /// `zip_error_strerror`: the sentence, then `: ` and the `errno`'s
    /// text, zlib's `zError`, or libzip's detail, for a code whose kind
    /// has one.
    pub(crate) fn text(self, req: &Request) -> String {
        let (kind, sentence) = self.er.row();
        let number = match self.sys {
            Sys::Errno(n) | Sys::Zlib(n) => n,
            Sys::None | Sys::Detail(..) => 0,
        };
        match (kind, self.sys) {
            (System::Errno, _) => {
                format!("{sentence}: {}", req.environment().os_error_text(number))
            }
            (System::Zlib, _) => format!("{sentence}: {}", z_error(number)),
            (System::Libzip, Sys::Detail(detail, at)) => {
                let (scope, description) = detail.row();
                match (scope, at) {
                    (Scope::Entry, At::Entry(i)) => format!("{sentence}: entry {i}: {description}"),
                    _ => format!("{sentence}: {description}"),
                }
            }
            (System::None | System::Libzip, _) => sentence.to_owned(),
        }
    }
}

/// `ZIP_ER_DETAIL_*`: the details of `ZIP_ER_INCONS` this crate sets, by
/// libzip's number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// What a detail is of, `ZIP_DETAIL_ET_GLOBAL` or `ZIP_DETAIL_ET_ENTRY`.
#[derive(Clone, Copy)]
enum Scope {
    Archive,
    Entry,
}

impl Detail {
    /// `_zip_err_details`' row, as libzip's build generates it from
    /// `zipint.h`'s comments: what the detail is of, and its description.
    fn row(self) -> (Scope, &'static str) {
        use Scope::{Archive, Entry};
        match self {
            Detail::CdirOverlapsEocd => (
                Archive,
                "central directory overlaps EOCD, or there is space between them",
            ),
            Detail::CommentLengthInvalid => (Archive, "archive comment length incorrect"),
            Detail::CdirLengthInvalid => (Archive, "central directory length invalid"),
            Detail::CdirEntryInvalid => (Entry, "central header invalid"),
            Detail::CdirWrongEntriesCount => {
                (Archive, "central directory count of entries is incorrect")
            }
            Detail::EntryHeaderMismatch => (Entry, "local and central headers do not match"),
            Detail::EocdLengthInvalid => (Archive, "wrong EOCD length"),
            Detail::Eocd64OverlapsEocd => (
                Archive,
                "EOCD64 overlaps EOCD, or there is space between them",
            ),
            Detail::Eocd64WrongMagic => (Archive, "EOCD64 magic incorrect"),
            Detail::Eocd64Mismatch => (Archive, "EOCD64 and EOCD do not match"),
            Detail::CdirInvalid => (Archive, "invalid value in central directory"),
            Detail::VariableSizeOverflow => (Entry, "variable size fields overflow header"),
            Detail::InvalidUtf8InFilename => (Entry, "invalid UTF-8 in filename"),
            Detail::InvalidUtf8InComment => (Entry, "invalid UTF-8 in comment"),
            Detail::InvalidZip64Ef => (Entry, "invalid Zip64 extra field"),
            Detail::InvalidWinzipaesEf => (Entry, "invalid WinZip AES extra field"),
            Detail::EfTrailingGarbage => (Entry, "garbage at end of extra fields"),
            Detail::InvalidEfLength => (Entry, "extra field length is invalid"),
            Detail::InvalidFileLength => (
                Entry,
                "file length in header doesn't match actual file length",
            ),
            Detail::StoredSizeMismatch => (
                Entry,
                "compressed and uncompressed sizes don't match for stored file",
            ),
            Detail::DataDescriptorMismatch => {
                (Entry, "local header and data descriptor do not match")
            }
            Detail::Eocd64LocatorMismatch => (Archive, "EOCD64 and EOCD64 locator do not match"),
            Detail::Utf8FilenameMismatch => {
                (Entry, "UTF-8 filename is ASCII and doesn't match filename")
            }
            Detail::Utf8CommentMismatch => {
                (Entry, "UTF-8 comment is ASCII and doesn't match comment")
            }
            Detail::NulInFilename => (Entry, "NUL byte in file name"),
            Detail::MissingZip64Ef => (Entry, "missing Zip64 extra field"),
        }
    }
}

/// `MAX_DETAIL_INDEX`, the index of a detail that names no entry.
const MAX_DETAIL_INDEX: u32 = 0x7f_ffff;

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
