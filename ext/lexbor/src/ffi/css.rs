//! lexbor's CSS selector parser (`lexbor/css`), as
//! `parentnode/css_selectors.c` drives it, with the parsed
//! `lxb_css_selector_list_t` copied into an owned tree, so the matcher
//! ext/dom carries over from `selectors-adapted/selectors.c` reads Rust
//! values and no C pointer outlives the parser.
//!
//! The shape is lexbor's: a [`List`] is the comma-separated complex
//! selectors (`lxb_css_selector_list_t` chained by `next`); a [`Complex`]
//! is one of them, its simple selectors in source order, each with the
//! combinator that joins it to the one before (`Close` inside one
//! compound selector) and the complex selector's specificity.

use std::ptr;

use super::{Lent, sys};

/// The comma-separated complex selectors of one selector list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct List(pub Vec<Complex>);

impl std::ops::Deref for List {
    type Target = [Complex];

    fn deref(&self) -> &[Complex] {
        &self.0
    }
}

/// One complex selector, `lxb_css_selector_list_t`: its simple selectors
/// from `first` along `next`, and `specificity`
/// (`lxb_css_selector_specificity_t`, the packed form lexbor's
/// `lxb_css_selector_sp_*` macros read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Complex {
    pub selectors: Vec<Selector>,
    pub specificity: u32,
}

/// One simple selector, `lxb_css_selector_t`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    /// The combinator between this selector and the one before it.
    pub combinator: Combinator,
    /// `name`: the element name, id, class or attribute name.
    pub name: Vec<u8>,
    /// `ns`: the namespace prefix written before `|`, `None` where none
    /// was.
    pub ns: Option<Vec<u8>>,
    pub kind: Kind,
}

/// `lxb_css_selector_combinator_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    /// Whitespace.
    Descendant,
    /// Two simple selectors of one compound selector.
    Close,
    /// `>`.
    Child,
    /// `+`.
    Sibling,
    /// `~`.
    Following,
    /// `||`, the column combinator (lexbor's `CELL`).
    Column,
}

/// `lxb_css_selector_type_t` with the data each type carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `*`.
    Any,
    /// A tag name.
    Element,
    /// `#id`.
    Id,
    /// `.class`.
    Class,
    /// `[name]`, with the matcher where a value is given.
    Attribute(Option<AttributeMatch>),
    /// `:name`.
    PseudoClass(PseudoClass),
    /// `:name(...)`.
    PseudoClassFunction(PseudoClassFunction),
    /// `::name`.
    PseudoElement(PseudoElement),
    /// `::name(...)`: lexbor names none.
    PseudoElementFunction,
}

/// `lxb_css_selector_attribute_t` where a value is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeMatch {
    pub matcher: Match,
    pub modifier: Modifier,
    pub value: Vec<u8>,
}

/// `lxb_css_selector_match_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// `=`.
    Equal,
    /// `~=`.
    Include,
    /// `|=`.
    Dash,
    /// `^=`.
    Prefix,
    /// `$=`.
    Suffix,
    /// `*=`.
    Substring,
}

/// `lxb_css_selector_modifier_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Unset,
    /// ` i`: ASCII case-insensitive.
    I,
    /// ` s`: case-sensitive.
    S,
}

/// `lxb_css_syntax_anb_t` with the `of S` list of `:nth-child` and
/// `:nth-last-child` (`lxb_css_selector_anb_of_t`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anb {
    pub a: i64,
    pub b: i64,
    pub of: Option<List>,
}

/// `lxb_css_selector_pseudo_class_function_id_t` with its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PseudoClassFunction {
    Current(List),
    /// `:dir()`, whose argument the matcher does not read.
    Dir,
    Has(List),
    Is(List),
    /// `:lang()`, whose argument the matcher does not read.
    Lang,
    /// `:lexbor-contains()`, lexbor's own, which the matcher does not read.
    LexborContains,
    Not(List),
    NthChild(Anb),
    NthCol(Anb),
    NthLastChild(Anb),
    NthLastCol(Anb),
    NthLastOfType(Anb),
    NthOfType(Anb),
    Where(List),
    /// An id past lexbor's.
    Other(u32),
}

macro_rules! ids {
    ($(#[$meta:meta])* $name:ident { $($variant:ident),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($variant,)*
            /// `__UNDEF` or an id past lexbor's.
            Other(u32),
        }

        impl $name {
            fn from_raw(raw: u32) -> $name {
                const ALL: &[$name] = &[$($name::$variant),*];
                usize::try_from(raw)
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| ALL.get(i).copied())
                    .unwrap_or($name::Other(raw))
            }
        }
    };
}

ids! {
    /// `lxb_css_selector_pseudo_class_id_t`, from 1 in lexbor's order.
    PseudoClass {
        Active, AnyLink, Blank, Checked, Current, Default, Disabled, Empty, Enabled,
        FirstChild, FirstOfType, Focus, FocusVisible, FocusWithin, Fullscreen, Future,
        Hover, InRange, Indeterminate, Invalid, LastChild, LastOfType, Link, LocalLink,
        OnlyChild, OnlyOfType, Optional, OutOfRange, Past, PlaceholderShown, ReadOnly,
        ReadWrite, Required, Root, Scope, Target, TargetWithin, UserInvalid, Valid,
        Visited, Warning,
    }
}

ids! {
    /// `lxb_css_selector_pseudo_element_id_t`, from 1 in lexbor's order.
    PseudoElement {
        After, Backdrop, Before, FirstLetter, FirstLine, GrammarError, InactiveSelection,
        Marker, Placeholder, Selection, SpellingError, TargetText,
    }
}

/// Why a selector list was refused: lexbor's first log message, which php
/// shows as `Invalid selector (<message>)`, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub message: Option<Vec<u8>>,
}

impl List {
    /// `dom_parse_selector`: `lxb_css_parser_init`,
    /// `lxb_css_selectors_parse`, the list copied out and
    /// `lxb_css_selector_list_destroy_memory`; the first log message where
    /// lexbor refused the text.
    ///
    /// # Errors
    /// [`Refused`], with lexbor's first log message where it logged one.
    pub fn parse(_: &Lent, text: &[u8]) -> Result<List, Refused> {
        parse(text)
    }
}

/// [`List::parse`]'s body, under the lend its caller proved.
fn parse(text: &[u8]) -> Result<List, Refused> {
    // SAFETY: no argument; the parser is this function's.
    let parser = unsafe { sys::lxb_css_parser_create() };
    if parser.is_null() {
        return Err(Refused { message: None });
    }
    // SAFETY: a parser just created, with lexbor's own tokenizer.
    let status = unsafe { sys::lxb_css_parser_init(parser, ptr::null_mut()) };
    let outcome = if status == sys::LXB_STATUS_OK {
        // SAFETY: an initialised parser and the live bytes of `text`.
        let list = unsafe { sys::lxb_css_selectors_parse(parser, text.as_ptr(), text.len()) };
        if list.is_null() {
            Err(Refused {
                message: first_message(parser),
            })
        } else {
            // SAFETY: the list lexbor just made, read before it is freed.
            let copied = unsafe { copy_list(list) };
            // SAFETY: the list's memory, freed once.
            unsafe { sys::lxb_css_selector_list_destroy_memory(list) };
            Ok(copied)
        }
    } else {
        Err(Refused { message: None })
    };
    // SAFETY: the parser this function made, destroyed once with itself.
    unsafe { sys::lxb_css_parser_destroy(parser, true) };
    outcome
}

/// The first message of the parser's log, `parser->log->messages[0]`.
fn first_message(parser: *mut std::ffi::c_void) -> Option<Vec<u8>> {
    // SAFETY: a live parser.
    let log = unsafe { sys::lxb_css_parser_log_noi(parser) };
    // SAFETY: null or the parser's live log.
    let log = unsafe { log.as_ref() }?;
    let messages = &log.messages;
    if messages.length == 0 || messages.list.is_null() {
        return None;
    }
    let msg = messages.list.cast::<sys::lxb_css_log_message_t>();
    // SAFETY: the array holds at least one message at its head.
    let text = unsafe { &(*msg).text };
    Some(str_bytes(text.data, text.length))
}

/// A list chained from `list` along `next`.
///
/// # Safety
/// `list` is null or a live selector list lexbor made.
unsafe fn copy_list(mut list: *const sys::lxb_css_selector_list_t) -> List {
    let mut out = List::default();
    // SAFETY: the caller's contract, carried along `next`.
    while let Some(l) = unsafe { list.as_ref() } {
        let mut selectors = Vec::new();
        let mut at = l.first.cast_const();
        // SAFETY: the list's selectors, from `first` along `next`.
        while let Some(s) = unsafe { at.as_ref() } {
            // SAFETY: a live selector of the list.
            selectors.push(unsafe { copy_selector(s) });
            at = s.next;
        }
        out.0.push(Complex {
            selectors,
            specificity: l.specificity,
        });
        list = l.next;
    }
    out
}

/// One selector.
///
/// # Safety
/// `s` is a live selector lexbor made.
unsafe fn copy_selector(s: &sys::lxb_css_selector_t) -> Selector {
    let kind = match s.type_ {
        sys::LXB_CSS_SELECTOR_TYPE_ANY => Kind::Any,
        sys::LXB_CSS_SELECTOR_TYPE_ELEMENT => Kind::Element,
        sys::LXB_CSS_SELECTOR_TYPE_ID => Kind::Id,
        sys::LXB_CSS_SELECTOR_TYPE_CLASS => Kind::Class,
        sys::LXB_CSS_SELECTOR_TYPE_ATTRIBUTE => {
            // SAFETY: an attribute selector's union holds its attribute.
            let a = unsafe { s.u.attribute };
            Kind::Attribute((!a.value.data.is_null()).then(|| AttributeMatch {
                matcher: match a.match_ {
                    1 => Match::Include,
                    2 => Match::Dash,
                    3 => Match::Prefix,
                    4 => Match::Suffix,
                    5 => Match::Substring,
                    _ => Match::Equal,
                },
                modifier: match a.modifier {
                    1 => Modifier::I,
                    2 => Modifier::S,
                    _ => Modifier::Unset,
                },
                value: str_bytes(a.value.data, a.value.length),
            }))
        }
        sys::LXB_CSS_SELECTOR_TYPE_PSEUDO_CLASS => {
            // SAFETY: a pseudo-class selector's union holds its pseudo.
            let p = unsafe { s.u.pseudo };
            Kind::PseudoClass(PseudoClass::from_raw(p.type_))
        }
        sys::LXB_CSS_SELECTOR_TYPE_PSEUDO_CLASS_FUNCTION => {
            // SAFETY: as above.
            let p = unsafe { s.u.pseudo };
            // SAFETY: the data lexbor keeps for the function's id.
            Kind::PseudoClassFunction(unsafe { copy_function(p.type_, p.data) })
        }
        sys::LXB_CSS_SELECTOR_TYPE_PSEUDO_ELEMENT => {
            // SAFETY: as above.
            let p = unsafe { s.u.pseudo };
            Kind::PseudoElement(PseudoElement::from_raw(p.type_))
        }
        _ => Kind::PseudoElementFunction,
    };
    Selector {
        combinator: match s.combinator {
            1 => Combinator::Close,
            2 => Combinator::Child,
            3 => Combinator::Sibling,
            4 => Combinator::Following,
            5 => Combinator::Column,
            _ => Combinator::Descendant,
        },
        name: str_bytes(s.name.data, s.name.length),
        ns: (!s.ns.data.is_null()).then(|| str_bytes(s.ns.data, s.ns.length)),
        kind,
    }
}

/// A pseudo-class function's data: a selector list for the logical
/// functions, `lxb_css_selector_anb_of_t` for the `nth` ones.
///
/// # Safety
/// `data` is what lexbor keeps for the function `id`.
unsafe fn copy_function(id: u32, data: *mut std::ffi::c_void) -> PseudoClassFunction {
    let list = || {
        // SAFETY: the caller's contract: a selector list or null.
        unsafe { copy_list(data.cast::<sys::lxb_css_selector_list_t>()) }
    };
    #[allow(clippy::useless_conversion)] // `c_long` is `i64` where it is not `i32`
    let anb = || {
        // SAFETY: the caller's contract: an `anb_of` or null.
        let a = unsafe { data.cast::<sys::lxb_css_selector_anb_of_t>().as_ref() };
        a.map_or(
            Anb {
                a: 0,
                b: 0,
                of: None,
            },
            |a| Anb {
                a: a.a.into(),
                b: a.b.into(),
                // SAFETY: the `of` list, null or live.
                of: (!a.of.is_null()).then(|| unsafe { copy_list(a.of) }),
            },
        )
    };
    match id {
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_CURRENT => PseudoClassFunction::Current(list()),
        0x02 => PseudoClassFunction::Dir,
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_HAS => PseudoClassFunction::Has(list()),
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_IS => PseudoClassFunction::Is(list()),
        0x05 => PseudoClassFunction::Lang,
        0x06 => PseudoClassFunction::LexborContains,
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NOT => PseudoClassFunction::Not(list()),
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_CHILD => {
            PseudoClassFunction::NthChild(anb())
        }
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_COL => PseudoClassFunction::NthCol(anb()),
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_LAST_CHILD => {
            PseudoClassFunction::NthLastChild(anb())
        }
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_LAST_COL => {
            PseudoClassFunction::NthLastCol(anb())
        }
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_LAST_OF_TYPE => {
            PseudoClassFunction::NthLastOfType(anb())
        }
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_NTH_OF_TYPE => {
            PseudoClassFunction::NthOfType(anb())
        }
        sys::LXB_CSS_SELECTOR_PSEUDO_CLASS_FUNCTION_WHERE => PseudoClassFunction::Where(list()),
        other => PseudoClassFunction::Other(other),
    }
}

/// A copy of `length` bytes at `data`, empty for a null pointer.
fn str_bytes(data: *const u8, length: usize) -> Vec<u8> {
    if data.is_null() {
        return Vec::new();
    }
    // SAFETY: lexbor's strings are `length` bytes at `data`, live while the
    // list or log that holds them is.
    unsafe { std::slice::from_raw_parts(data, length) }.to_vec()
}
