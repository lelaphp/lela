//! What becomes of a [`TraceIr`] (`jit-design.md` §4.3): the seam is the
//! value, and two modules read it. [`dump`] renders it as text, which
//! `types` is held to before code is made; [`Backend`] compiles it to
//! final bytes over Cranelift, a [`Compiled`], which `table` publishes.
//! Nothing but `lower` names a Cranelift type, so a Cranelift bump is a
//! change here alone.
//!
//! **What compiled code is.** A trace is one function,
//! `extern "C" fn(cx: *mut ExitCtx, frame: *mut u8) -> u32`, entered with
//! the crossing's `ExitCtx` and the room of the frame it was recorded in,
//! php's `execute_data`, and answering the [`ExitId`] it left through.
//! It runs on the interpreter's frames: a slot is a load and a store at
//! `room + ROOM_BYTES + 16 * var` with the zval layout `php_heap` defines,
//! so an exit leaves every frame as the interpreter reads it once it has
//! stored the scalars it kept in registers with their type words. What
//! compiled code does to the frame chain, and to anything the engine
//! keeps behind its frames, is a call of a kernel through the one
//! emitter; [`Kernel`] lists them and [`Symbols`] holds their addresses.
//! Every kernel answers the crossing's status word, zero to go on, and on
//! anything else compiled code leaves through the opline's exit where an
//! inline form has one, or through its unwind, which stores the
//! registers and answers [`UNWOUND`], taking no exit, or, where the
//! opline names neither, answers [`ABANDONED`], for `enter` to read what
//! the kernel kept.
//!
//! **The interrupt test** (`jit-design.md` §4.3). A trace that loops loads
//! the interrupt pointer at offset [`INTERRUPT_AT`] of its `ExitCtx` once
//! as it enters, and at its header loads the byte it points to and leaves
//! through the header's interrupt exit when it is not zero: one load and
//! one branch per iteration, php's cost (`rfc-jit.md:118`).
//!
//! [`ExitId`]: crate::types::ExitId

mod clif;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the goldens' text; no host prints a trace's IR yet"
    )
)]
pub(crate) mod dump;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::SourceLoc;
use cranelift_codegen::isa::{self, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, Final, MachSrcLoc};
use php_engine::tier::{FuncRef, MapPtrRaw};
use php_unit::OplineIdx;

use crate::policy::{JitDebug, Mode, Policy};
use crate::record::Stop;
use crate::table::TraceName;
use crate::types::{Abort, End, ExitId, ExitSpec, Interrupt, Op, TraceIr};

/// A trace in final bytes (`jit-design.md` §4.3), what the `Backend` makes
/// and `table::TraceTable::publish` copies into the code arena once:
/// the bytes, where they are entered, the alignment their first byte
/// needs, one [`ExitSpec`] per exit in [`ExitId`](crate::types::ExitId)
/// order, the runtime-cache slots the code reads, where each recorded
/// opline's code begins, and what a profiler names the trace by.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub(crate) bytes: Vec<u8>,
    /// The offset in `bytes` the trace is entered at.
    pub(crate) entry_offset: usize,
    /// A power of two.
    pub(crate) align: usize,
    pub(crate) exits: Box<[ExitSpec]>,
    /// The exit a trace that does not loop ends through, none for a loop:
    /// php's return to the interpreter at a trace's end
    /// (`zend_jit_trace_return`), which no side trace grows from.
    pub(crate) leaves: Option<ExitId>,
    /// Where a loop whose interrupt test takes php's stub stands its frame
    /// once it answers [`INTERRUPTED`]: its header's opline. None for a
    /// loop whose test takes an exit and for a trace that does not loop.
    pub(crate) stub: Option<OplineIdx>,
    /// The op array and opline the trace's first recorded opline stands
    /// at, where a frame entering it stands.
    pub(crate) opens: Option<(FuncRef, OplineIdx)>,
    /// Where a loop's trace is entered at its body too: the opline its
    /// back edge lands on (`types::BodyEntry`).
    pub(crate) body: Option<OplineIdx>,
    /// The slots of the requests' runtime caches the code reads, which
    /// `enter` lays down on a request's first entry.
    pub(crate) map_ptrs: Box<[MapPtrRaw]>,
    pub(crate) marks: Box<[Mark]>,
    /// php's `zend_jit_trace_name` but the number, which the table
    /// assigns: the job's, named where the trace's `TRACE start` line is.
    pub(crate) name: TraceName,
    /// Under `jit.mode=exit-fuzz`, the byte per guard the code marks as
    /// the guard fails once, once per trace and so per process; the code
    /// holds their address, so they live as long as it. Empty otherwise.
    pub(crate) fuzzed: Fuzzed,
}

/// The bytes compiled code reads and writes under `jit.mode=exit-fuzz`
/// (`jit-design.md` §4.6, §7): one per guard, zero until the guard has
/// failed once, at an address the code holds as a constant, so a clone
/// shares them as it shares the bytes that name them.
///
/// A loop whose guards each fail once, the interrupt test's among them
/// and its jump's direction not (#3055's ruling), lands in the
/// interpreter at each and prints what php prints:
///
/// ```php
/// $a = [3, 1, 4, 1, 5]; $s = 0;
/// for ($i = 0; $i < 100; $i++) { $s += $a[$i % 5] * $i; }
/// echo $s, "\n";
/// ```
/// ```expect exit=0 regime=jit-exit-fuzz process
/// 13940
/// ```
#[derive(Debug, Clone, Default)]
pub(crate) struct Fuzzed(Arc<[AtomicU8]>);

impl Fuzzed {
    /// `n` bytes, none failed yet.
    fn new(n: usize) -> Fuzzed {
        Fuzzed((0..n).map(|_| AtomicU8::new(0)).collect())
    }

    /// The bytes as the lowering hands them out.
    fn fuzz(&self) -> clif::Fuzz {
        clif::Fuzz {
            base: self.0.as_ptr().addr(),
            len: self.0.len(),
        }
    }
}

/// Where one recorded opline's code begins: the op array, named with
/// its function since a trace inlines calls, the opline, and the offset
/// in the trace's bytes. What a profiler and a disassembly read; it costs
/// nothing in the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) func: FuncRef,
    pub(crate) opline: OplineIdx,
    pub(crate) offset: u32,
}

/// Why a host compiles no trace (`jit-design.md` §4.3): Cranelift has no
/// ISA for it, or none this build carries, or it is not a 64-bit
/// host. The tier then runs with `Mode::Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unsupported(pub(crate) &'static str);

/// What compiled code answers where a kernel answered other than zero
/// and the opline that called it names no exit: past every
/// [`ExitId`](crate::types::ExitId), which is sixteen bits. `enter` reads
/// what the kernel kept in the `ExitCtx`, a fatal or a panic to resume.
pub(crate) const ABANDONED: u32 = u32::MAX;

/// What compiled code answers where a kernel threw, bailed out or
/// panicked and the trace stored what it keeps in registers, taking no
/// exit (`types::Unwind`): past every [`ExitId`](crate::types::ExitId),
/// and not [`ABANDONED`]. `enter` reads what the kernel kept, and a
/// throwable pending on the request is unwound from where it was raised.
pub(crate) const UNWOUND: u32 = u32::MAX - 1;

/// What compiled code answers where a loop's interrupt test takes php's
/// interrupt stub, `jit_stub_interrupt_handler`, and no exit
/// (`types::Interrupt::Stub`): past every
/// [`ExitId`](crate::types::ExitId), and neither [`ABANDONED`] nor
/// [`UNWOUND`]. Nothing is stored; `enter` stands the frame at the loop's
/// header, [`Compiled::stub`], for `dispatch` to make the loop's
/// interrupt check and enter the trace again there.
pub(crate) const INTERRUPTED: u32 = u32::MAX - 2;

/// What compiled code answers where a loop's trace entered at its body
/// (`types::BodyEntry`) finds the frame holding other than it knows:
/// past every [`ExitId`](crate::types::ExitId), and none of
/// [`ABANDONED`], [`UNWOUND`] and [`INTERRUPTED`]. Nothing ran and
/// nothing is stored.
pub(crate) const DECLINED: u32 = u32::MAX - 3;

/// What compiled code answers where a return past the trace's lowest
/// frame finds the interpreter standing elsewhere than the trace goes on
/// at (`types::Op::Back`'s `escapes`): php's `jit_stub_trace_escape`,
/// which no exit point numbers, so no exit line is written and no word is
/// bumped. Past every [`ExitId`](crate::types::ExitId), and none of
/// [`ABANDONED`], [`UNWOUND`], [`INTERRUPTED`] and [`DECLINED`]. Nothing is
/// stored, since the trace knows nothing of the frame it returned into;
/// `enter` hands back the frame that stands, at the opline the return
/// left it, whose handler `dispatch` runs as php's interpreter does.
pub(crate) const ESCAPED: u32 = u32::MAX - 4;

/// Where compiled code reads, in the `ExitCtx`, where it is entered
/// (`enter::At`), a `u32` that is not zero at a loop's body
/// (`types::BodyEntry`): the field `enter` lays down past the interrupt
/// pointer.
pub(crate) const ENTRY_AT: i32 = 16;

/// Where compiled code reads the interrupt pointer in the `ExitCtx`
/// (`jit-design.md` §4.6): the request at offset zero, the pointer to its
/// interrupt byte at offset eight, the field `enter` lays down with the
/// first trace it enters (#2957).
pub(crate) const INTERRUPT_AT: i32 = 8;

/// Where the trace's first recorded opline stands.
fn opens_at(ir: &TraceIr) -> Option<(FuncRef, OplineIdx)> {
    ir.ops.iter().find_map(|op| match op {
        Op::Line(line) => {
            let frame = ir.frames.get(usize::from(line.frame.0))?;
            Some((frame.func, line.opline))
        }
        _ => None,
    })
}

/// The opline a loop whose interrupt test takes php's stub stands its
/// frame at, its header's.
fn stub_at(ir: &TraceIr) -> Option<OplineIdx> {
    let End::Loop(header) = ir.end else {
        return None;
    };
    match header.interrupt {
        Interrupt::Stub(at) => Some(at),
        Interrupt::Exit(_) => None,
    }
}

/// A kernel compiled code calls (`jit-design.md` §4.6, §5.2), by the
/// wrapper `enter`'s `kernels!` list makes of it. Every wrapper is
/// `extern "C"` on the host's convention, takes the `ExitCtx`, the room
/// of the frame the opline runs in and the opline's index first, and
/// answers the crossing's status word; where it writes a value, the
/// pointer it writes through is its last argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kernel {
    /// `step(cx, frame, opline: u32, func: u32, stood: *mut Stood)`: the
    /// opline served whole through `php_engine::tier::step_held` (§5.2)
    /// over the op array the engine numbers `func`, the one the trace
    /// recorded, and where the interpreter stands after it written to
    /// `stood`.
    Step,
    /// `push_call(cx, frame, opline: u32, func: u32, callee: u32, stood:
    /// *mut Stood)`: an inline `INIT_*` of the op array the engine numbers
    /// `func`, served as its handler serves it where
    /// its callee resolves to the op array the engine numbers `callee`;
    /// where it resolves to another, nothing is pushed and `stood.call`
    /// is null.
    PushCall,
    /// `call(cx, frame, opline: u32, func: u32, callee: u32, stood: *mut
    /// Stood)`: a `DO_*CALL` of the op array the engine numbers `func`,
    /// to the callee the trace recorded, which it numbers `callee`, made
    /// through `php_engine::tier::call_held` with no dispatch, and where
    /// the interpreter stands after it, in the callee, written to `stood`.
    Call,
    /// `release(cx, frame, opline: u32, word: u64, type_info: u32)`:
    /// php's `zval_ptr_dtor` of the counted value the zval names, a
    /// destructor's throw answered as one.
    Release,
    /// `copy(cx, frame, opline: u32, word: u64, type_info: u32, out: *mut
    /// Zval)`: php's `ZVAL_COPY_DEREF` of the value the zval names into
    /// `out`.
    Copy,
}

impl Kernel {
    /// Every kernel, in the order [`Symbols`] keeps them.
    pub(crate) const ALL: [Kernel; 5] = [
        Kernel::Step,
        Kernel::PushCall,
        Kernel::Call,
        Kernel::Release,
        Kernel::Copy,
    ];

    /// The kernel's place in [`Kernel::ALL`].
    const fn index(self) -> usize {
        self as usize
    }
}

/// The kernel symbol table (`jit-design.md` §4.3): the address of each
/// kernel's wrapper, which compiled code calls as a 64-bit constant, so
/// its bytes are final. `enter`'s `kernels!` list makes the one the
/// engine's `Backend` is built with (#2957).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Symbols([usize; Kernel::ALL.len()]);

impl Symbols {
    /// The table of the addresses `address` names, one per kernel.
    pub(crate) fn new(address: impl Fn(Kernel) -> usize) -> Symbols {
        Symbols(Kernel::ALL.map(address))
    }

    /// The address of `kernel`'s wrapper.
    pub(crate) fn address(&self, kernel: Kernel) -> usize {
        self.0[kernel.index()]
    }
}

/// Where the interpreter stands after a kernel served an opline whole
/// ([`Kernel::Step`], [`Kernel::PushCall`]): the room of the frame that
/// runs, php's `EX`; the room of the call under way in it, `EX(call)`,
/// null where none is; the op array it serves, by the engine offset that
/// numbers it (`FuncRef`); and the opline it goes on at. A local of
/// compiled code the kernel writes, by which compiled code follows the
/// frame chain: the call it enters, the return it lands from, the call
/// its sends write, the caller php's IP guard holds, and the way a
/// stepped jump went.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stood {
    pub(crate) frame: *mut u8,
    pub(crate) call: *mut u8,
    pub(crate) func: u32,
    pub(crate) opline: u32,
}

/// The host's `Backend` (`jit-design.md` §4.3): its ISA with the flags the
/// [`Policy`] asks for, and the kernel table compiled code calls. It holds
/// no state of one compile, so one serves every compile of the engine.
pub(crate) struct Backend {
    isa: OwnedTargetIsa,
    symbols: Symbols,
    /// Whether every guard fails once, `jit.mode=exit-fuzz`.
    fuzz: bool,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("isa", &self.isa.triple().to_string())
            .finish_non_exhaustive()
    }
}

// The worker compiles on its own thread with the engine's one backend
// (`jit-design.md` §4.3, §4.8), which the design left unverified.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Backend>();
};

/// How a host's ISA is found: `cranelift-native`'s lookup on a real host.
type Host = fn() -> Result<isa::Builder, &'static str>;

impl Backend {
    /// The host's `Backend`, its ISA found through `cranelift-native`, with the
    /// flags `policy` asks for, calling the kernels `symbols` names; or
    /// `Unsupported` where the host has none, which runs the tier with
    /// `Mode::Off` (`jit-design.md` §4).
    pub(crate) fn new(policy: &Policy, symbols: Symbols) -> Result<Backend, Unsupported> {
        Backend::on(cranelift_native::builder, policy, symbols)
    }

    /// The `Backend` of the ISA `host` finds.
    fn on(host: Host, policy: &Policy, symbols: Symbols) -> Result<Backend, Unsupported> {
        let builder = host().map_err(Unsupported)?;
        let isa = builder
            .finish(flags(policy)?)
            .map_err(|_| Unsupported("the host's ISA refused the tier's flags"))?;
        if isa.pointer_bits() != 64 {
            return Err(Unsupported("a 32-bit host"));
        }
        Ok(Backend {
            isa,
            symbols,
            fuzz: policy.mode() == Mode::ExitFuzz,
        })
    }

    /// `ir` in final bytes, named `name`: the function [`clif::lower`]
    /// builds, compiled for the host, with a [`Mark`] per recorded
    /// opline. Bytes that would need relocating are php's
    /// `COMPILER_ERROR`, as is any refusal of Cranelift's: kernel
    /// addresses are constants, so a trace's bytes work wherever the arena
    /// places them.
    pub(crate) fn compile(&self, ir: &TraceIr, name: TraceName) -> Result<Compiled, Abort> {
        let config = self.isa.frontend_config();
        // Under exit-fuzz a first pass counts the guards, one byte each.
        let fuzzed = if self.fuzz {
            let (_, sites) = clif::lower(ir, config, &self.symbols, Some(clif::Fuzz::COUNTING))?;
            Fuzzed::new(sites)
        } else {
            Fuzzed::default()
        };
        let fuzz = self.fuzz.then(|| fuzzed.fuzz());
        let (func, _) = clif::lower(ir, config, &self.symbols, fuzz)?;
        let mut ctx = Context::for_function(func);
        let code = ctx
            .compile(&*self.isa, &mut ControlPlane::default())
            .map_err(|_| refused("Cranelift refused the trace's function"))?;
        if !code.buffer.relocs().is_empty() {
            return Err(refused("the trace's code needs relocating"));
        }
        let bytes = code.code_buffer().to_vec();
        let align = usize::try_from(code.buffer.alignment.max(1))
            .map_err(|_| refused("an alignment past the host's"))?;
        let marks = marks(ir, code.buffer.get_srclocs_sorted(), bytes.len());
        Ok(Compiled {
            bytes,
            entry_offset: 0,
            align,
            exits: ir.exits.clone().into_boxed_slice(),
            leaves: match ir.end {
                End::Leave(exit) => Some(exit),
                End::Loop(_) => None,
            },
            stub: stub_at(ir),
            opens: opens_at(ir),
            body: ir.body.as_ref().map(|body| body.opline),
            map_ptrs: ir.map_ptrs.clone().into_boxed_slice(),
            marks,
            name,
            fuzzed,
        })
    }

    /// The function `ir` lowers to, before Cranelift's passes: the CLIF
    /// the goldens hold.
    #[cfg(test)]
    fn clif(&self, ir: &TraceIr) -> Result<cranelift_codegen::ir::Function, Abort> {
        clif::lower(ir, self.isa.frontend_config(), &self.symbols, None).map(|(func, _)| func)
    }
}

/// The ISA's flags (`jit-design.md` §4.3): optimised for speed, the
/// verifier where assertions are, and the frame pointers a profiler walks
/// where `opcache.jit_debug` asks for `PERF` or `PERF_DUMP`.
fn flags(policy: &Policy) -> Result<settings::Flags, Unsupported> {
    let mut flags = settings::builder();
    let mut set = |name: &str, value: &str| {
        flags
            .set(name, value)
            .map_err(|_| Unsupported("Cranelift lacks a flag the tier sets"))
    };
    set("opt_level", "speed")?;
    set(
        "enable_verifier",
        if cfg!(debug_assertions) {
            "true"
        } else {
            "false"
        },
    )?;
    if policy.debug().any(JitDebug::PERF | JitDebug::PERF_DUMP) {
        set("preserve_frame_pointers", "true")?;
    }
    Ok(settings::Flags::new(flags))
}

/// php's `COMPILER_ERROR`, for a trace Cranelift would not compile.
fn refused(why: &'static str) -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why,
    }
}

/// Where each recorded opline's code begins: the lowest offset of the
/// code its op left, and where Cranelift folded an op's code away, where
/// the next op's begins.
fn marks(ir: &TraceIr, srclocs: &[MachSrcLoc<Final>], len: usize) -> Box<[Mark]> {
    let end = u32::try_from(len).unwrap_or(u32::MAX);
    let mut starts = vec![None::<u32>; ir.ops.len()];
    for loc in srclocs {
        if loc.loc.is_default() {
            continue;
        }
        if let Some(start) = starts.get_mut(loc.loc.bits() as usize) {
            *start = Some(start.map_or(loc.start, |s| s.min(loc.start)));
        }
    }
    let mut next = end;
    for start in starts.iter_mut().rev() {
        next = *start.get_or_insert(next);
    }
    ir.ops
        .iter()
        .zip(starts)
        .filter_map(|(op, start)| match op {
            Op::Line(line) => Some(Mark {
                func: ir.frames.get(usize::from(line.frame.0))?.func,
                opline: line.opline,
                offset: start.unwrap_or(end),
            }),
            _ => None,
        })
        .collect()
}

/// The source location compiled code carries for the op at `at`, which
/// [`marks`] reads back.
fn located(at: usize) -> SourceLoc {
    SourceLoc::new(u32::try_from(at).unwrap_or(u32::MAX - 1))
}
