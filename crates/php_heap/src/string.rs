//! Interned byte strings (design §5.2, §5.14): what a `CONST` operand of
//! an op array holds and what a [`crate::Zval`] of type `String` names.
//!
//! php allocates a compiled program's strings with `persistent = 1` and
//! keeps them in one interned table that outlives every request and is
//! never freed; `zend_string`'s refcount is untouched for them. That is
//! all of what is here: one append-only table, so a `Zval` stays sixteen
//! bytes, `Copy` and self-contained while `php_heap` is still safe. The
//! request-scoped, refcounted `zend_string` this shares its type byte
//! with lands with the allocator, and the table moves onto the engine
//! where design §5.14 puts it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One interned byte string: its place in the table, not a pointer, so
/// naming one is `Copy` and fits a zval's payload word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Str(u32);

/// The table: the bytes in the order they were first asked for, and the
/// place of each, so asking twice names one string.
#[derive(Default)]
struct Table {
    bytes: Vec<&'static [u8]>,
    at: HashMap<&'static [u8], u32>,
}

fn table() -> &'static Mutex<Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(Table::default()))
}

/// The table, taken as it stands after a thread ended while holding it:
/// what is here is append-only, so it was left consistent.
fn locked() -> std::sync::MutexGuard<'static, Table> {
    match table().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Str {
    /// The string for these bytes, interned on the first ask.
    #[must_use]
    pub fn intern(bytes: &[u8]) -> Str {
        let mut table = locked();
        if let Some(at) = table.at.get(bytes) {
            return Str(*at);
        }
        let kept: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
        let at = u32::try_from(table.bytes.len()).unwrap_or(u32::MAX);
        table.bytes.push(kept);
        table.at.insert(kept, at);
        Str(at)
    }

    /// The bytes. An interned string is never freed, so the borrow is
    /// `'static` and no lifetime travels with a zval.
    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        locked()
            .bytes
            .get(self.0 as usize)
            .copied()
            .unwrap_or_default()
    }

    /// The place in the table, as a zval's payload word holds it.
    pub(crate) const fn payload(self) -> u64 {
        self.0 as u64
    }

    /// The string a zval's payload word names.
    pub(crate) fn from_payload(payload: u64) -> Str {
        Str(u32::try_from(payload).unwrap_or(u32::MAX))
    }

    /// The length in bytes, `strlen`.
    #[must_use]
    pub fn len(self) -> usize {
        self.bytes().len()
    }

    /// Whether the length is zero.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.bytes().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_the_same_bytes_twice_gives_one_handle() {
        let first = Str::intern(b"var_dump");
        let again = Str::intern(b"var_dump");
        let other = Str::intern(b"extension_loaded");
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(first.bytes(), b"var_dump");
        assert_eq!(other.len(), 16);
        assert!(Str::intern(b"").is_empty());
    }
}
