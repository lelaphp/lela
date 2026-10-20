//! A [`TraceIr`] as text (`jit-design.md` §4.3), the value's second
//! reader: what `types` built, op by op in the order the trace goes,
//! each value where it is written with its slot, its types in php's
//! `zend_dump_type_info` words and whether a register holds it, each
//! guard where it is held, and every exit with what it stores.
//!
//! ```text
//! trace enter e1, stop recursive call
//! frame f0 e1 root, 1 cvs 6 temps
//! entry
//!   #0 f0.CV0 [long]
//! loop at 0, recursive call, interrupt exit 0
//!   #1 f0.CV0 [long] = phi(#0, #5)
//! 0001 f0 IS_SMALLER #1, int(2) fused inline
//!   checks direction taken -> exit 1
//! ```
//!
//! A slot is its frame and its variable, `CV`, or its temporary, `T`,
//! numbered from the first temporary; a guard held before an op is
//! written above it, one the op holds itself below it as `checks`.

use std::fmt::{self, Write as _};

use php_heap::ZvalType;

use crate::record::func_name as func;
use crate::types::{
    Check, Constant, Def, End, ExitSpec, Form, Guard, GuardAt, Home, Landing, Layout, Op, OpId,
    Operand, Place, Repeat, Slot, Stored, TraceIr, Types, ValueId,
};

/// The text of `ir`.
pub(crate) fn render(ir: &TraceIr) -> String {
    let mut text = String::new();
    // Writing to a `String` does not fail.
    Dump::new(ir).write(&mut text).unwrap_or_default();
    text
}

/// The trace and its values grouped by where each is written, read once.
struct Dump<'a> {
    ir: &'a TraceIr,
    /// Per op, the values it writes and the ones it reads again.
    written: Vec<Vec<ValueId>>,
    reloaded: Vec<Vec<ValueId>>,
}

impl<'a> Dump<'a> {
    fn new(ir: &'a TraceIr) -> Dump<'a> {
        let mut written = vec![Vec::new(); ir.ops.len()];
        let mut reloaded = vec![Vec::new(); ir.ops.len()];
        for (id, value) in ids(ir) {
            match value.def {
                Def::Op(at) => written.get_mut(at.index()).map(|list| list.push(id)),
                Def::Reload(at) => reloaded.get_mut(at.index()).map(|list| list.push(id)),
                Def::Entry | Def::Phi { .. } => None,
            };
        }
        Dump {
            ir,
            written,
            reloaded,
        }
    }

    fn write(&self, text: &mut String) -> fmt::Result {
        self.head(text)?;
        let header = match self.ir.end {
            End::Loop(header) => Some(header),
            End::Leave(_) => None,
        };
        for (at, op) in (0u32..).map(OpId).zip(&self.ir.ops) {
            if let Some(header) = header.filter(|header| header.at == at) {
                let repeat = match header.repeat {
                    Repeat::Loop => "loop",
                    Repeat::Call => "recursive call",
                    Repeat::Return => "recursive return",
                };
                let interrupt = header.interrupt.0;
                writeln!(
                    text,
                    "loop at {}, {repeat}, interrupt exit {interrupt}",
                    at.0
                )?;
                for (id, value) in ids(self.ir) {
                    if let Def::Phi { entry, back } = value.def {
                        let phi = self.value(id);
                        writeln!(text, "  {phi} = phi(#{}, #{})", entry.0, back.0)?;
                    }
                }
            }
            self.guards(text, GuardAt::Before(at), "guard")?;
            self.op(text, op)?;
            for id in self.written.get(at.index()).into_iter().flatten() {
                writeln!(text, "  -> {}", self.value(*id))?;
            }
            for id in self.reloaded.get(at.index()).into_iter().flatten() {
                writeln!(text, "  reload {}", self.value(*id))?;
            }
            self.guards(text, GuardAt::Within(at), "checks")?;
        }
        match self.ir.end {
            End::Loop(header) => writeln!(text, "end: back to {}", header.at.0)?,
            End::Leave(exit) => writeln!(text, "end: exit {}", exit.0)?,
        }
        for (at, exit) in self.ir.exits.iter().enumerate() {
            self.exit(text, at, exit)?;
        }
        for (at, raw) in self.ir.map_ptrs.iter().enumerate() {
            writeln!(text, "map {at}: offset {} slot {}", raw.offset, raw.slot)?;
        }
        Ok(())
    }

    /// The trace's start and stop, its frames, and what it enters with.
    fn head(&self, text: &mut String) -> fmt::Result {
        let entry = self.ir.entry;
        let (start, stop) = (entry.start.written(), entry.stop.written());
        writeln!(text, "trace {start} {}, stop {stop}", func(entry.root))?;
        for (at, frame) in self.ir.frames.iter().enumerate() {
            let place = match frame.place {
                Place::Root => "root".to_owned(),
                Place::Callee(of) => format!("callee of f{}", of.0),
                Place::Caller(of) => format!("caller of f{}", of.0),
            };
            let (cvs, temps) = (frame.cvs, frame.temps);
            let named = func(frame.func);
            writeln!(text, "frame f{at} {named} {place}, {cvs} cvs {temps} temps")?;
        }
        writeln!(text, "entry")?;
        for (id, value) in ids(self.ir) {
            if value.def == Def::Entry {
                writeln!(text, "  {}", self.value(id))?;
            }
        }
        self.guards(text, GuardAt::Entry, "guard")
    }

    /// The guards held at `at`, each named `word`.
    fn guards(&self, text: &mut String, at: GuardAt, word: &str) -> fmt::Result {
        for Guard {
            check: held, exit, ..
        } in self.ir.guards.iter().filter(|g| g.at == at)
        {
            writeln!(text, "  {word} {} -> exit {}", check(*held), exit.0)?;
        }
        Ok(())
    }

    fn op(&self, text: &mut String, op: &Op) -> fmt::Result {
        match op {
            Op::Line(line) => {
                let name = line.opcode.name();
                let name = name.strip_prefix("ZEND_").unwrap_or(name);
                write!(text, "{:04} f{} {name}", line.opline.0, line.frame.0)?;
                let operands: Vec<String> = [line.op1, line.op2, line.data]
                    .into_iter()
                    .filter_map(operand)
                    .collect();
                if !operands.is_empty() {
                    write!(text, " {}", operands.join(", "))?;
                }
                if line.fused.is_some() {
                    write!(text, " fused")?;
                }
                let form = match line.form {
                    Form::Inline => "inline",
                    Form::Step => "step",
                };
                write!(text, " {form}")?;
                if let Some(map) = line.map_ptr {
                    write!(text, " map {map}")?;
                }
                if let Some(exit) = line.exit {
                    write!(text, " leaves -> exit {}", exit.0)?;
                }
                if let Some(exit) = line.freed {
                    write!(text, " frees -> exit {}", exit.0)?;
                }
                writeln!(text)
            }
            Op::InitCall { frame, target } => {
                let target = target.map_or("native".to_owned(), func);
                writeln!(text, "     f{} init {target}", frame.0)
            }
            Op::Enter { frame } => {
                let named = self
                    .ir
                    .frames
                    .get(usize::from(frame.0))
                    .map_or("?".to_owned(), |info| func(info.func));
                writeln!(text, "     f{} enter {named}", frame.0)
            }
            Op::Back { frame, out } => {
                write!(text, "     f{} back", frame.0)?;
                if let Some(out) = out {
                    write!(text, " to #{}", out.0)?;
                }
                writeln!(text)
            }
        }
    }

    fn exit(&self, text: &mut String, at: usize, exit: &ExitSpec) -> fmt::Result {
        let opline = exit
            .opline
            .map_or("where it stands".to_owned(), |at| format!("{:04}", at.0));
        let landing = match exit.landing {
            Landing::Plain => "plain".to_owned(),
            Landing::RestoreCall => "restore call".to_owned(),
            Landing::FreeTemps { op1, op2 } => {
                let freed: Vec<&str> = [(op1, "op1"), (op2, "op2")]
                    .into_iter()
                    .filter_map(|(on, name)| on.then_some(name))
                    .collect();
                format!("free {}", freed.join(" "))
            }
        };
        write!(text, "exit {at}: f{} {opline} {landing}", exit.frame.0)?;
        for store in &exit.stores {
            let from = match store.from {
                Stored::Value(value) => format!("#{}", value.0),
                Stored::Bool(on) => on.to_string(),
                Stored::Copied(value) => format!("copied #{}", value.0),
            };
            write!(text, " {}={from}", self.slot(store.slot))?;
        }
        if let Some(link) = exit.link {
            write!(text, " link {}", link.get())?;
        }
        writeln!(text)
    }

    /// A value with its slot, its types and its home.
    fn value(&self, id: ValueId) -> String {
        let Some(value) = self.ir.values.get(id.0 as usize) else {
            return format!("#{}", id.0);
        };
        let home = match value.home {
            Home::Register => " reg",
            Home::Memory => "",
        };
        format!(
            "#{} {} [{}]{home}",
            id.0,
            self.slot(value.slot),
            value.types
        )
    }

    fn slot(&self, slot: Slot) -> String {
        let cvs = self
            .ir
            .frames
            .get(usize::from(slot.frame.0))
            .map_or(0, |frame| frame.cvs);
        if slot.var < cvs {
            format!("f{}.CV{}", slot.frame.0, slot.var)
        } else {
            format!("f{}.T{}", slot.frame.0, slot.var - cvs)
        }
    }
}

/// The trace's values with their ids.
fn ids(ir: &TraceIr) -> impl Iterator<Item = (ValueId, &crate::types::Value)> {
    (0u32..).map(ValueId).zip(&ir.values)
}

fn operand(operand: Operand) -> Option<String> {
    Some(match operand {
        Operand::Unused => return None,
        Operand::Value(value) => format!("#{}", value.0),
        Operand::Constant(Constant::Null) => "null".to_owned(),
        Operand::Constant(Constant::False) => "bool(false)".to_owned(),
        Operand::Constant(Constant::True) => "bool(true)".to_owned(),
        Operand::Constant(Constant::Long(n)) => format!("int({n})"),
        Operand::Constant(Constant::Double(d)) => format!("float({d})"),
        Operand::Constant(Constant::Other(at)) => format!("const#{}", at.0),
    })
}

fn check(check: Check) -> String {
    let tag = |tag: ZvalType| Types::of_tag(tag).to_string();
    match check {
        Check::Type { value, tag: t } => format!("#{} {}", value.0, tag(t)),
        Check::Deref { value, tag: t } => format!("#{} ref to {}", value.0, tag(t)),
        Check::Layout { value, layout } => format!(
            "#{} {}",
            value.0,
            match layout {
                Layout::Packed => "packed",
                Layout::Hash => "hash",
            }
        ),
        Check::Direction { taken } => {
            format!("direction {}", if taken { "taken" } else { "fell" })
        }
        Check::Table => "table".to_owned(),
        Check::Caller { func: f, opline } => format!("caller {} {:04}", func(f), opline.0),
    }
}
