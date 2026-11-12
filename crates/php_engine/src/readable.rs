//! The frames another process reads (design §5.6, revision 59): php's
//! `EG(current_execute_data)` chain as `fpm_php_trace_dump` walks it
//! out of a stopped child, reading bytes and never a Rust value.
//!
//! Each frame's room starts with the VM stack's link, the address of the
//! room pushed before it, then the engine's header ([`exec::Header`]),
//! whose `#[repr(C)]` prefix a reader takes at fixed offsets: the
//! address of a [`Record`], php's `EX(func)`, one word; the opline
//! the frame last saved, php's `EX(opline)`, as its place among the op
//! array's oplines; and the frame's flags, php's call info. A record
//! is php's `zend_function` as the slow log reads it, the name and, for
//! user code, the file and the oplines, each as an address and a length
//! into what the unit or the linked tables hold, so the line is read as
//! php reads `opline->lineno`. The word the stack keeps names the
//! innermost room; a host publishes its address once per request
//! ([`Innermost`]).
//!
//! What crosses to the host is the decoding alone ([`Room::read`],
//! [`Callee::read`], [`Code::lineno_at`]): the walk is the host's, as
//! `fpm_php_trace_dump` is php-fpm's.
//!
//! [`exec::Header`]: crate::exec::Header

use php_unit::{OpArray, Opline};

/// The address of what a frame serves as another process reads it,
/// php's `EX(func)`: a [`Record`]'s, or zero where the frame has
/// none. Every push writes it and nothing in the engine reads it back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CalleeAt(usize);

impl CalleeAt {
    /// Whether no record is named yet.
    pub(crate) fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// php's `type` of a `zend_function`: `ZEND_INTERNAL_FUNCTION`.
const NATIVE: u64 = 1;
/// `ZEND_USER_FUNCTION`: an op array, a function's or a file's.
const USER: u64 = 2;

/// php's `zend_function` as the slow log reads it: its `type`, its
/// `common.function_name`, and for user code its `op_array.filename`
/// and `op_array.opcodes`, each an address and a length into bytes the
/// function's unit or the engine's linked tables hold for as long as
/// the record stands. The name is empty for a file's op array,
/// php's `NULL` `function_name`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Record {
    kind: u64,
    name: usize,
    name_len: usize,
    file: usize,
    file_len: usize,
    oplines: usize,
    oplines_len: usize,
}

impl Record {
    /// A native's: its name and no code.
    pub(crate) fn native(name: &[u8]) -> Record {
        Record {
            kind: NATIVE,
            name: name.as_ptr().expose_provenance(),
            name_len: name.len(),
            ..Record::default()
        }
    }

    /// An op array's under `name`, empty for a file's: its file and its
    /// oplines.
    pub(crate) fn user(name: &[u8], code: &OpArray) -> Record {
        Record {
            kind: USER,
            name: name.as_ptr().expose_provenance(),
            name_len: name.len(),
            file: code.filename.0.as_ptr().expose_provenance(),
            file_len: code.filename.0.len(),
            oplines: code.oplines.as_ptr().expose_provenance(),
            oplines_len: code.oplines.len(),
        }
    }

    /// Where this record stands, which a frame's header names; the
    /// record must stand there while a frame names it.
    pub(crate) fn at(&self) -> CalleeAt {
        CalleeAt(std::ptr::from_ref(self).expose_provenance())
    }
}

/// A record and the name it points into, boxed so neither moves:
/// what a function no table holds a record for is named by, made the
/// first time a frame serves it.
#[derive(Debug)]
pub(crate) struct Named {
    name: Box<[u8]>,
    record: Record,
}

impl Named {
    /// A native's under `name`, the record pointing into the name's
    /// own allocation, which moves with nothing.
    pub(crate) fn native(name: Vec<u8>) -> Named {
        let name = name.into_boxed_slice();
        let record = Record::native(&name);
        Named { name, record }
    }

    /// Where the record stands.
    pub(crate) fn at(&self) -> CalleeAt {
        debug_assert_eq!(self.record.name, self.name.as_ptr().addr());
        self.record.at()
    }
}

/// Where a request's chain starts, `&EG(current_execute_data)`: the
/// address of the word that names the innermost room, which stands
/// where it is from the request's start to its end. A host hands it to
/// whoever reads the request's frames from another process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Innermost(u64);

impl Innermost {
    pub(crate) fn of(word: usize) -> Innermost {
        Innermost(word as u64)
    }

    /// The word's address in the serving process.
    #[must_use]
    pub fn address(self) -> u64 {
        self.0
    }
}

/// Where a room stands on php's chain, read off its header's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// On it: a user frame, a file's included or evaluated, or a
    /// native's call being served.
    Frame,
    /// php's `ZEND_CALL_TOP_CODE`: the main op array's frame, where
    /// `fpm_php_trace_dump` stops.
    Top,
    /// Off it, as php's `prev_execute_data` passes it: a call still being
    /// built, or a frame being popped.
    Passed,
}

/// One room read back from its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    /// The room linked before it, `prev_execute_data`; zero at the
    /// bottom.
    pub below: u64,
    /// Its [`Callee`]'s address, `EX(func)`; zero where it names none.
    pub callee: u64,
    /// The opline it last saved, by its place.
    pub opline: u32,
    pub standing: Standing,
}

/// A word as the serving process wrote it, which ran this binary.
fn word(bytes: &[u8], at: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_ne_bytes(raw))
}

/// A `u32` as the serving process wrote it.
fn half(bytes: &[u8], at: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_ne_bytes(raw))
}

impl Room {
    /// The bytes a room takes, php's `sizeof(zend_execute_data)`.
    pub const BYTES: usize = php_heap::ROOM_BYTES;

    /// The room `bytes` are, read from its address: none where fewer
    /// than [`Room::BYTES`] were read.
    #[must_use]
    pub fn read(bytes: &[u8]) -> Option<Room> {
        let header = php_heap::HEADER_AT;
        let info = crate::exec::CallInfo::of(half(bytes, header + crate::exec::Header::INFO_AT)?);
        let standing = if info.leaving() || info.building() {
            Standing::Passed
        } else if info.top_code() {
            Standing::Top
        } else {
            Standing::Frame
        };
        Some(Room {
            below: word(bytes, 0)?,
            callee: word(bytes, header + crate::exec::Header::CALLEE_AT)?,
            opline: half(bytes, header + crate::exec::Header::OPLINE_AT)?,
            standing,
        })
    }
}

/// A span of bytes in the serving process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub at: u64,
    pub len: u64,
}

/// A user function's or a file's code, as its record names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    /// `op_array.filename`.
    pub file: Span,
    oplines: Span,
}

impl Code {
    /// Where the line of opline `opline` stands, `&opline->lineno`; none
    /// past the op array's end.
    #[must_use]
    pub fn lineno_at(&self, opline: u32) -> Option<u64> {
        if u64::from(opline) >= self.oplines.len {
            return None;
        }
        let size = std::mem::size_of::<Opline>() as u64;
        let lineno = std::mem::offset_of!(Opline, lineno) as u64;
        self.oplines
            .at
            .checked_add(u64::from(opline).checked_mul(size)?)?
            .checked_add(lineno)
    }
}

/// What a frame serves, read back from its record's bytes: php's
/// `function_name`, empty for a file's op array, and the code where it
/// is user code (`ZEND_USER_CODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Callee {
    pub name: Span,
    pub code: Option<Code>,
}

impl Callee {
    /// The bytes a record takes.
    pub const BYTES: usize = std::mem::size_of::<Record>();

    /// The record `bytes` are; none where fewer than
    /// [`Callee::BYTES`] were read.
    #[must_use]
    pub fn read(bytes: &[u8]) -> Option<Callee> {
        let field = |at: usize| word(bytes, at);
        let kind = field(std::mem::offset_of!(Record, kind))?;
        let name = Span {
            at: field(std::mem::offset_of!(Record, name))?,
            len: field(std::mem::offset_of!(Record, name_len))?,
        };
        let code = (kind == USER)
            .then(|| {
                Some(Code {
                    file: Span {
                        at: field(std::mem::offset_of!(Record, file))?,
                        len: field(std::mem::offset_of!(Record, file_len))?,
                    },
                    oplines: Span {
                        at: field(std::mem::offset_of!(Record, oplines))?,
                        len: field(std::mem::offset_of!(Record, oplines_len))?,
                    },
                })
            })
            .flatten();
        Some(Callee { name, code })
    }
}
