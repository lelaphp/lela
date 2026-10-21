//! Which `#[cfg(..)]` hold for the target this harness is built for, read
//! as rustc reads them (#1068).
//!
//! The set is the one cargo gave the build: `build.rs` asks rustc for
//! `--print cfg` over the build's target and flags, as cargo itself does,
//! and the lines it printed are [`Target::built`]. The engine the witness
//! lane serves a fence through is linked into this binary, and the binary
//! that serves a fence marked `process` is built for the same host by the
//! same cargo, so a native whose `#[cfg]` this set denies is one neither
//! of them compiled, and no fence can enter it.
//!
//! A predicate is `name`, `name = "value"`, `any(..)`, `all(..)` or
//! `not(..)`, and a name the set does not carry is false, as it is to
//! rustc. One name is the crate's and not the target's: `feature`, whose
//! set is each crate's as the build resolved it and is not in rustc's
//! lines. A predicate over it is undecided here, and so is an attribute
//! the grammar cannot read; a reader of [`Target::admits`] keeps an item
//! whose `#[cfg]` is undecided, so what is not known to be compiled out
//! is never dropped unseen.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use syn::ext::IdentExt;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::{Ident, LitStr, Token};

/// What rustc printed for `--print cfg` over this build's target, written
/// by `build.rs`.
const PRINTED: &str = include_str!(concat!(env!("OUT_DIR"), "/target.cfg"));

/// The name whose set is a crate's and not the target's.
const FEATURE: &str = "feature";

/// One configuration predicate, as rustc reads the inside of `cfg(..)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Predicate {
    /// `name` or `name = "value"`.
    Option {
        name: String,
        value: Option<String>,
    },
    Any(Vec<Predicate>),
    All(Vec<Predicate>),
    Not(Box<Predicate>),
}

impl Predicate {
    /// The predicate of an attribute, its text between `#[` (or `#![`) and
    /// `]`, where the attribute is a `cfg(..)` the grammar reads; `None`
    /// for another attribute and for a `cfg` it cannot read.
    pub(crate) fn of_attribute(attribute: &str) -> Option<Predicate> {
        let attribute = attribute.trim();
        let rest = attribute.strip_prefix("cfg")?;
        if !rest.trim_start().starts_with('(') {
            return None;
        }
        let parser = |input: ParseStream| {
            input.parse::<Ident>()?;
            let content;
            syn::parenthesized!(content in input);
            let predicate = Predicate::parse(&content)?;
            // rustc takes one trailing comma after the one predicate.
            if content.peek(Token![,]) {
                content.parse::<Token![,]>()?;
            }
            if content.is_empty() {
                Ok(predicate)
            } else {
                Err(content.error("one predicate"))
            }
        };
        parser.parse_str(attribute).ok()
    }

    fn parse(input: ParseStream) -> syn::Result<Predicate> {
        let name = Ident::parse_any(input)?;
        if input.peek(syn::token::Paren) {
            let content;
            syn::parenthesized!(content in input);
            let list: Punctuated<Predicate, Token![,]> =
                Punctuated::parse_terminated_with(&content, Predicate::parse)?;
            let mut list: Vec<Predicate> = list.into_iter().collect();
            return match name.to_string().as_str() {
                "any" => Ok(Predicate::Any(list)),
                "all" => Ok(Predicate::All(list)),
                "not" if list.len() == 1 => Ok(Predicate::Not(Box::new(list.remove(0)))),
                _ => Err(syn::Error::new(name.span(), "any, all or not(one)")),
            };
        }
        let value = if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            Some(input.parse::<LitStr>()?.value())
        } else {
            None
        };
        Ok(Predicate::Option {
            name: name.to_string(),
            value,
        })
    }
}

/// The options set for one target, each a name with its value or none,
/// as `rustc --print cfg` prints them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Target {
    options: BTreeSet<(String, Option<String>)>,
}

impl Target {
    /// The set of the target this binary was built for.
    pub(crate) fn built() -> &'static Target {
        static BUILT: OnceLock<Target> = OnceLock::new();
        BUILT.get_or_init(|| Target::parse(PRINTED))
    }

    /// The set `rustc --print cfg` printed: one option a line, `name` or
    /// `name="value"`.
    pub(crate) fn parse(printed: &str) -> Target {
        let options = printed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| match line.split_once('=') {
                Some((name, value)) => (
                    name.trim().to_owned(),
                    Some(value.trim().trim_matches('"').to_owned()),
                ),
                None => (line.to_owned(), None),
            })
            .collect();
        Target { options }
    }

    /// Whether `predicate` holds for this target: `None` where it turns on
    /// what the target's set does not say, a crate's `feature`.
    pub(crate) fn holds(&self, predicate: &Predicate) -> Option<bool> {
        match predicate {
            Predicate::Option { name, .. } if name == FEATURE => None,
            Predicate::Option { name, value } => {
                Some(self.options.contains(&(name.clone(), value.clone())))
            }
            Predicate::Any(list) => self.decided(list, true),
            Predicate::All(list) => self.decided(list, false),
            Predicate::Not(p) => self.holds(p).map(|b| !b),
        }
    }

    /// `any` (`wins` true) or `all` (`wins` false), three-valued: a member
    /// decided `wins` decides the list whatever the undecided members say;
    /// otherwise an undecided member leaves it undecided.
    fn decided(&self, list: &[Predicate], wins: bool) -> Option<bool> {
        let mut undecided = false;
        for p in list {
            match self.holds(p) {
                Some(held) if held == wins => return Some(wins),
                None => undecided = true,
                Some(_) => {}
            }
        }
        (!undecided).then_some(!wins)
    }

    /// Whether the item an attribute stands on is compiled for this target
    /// as far as that attribute says: false only for a `cfg(..)` this set
    /// decides against; every other attribute, and a `cfg` it cannot read
    /// or decide, admits.
    pub(crate) fn admits(&self, attribute: &str) -> bool {
        Predicate::of_attribute(attribute)
            .is_none_or(|predicate| self.holds(&predicate) != Some(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What rustc prints for a Linux target and for a macOS one, trimmed to
    /// the lines the predicates below read.
    const LINUX: &str = "debug_assertions\npanic=\"unwind\"\ntarget_abi=\"\"\ntarget_family=\"unix\"\ntarget_os=\"linux\"\ntarget_vendor=\"unknown\"\nunix\n";
    const MACOS: &str =
        "target_family=\"unix\"\ntarget_os=\"macos\"\ntarget_vendor=\"apple\"\nunix\n";

    #[test]
    fn the_forms_rustc_reads_are_decided_against_the_printed_set() {
        let (linux, macos) = (Target::parse(LINUX), Target::parse(MACOS));
        for (attribute, on_linux, on_macos) in [
            ("cfg(unix)", true, true),
            ("cfg(windows)", false, false),
            ("cfg(target_vendor = \"apple\")", false, true),
            ("cfg(not(target_vendor = \"apple\"))", true, false),
            (
                "cfg(any(target_os = \"linux\", target_os = \"freebsd\"))",
                true,
                false,
            ),
            ("cfg(all(unix, not(target_os = \"macos\"),))", true, false),
            ("cfg(target_abi = \"\")", true, false),
            ("cfg(test)", false, false),
        ] {
            assert_eq!(linux.admits(attribute), on_linux, "{attribute} on linux");
            assert_eq!(macos.admits(attribute), on_macos, "{attribute} on macos");
        }
    }

    #[test]
    fn a_feature_or_an_unread_cfg_is_undecided_and_admits() {
        let linux = Target::parse(LINUX);
        let feature = Predicate::of_attribute("cfg(feature = \"x\")");
        assert_eq!(linux.holds(&feature.unwrap()), None);
        assert!(linux.admits("cfg(feature = \"x\")"));
        assert!(!linux.admits("cfg(all(windows, feature = \"x\"))"));
        assert!(linux.admits("cfg(any(windows, feature = \"x\"))"));
        assert!(linux.admits("cfg(not(feature = \"x\"))"));
        assert_eq!(Predicate::of_attribute("cfg(not(a, b))"), None);
        assert!(linux.admits("cfg(not(a, b))"));
        assert_eq!(Predicate::of_attribute("cfg_attr(windows, allow(x))"), None);
        assert_eq!(Predicate::of_attribute("php_function"), None);
    }

    /// The build's own set is rustc's for the target cargo built it for:
    /// on every host the lane runs on, a unix one.
    #[test]
    fn the_built_set_is_the_targets() {
        let built = Target::built();
        assert!(built.admits("cfg(unix)"), "{built:?}");
        assert!(!built.admits("cfg(windows)"), "{built:?}");
        assert_eq!(
            built.admits("cfg(target_os = \"macos\")"),
            cfg!(target_os = "macos")
        );
    }
}
