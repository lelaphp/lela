//! `xtask fold`: the sensor for a toolchain that loses LLVM's fold of a
//! request field's address into each access (design §10 Profiles).
//! In `serve_frame` a field of the request is reached through one
//! address computed from `%req`, which LLVM hoists to the function's entry
//! and shares among every handler laid in the loop; `CodeGenPrepare` folds
//! it back into each access, `ldr x8, [x19, #0x5c8]`, only while the
//! address has at most `--cgp-max-address-users-to-scan` users, which
//! every profile raises to 1000 through `.cargo/config.toml`. An LLVM
//! that keeps the option's name and changes its meaning builds and runs,
//! and the loop is slower by an instruction at every frame lookup; so the
//! release `php` is disassembled and each instantiation of `serve_frame`
//! is held to the fold.
//!
//! The shape held is matched by its form and never by an offset: an
//! `add xN, <req>, #C`, whose sum the next instruction that names `xN`
//! uses as the base of a load or store of one register at an immediate
//! offset that `[<req>, #C + offset]` could encode. `<req>` is the
//! register the prologue copies the first argument into, `mov xR,
//! x0`. An address computed for a call's argument, for a register-offset
//! access (`ldr x11, [x12, x11]`) or for a pair (`ldp`), whose offsets
//! reach no further than 504, is no such site. A call ends what a
//! register below `x19` held, and an unconditional branch does too, since
//! the next instruction in the listing is not the next one run; a
//! register from `x19` up is the callee's to keep, so it is followed
//! across both.
//!
//! The reading is at `sapi/cli`, the crate whose release `php` is held,
//! keyed over its closure and the files rustc is built under
//! ([`crate::key::BUILT_UNDER`]), and `done` asks for it fresh and passing
//! on a diff that changes one of those files (harness §4.5). The sensor
//! reads `llvm-objdump` on an `aarch64` Mac alone, the machines the bench runs on; elsewhere the
//! reading is a skip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::hosts::CLI;
use crate::oracle::profile;
use crate::reading::{Number, Reading, Status, Store};

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "fold";

/// The crate whose release binary is held, the reading's scope.
pub(crate) const SCOPE: &str = "sapi/cli";

/// What a mangled symbol of an instantiation of `serve_frame` holds:
/// `php_engine::tier::serve_frame` under the legacy mangling, before the
/// hash that tells one instantiation from another.
const INSTANTIATION: &str = "10php_engine4tier11serve_frame17h";

/// What one `xtask fold` run was asked for.
#[derive(Debug, Clone, Default, clap::Args)]
pub(crate) struct Ask {
    /// The binary to hold, in place of the release `php` built now: a
    /// binary the tree did not build here, so the reading carries no key
    /// and `done` never reads it as fresh.
    #[arg(long, value_name = "PATH")]
    pub(crate) binary: Option<PathBuf>,
}

/// The run as the command line asks it, recorded.
pub(crate) fn command(root: &Path, manifest: &Manifest, store: &Store, ask: &Ask) -> Result<()> {
    crate::record(store, &[run(root, manifest, ask)?])
}

/// The one reading: the release `php` built, or `ask`'s binary, each
/// instantiation of `serve_frame` in it disassembled and held.
pub(crate) fn run(root: &Path, manifest: &Manifest, ask: &Ask) -> Result<Reading> {
    let krate = manifest
        .crate_named(SCOPE)
        .with_context(|| format!("the manifest has no [[crate]] row for {SCOPE}"))?;
    let mut reading = match &ask.binary {
        Some(_) => unkeyed()?,
        None => Reading::for_crate(INSTRUMENT, krate, &Snapshot::disk(root)?)?,
    };
    if !cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        reading.status = Status::Skip;
        reading.message = format!(
            "{SCOPE}: not held; the sensor reads llvm-objdump's aarch64 Mach-O disassembly and this host is {} {}",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
        return Ok(reading);
    }
    let binary = if let Some(binary) = &ask.binary {
        binary.clone()
    } else {
        reading.profile = Some(profile::Profile::Release);
        profile::released(root, (&CLI.package(), CLI.binary), None, (None, None))?
    };
    let instantiations = instantiations(&binary)?;
    let over = match &ask.binary {
        Some(binary) => format!("{}, a binary the tree did not build here", binary.display()),
        None => format!("the release {}", CLI.binary),
    };
    match judged(&disassembled(&binary, &instantiations)?) {
        Ok(held) => said(&mut reading, &over, &instantiations, &held),
        Err(why) => {
            reading.status = Status::Fail;
            reading.message = format!("{SCOPE}: {over} could not be held: {why:#}");
        }
    }
    Ok(reading)
}

/// The reading over a binary the tree did not build here: timed as it
/// is made, with no key, so `done` never reads it as fresh.
fn unkeyed() -> Result<Reading> {
    Reading::keyed(INSTRUMENT, SCOPE, String::new())
}

/// Write what `held` says of the instantiations into `reading`.
fn said(reading: &mut Reading, over: &str, sizes: &[(String, u64)], held: &[Held]) {
    let folded: usize = held.iter().map(|h| h.folded).sum();
    let hoisted: usize = held.iter().map(|h| h.hoisted.len()).sum();
    let escaped: usize = held.iter().map(|h| h.escaped.len()).sum();
    let bytes: u64 = sizes.iter().map(|(_, size)| size).sum();
    let count = |n: usize| Number::Int(i64::try_from(n).unwrap_or(i64::MAX));
    for (key, value) in [
        ("instantiations", held.len()),
        ("folded", folded),
        ("hoisted", hoisted),
        ("escaped", escaped),
    ] {
        reading.numbers.insert(key.to_owned(), count(value));
    }
    reading.numbers.insert(
        "bytes".to_owned(),
        Number::Int(i64::try_from(bytes).unwrap_or(i64::MAX)),
    );
    // Each field whose address escapes and is accessed through it, once,
    // with the calls it reaches.
    let mut handed: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
    let accessed = |offset: i64| {
        held.iter()
            .flat_map(|h| &h.escaped)
            .any(|site| site.offset == offset)
    };
    for escape in held
        .iter()
        .flat_map(|h| &h.escapes)
        .filter(|e| accessed(e.offset))
    {
        let calls = handed.entry(escape.offset).or_default();
        if !calls.contains(&escape.call.as_str()) {
            calls.push(&escape.call);
        }
    }
    let fields: Vec<String> = handed
        .iter()
        .map(|(offset, calls)| format!("+{offset:#x} to {}", calls.join(" and ")))
        .collect();
    let beside = if escaped > 0 {
        format!(
            "; {escaped} accesses go through an address a call is also handed, which no limit folds and the option is not held to: {}",
            fields.join(", ")
        )
    } else {
        String::new()
    };
    let n = held.len();
    if held.is_empty() {
        reading.status = Status::Fail;
        reading.message = format!(
            "{SCOPE}: {over} has no instantiation of php_engine::tier::serve_frame to hold; a symbol named otherwise is the sensor's to follow"
        );
    } else if hoisted == 0 {
        reading.status = Status::Pass;
        reading.message = format!(
            "{SCOPE}: {over}'s {n} instantiations of serve_frame fold every access to a request field whose address no call is handed, {folded} accesses through the request's register{beside}"
        );
    } else {
        reading.status = Status::Fail;
        let each: Vec<String> = held
            .iter()
            .filter_map(|one| {
                let first = one.hoisted.first()?;
                Some(format!(
                    "{} {} times, the first at {}: `{}` then `{}`",
                    one.name,
                    one.hoisted.len(),
                    first.at,
                    first.add,
                    first.access
                ))
            })
            .collect();
        reading.message = format!(
            "{SCOPE}: {over}'s serve_frame reaches a request field through an address held apart from the request's register {hoisted} times, which LLVM's --cgp-max-address-users-to-scan folds (design §10 Profiles): a toolchain that keeps the option's name may have changed its meaning; {}{beside}",
            each.join("; ")
        );
    }
}

/// Each instantiation of `serve_frame` in `binary` and its size in bytes,
/// read off `nm -n`: the distance to the next symbol.
fn instantiations(binary: &Path) -> Result<Vec<(String, u64)>> {
    let out = Command::new("nm").arg("-n").arg(binary).output();
    let listed = crate::printed(out, &format!("nm -n {}", binary.display()))?;
    let symbols: Vec<(u64, &str)> = listed
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let at = u64::from_str_radix(words.next()?, 16).ok()?;
            let name = words.nth(1)?;
            Some((at, name))
        })
        .collect();
    Ok(symbols
        .iter()
        .enumerate()
        .filter(|(_, (_, name))| name.contains(INSTANTIATION))
        .map(|(i, (at, name))| {
            let next = symbols[i + 1..].iter().find(|(n, _)| n > at);
            let size = next.map_or(0, |(n, _)| n - at);
            ((*name).to_owned(), size)
        })
        .collect())
}

/// The disassembly of each of `symbols` in `binary`, `objdump`'s lines.
fn disassembled(binary: &Path, symbols: &[(String, u64)]) -> Result<String> {
    if symbols.is_empty() {
        return Ok(String::new());
    }
    let names: Vec<&str> = symbols.iter().map(|(name, _)| name.as_str()).collect();
    let out = Command::new("objdump")
        .arg("--no-show-raw-insn")
        .arg(format!("--disassemble-symbols={}", names.join(",")))
        .arg(binary)
        .output();
    crate::printed(
        out,
        &format!("objdump --disassemble-symbols over {}", binary.display()),
    )
}

/// What one instantiation holds: its symbol, the accesses through the
/// request's register at an immediate offset, and each site that reaches
/// a field through an address held apart from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held {
    /// The symbol, as the disassembly heads it.
    pub(crate) name: String,
    /// The loads and stores whose base is the request's register.
    pub(crate) folded: usize,
    /// Each access through a hoisted address no call is handed.
    pub(crate) hoisted: Vec<Site>,
    /// Each access through a hoisted address a call is also handed: the
    /// field's address escapes, and no limit of `CodeGenPrepare`'s folds
    /// it, so it is no sign of the option's meaning (the frame table's,
    /// handed to `Frames::grow`, was one).
    pub(crate) escaped: Vec<Site>,
    /// Each field whose address is handed to a call, and the call.
    pub(crate) escapes: Vec<Escape>,
}

/// A field's address handed to a call in an argument register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Escape {
    /// The field's offset from the request.
    pub(crate) offset: i64,
    /// The callee, as the disassembly names it.
    pub(crate) call: String,
}

/// One access through a hoisted address: the `add` that made it and the
/// access that used it, as the disassembly prints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Site {
    /// The field's offset from the request, the `add`'s immediate.
    pub(crate) offset: i64,
    /// The access's address.
    pub(crate) at: String,
    /// The `add`.
    pub(crate) add: String,
    /// The load or store.
    pub(crate) access: String,
}

/// Every function of `text`, an `objdump` disassembly, held: one
/// [`Held`] per function it heads with `<address> <symbol>:`. A function
/// whose prologue copies no register from `x0` is refused, since then
/// nothing says which register holds the request.
pub(crate) fn judged(text: &str) -> Result<Vec<Held>> {
    let mut functions: Vec<(String, Vec<Instruction>)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = heading(line) {
            functions.push((name.to_owned(), Vec::new()));
        } else if let (Some((_, body)), Some(instruction)) =
            (functions.last_mut(), Instruction::of(line))
        {
            body.push(instruction);
        }
    }
    functions
        .into_iter()
        .map(|(name, body)| {
            let Some(request) = request(&body) else {
                bail!("{name}: its prologue copies no register from x0, so no register is known to hold the request");
            };
            let (sites, escapes) = hoisted(&body, request);
            let (through_a_call, unfolded) = sites
                .into_iter()
                .partition(|site| escapes.iter().any(|e| e.offset == site.offset));
            let mut escapes = escapes;
            escapes.sort_by(|a, b| (a.offset, &a.call).cmp(&(b.offset, &b.call)));
            escapes.dedup();
            Ok(Held {
                folded: body
                    .iter()
                    .filter(|i| i.memory().is_some_and(|m| m.at(Gpr::X(request)).is_some()))
                    .count(),
                hoisted: unfolded,
                escaped: through_a_call,
                escapes,
                name,
            })
        })
        .collect()
}

/// The symbol a line `<address> <symbol>:` heads a function with.
fn heading(line: &str) -> Option<&str> {
    let (at, rest) = line.split_once(' ')?;
    if at.is_empty() || !at.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    rest.strip_prefix('<')?.strip_suffix(">:")
}

/// The register the prologue copies the request into: the first
/// `mov xR, x0` before the first branch.
fn request(body: &[Instruction]) -> Option<u8> {
    for instruction in body {
        if instruction.branches() {
            return None;
        }
        if instruction.mnemonic == "mov"
            && let [to, from] = &instruction.operands[..]
            && Gpr::of(from) == Some(Gpr::X(0))
            && let Some(Gpr::X(n)) = Gpr::of(to)
        {
            return Some(n);
        }
    }
    None
}

/// Each access in `body` through an address `add xN, x<request>, #C`
/// made, where the access could have encoded `[x<request>, #C + offset]`,
/// the address followed through a spill to the stack and back, and each
/// such address handed to a call in an argument register.
fn hoisted(body: &[Instruction], request: u8) -> (Vec<Site>, Vec<Escape>) {
    // What each register holds: the request plus an offset, and the `add`.
    let mut holding: BTreeMap<u8, (i64, &Instruction)> = BTreeMap::new();
    // What each stack slot holds of the same, by base and offset.
    let mut spilled: BTreeMap<(Gpr, i64), (i64, &Instruction)> = BTreeMap::new();
    let mut sites = Vec::new();
    let mut escapes = Vec::new();
    for instruction in body {
        let memory = instruction.memory();
        let size = instruction.single_access();
        if let (Some(memory), Some(size)) = (memory, size)
            && let Some(n) = memory.base.number()
            && let Some((offset, add)) = holding.get(&n)
            && let Some(at) = memory.at(Gpr::X(n))
            && encodable(offset + at, size)
        {
            sites.push(Site {
                offset: *offset,
                at: instruction.at.clone(),
                add: add.text.clone(),
                access: instruction.text.clone(),
            });
        }
        // A held address spilled to the stack, a slot written over, and a
        // spilled one loaded back into a register.
        let mut reloaded = None;
        let stack = [Gpr::Sp, Gpr::X(29)];
        if let Some(memory) = memory
            && let Some((base, at)) = stack.iter().find_map(|b| Some((*b, memory.at(*b)?)))
        {
            let slot = (base, at);
            let data = instruction.operands.first().and_then(|o| Gpr::of(o));
            if instruction.mnemonic.starts_with("st") {
                spilled.remove(&slot);
                if let (Some(_), Some(Gpr::X(n))) = (size, data)
                    && let Some(kept) = holding.get(&n)
                {
                    spilled.insert(slot, *kept);
                }
            } else if let (Some(8), Some(Gpr::X(n))) = (size, data) {
                reloaded = spilled.get(&slot).map(|kept| (n, *kept));
            }
        }
        for named in instruction.registers() {
            holding.remove(&named);
        }
        if let Some((n, kept)) = reloaded {
            holding.insert(n, kept);
        }
        if instruction.calls() {
            escapes.extend(holding.range(..ARGUMENTS).map(|(_, (offset, _))| Escape {
                offset: *offset,
                call: instruction.callee().to_owned(),
            }));
            holding.retain(|n, _| *n >= CALLEE_SAVED && *n != LINK);
        } else if instruction.ends_flow() {
            holding.retain(|n, _| *n >= CALLEE_SAVED);
        }
        if let Some((to, offset)) = instruction.field_address(request) {
            holding.insert(to, (offset, instruction));
        }
    }
    (sites, escapes)
}

/// Whether a load or store of `size` bytes encodes `offset` from its base
/// register: scaled and unsigned below 4096 units, or unscaled within a
/// signed byte and a half, as `ldur` takes it.
fn encodable(offset: i64, size: i64) -> bool {
    let scaled = offset >= 0 && offset % size == 0 && offset / size <= 4095;
    scaled || (-256..=255).contains(&offset)
}

/// A general-purpose register as an operand names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Gpr {
    /// `xN`, N from 0 to 30, the whole register.
    X(u8),
    /// `wN`, its low half.
    W(u8),
    /// `sp` or `wsp`.
    Sp,
    /// `xzr` or `wzr`.
    Zero,
}

impl Gpr {
    fn of(word: &str) -> Option<Gpr> {
        match word {
            "sp" | "wsp" => Some(Gpr::Sp),
            "xzr" | "wzr" => Some(Gpr::Zero),
            "fp" => Some(Gpr::X(29)),
            "lr" => Some(Gpr::X(30)),
            _ => {
                let (whole, n) = match word.split_at_checked(1)? {
                    ("x", n) => (true, n),
                    ("w", n) => (false, n),
                    _ => return None,
                };
                let n: u8 = n.parse().ok().filter(|n| *n <= LINK)?;
                Some(if whole { Gpr::X(n) } else { Gpr::W(n) })
            }
        }
    }

    /// The register's number, whichever half is named.
    fn number(self) -> Option<u8> {
        match self {
            Gpr::X(n) | Gpr::W(n) => Some(n),
            Gpr::Sp | Gpr::Zero => None,
        }
    }
}

/// The registers a call hands its arguments in, `x0` to `x7`, as AAPCS64
/// names them.
const ARGUMENTS: u8 = 8;

/// The first register a callee keeps, `x19`; `x19` to `x28` are kept.
const CALLEE_SAVED: u8 = 19;

/// The link register, `x30`, which a call writes.
const LINK: u8 = 30;

/// A memory operand: its base, what is added to it, and whether the
/// access writes the base back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Memory {
    base: Gpr,
    offset: Offset,
    writes_back: bool,
}

/// What a memory operand adds to its base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offset {
    /// An immediate, zero where none is written.
    Imm(i64),
    /// A register, which no field's address folds into.
    Indexed,
}

impl Memory {
    /// The immediate added to `base`, where the base is `base` and no
    /// register indexes it and the base is not written back.
    fn at(&self, base: Gpr) -> Option<i64> {
        match self.offset {
            Offset::Imm(offset) if self.base == base && !self.writes_back => Some(offset),
            _ => None,
        }
    }
}

/// One instruction of the disassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Instruction {
    /// Its address, as printed.
    at: String,
    mnemonic: String,
    /// Its operands, split at the commas outside a memory operand.
    operands: Vec<String>,
    /// The mnemonic and operands as printed, a comment left out.
    text: String,
}

impl Instruction {
    /// The instruction a line `<address>: <mnemonic> <operands>` prints.
    fn of(line: &str) -> Option<Instruction> {
        let (at, rest) = line.trim_start().split_once(':')?;
        if at.is_empty() || !at.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let rest = rest.split_once(';').map_or(rest, |(code, _)| code).trim();
        let (mnemonic, operands) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(m, o)| (m, o.trim()));
        if mnemonic.is_empty() {
            return None;
        }
        let mut split = Vec::new();
        let mut depth = 0usize;
        let mut open = String::new();
        for c in operands.chars() {
            match c {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    split.push(std::mem::take(&mut open).trim().to_owned());
                    continue;
                }
                _ => {}
            }
            open.push(c);
        }
        if !open.trim().is_empty() {
            split.push(open.trim().to_owned());
        }
        Some(Instruction {
            at: at.to_owned(),
            mnemonic: mnemonic.to_owned(),
            text: format!("{mnemonic} {}", split.join(", ")),
            operands: split,
        })
    }

    /// The memory operand, where the instruction has one, and whether an
    /// immediate after it writes the base back (post-index).
    fn memory(&self) -> Option<Memory> {
        let at = self.operands.iter().position(|o| o.starts_with('['))?;
        let operand = &self.operands[at];
        let writes_back = operand.ends_with('!') || self.operands.len() > at + 1;
        let inner = operand
            .trim_end_matches('!')
            .strip_prefix('[')?
            .strip_suffix(']')?;
        let mut parts = inner.split(',').map(str::trim);
        let base = Gpr::of(parts.next()?)?;
        let offset = match parts.next() {
            None => Offset::Imm(0),
            Some(imm) if imm.starts_with('#') => Offset::Imm(immediate(imm)?),
            Some(_) => Offset::Indexed,
        };
        Some(Memory {
            base,
            offset,
            writes_back,
        })
    }

    /// The size in bytes of a load or store of one register, the kind
    /// whose immediate offset can take a field's: `ldr`, `str`, `ldur`,
    /// `stur` and their byte, half and sign-extending forms. A pair, an
    /// exclusive or acquiring access, a vector one and a prefetch are
    /// none of these.
    fn single_access(&self) -> Option<i64> {
        match self.mnemonic.as_str() {
            "ldrb" | "strb" | "ldrsb" | "ldurb" | "sturb" | "ldursb" => Some(1),
            "ldrh" | "strh" | "ldrsh" | "ldurh" | "sturh" | "ldursh" => Some(2),
            "ldrsw" | "ldursw" => Some(4),
            "ldr" | "str" | "ldur" | "stur" => width(self.operands.first()?),
            _ => None,
        }
    }

    /// `add xN, x<request>, #C`, or `#C, lsl #12`, the address of a
    /// request field made into a register of its own: `N` and the offset.
    fn field_address(&self, request: u8) -> Option<(u8, i64)> {
        if self.mnemonic != "add" {
            return None;
        }
        let (to, from, offset) = match &self.operands[..] {
            [to, from, imm] => (to, from, immediate(imm)?),
            [to, from, imm, shift] if shift == "lsl #12" => (to, from, immediate(imm)? << 12),
            _ => return None,
        };
        match (Gpr::of(to)?, Gpr::of(from)?) {
            (Gpr::X(to), Gpr::X(from)) if from == request && to != request => Some((to, offset)),
            _ => None,
        }
    }

    /// Every general-purpose register the instruction names, `xN` and
    /// `wN` alike.
    fn registers(&self) -> Vec<u8> {
        self.operands
            .iter()
            .flat_map(|o| o.split(|c: char| !c.is_ascii_alphanumeric()))
            .filter_map(|word| Gpr::of(word)?.number())
            .collect()
    }

    /// The callee a call names, `<symbol>` where the disassembly prints
    /// one, else its operand.
    fn callee(&self) -> &str {
        let operand = self.operands.first().map_or("", String::as_str);
        operand
            .split_once('<')
            .and_then(|(_, name)| name.strip_suffix('>'))
            .unwrap_or(operand)
    }

    /// Whether it is a call, after which the registers below `x19` and
    /// the link register hold what the callee left.
    fn calls(&self) -> bool {
        matches!(
            self.mnemonic.as_str(),
            "bl" | "blr" | "blraa" | "blraaz" | "blrab" | "blrabz"
        )
    }

    /// Whether the next instruction listed is not the next run: an
    /// unconditional branch or a return.
    fn ends_flow(&self) -> bool {
        matches!(
            self.mnemonic.as_str(),
            "b" | "br" | "braa" | "braaz" | "brab" | "brabz" | "ret" | "retaa" | "retab"
        )
    }

    /// Whether it branches at all, conditional or not, a call included.
    fn branches(&self) -> bool {
        let m = self.mnemonic.as_str();
        self.calls()
            || self.ends_flow()
            || m.starts_with("b.")
            || matches!(m, "cbz" | "cbnz" | "tbz" | "tbnz")
    }
}

/// The bytes a load or store moves through `register`, by the letter its
/// name starts with: a general register's either half, or a vector
/// register's byte, half, single, double or quad view.
fn width(register: &str) -> Option<i64> {
    match register.chars().next()? {
        'x' | 'd' => Some(8),
        'w' | 's' => Some(4),
        'q' => Some(16),
        'h' => Some(2),
        'b' => Some(1),
        _ => None,
    }
}

/// The value of an immediate operand, `#0x5c8`, `#-8` or `#16`.
fn immediate(operand: &str) -> Option<i64> {
    let digits = operand.strip_prefix('#')?;
    let (negative, digits) = digits
        .strip_prefix('-')
        .map_or((false, digits), |d| (true, d));
    let value = match digits.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held binary the tree did not build carries the time it was
    /// held, and no key (Round 1, Q23, c3.b).
    #[test]
    fn a_binary_the_tree_did_not_build_is_held_at_a_time_and_no_key() {
        let reading = unkeyed().unwrap();
        assert!(!reading.at.to_string().starts_with("1970-"), "{reading:#?}");
        assert!(reading.key().is_empty(), "{reading:#?}");
    }

    const HEAD: &str = "\nbin/php:\tfile format mach-o arm64\n\nDisassembly of section __TEXT,__text:\n\n0000000100001000 <__ZN10php_engine4tier11serve_frame17h0123456789abcdefE>:\n100001000:     \tstp\tx20, x19, [sp, #-0x20]!\n100001004:     \tmov\tx19, x0\n";

    fn one(body: &str) -> Held {
        let mut held = judged(&format!("{HEAD}{body}")).unwrap();
        assert_eq!(held.len(), 1, "{held:#?}");
        held.remove(0)
    }

    /// An access through a hoisted address is a site, a folded access is
    /// none, and the request's register is the one the prologue copies `x0`
    /// into.
    #[test]
    fn an_access_through_an_address_made_from_the_request_is_a_site() {
        let held = one(
            "100001008:     \tldr\tx8, [x19, #0x5c8]\n10000100c:     \tadd\tx27, x19, #0x5c8\n100001010:     \tldr\tx9, [x27]\n100001014:     \tadd\tx8, x19, #0x5d0\n100001018:     \tstr\tw9, [x8, #0x8]\n",
        );
        assert_eq!(held.folded, 1);
        assert_eq!(
            held.hoisted,
            vec![
                Site {
                    offset: 0x5c8,
                    at: "100001010".into(),
                    add: "add x27, x19, #0x5c8".into(),
                    access: "ldr x9, [x27]".into(),
                },
                Site {
                    offset: 0x5d0,
                    at: "100001018".into(),
                    add: "add x8, x19, #0x5d0".into(),
                    access: "str w9, [x8, #0x8]".into(),
                },
            ]
        );
    }

    /// What no fold could encode is no site: a register-offset access, a
    /// pair, a call's argument a callee overwrote, a register redefined
    /// before it is used, an offset past the access's range, and a
    /// write-back.
    #[test]
    fn what_no_fold_could_encode_is_no_site() {
        let held = one(concat!(
            "100001008:     \tadd\tx12, x19, #0x670\n",
            "10000100c:     \tldr\tx11, [x12, x11]\n",
            "100001010:     \tadd\tx13, x19, #0x5c8\n",
            "100001014:     \tldp\tx8, x9, [x13]\n",
            "100001018:     \tadd\tx0, x19, #0xf58\n",
            "10000101c:     \tbl\t0x100002000 <_callee>\n",
            "100001020:     \tldr\tx8, [x0]\n",
            "100001024:     \tadd\tx14, x19, #0x5c8\n",
            "100001028:     \tmov\tx14, x2\n",
            "10000102c:     \tldr\tx8, [x14]\n",
            "100001030:     \tadd\tx15, x19, #0x7ff8\n",
            "100001034:     \tldr\tx8, [x15, #0x10]\n",
            "100001038:     \tadd\tx16, x19, #0x5c8\n",
            "10000103c:     \tldr\tx8, [x16, #0x8]!\n",
        ));
        assert_eq!(held.hoisted, vec![], "{held:#?}");
        // The callee overwrote `x0`, so its load is no site at all, not
        // one the escape at the same offset hides.
        assert_eq!(held.escaped, vec![], "{held:#?}");
    }

    /// An access through an address a call is also handed is escaped and
    /// not hoisted: no limit of `CodeGenPrepare`'s folds a field whose
    /// address escapes, as `Frames::grow`'s did.
    #[test]
    fn an_address_a_call_is_handed_escapes_and_is_no_site() {
        let held = one(concat!(
            "100001008:     \tadd\tx0, x19, #0x918\n",
            "10000100c:     \tbl\t0x100002000 <__ZN8php_heap5stack6frames15Frames$LT$H$GT$4grow17hce1896af0dd227cfE>\n",
            "100001010:     \tadd\tx8, x19, #0x918\n",
            "100001014:     \tldr\tx8, [x8]\n",
            "100001018:     \tadd\tx9, x19, #0x920\n",
            "10000101c:     \tldr\tx8, [x9]\n",
        ));
        assert_eq!(held.escaped.len(), 1, "{held:#?}");
        assert_eq!(held.hoisted.len(), 1, "{held:#?}");
        assert_eq!(held.hoisted[0].offset, 0x920);
        assert_eq!(
            held.escapes,
            vec![Escape {
                offset: 0x918,
                call: "__ZN8php_heap5stack6frames15Frames$LT$H$GT$4grow17hce1896af0dd227cfE".into(),
            }]
        );
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        let escaped_only = Held {
            hoisted: Vec::new(),
            ..held
        };
        said(&mut reading, "the fixture", &[], &[escaped_only]);
        assert_eq!(reading.status, Status::Pass, "{}", reading.message);
        assert!(
            reading
                .message
                .contains("+0x918 to __ZN8php_heap5stack6frames15Frames"),
            "{}",
            reading.message
        );
    }

    /// An address spilled to the stack and loaded back is followed, as
    /// the allocator may hold an unfolded field; a slot written
    /// over in between holds it no longer. Each slot is spilled from a
    /// register that still holds the address, since a store names its
    /// data register and drops it.
    #[test]
    fn a_spilled_address_is_followed_back_from_its_slot() {
        let held = one(concat!(
            "100001008:     \tadd\tx8, x19, #0x5c8\n",
            "10000100c:     \tstr\tx8, [sp, #0x10]\n",
            "100001010:     \tadd\tx8, x19, #0x5c8\n",
            "100001014:     \tstur\tx8, [x29, #-0x18]\n",
            "100001018:     \tbl\t0x100002000 <_callee>\n",
            "10000101c:     \tldr\tx9, [sp, #0x10]\n",
            "100001020:     \tldr\tx10, [x9]\n",
            "100001024:     \tstr\txzr, [x29, #-0x18]\n",
            "100001028:     \tldur\tx11, [x29, #-0x18]\n",
            "10000102c:     \tldr\tx12, [x11]\n",
        ));
        assert_eq!(held.hoisted.len(), 1, "{held:#?}");
        assert_eq!(held.hoisted[0].access, "ldr x10, [x9]");
    }

    /// `add xN, x19, #C, lsl #12` makes the address of a field past 4095,
    /// which an access can still encode, so its access is a site at the
    /// shifted offset.
    #[test]
    fn a_shifted_add_is_the_field_past_4095() {
        let held = one(concat!(
            "100001008:     \tadd\tx8, x19, #0x1, lsl #12\n",
            "10000100c:     \tldr\tx9, [x8]\n",
        ));
        assert_eq!(held.hoisted.len(), 1, "{held:#?}");
        assert_eq!(held.hoisted[0].offset, 0x1000);
    }

    /// A register the callee keeps is followed across a call and a
    /// branch; one it does not is dropped at each on its own: a
    /// caller-saved register no argument hands on at the call, the link
    /// register at the call, and a caller-saved one at a branch with no
    /// call before it.
    #[test]
    fn a_kept_register_is_followed_across_a_call_and_a_branch() {
        let branch =
            "\tb\t0x100001020 <__ZN10php_engine4tier11serve_frame17h0123456789abcdefE+0x20>\n";
        let held = one(&format!(
            "100001008:     \tadd\tx27, x19, #0x5c8\n\
             10000100c:     \tadd\tx9, x19, #0x5d0\n\
             100001010:     \tadd\tx30, x19, #0x5d8\n\
             100001014:     \tbl\t0x100002000 <_callee>\n\
             100001018:     {branch}\
             10000101c:     \tldr\tx8, [x9]\n\
             100001020:     \tldr\tx8, [x30]\n\
             100001024:     \tldr\tx8, [x27, #0x8]\n"
        ));
        assert_eq!(held.hoisted.len(), 1, "{held:#?}");
        assert_eq!(held.hoisted[0].add, "add x27, x19, #0x5c8");
        for dropped in ["x9", "x30"] {
            let held = one(&format!(
                "100001008:     \tadd\t{dropped}, x19, #0x5d0\n\
                 10000100c:     \tbl\t0x100002000 <_callee>\n\
                 100001010:     \tldr\tx8, [{dropped}]\n"
            ));
            assert_eq!(held.hoisted, vec![], "{dropped} at the call: {held:#?}");
            assert_eq!(held.escaped, vec![], "{dropped} at the call: {held:#?}");
        }
        let held = one(&format!(
            "100001008:     \tadd\tx9, x19, #0x5d0\n\
             10000100c:     {branch}\
             100001010:     \tldr\tx8, [x9]\n"
        ));
        assert_eq!(held.hoisted, vec![], "x9 at the branch: {held:#?}");
    }

    /// A function whose prologue names no register for the request is
    /// refused rather than held over a guess.
    #[test]
    fn a_function_with_no_register_for_the_request_is_refused() {
        let text = HEAD.replace("mov\tx19, x0", "mov\tx19, x1");
        let err = judged(&format!("{text}100001008:     \tret\n")).unwrap_err();
        assert!(
            err.to_string().contains("copies no register from x0"),
            "{err}"
        );
    }

    /// The negative controls and their pair: the instantiation at
    /// 037f359d8 built without the option, and main e61851ebe's, hold sites;
    /// this branch's, built with it, holds none and one escape.
    #[test]
    fn the_build_without_the_option_fails_and_the_one_with_it_passes() {
        let dir = crate::package_dir().join("fixtures/fold");
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        let unfolded = judged(&read("unfolded.s")).unwrap();
        assert!(
            unfolded.iter().any(|h| !h.hoisted.is_empty()),
            "{unfolded:#?}"
        );
        let folded = judged(&read("folded.s")).unwrap();
        assert!(folded.iter().all(|h| h.hoisted.is_empty()), "{folded:#?}");
        assert!(folded.iter().all(|h| h.folded > 0), "{folded:#?}");
        assert!(folded.iter().any(|h| !h.escaped.is_empty()), "{folded:#?}");
        // main e61851ebe without the option: the second word hoisted, the
        // first escaped.
        let main = judged(&read("main-unfolded.s")).unwrap();
        let hoisted: Vec<i64> = main
            .iter()
            .flat_map(|h| &h.hoisted)
            .map(|s| s.offset)
            .collect();
        let escaped: Vec<i64> = main
            .iter()
            .flat_map(|h| &h.escaped)
            .map(|s| s.offset)
            .collect();
        assert_eq!((hoisted, escaped), (vec![0x920], vec![0x918]), "{main:#?}");

        let sizes = [("serve_frame".to_owned(), 64)];
        let mut reading = Reading::new(INSTRUMENT, SCOPE);
        said(&mut reading, "the fixture", &sizes, &unfolded);
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        assert!(
            reading.message.contains("--cgp-max-address-users-to-scan"),
            "{}",
            reading.message
        );
        said(&mut reading, "the fixture", &sizes, &folded);
        assert_eq!(reading.status, Status::Pass, "{}", reading.message);
        said(&mut reading, "the fixture", &sizes, &[]);
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    }
}
