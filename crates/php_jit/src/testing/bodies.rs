//! Trace bodies assembled by hand for the one target whose arena maps
//! code today, macOS aarch64: what `enter`'s and `dispatch`'s tests enter
//! in place of what `lower` makes, so each asserts the crossing or the
//! trace-to-trace loop alone.

use std::mem::offset_of;

use php_unit::OplineIdx;

use crate::lower::{Compiled, Fuzzed, Stood};
use crate::table::TraceName;
use crate::types::{ExitSpec, Frame, Grows, Landing};

/// aarch64, encoded by hand: a body keeps a 48-byte frame, the
/// `Stood` a kernel writes at `sp + 16`.
pub(crate) mod a64 {
    /// `stp x29, x30, [sp, #-48]!; mov x29, sp`.
    pub(crate) const PROLOGUE: [u32; 2] = [0xA9BD_7BFD, 0x9100_03FD];
    /// `ldp x29, x30, [sp], #48; ret`.
    pub(crate) const EPILOGUE: [u32; 2] = [0xA8C3_7BFD, 0xD65F_03C0];
    /// `blr x16`.
    pub(crate) const CALL: u32 = 0xD63F_0200;
    /// `movn w0, #0`: `lower::ABANDONED`.
    pub(crate) const ABANDONED: u32 = 0x1280_0000;
    /// Where the `Stood` a kernel writes stands in the frame.
    pub(crate) const STOOD: u32 = 16;

    /// `movz w<r>, #imm`.
    pub(crate) fn movz(r: u32, imm: u16) -> u32 {
        0x5280_0000 | u32::from(imm) << 5 | r
    }

    /// `w<r>` set to `value`: `movz`, and `movk #hi, lsl #16`.
    pub(crate) fn word(r: u32, value: u32) -> [u32; 2] {
        let [lo, hi] = [value & 0xFFFF, value >> 16];
        [0x5280_0000 | lo << 5 | r, 0x72A0_0000 | hi << 5 | r]
    }

    /// `x16` set to `address`, sixteen bits at a time.
    pub(crate) fn x16(address: usize) -> [u32; 4] {
        let a = address as u64;
        let part = |n: u32| u32::try_from((a >> (16 * n)) & 0xFFFF).unwrap_or(0) << 5;
        [
            0xD280_0010 | part(0),
            0xF2A0_0010 | part(1),
            0xF2C0_0010 | part(2),
            0xF2E0_0010 | part(3),
        ]
    }

    /// `add x<r>, sp, #imm`.
    pub(crate) fn sp_plus(r: u32, imm: u32) -> u32 {
        0x9100_03E0 | imm << 10 | r
    }

    /// `ldr w0, [sp, #at]`.
    pub(crate) fn load_w0(at: u32) -> u32 {
        0xB940_03E0 | (at / 4) << 10
    }

    /// `ldr x0, [sp, #at]`.
    pub(crate) fn load_x0(at: u32) -> u32 {
        0xF940_03E0 | (at / 8) << 10
    }

    /// `cbnz w0`, over the next `skip` instructions.
    pub(crate) fn unless_zero(skip: u32) -> u32 {
        0x3500_0000 | (skip + 1) << 5
    }

    /// `cbz x0`, over the next `skip` instructions.
    pub(crate) fn if_null(skip: u32) -> u32 {
        0xB400_0000 | (skip + 1) << 5
    }

    /// `b`, over the next `skip` instructions.
    pub(crate) fn over(skip: u32) -> u32 {
        0x1400_0000 | (skip + 1)
    }
}

// The offsets the bodies read, held to `Stood`'s layout.
pub(crate) const STOOD_CALL: u32 = a64::STOOD + 8;
pub(crate) const STOOD_OPLINE: u32 = a64::STOOD + 20;
const _: () = assert!(offset_of!(Stood, call) == 8 && offset_of!(Stood, opline) == 20);

/// The head of a body that calls the wrapper at `kernel` as compiled
/// code does, with the `ExitCtx` and the room it was entered with,
/// opline `at`, the words `args` and, where it has one, the `Stood`
/// in its frame as its out pointer.
pub(crate) fn calling(kernel: usize, at: u32, args: &[u32], out: bool) -> Vec<u32> {
    let mut words = a64::PROLOGUE.to_vec();
    words.extend(a64::word(2, at));
    for (r, value) in (3..).zip(args) {
        words.extend(a64::word(r, *value));
    }
    if out {
        let r = 3 + u32::try_from(args.len()).unwrap_or(0);
        words.push(a64::sp_plus(r, a64::STOOD));
    }
    words.extend(a64::x16(kernel));
    words.push(a64::CALL);
    words
}

/// `head`, then exit `exit` where the kernel answered `Continue` and
/// `ABANDONED` where it did not.
pub(crate) fn answering(mut head: Vec<u32>, exit: u16) -> Vec<u32> {
    head.extend([
        a64::unless_zero(2),
        a64::movz(0, exit),
        a64::over(1),
        a64::ABANDONED,
    ]);
    head.extend(a64::EPILOGUE);
    head
}

/// `words` as the bytes of a trace with `exits`.
pub(crate) fn compiled(words: &[u32], exits: Vec<ExitSpec>) -> Compiled {
    Compiled {
        bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        entry_offset: 0,
        align: 4,
        exits: exits.into(),
        leaves: None,
        stub: None,
        opens: None,
        ip_guard: false,
        body: None,
        fuzzed: Fuzzed::default(),
        map_ptrs: Box::new([]),
        marks: Box::new([]),
        name: TraceName {
            at: b"hand".to_vec().into_boxed_slice(),
            line: 1,
        },
    }
}

/// An exit that lands at `opline` of the frame the loop goes on in.
pub(crate) fn landing(opline: u32, landing: Landing) -> ExitSpec {
    ExitSpec {
        frame: Frame::ROOT,
        opline: Some(OplineIdx(opline)),
        landing,
        stores: Vec::new(),
        known: Vec::new(),
        grows: Grows::Side,
        link: None,
    }
}

/// The numbers of the traces that called `enter`'s test kernel `number`,
/// in the order they called it: the request's, for `dispatch`'s tests.
#[derive(Debug, Default)]
pub(crate) struct Numbered(pub(crate) Vec<u32>);
