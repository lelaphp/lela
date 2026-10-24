//! `EG(ini_directives)` as the engine holds it: every entry in
//! registration order, the engine's own first and then each extension's
//! in bundle order, filed by name as php's `HashTable` files them, so an
//! entry is found by one hash and not by a walk of the table. Laid and
//! filed as the engine is built, its values altered only while no
//! request can see it
//! ([`crate::engine::Engine::alter_master_ini`]); immutable and shared
//! by every request after (design §3.4).

use php_link::{Exact, Named, Names};

use crate::api::abi::{ExtensionEntry, IniHandler};
use crate::api::request::ExtensionId;

/// One entry, php's `zend_ini_entry` as every request starts with it.
pub(crate) struct Directive {
    pub(crate) name: Vec<u8>,
    /// The master value, `None` where it has none (php's `NULL`).
    pub(crate) value: Option<Vec<u8>>,
    /// The declaration of an extension's entry: the extension,
    /// `module_number`, and the entry's place among its declarations;
    /// `None` for an entry of the core table.
    pub(crate) declared: Option<(ExtensionId, usize)>,
    /// `on_modify`, beside the extension that gave it.
    pub(crate) on_modify: Option<(ExtensionId, IniHandler)>,
}

impl Named for Directive {
    fn name(&self) -> &[u8] {
        &self.name
    }
}

/// The entry's place in the table, which it keeps for the engine's
/// life: a request names an entry it changed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectiveAt(u32);

/// The table: its rows in registration order and their [`Names`].
pub(crate) struct IniTable {
    rows: Vec<Directive>,
    names: Names<Exact>,
}

impl IniTable {
    /// `zend_register_ini_entries` over the core table `core`, each name
    /// beside its value, then each extension's declarations, the bundle
    /// `extensions` numbered from 1 as `module_registry` numbers it past
    /// `Core`, each entry's `on_modify` the first extension's that gave
    /// one.
    pub(crate) fn register(
        core: impl Iterator<Item = (Vec<u8>, Option<Vec<u8>>)>,
        extensions: &[ExtensionEntry],
    ) -> IniTable {
        let numbered = || (1..).map(ExtensionId::at).zip(extensions);
        let declared = numbered().flat_map(|(extension, entry)| {
            (entry.ini.iter().enumerate()).map(move |(at, decl)| {
                let value = decl.default.as_deref().map(<[u8]>::to_vec);
                (decl.name.0.to_vec(), value, Some((extension, at)))
            })
        });
        let rows: Vec<Directive> = (core.map(|(name, value)| (name, value, None)))
            .chain(declared)
            .map(|(name, value, declared)| Directive {
                name,
                value,
                declared,
                on_modify: None,
            })
            .collect();
        let mut table = IniTable {
            names: Names::over(&rows),
            rows,
        };
        for (extension, entry) in numbered() {
            for (name, handler) in &entry.on_modify {
                if let Some(row) = table.find_mut(name) {
                    row.on_modify.get_or_insert((extension, *handler));
                }
            }
        }
        table
    }

    /// The rows in the table's order.
    pub(crate) fn rows(&self) -> &[Directive] {
        &self.rows
    }

    /// `zend_hash_find(EG(ini_directives), name)`'s place.
    pub(crate) fn at(&self, name: &[u8]) -> Option<DirectiveAt> {
        let at = self.names.find(&self.rows, name)?;
        u32::try_from(at).ok().map(DirectiveAt)
    }

    /// The entry at `at`.
    pub(crate) fn row(&self, at: DirectiveAt) -> Option<&Directive> {
        self.rows.get(usize::try_from(at.0).ok()?)
    }

    /// The entry `name`.
    pub(crate) fn find(&self, name: &[u8]) -> Option<&Directive> {
        self.rows.get(self.names.find(&self.rows, name)?)
    }

    fn find_mut(&mut self, name: &[u8]) -> Option<&mut Directive> {
        (self.names.find(&self.rows, name)).and_then(|at| self.rows.get_mut(at))
    }

    /// The master value of `name` as `zend_ini_get_value` reads it, the
    /// empty string where it has none (php's `NULL`); `None` where php
    /// has no such entry.
    pub(crate) fn master(&self, name: &[u8]) -> Option<&[u8]> {
        self.find(name)
            .map(|row| row.value.as_deref().unwrap_or_default())
    }

    /// The master value of the entry `name` set to `value` where it
    /// stands; a name the table holds no entry under is none, as
    /// `zend_alter_ini_entry` finds none, so the names are filed once.
    pub(crate) fn set(&mut self, name: &[u8], value: Option<Vec<u8>>) {
        if let Some(row) = self.find_mut(name) {
            row.value = value;
        }
    }
}
