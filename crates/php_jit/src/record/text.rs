//! A [`TraceRecord`]'s text form (`jit-design.md` §4.1): one line per
//! field and per event, [`TraceRecord::dump`] writing it and
//! [`TraceRecord::parse`] reading it back to the same record, so a
//! recorded trace is a fixture `types` reads with no engine.
//!
//! ```text
//! start loop
//! root e12
//! op e12 5 long - - taken
//! op e12 7 array.packed long.reference.indirect - -
//! init e14
//! enter e14
//! back
//! stop loop
//! ```
//!
//! An op array is its map-pointer offset, `e` for the engine's and `r`
//! for a request's; an operand not sampled is `-`, a sampled one its
//! type with what was followed to it and its class, `object#3`; a
//! conditional jump is `taken` or `fell`; a side trace's start is
//! `side <parent>/<exit>`, the slots its parent's exit knew the types of
//! an `entered` line of `var:kind` after the root, and a link's stop
//! `link <trace>`, after a `link-at` line naming the op array and opline
//! it hands on at.

use std::fmt::Write as _;

use php_engine::tier::{self, FuncRef, Sample};
use php_heap::ZvalType;
use php_unit::{MapPtrOffset, OplineIdx};

use super::{Entered, Event, Start, Stop, TraceRecord, func_name as func};
use crate::table::TraceId;
use crate::types::ExitId;

/// Why a text is no [`TraceRecord`]: the line, from one, and what it
/// held where a field was due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub(crate) line: usize,
    pub(crate) found: String,
}

/// The type names php's `zend_get_type_by_const` gives, with this
/// engine's internal kinds named for their `IS_*`.
const KINDS: [(ZvalType, &str); 14] = [
    (ZvalType::Undef, "undef"),
    (ZvalType::Null, "null"),
    (ZvalType::False, "false"),
    (ZvalType::True, "true"),
    (ZvalType::Long, "long"),
    (ZvalType::Double, "double"),
    (ZvalType::String, "string"),
    (ZvalType::Array, "array"),
    (ZvalType::Object, "object"),
    (ZvalType::Resource, "resource"),
    (ZvalType::Reference, "reference"),
    (ZvalType::ConstExpr, "constant-ast"),
    (ZvalType::Indirect, "indirect"),
    (ZvalType::Ptr, "ptr"),
];

impl TraceRecord {
    /// The record's text form.
    pub(crate) fn dump(&self) -> String {
        let mut text = String::new();
        let start = self.start.written();
        // Writing to a `String` does not fail.
        writeln!(text, "start {start}").unwrap_or_default();
        writeln!(text, "root {}", func(self.root)).unwrap_or_default();
        if !self.entered.is_empty() {
            let slots: Vec<String> = (self.entered.iter())
                .map(|slot| format!("{}:{}", slot.var, kind_name(slot.tag)))
                .collect();
            writeln!(text, "entered {}", slots.join(" ")).unwrap_or_default();
        }
        for event in &self.events {
            match event {
                Event::Op {
                    func: f,
                    opline,
                    op1,
                    op2,
                    data,
                    branch,
                } => writeln!(
                    text,
                    "op {} {} {} {} {} {}",
                    func(*f),
                    opline.0,
                    sample(*op1),
                    sample(*op2),
                    sample(*data),
                    match branch {
                        None => "-",
                        Some(true) => "taken",
                        Some(false) => "fell",
                    }
                ),
                Event::InitCall { target } => {
                    writeln!(text, "init {}", target.map_or("-".to_owned(), func))
                }
                Event::Enter { func: f } => writeln!(text, "enter {}", func(*f)),
                Event::Back => writeln!(text, "back"),
            }
            .unwrap_or_default();
        }
        if let Some((f, at)) = self.link_at {
            writeln!(text, "link-at {} {}", func(f), at.0).unwrap_or_default();
        }
        writeln!(text, "stop {}", self.stop.written()).unwrap_or_default();
        text
    }

    /// The record `text` is the form of.
    pub(crate) fn parse(text: &str) -> Result<TraceRecord, ParseError> {
        let mut lines = text.lines().enumerate().map(|(at, line)| (at + 1, line));
        let (at, line) = lines.next().unwrap_or((1, ""));
        let start = line
            .strip_prefix("start ")
            .and_then(parse_start)
            .ok_or_else(|| refused(at, line))?;
        let (at, line) = lines.next().unwrap_or((2, ""));
        let root = line
            .strip_prefix("root ")
            .and_then(parse_func)
            .ok_or_else(|| refused(at, line))?;
        let mut events = Vec::new();
        let mut entered = Vec::new();
        let mut link_at = None;
        for (at, line) in lines {
            if let Some(stop) = line.strip_prefix("stop ") {
                let stop = parse_stop(stop).ok_or_else(|| refused(at, line))?;
                return Ok(TraceRecord {
                    start,
                    root,
                    entered,
                    events,
                    stop,
                    link_at,
                });
            }
            if let Some(place) = line.strip_prefix("link-at ")
                && link_at.is_none()
            {
                let (f, opline) = place.split_once(' ').ok_or_else(|| refused(at, line))?;
                let f = parse_func(f).ok_or_else(|| refused(at, line))?;
                let opline = opline.parse().map_err(|_| refused(at, line))?;
                link_at = Some((f, OplineIdx(opline)));
                continue;
            }
            if let Some(slots) = line.strip_prefix("entered ")
                && events.is_empty()
                && entered.is_empty()
            {
                entered = parse_entered(slots).ok_or_else(|| refused(at, line))?;
                continue;
            }
            events.push(parse_event(line).ok_or_else(|| refused(at, line))?);
        }
        Err(refused(0, "no stop"))
    }
}

fn refused(line: usize, found: &str) -> ParseError {
    ParseError {
        line,
        found: found.to_owned(),
    }
}

fn parse_func(text: &str) -> Option<FuncRef> {
    let offset = if let Some(n) = text.strip_prefix('e') {
        MapPtrOffset::Engine(n.parse().ok()?)
    } else {
        MapPtrOffset::Request(text.strip_prefix('r')?.parse().ok()?)
    };
    Some(FuncRef::at(offset))
}

/// A kind's name in the text form.
fn kind_name(tag: ZvalType) -> &'static str {
    KINDS
        .iter()
        .find(|(kind, _)| *kind == tag)
        .map_or("undef", |(_, name)| name)
}

/// The slots an `entered` line names, `var:kind` each; none for a text
/// that names none or a word it does not know.
fn parse_entered(text: &str) -> Option<Vec<Entered>> {
    let slots: Option<Vec<Entered>> = (text.split(' '))
        .map(|slot| {
            let (var, name) = slot.split_once(':')?;
            let tag = KINDS.iter().find(|(_, n)| *n == name)?.0;
            Some(Entered {
                var: var.parse().ok()?,
                tag,
            })
        })
        .collect();
    slots.filter(|slots| !slots.is_empty())
}

/// A sample in the text form.
fn sample(sampled: Option<Sample>) -> String {
    let Some(sampled) = sampled else {
        return "-".to_owned();
    };
    let mut text = kind_name(sampled.kind).to_owned();
    for (on, mark) in [
        (sampled.packed, ".packed"),
        (sampled.reference, ".reference"),
        (sampled.indirect, ".indirect"),
    ] {
        if on {
            text.push_str(mark);
        }
    }
    if let Some(class) = sampled.class {
        write!(text, "#{}", tier::class_number(class)).unwrap_or_default();
    }
    text
}

/// A sample's text: none for `-`, and `Err` for a text that is no
/// sample.
fn parse_sample(text: &str) -> Result<Option<Sample>, Unknown> {
    if text == "-" {
        return Ok(None);
    }
    let (marks, class) = match text.split_once('#') {
        Some((marks, class)) => {
            let number = class.parse().map_err(|_| Unknown)?;
            (marks, Some(tier::numbered_class(number)))
        }
        None => (text, None),
    };
    let mut parts = marks.split('.');
    let name = parts.next().ok_or(Unknown)?;
    let kind = KINDS.iter().find(|(_, n)| *n == name).ok_or(Unknown)?.0;
    let mut sampled = Sample {
        kind,
        indirect: false,
        reference: false,
        packed: false,
        class,
    };
    for mark in parts {
        match mark {
            "packed" => sampled.packed = true,
            "reference" => sampled.reference = true,
            "indirect" => sampled.indirect = true,
            _ => return Err(Unknown),
        }
    }
    Ok(Some(sampled))
}

/// A word the text form does not know.
struct Unknown;

fn parse_start(text: &str) -> Option<Start> {
    match text {
        "loop" => Some(Start::Loop),
        "enter" => Some(Start::Enter),
        "return" => Some(Start::Return),
        _ => {
            let (parent, exit) = text.strip_prefix("side ")?.split_once('/')?;
            Some(Start::Side {
                parent: TraceId::new(parent.parse().ok()?)?,
                exit: ExitId(exit.parse().ok()?),
            })
        }
    }
}

fn parse_stop(text: &str) -> Option<Stop> {
    if let Some(to) = text.strip_prefix("link ") {
        return TraceId::new(to.parse().ok()?).map(Stop::Link);
    }
    (1..=u8::MAX)
        .map_while(Stop::of_code)
        .find(|stop| stop.describe() == text)
}

fn parse_event(line: &str) -> Option<Event> {
    let mut words = line.split(' ');
    let event = match words.next()? {
        "op" => Event::Op {
            func: parse_func(words.next()?)?,
            opline: OplineIdx(words.next()?.parse().ok()?),
            op1: parse_sample(words.next()?).ok()?,
            op2: parse_sample(words.next()?).ok()?,
            data: parse_sample(words.next()?).ok()?,
            branch: match words.next()? {
                "-" => None,
                "taken" => Some(true),
                "fell" => Some(false),
                _ => return None,
            },
        },
        "init" => Event::InitCall {
            target: match words.next()? {
                "-" => None,
                f => Some(parse_func(f)?),
            },
        },
        "enter" => Event::Enter {
            func: parse_func(words.next()?)?,
        },
        "back" => Event::Back,
        _ => return None,
    };
    words.next().is_none().then_some(event)
}
