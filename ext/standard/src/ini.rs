//! The callbacks `parse_ini_string` and `parse_ini_file` hand php's INI
//! grammar, `php_simple_ini_parser_cb` and
//! `php_ini_parser_cb_with_sections` (`Z/ext/standard/basic_functions.c`),
//! over the events of the one grammar there is, the engine's
//! ([`php_ext::syntax::parse_ini`]), which reads its startup entries and
//! every `-d` by the same rules.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::syntax::{IniEvent as Event, IniScanner, IniValue as Scalar};
use php_ext::values::{Key, PhpArray, PhpString, Value};

use crate::basic_functions::{INI_SCANNER_NORMAL, INI_SCANNER_RAW, INI_SCANNER_TYPED};

/// The scanner an `INI_SCANNER_*` number names, or none.
pub(super) fn scanner(mode: i64) -> Option<IniScanner> {
    match mode {
        INI_SCANNER_NORMAL => Some(IniScanner::Normal),
        INI_SCANNER_RAW => Some(IniScanner::Raw),
        INI_SCANNER_TYPED => Some(IniScanner::Typed),
        _ => None,
    }
}

/// A key of the table the callbacks build.
#[derive(Clone, PartialEq, Debug)]
enum Slot {
    Int(i64),
    Str(Vec<u8>),
}

impl Slot {
    /// `zend_symtable_update`'s key: a canonical decimal integer in
    /// range is an index, anything else the string.
    fn symtable(bytes: &[u8]) -> Slot {
        let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
        let canonical = !digits.is_empty()
            && digits.iter().all(u8::is_ascii_digit)
            && (digits[0] != b'0' || (digits.len() == 1 && digits.len() == bytes.len()));
        if canonical && let Ok(n) = std::str::from_utf8(bytes).unwrap_or("x").parse::<i64>() {
            return Slot::Int(n);
        }
        Slot::Str(bytes.to_vec())
    }
}

/// One element: a value or a nested table.
enum Node {
    Leaf(Scalar),
    Table(Table),
}

/// The array a callback writes into, in insertion order with php's
/// `nNextFreeElement`.
struct Table {
    entries: Vec<(Slot, Node)>,
    next: Option<i64>,
}

impl Table {
    fn new() -> Table {
        Table {
            entries: Vec::new(),
            next: None,
        }
    }

    fn find(&self, key: &Slot) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == key)
    }

    /// `zend_hash_update`: in place where present, at the end otherwise.
    fn update(&mut self, key: Slot, node: Node) -> usize {
        if let Slot::Int(n) = key
            && self.next.is_none_or(|next| n >= next)
        {
            self.next = Some(n.saturating_add(1));
        }
        if let Some(at) = self.find(&key) {
            self.entries[at].1 = node;
            at
        } else {
            self.entries.push((key, node));
            self.entries.len() - 1
        }
    }

    /// `add_next_index_zval`.
    fn append(&mut self, node: Node) {
        let key = self.next.unwrap_or(0);
        self.update(Slot::Int(key), node);
    }

    /// `php_simple_ini_parser_cb`.
    fn simple(&mut self, event: Event) {
        match event {
            Event::Entry(label, Some(value)) => {
                self.update(Slot::symtable(&label_bytes(&label)), Node::Leaf(value));
            }
            Event::PopEntry(label, value, offset) => {
                let name = label_bytes(&label);
                let numeric = !(name.len() > 1 && name[0] == b'0')
                    && matches!(
                        php_ext::coerce::numeric_string(&name)
                            .as_ref()
                            .map(Value::as_ref),
                        Some(php_ext::values::ValueRef::Int(_))
                    );
                let key = match php_ext::coerce::numeric_string(&name)
                    .as_ref()
                    .map(Value::as_ref)
                {
                    Some(php_ext::values::ValueRef::Int(n)) if numeric => Slot::Int(n),
                    _ => Slot::Str(name),
                };
                let at = match self.find(&key) {
                    Some(at) => at,
                    None => self.update(key, Node::Table(Table::new())),
                };
                if !matches!(self.entries[at].1, Node::Table(_)) {
                    self.entries[at].1 = Node::Table(Table::new());
                }
                let Node::Table(inner) = &mut self.entries[at].1 else {
                    return;
                };
                match &offset {
                    Scalar::Str(o) if !o.is_empty() => {
                        inner.update(Slot::symtable(o), Node::Leaf(value));
                    }
                    _ => inner.append(Node::Leaf(value)),
                }
            }
            Event::Entry(_, None) | Event::Section(_) => {}
        }
    }
}

/// A label's bytes: labels are strings from the scanner.
fn label_bytes(val: &Scalar) -> Vec<u8> {
    match val {
        Scalar::Str(s) => s.clone(),
        _ => Vec::new(),
    }
}

/// The callbacks' array for the parse's events:
/// `php_simple_ini_parser_cb` over them all, or with `sections`
/// `php_ini_parser_cb_with_sections`, a section's entries into the
/// section's own array from its header on.
pub(super) fn assemble(
    req: &mut Request,
    into: PhpArray,
    events: Vec<Event>,
    sections: bool,
) -> Result<(), Thrown> {
    let mut top = Table::new();
    let mut active: Option<usize> = None;
    for event in events {
        if !sections {
            top.simple(event);
            continue;
        }
        match event {
            Event::Section(name) => {
                let at = top.update(
                    Slot::symtable(&label_bytes(&name)),
                    Node::Table(Table::new()),
                );
                active = Some(at);
            }
            Event::Entry(_, None) => {}
            event => match active {
                Some(at) => {
                    if let Node::Table(section) = &mut top.entries[at].1 {
                        section.simple(event);
                    }
                }
                None => top.simple(event),
            },
        }
    }
    fill(req, into, top)
}

/// The table written into `array`. Every name and string value is a
/// string of the request's however short, as the scanner's
/// `zend_ini_copy_value` makes each token's with `zend_string_init` and
/// the callbacks keep it.
fn fill(req: &mut Request, mut array: PhpArray, table: Table) -> Result<(), Thrown> {
    for (slot, node) in table.entries {
        let key = match slot {
            Slot::Int(n) => Key::Int(n),
            Slot::Str(s) => Key::Str(token(req, &s)?),
        };
        let value = match node {
            Node::Table(inner) => {
                let nested = PhpArray::new(req)?;
                fill(req, nested, inner).inspect_err(|_| req.release(Value::array(nested)))?;
                Value::array(nested)
            }
            Node::Leaf(val) => match val {
                Scalar::Str(s) => Value::string(token(req, &s)?),
                Scalar::Bool(b) => Value::bool(b),
                Scalar::Null => Value::null(),
                Scalar::Long(n) => Value::int(n),
                Scalar::Double(d) => Value::float(d),
            },
        };
        array.set(req, key, value)?;
    }
    Ok(())
}

/// `zend_ini_copy_value`'s `zend_string_init(str, len, ZEND_SYSTEM_INI)`
/// for a request's parse: no interned string for the empty one or one of
/// a byte.
fn token(req: &mut Request, bytes: &[u8]) -> Result<PhpString, Thrown> {
    PhpString::filled(req, bytes.len(), |b| b.copy_from_slice(bytes))
}
