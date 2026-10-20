//! The callbacks `parse_ini_string` and `parse_ini_file` hand php's INI
//! grammar, `php_simple_ini_parser_cb` and
//! `php_ini_parser_cb_with_sections` (`Z/ext/standard/basic_functions.c`),
//! called with each statement of the one grammar there is, the engine's
//! ([`php_ext::syntax::parse_ini`]), as it is reduced, so the array grows
//! beside the scanner's strings as php's does.

use php_ext::coerce::numeric_string;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::syntax::{IniEvent as Event, IniScanner};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

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

/// The callback's array and, with sections, `BG(active_ini_file_section)`,
/// the section array a header laid down, which entries go into from
/// there on.
pub(super) struct Callback {
    top: PhpArray,
    sections: bool,
    active: Option<PhpArray>,
}

impl Callback {
    /// The callback over `top`, `php_ini_parser_cb_with_sections` where
    /// `sections` is set and `php_simple_ini_parser_cb` otherwise.
    pub(super) const fn new(top: PhpArray, sections: bool) -> Callback {
        Callback {
            top,
            sections,
            active: None,
        }
    }

    /// One statement's event. The strings are the parser's, given up
    /// after; what the array keeps takes an owner of its own.
    ///
    /// The scanner's token strings are made, kept and given up as php's
    /// grammar does, and its state stack allocated as it grows, so a
    /// parse that is its own peak reads php's:
    ///
    /// ```php
    /// $p = tempnam(sys_get_temp_dir(), 'ini');
    /// foreach (["a = 1\nb = on\nc = \"q\"\n", "d[] = 1\nd[] = 2\n"] as $c) {
    ///     file_put_contents($p, $c); parse_ini_file($p, true);
    ///     memory_reset_peak_usage(); $m = memory_get_usage(); $a = parse_ini_file($p, true);
    ///     var_dump(memory_get_peak_usage() - $m, memory_get_usage() - $m); unset($a);
    ///     memory_reset_peak_usage(); $m = memory_get_usage(); $a = parse_ini_string($c, true);
    ///     var_dump(memory_get_peak_usage() - $m); unset($a);
    /// }
    /// unlink($p);
    /// ```
    /// ```expect
    /// int(1376)
    /// int(568)
    /// int(720)
    /// int(1424)
    /// int(688)
    /// int(768)
    /// ```
    pub(super) fn event(&mut self, req: &mut Request, event: Event<Value>) -> Result<(), Thrown> {
        if !self.sections {
            return simple(req, self.top, &event);
        }
        match event {
            Event::Section(name) => {
                let section = PhpArray::new(req)?;
                let key = key_of(req, &name)?;
                let held = *section;
                self.top.set(req, key, section.into_value())?;
                let section = held;
                self.active = Some(section);
                Ok(())
            }
            Event::Entry(_, None) => Ok(()),
            event => simple(req, self.active.unwrap_or(self.top), &event),
        }
    }
}

/// `zend_symtable_update`'s key for a label: an index for a canonical
/// decimal integer, the string otherwise, with the owner the table takes.
fn key_of(req: &mut Request, label: &Value) -> Result<Owner<Key>, Thrown> {
    match label.as_php_string() {
        Some(s) => Ok(Owner::take(req, Key::lookup(s))),
        None => Ok(PhpString::new(req, b"")?.into_key()),
    }
}

/// `php_simple_ini_parser_cb` into `arr`.
fn simple(req: &mut Request, mut arr: PhpArray, event: &Event<Value>) -> Result<(), Thrown> {
    match event {
        Event::Entry(label, Some(value)) => {
            let key = key_of(req, label)?;
            arr.set(req, key, Owner::take(req, *value))
        }
        Event::PopEntry(label, value, offset) => {
            let name = label.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
            // `x[a]=b` where `x` might need to be an array index.
            let index = match numeric_string(&name).as_ref().map(Value::as_ref) {
                Some(ValueRef::Int(n)) if !(name.len() > 1 && name[0] == b'0') => Some(n),
                _ => None,
            };
            let key = match (index, label.as_php_string()) {
                (Some(n), _) => Owner::index(n),
                (None, Some(s)) => Owner::take(req, Key::Str(s)),
                (None, None) => PhpString::new(req, b"")?.into_key(),
            };
            let found = arr.get(&key).and_then(|v| v.as_array());
            let mut inner = if let Some(inner) = found {
                key.give_up(req);
                inner
            } else {
                let inner = match PhpArray::new(req) {
                    Ok(inner) => inner,
                    Err(thrown) => {
                        key.give_up(req);
                        return Err(thrown);
                    }
                };
                let held = *inner;
                arr.set(req, key, inner.into_value())?;
                held
            };
            let value = Owner::take(req, *value);
            match offset.as_php_string() {
                Some(o) if !o.as_bytes().is_empty() => {
                    let key = Owner::take(req, Key::lookup(o));
                    inner.set(req, key, value)
                }
                _ => {
                    inner.try_append(req, value)?;
                    Ok(())
                }
            }
        }
        Event::Entry(_, None) | Event::Section(_) => Ok(()),
    }
}
