//! php's C-declaration parser, `ffi_parser.c` as llk generated it from
//! `ffi.g`, written by hand as the same recursive descent: one function
//! per rule, the same one-token lookahead with the same saved-position
//! backtracking where the grammar asks for more, and every error worded
//! as the generated parser words it. The callbacks it makes as it goes
//! are `ffi.c`'s, here as methods of the [`Parser`] over the request's
//! type arena ([`crate::types`]). An error ends the parse, where php's
//! `longjmp`s out of it.

use std::collections::HashMap;

use crate::types::{Field, Func, Int, Kind, Record, Type, TypeId, Types, attr, predefined};
use php_ext::abi::EngineState;

/// `zend_ffi_symbol_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub(crate) enum SymbolKind {
    Type,
    Const,
    Var,
    Func,
}

/// `zend_ffi_symbol`: a typedef, an enumerator, a variable or a function,
/// and the address `dlsym` found for the last two.
#[derive(Debug, Clone, EngineState)]
pub(crate) struct Symbol {
    pub(crate) kind: SymbolKind,
    pub(crate) is_const: bool,
    pub(crate) ty: TypeId,
    pub(crate) addr: usize,
    pub(crate) value: i64,
}

/// `zend_ffi_tag_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub(crate) enum TagKind {
    Enum,
    Struct,
    Union,
}

/// `zend_ffi_tag`.
#[derive(Debug, Clone, Copy, EngineState)]
pub(crate) struct Tag {
    pub(crate) kind: TagKind,
    pub(crate) ty: TypeId,
}

/// A `HashTable` keyed by name, in insertion order.
#[derive(Debug, Clone, EngineState)]
pub(crate) struct Table<T> {
    rows: Vec<(Vec<u8>, T)>,
    index: HashMap<Vec<u8>, usize>,
}

impl<T> Default for Table<T> {
    fn default() -> Table<T> {
        Table {
            rows: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<T> Table<T> {
    pub(crate) fn get(&self, name: &[u8]) -> Option<&T> {
        let at = *self.index.get(name)?;
        self.rows.get(at).map(|(_, value)| value)
    }

    /// `zend_hash_add`: false where the name is already taken.
    pub(crate) fn add(&mut self, name: &[u8], value: T) -> bool {
        if self.index.contains_key(name) {
            return false;
        }
        self.index.insert(name.to_vec(), self.rows.len());
        self.rows.push((name.to_vec(), value));
        true
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], &T)> {
        self.rows
            .iter()
            .map(|(name, value)| (name.as_slice(), value))
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (&[u8], &mut T)> {
        self.rows
            .iter_mut()
            .map(|(name, value)| (name.as_slice(), value))
    }
}

/// What a declaration is parsed against and declares into: php's
/// `FFI_G(symbols)` and `FFI_G(tags)`, each absent until the first
/// declaration makes it.
#[derive(Debug, Clone, Default, EngineState)]
pub(crate) struct Scope {
    pub(crate) symbols: Option<Table<Symbol>>,
    pub(crate) tags: Option<Table<Tag>>,
}

impl Scope {
    /// What php `emalloc`s for the tables `zend_ffi_declare`,
    /// `zend_ffi_add_enum_val` and `zend_ffi_declare_tag` fill, which
    /// `zend_ffi_free_obj` frees with the `FFI` object: each table's
    /// `HashTable`, 56, and the buckets `zend_hash_real_init_mixed` lays
    /// at its first add, eight or the power of two its rows double to,
    /// 32 bytes and two `uint32_t` hash slots each; then each
    /// `zend_ffi_symbol`, 24, and its key's `zend_string`, and each
    /// `zend_ffi_tag`, 16, and its name's, the one the type's
    /// `tag_name` shares.
    pub(crate) fn emallocs(&self) -> Vec<usize> {
        fn table<T>(sizes: &mut Vec<usize>, table: &Table<T>, row: usize) {
            sizes.push(56);
            if table.rows.is_empty() {
                return;
            }
            let slots = table.rows.len().next_power_of_two().max(8);
            sizes.push(slots * 32 + slots * 2 * 4);
            for (name, _) in &table.rows {
                sizes.push(row);
                sizes.push(24 + name.len() + 1);
            }
        }
        let mut sizes = Vec::new();
        if let Some(symbols) = &self.symbols {
            table(&mut sizes, symbols, 24);
        }
        if let Some(tags) = &self.tags {
            table(&mut sizes, tags, 16);
        }
        sizes
    }
}

/// `ZEND_FFI_DCL_*`, the bits of a declaration's `flags`.
pub(crate) mod dcl {
    pub(crate) const VOID: u32 = 1 << 0;
    pub(crate) const CHAR: u32 = 1 << 1;
    pub(crate) const SHORT: u32 = 1 << 2;
    pub(crate) const INT: u32 = 1 << 3;
    pub(crate) const LONG: u32 = 1 << 4;
    pub(crate) const LONG_LONG: u32 = 1 << 5;
    pub(crate) const FLOAT: u32 = 1 << 6;
    pub(crate) const DOUBLE: u32 = 1 << 7;
    pub(crate) const SIGNED: u32 = 1 << 8;
    pub(crate) const UNSIGNED: u32 = 1 << 9;
    pub(crate) const BOOL: u32 = 1 << 10;
    pub(crate) const COMPLEX: u32 = 1 << 11;
    pub(crate) const STRUCT: u32 = 1 << 12;
    pub(crate) const UNION: u32 = 1 << 13;
    pub(crate) const ENUM: u32 = 1 << 14;
    pub(crate) const TYPEDEF_NAME: u32 = 1 << 15;
    pub(crate) const TYPE_SPECIFIERS: u32 = (1 << 16) - 1;
    pub(crate) const TYPEDEF: u32 = 1 << 16;
    pub(crate) const EXTERN: u32 = 1 << 17;
    pub(crate) const STATIC: u32 = 1 << 18;
    pub(crate) const AUTO: u32 = 1 << 19;
    pub(crate) const REGISTER: u32 = 1 << 20;
    pub(crate) const STORAGE_CLASS: u32 = TYPEDEF | EXTERN | STATIC | AUTO | REGISTER;
    pub(crate) const CONST: u32 = 1 << 21;
    pub(crate) const RESTRICT: u32 = 1 << 22;
    pub(crate) const VOLATILE: u32 = 1 << 23;
    pub(crate) const ATOMIC: u32 = 1 << 24;
    pub(crate) const TYPE_QUALIFIERS: u32 = CONST | RESTRICT | VOLATILE | ATOMIC;
    pub(crate) const INLINE: u32 = 1 << 25;
    pub(crate) const NO_RETURN: u32 = 1 << 26;
}

/// `ZEND_FFI_ABI_*`, the calling conventions a declaration names.
mod abi {
    pub(super) const DEFAULT: u16 = 0;
    pub(super) const CDECL: u16 = 1;
    pub(super) const FASTCALL: u16 = 2;
    pub(super) const THISCALL: u16 = 3;
    pub(super) const STDCALL: u16 = 4;
    pub(super) const REGISTER: u16 = 6;
    pub(super) const MS: u16 = 7;
    pub(super) const SYSV: u16 = 8;
    pub(super) const VECTORCALL: u16 = 9;
}

/// `__BIGGEST_ALIGNMENT__`: sixteen on x86-64, eight on arm64.
#[cfg(target_arch = "aarch64")]
pub(crate) const BIGGEST_ALIGNMENT: u32 = 8;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const BIGGEST_ALIGNMENT: u32 = 16;

/// `zend_ffi_dcl`: what a declaration has said so far, and whether the
/// type it names is one it made (`ZEND_FFI_TYPE_OWNED`), which it may
/// change in place.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Dcl {
    pub(crate) flags: u32,
    pub(crate) align: u32,
    pub(crate) attr: u32,
    pub(crate) abi: u16,
    pub(crate) ty: Option<TypeId>,
    pub(crate) owned: bool,
}

impl Dcl {
    /// The nested declarator's start, `{ZEND_FFI_DCL_CHAR, 0, 0, 0,
    /// NULL}`: `char` is the hole the outer type fills.
    fn nested() -> Dcl {
        Dcl {
            flags: dcl::CHAR,
            ..Dcl::default()
        }
    }
}

/// `zend_ffi_val_kind`, ordered as php orders it, since `MAX(kind,
/// op2->kind)` picks the wider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ValKind {
    Empty,
    Error,
    Int32,
    Int64,
    Uint32,
    Uint64,
    Float,
    Double,
    LongDouble,
    Char,
    Str,
    Name,
}

/// `zend_ffi_val`: the kind and the union's eight bytes, read as the
/// field the code names, so a value read through the wrong field reads
/// what php's union reads; and a string's place in the source.
#[derive(Debug, Clone, Copy)]
struct Val {
    kind: ValKind,
    raw: u64,
    at: usize,
    len: usize,
}

impl Val {
    const EMPTY: Val = Val {
        kind: ValKind::Empty,
        raw: 0,
        at: 0,
        len: 0,
    };

    fn i64(self) -> i64 {
        self.raw.cast_signed()
    }

    fn u64(self) -> u64 {
        self.raw
    }

    fn d(self) -> f64 {
        f64::from_bits(self.raw)
    }

    fn ch(self) -> i8 {
        (self.raw & 0xff) as u8 as i8
    }

    fn set_i64(&mut self, value: i64) {
        self.raw = value.cast_unsigned();
    }

    fn set_d(&mut self, value: f64) {
        self.raw = value.to_bits();
    }

    fn set_ch(&mut self, value: i8) {
        self.raw = (self.raw & !0xff) | u64::from(value as u8);
    }

    fn is_unsigned(self) -> bool {
        matches!(self.kind, ValKind::Uint32 | ValKind::Uint64)
    }

    fn is_signed(self) -> bool {
        matches!(self.kind, ValKind::Int32 | ValKind::Int64)
    }

    fn is_int(self) -> bool {
        self.is_unsigned() || self.is_signed()
    }

    fn is_float(self) -> bool {
        matches!(
            self.kind,
            ValKind::Float | ValKind::Double | ValKind::LongDouble
        )
    }

    fn is_char(self) -> bool {
        self.kind == ValKind::Char
    }

    fn error(&mut self) {
        self.kind = ValKind::Error;
    }
}

/// A binary operator of a constant expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Or,
    Xor,
    And,
    Shl,
    Shr,
}

impl Op {
    fn is_shift(self) -> bool {
        matches!(self, Op::Shl | Op::Shr)
    }

    /// `a OP b` over `uint64_t`, as arm64 computes it: a division by
    /// zero answers zero and a remainder by zero the dividend, a shift
    /// counts its low six bits.
    fn unsigned(self, a: u64, b: u64) -> u64 {
        match self {
            Op::Add => a.wrapping_add(b),
            Op::Sub => a.wrapping_sub(b),
            Op::Mul => a.wrapping_mul(b),
            Op::Div => a.checked_div(b).unwrap_or(0),
            Op::Mod => a.checked_rem(b).unwrap_or(a),
            Op::Or => a | b,
            Op::Xor => a ^ b,
            Op::And => a & b,
            Op::Shl => a.wrapping_shl(b as u32),
            Op::Shr => a.wrapping_shr(b as u32),
        }
    }

    /// `a OP b` over `int64_t`, as arm64 computes it.
    fn signed(self, a: i64, b: i64) -> i64 {
        match self {
            Op::Add => a.wrapping_add(b),
            Op::Sub => a.wrapping_sub(b),
            Op::Mul => a.wrapping_mul(b),
            Op::Div => {
                if b == 0 {
                    0
                } else {
                    a.wrapping_div(b)
                }
            }
            Op::Mod => {
                if b == 0 {
                    a
                } else {
                    a.wrapping_rem(b)
                }
            }
            Op::Or => a | b,
            Op::Xor => a ^ b,
            Op::And => a & b,
            Op::Shl => a.wrapping_shl(b as u32),
            Op::Shr => a.wrapping_shr(b as u32),
        }
    }

    /// `a OP b` over doubles; only the arithmetic operators reach here.
    fn float(self, a: f64, b: f64) -> f64 {
        match self {
            Op::Add => a + b,
            Op::Sub => a - b,
            Op::Mul => a * b,
            _ => a / b,
        }
    }

    /// `a OP b` where C converts both to `uint64_t`, a shift keeping
    /// its left operand's signedness.
    fn mixed_signed(self, a: i64, b: u64) -> i64 {
        if self.is_shift() {
            self.signed(a, b.cast_signed())
        } else {
            self.unsigned(a.cast_unsigned(), b).cast_signed()
        }
    }
}

/// A comparison of a constant expression.
#[derive(Debug, Clone, Copy)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl Cmp {
    fn test<T: PartialOrd>(self, a: &T, b: &T) -> bool {
        match self {
            Cmp::Eq => a == b,
            Cmp::Ne => a != b,
            Cmp::Lt => a < b,
            Cmp::Gt => a > b,
            Cmp::Le => a <= b,
            Cmp::Ge => a >= b,
        }
    }
}

/// The tokens, numbered as `ffi_parser.c` numbers them.
mod tok {
    pub(super) const EOF: u8 = 0;
    pub(super) const EXTENSION: u8 = 1;
    pub(super) const ASM: u8 = 2;
    pub(super) const LPAREN: u8 = 3;
    pub(super) const RPAREN: u8 = 4;
    pub(super) const COMMA: u8 = 5;
    pub(super) const SEMICOLON: u8 = 6;
    pub(super) const TYPEDEF: u8 = 7;
    pub(super) const EXTERN: u8 = 8;
    pub(super) const STATIC: u8 = 9;
    pub(super) const AUTO: u8 = 10;
    pub(super) const REGISTER: u8 = 11;
    pub(super) const INLINE: u8 = 12;
    pub(super) const INLINE3: u8 = 14;
    pub(super) const NORETURN: u8 = 15;
    pub(super) const ALIGNAS: u8 = 16;
    pub(super) const CONST: u8 = 17;
    pub(super) const CONST2: u8 = 18;
    pub(super) const CONST3: u8 = 19;
    pub(super) const RESTRICT: u8 = 20;
    pub(super) const RESTRICT3: u8 = 22;
    pub(super) const VOLATILE: u8 = 23;
    pub(super) const VOLATILE3: u8 = 25;
    pub(super) const ATOMIC: u8 = 26;
    pub(super) const VOID: u8 = 27;
    pub(super) const CHAR: u8 = 28;
    pub(super) const SHORT: u8 = 29;
    pub(super) const INT: u8 = 30;
    pub(super) const LONG: u8 = 31;
    pub(super) const FLOAT: u8 = 32;
    pub(super) const DOUBLE: u8 = 33;
    pub(super) const SIGNED: u8 = 34;
    pub(super) const UNSIGNED: u8 = 35;
    pub(super) const BOOL: u8 = 36;
    pub(super) const COMPLEX: u8 = 37;
    pub(super) const COMPLEX4: u8 = 40;
    pub(super) const STRUCT: u8 = 41;
    pub(super) const UNION: u8 = 42;
    pub(super) const LBRACE: u8 = 43;
    pub(super) const RBRACE: u8 = 44;
    pub(super) const COLON: u8 = 45;
    pub(super) const ENUM: u8 = 46;
    pub(super) const EQUAL: u8 = 47;
    pub(super) const STAR: u8 = 48;
    pub(super) const LBRACK: u8 = 49;
    pub(super) const RBRACK: u8 = 50;
    pub(super) const ELLIPSIS: u8 = 51;
    pub(super) const ATTRIBUTE: u8 = 52;
    pub(super) const ATTRIBUTE2: u8 = 53;
    pub(super) const DECLSPEC: u8 = 54;
    pub(super) const CDECL: u8 = 55;
    pub(super) const STDCALL: u8 = 56;
    pub(super) const FASTCALL: u8 = 57;
    pub(super) const THISCALL: u8 = 58;
    pub(super) const VECTORCALL: u8 = 59;
    pub(super) const POINT: u8 = 60;
    pub(super) const QUERY: u8 = 61;
    pub(super) const BAR_BAR: u8 = 62;
    pub(super) const AND_AND: u8 = 63;
    pub(super) const BAR: u8 = 64;
    pub(super) const UPARROW: u8 = 65;
    pub(super) const AND: u8 = 66;
    pub(super) const EQUAL_EQUAL: u8 = 67;
    pub(super) const BANG_EQUAL: u8 = 68;
    pub(super) const LESS: u8 = 69;
    pub(super) const GREATER: u8 = 70;
    pub(super) const LESS_EQUAL: u8 = 71;
    pub(super) const GREATER_EQUAL: u8 = 72;
    pub(super) const LESS_LESS: u8 = 73;
    pub(super) const GREATER_GREATER: u8 = 74;
    pub(super) const PLUS: u8 = 75;
    pub(super) const MINUS: u8 = 76;
    pub(super) const SLASH: u8 = 77;
    pub(super) const PERCENT: u8 = 78;
    pub(super) const MINUS_GREATER: u8 = 79;
    pub(super) const PLUS_PLUS: u8 = 80;
    pub(super) const MINUS_MINUS: u8 = 81;
    pub(super) const TILDE: u8 = 82;
    pub(super) const BANG: u8 = 83;
    pub(super) const SIZEOF: u8 = 84;
    pub(super) const ALIGNOF: u8 = 85;
    pub(super) const ALIGNOF2: u8 = 86;
    pub(super) const ALIGNOF3: u8 = 87;
    pub(super) const RESTICT: u8 = 88;
    pub(super) const ID: u8 = 89;
    pub(super) const OCTNUMBER: u8 = 90;
    pub(super) const DECNUMBER: u8 = 91;
    pub(super) const HEXNUMBER: u8 = 92;
    pub(super) const FLOATNUMBER: u8 = 93;
    pub(super) const STRING: u8 = 94;
    pub(super) const CHARACTER: u8 = 95;
    pub(super) const EOL: u8 = 96;
    pub(super) const WS: u8 = 97;
    pub(super) const ONE_LINE_COMMENT: u8 = 98;
    pub(super) const COMMENT: u8 = 99;

    /// `sym_name`.
    pub(super) const NAMES: [&str; 100] = [
        "<EOF>",
        "__extension__",
        "__asm__",
        "(",
        ")",
        ",",
        ";",
        "typedef",
        "extern",
        "static",
        "auto",
        "register",
        "inline",
        "__inline",
        "__inline__",
        "_Noreturn",
        "_Alignas",
        "const",
        "__const",
        "__const__",
        "restrict",
        "__restrict",
        "__restrict__",
        "volatile",
        "__volatile",
        "__volatile__",
        "_Atomic",
        "void",
        "char",
        "short",
        "int",
        "long",
        "float",
        "double",
        "signed",
        "unsigned",
        "_Bool",
        "_Complex",
        "complex",
        "__complex",
        "__complex__",
        "struct",
        "union",
        "{",
        "}",
        ":",
        "enum",
        "=",
        "*",
        "[",
        "]",
        "...",
        "__attribute",
        "__attribute__",
        "__declspec",
        "__cdecl",
        "__stdcall",
        "__fastcall",
        "__thiscall",
        "__vectorcall",
        ".",
        "?",
        "||",
        "&&",
        "|",
        "^",
        "&",
        "==",
        "!=",
        "<",
        ">",
        "<=",
        ">=",
        "<<",
        ">>",
        "+",
        "-",
        "/",
        "%",
        "->",
        "++",
        "--",
        "~",
        "!",
        "sizeof",
        "_Alignof",
        "__alignof",
        "__alignof__",
        "__restict",
        "<ID>",
        "<OCTNUMBER>",
        "<DECNUMBER>",
        "<HEXNUMBER>",
        "<FLOATNUMBER>",
        "<STRING>",
        "<CHARACTER>",
        "<EOL>",
        "<WS>",
        "<ONE_LINE_COMMENT>",
        "<COMMENT>",
    ];

    /// The keyword an identifier is, if any.
    pub(super) fn keyword(word: &[u8]) -> Option<u8> {
        let at = NAMES.iter().position(|name| name.as_bytes() == word)?;
        let at = u8::try_from(at).ok()?;
        let reserved = (EXTENSION..=ASM).contains(&at)
            || (TYPEDEF..=STRUCT).contains(&at)
            || at == UNION
            || at == ENUM
            || (ATTRIBUTE..=VECTORCALL).contains(&at)
            || (SIZEOF..=RESTICT).contains(&at);
        reserved.then_some(at)
    }

    /// `__attribute`, `__attribute__`, `__declspec` and the four calling
    /// conventions: what opens `attributes`.
    pub(super) fn is_attribute(sym: u8) -> bool {
        (ATTRIBUTE..=VECTORCALL).contains(&sym)
    }

    /// What opens `type_qualifier`.
    pub(super) fn is_qualifier(sym: u8) -> bool {
        (CONST..=ATOMIC).contains(&sym)
    }

    /// What opens `type_specifier`.
    pub(super) fn is_specifier(sym: u8) -> bool {
        (VOID..=COMPLEX4).contains(&sym)
            || sym == STRUCT
            || sym == UNION
            || sym == ENUM
            || sym == ID
    }

    /// What opens `specifier_qualifier_list`.
    pub(super) fn is_specifier_qualifier(sym: u8) -> bool {
        sym == EXTENSION || is_specifier(sym) || is_qualifier(sym) || is_attribute(sym)
    }

    /// What opens `declaration_specifiers` past `__extension__`.
    pub(super) fn is_declaration_specifier(sym: u8) -> bool {
        (TYPEDEF..=ALIGNAS).contains(&sym)
            || is_specifier(sym)
            || is_qualifier(sym)
            || is_attribute(sym)
    }

    /// What opens an expression; `*` with `star`.
    pub(super) fn is_expression(sym: u8, star: bool) -> bool {
        sym == LPAREN
            || (star && sym == STAR)
            || sym == AND
            || sym == PLUS
            || sym == MINUS
            || (PLUS_PLUS..=ALIGNOF3).contains(&sym)
            || (ID..=CHARACTER).contains(&sym)
    }
}

/// A parse that ended in php's error: the sentence `FFI\ParserException`
/// carries.
#[derive(Debug, Clone)]
pub(crate) struct Bail(pub(crate) String);

type Parse<T> = Result<T, Bail>;

/// Where the scanner got stuck, and the token of the last accepting
/// state it passed, if any.
#[derive(Debug, Clone, Copy)]
struct Stuck {
    at: usize,
    accept: Option<u8>,
}

/// Where the lexer stands: `yy_pos`, `yy_text` and `yy_line`, saved and
/// restored around a lookahead as the generated parser saves them.
#[derive(Debug, Clone, Copy)]
struct Mark {
    pos: usize,
    text: usize,
    line: i64,
}

/// The parser over one source, declaring into one scope and the
/// request's type arena.
pub(crate) struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    text: usize,
    line: i64,
    types: &'a mut Types,
    scope: &'a mut Scope,
    allow_vla: bool,
    attribute_parsing: bool,
    default_attr: u32,
}

impl<'a> Parser<'a> {
    /// A parser over `buf`, its types made with `default_attr`
    /// (`FFI_G(default_type_attr)`).
    pub(crate) fn new(
        buf: &'a [u8],
        types: &'a mut Types,
        scope: &'a mut Scope,
        default_attr: u32,
    ) -> Parser<'a> {
        Parser {
            buf,
            pos: 0,
            text: 0,
            line: 1,
            types,
            scope,
            allow_vla: false,
            attribute_parsing: false,
            default_attr,
        }
    }

    /// `zend_ffi_parse_decl`: every declaration of the source.
    pub(crate) fn parse_decl(&mut self) -> Parse<()> {
        self.pos = 0;
        self.text = 0;
        self.line = 1;
        let sym = self.get_sym()?;
        let sym = self.parse_declarations(sym)?;
        if sym != tok::EOF {
            return Err(self.error_sym("<EOF> expected, got", sym));
        }
        Ok(())
    }

    /// `zend_ffi_parse_type`: one type name and nothing after it,
    /// validated as `zend_ffi_validate_type_name` holds it.
    pub(crate) fn parse_type(&mut self, dcl: &mut Dcl) -> Parse<()> {
        self.pos = 0;
        self.text = 0;
        self.line = 1;
        let sym = self.get_sym()?;
        let sym = self.parse_type_name(sym, dcl)?;
        if sym != tok::EOF {
            return Err(self.error_sym("<EOF> expected, got", sym));
        }
        let ty = self.finalize(dcl)?;
        self.validate_var_type(ty, false)
    }

    // ---------------------------------------------------------------- errors

    /// `yy_error_sym(msg, sym)`.
    fn error_sym(&self, message: &str, sym: u8) -> Bail {
        let name = tok::NAMES
            .get(usize::from(sym))
            .copied()
            .unwrap_or_default();
        Bail(format!("{message} '{name}' at line {}", self.line))
    }

    /// `"%s at line %d"` with the line the lexer stands at.
    fn at_line(&self, message: &str) -> Bail {
        Bail(format!("{message} at line {}", self.line))
    }

    // ----------------------------------------------------------------- lexer

    fn byte(&self, at: usize) -> u8 {
        self.buf.get(at).copied().unwrap_or(0)
    }

    fn mark(&self) -> Mark {
        Mark {
            pos: self.pos,
            text: self.text,
            line: self.line,
        }
    }

    fn reset(&mut self, mark: Mark) {
        self.pos = mark.pos;
        self.text = mark.text;
        self.line = mark.line;
    }

    /// The current token's bytes.
    fn token(&self) -> Vec<u8> {
        self.buf
            .get(self.text..self.pos)
            .unwrap_or_default()
            .to_vec()
    }

    /// `get_sym`: the next token past whitespace, newlines and comments.
    fn get_sym(&mut self) -> Parse<u8> {
        loop {
            let sym = self.get_skip_sym()?;
            if !matches!(
                sym,
                tok::EOL | tok::WS | tok::ONE_LINE_COMMENT | tok::COMMENT
            ) {
                return Ok(sym);
            }
        }
    }

    /// `_yy_state_error`'s three sentences: the end of the source, a
    /// character no token starts with, or a token that went wrong.
    fn lex_error(&self, at: usize) -> Bail {
        if at >= self.buf.len() {
            return self.at_line("unexpected <EOF>");
        }
        let escaped = if at == self.text {
            escape(&[self.byte(at)], 64)
        } else {
            escape(self.buf.get(self.text..=at).unwrap_or_default(), 64)
        };
        let what = if at == self.text {
            "unexpected character"
        } else {
            "unexpected sequence"
        };
        Bail(format!("{what} '{escaped}' at line {}\n", self.line))
    }

    /// `get_skip_sym`: one token, whitespace and comments included. A
    /// token that goes wrong after the scanner passed an accepting state
    /// is that state's token with no length, `yy_pos` put back to the
    /// token's start as the generated scanner puts it back
    /// (`accept_pos = yy_pos`); one that passed none is the error.
    fn get_skip_sym(&mut self) -> Parse<u8> {
        self.text = self.pos;
        match self.scan(self.pos) {
            Ok((sym, end)) => {
                self.pos = end;
                Ok(sym)
            }
            Err(Stuck {
                at,
                accept: Some(sym),
            }) => {
                let _ = at;
                Ok(sym)
            }
            Err(Stuck { at, accept: None }) => Err(self.lex_error(at)),
        }
    }

    /// The scanner's states from `start`: the token and where it ends,
    /// counting the newlines it crosses, or where it got stuck.
    #[expect(
        clippy::too_many_lines,
        reason = "one function per rule of ffi.g, or per callback of ffi.c, as long as the original"
    )]
    fn scan(&mut self, start: usize) -> Result<(u8, usize), Stuck> {
        let b = |at: usize| self.byte(at);
        let end = self.buf.len();
        let one = |sym| Ok((sym, start + 1));
        let two = |sym| Ok((sym, start + 2));
        let stuck = |at, accept| Err(Stuck { at, accept });
        match b(start) {
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let mut at = start + 1;
                while b(at).is_ascii_alphanumeric() || b(at) == b'_' {
                    at += 1;
                }
                let word = self.buf.get(start..at).unwrap_or_default();
                if b(at) == b'"' && matches!(word, b"u8" | b"u" | b"U" | b"L") {
                    return self.quoted(at, b'"', tok::STRING);
                }
                if b(at) == b'\'' && matches!(word, b"u" | b"U" | b"L") {
                    return self.quoted(at, b'\'', tok::CHARACTER);
                }
                Ok((tok::keyword(word).unwrap_or(tok::ID), at))
            }
            b'0' => {
                if matches!(b(start + 1), b'x' | b'X') {
                    if !b(start + 2).is_ascii_hexdigit() {
                        return stuck(start + 2, None);
                    }
                    let mut at = start + 3;
                    while b(at).is_ascii_hexdigit() {
                        at += 1;
                    }
                    return self.int_suffix(at, tok::HEXNUMBER);
                }
                let mut at = start + 1;
                while matches!(b(at), b'0'..=b'7') {
                    at += 1;
                }
                match b(at) {
                    b'8' | b'9' => {
                        while b(at).is_ascii_digit() {
                            at += 1;
                        }
                        match b(at) {
                            b'e' | b'E' => self.exponent(at, tok::OCTNUMBER),
                            b'.' => self.fraction(at + 1),
                            _ => stuck(at, Some(tok::OCTNUMBER)),
                        }
                    }
                    b'e' | b'E' => self.exponent(at, tok::OCTNUMBER),
                    b'.' => self.fraction(at + 1),
                    _ => self.int_suffix(at, tok::OCTNUMBER),
                }
            }
            b'1'..=b'9' => {
                let mut at = start + 1;
                while b(at).is_ascii_digit() {
                    at += 1;
                }
                match b(at) {
                    b'e' | b'E' => self.exponent(at, tok::DECNUMBER),
                    b'.' => self.fraction(at + 1),
                    _ => self.int_suffix(at, tok::DECNUMBER),
                }
            }
            b'.' => {
                if b(start + 1).is_ascii_digit() {
                    return self.fraction(start + 1);
                }
                if b(start + 1) == b'.' {
                    if b(start + 2) == b'.' {
                        return Ok((tok::ELLIPSIS, start + 3));
                    }
                    return stuck(start + 2, Some(tok::POINT));
                }
                one(tok::POINT)
            }
            b'"' => self.quoted(start, b'"', tok::STRING),
            b'\'' => self.quoted(start, b'\'', tok::CHARACTER),
            b'(' => one(tok::LPAREN),
            b')' => one(tok::RPAREN),
            b',' => one(tok::COMMA),
            b';' => one(tok::SEMICOLON),
            b'{' => one(tok::LBRACE),
            b'}' => one(tok::RBRACE),
            b'[' => one(tok::LBRACK),
            b']' => one(tok::RBRACK),
            b':' => one(tok::COLON),
            b'?' => one(tok::QUERY),
            b'~' => one(tok::TILDE),
            b'^' => one(tok::UPARROW),
            b'%' => one(tok::PERCENT),
            b'*' => one(tok::STAR),
            b'=' if b(start + 1) == b'=' => two(tok::EQUAL_EQUAL),
            b'=' => one(tok::EQUAL),
            b'!' if b(start + 1) == b'=' => two(tok::BANG_EQUAL),
            b'!' => one(tok::BANG),
            b'|' if b(start + 1) == b'|' => two(tok::BAR_BAR),
            b'|' => one(tok::BAR),
            b'&' if b(start + 1) == b'&' => two(tok::AND_AND),
            b'&' => one(tok::AND),
            b'<' if b(start + 1) == b'=' => two(tok::LESS_EQUAL),
            b'<' if b(start + 1) == b'<' => two(tok::LESS_LESS),
            b'<' => one(tok::LESS),
            b'>' if b(start + 1) == b'=' => two(tok::GREATER_EQUAL),
            b'>' if b(start + 1) == b'>' => two(tok::GREATER_GREATER),
            b'>' => one(tok::GREATER),
            b'+' if b(start + 1) == b'+' => two(tok::PLUS_PLUS),
            b'+' => one(tok::PLUS),
            b'-' if b(start + 1) == b'-' => two(tok::MINUS_MINUS),
            b'-' if b(start + 1) == b'>' => two(tok::MINUS_GREATER),
            b'-' => one(tok::MINUS),
            b'/' if b(start + 1) == b'/' => self.line_comment(start + 2, Some(tok::SLASH)),
            b'/' if b(start + 1) == b'*' => self.block_comment(start + 2),
            b'/' => one(tok::SLASH),
            b'#' => self.line_comment(start + 1, None),
            b'\r' if b(start + 1) == b'\n' => {
                self.line += 1;
                two(tok::EOL)
            }
            b'\r' => one(tok::EOL),
            b'\n' => {
                self.line += 1;
                one(tok::EOL)
            }
            b' ' | b'\t' | 0x0b | 0x0c => {
                let mut at = start + 1;
                while matches!(b(at), b' ' | b'\t' | 0x0b | 0x0c) {
                    at += 1;
                }
                Ok((tok::WS, at))
            }
            0 if start >= end => one(tok::EOF),
            _ => stuck(start, None),
        }
    }

    /// `(\/\/|#)[^\r\n]*(\r\n|\r|\n)` from past its opener: a comment
    /// to its line's end, which the end of the source does not close.
    fn line_comment(&mut self, from: usize, accept: Option<u8>) -> Result<(u8, usize), Stuck> {
        let mut at = from;
        loop {
            match self.byte(at) {
                b'\n' => {
                    self.line += 1;
                    return Ok((tok::ONE_LINE_COMMENT, at + 1));
                }
                b'\r' if self.byte(at + 1) == b'\n' => {
                    self.line += 1;
                    return Ok((tok::ONE_LINE_COMMENT, at + 2));
                }
                b'\r' => return Ok((tok::ONE_LINE_COMMENT, at + 1)),
                _ if at >= self.buf.len() => return Err(Stuck { at, accept }),
                _ => at += 1,
            }
        }
    }

    /// `\/\*([^\*]|\*+[^\*\/])*\*+\/` from past its `/*`.
    fn block_comment(&mut self, from: usize) -> Result<(u8, usize), Stuck> {
        let mut at = from;
        loop {
            if at >= self.buf.len() {
                return Err(Stuck {
                    at,
                    accept: Some(tok::SLASH),
                });
            }
            match self.byte(at) {
                b'*' if self.byte(at + 1) == b'/' => return Ok((tok::COMMENT, at + 2)),
                b'\n' => {
                    self.line += 1;
                    at += 1;
                }
                _ => at += 1,
            }
        }
    }

    /// A string or a character constant from its opening quote at
    /// `open`: any byte but the quote, a backslash escaping the next.
    fn quoted(&mut self, open: usize, quote: u8, sym: u8) -> Result<(u8, usize), Stuck> {
        let mut at = open + 1;
        loop {
            if at >= self.buf.len() {
                return Err(Stuck { at, accept: None });
            }
            match self.byte(at) {
                b'\\' => {
                    at += 1;
                    if at >= self.buf.len() {
                        return Err(Stuck { at, accept: None });
                    }
                    if self.byte(at) == b'\n' {
                        self.line += 1;
                    }
                    at += 1;
                }
                b if b == quote => return Ok((sym, at + 1)),
                b'\n' => {
                    self.line += 1;
                    at += 1;
                }
                _ => at += 1,
            }
        }
    }

    /// The digits after a decimal point, from `at`: an exponent and a
    /// suffix may follow.
    fn fraction(&self, mut at: usize) -> Result<(u8, usize), Stuck> {
        while self.byte(at).is_ascii_digit() {
            at += 1;
        }
        match self.byte(at) {
            b'f' | b'F' | b'l' | b'L' => Ok((tok::FLOATNUMBER, at + 1)),
            b'e' | b'E' => self.exponent(at, tok::FLOATNUMBER),
            _ => Ok((tok::FLOATNUMBER, at)),
        }
    }

    /// `[Ee][\+\-]?[0-9]+[flFL]?` from the `e` at `at`, stuck with
    /// `accept` where no digit follows.
    fn exponent(&self, at: usize, accept: u8) -> Result<(u8, usize), Stuck> {
        let mut at = at + 1;
        if matches!(self.byte(at), b'+' | b'-') {
            at += 1;
        }
        if !self.byte(at).is_ascii_digit() {
            return Err(Stuck {
                at,
                accept: Some(accept),
            });
        }
        while self.byte(at).is_ascii_digit() {
            at += 1;
        }
        if matches!(self.byte(at), b'f' | b'F' | b'l' | b'L') {
            at += 1;
        }
        Ok((tok::FLOATNUMBER, at))
    }

    /// `([Uu](L|l|LL|l)?|[Ll][Uu]?|(LL|ll)[Uu])?` from `at`, as the
    /// scanner's states take it: `UL` takes one more `L` and `Ul` none,
    /// and `LL` or `ll` with no `U` after it is stuck.
    fn int_suffix(&self, at: usize, sym: u8) -> Result<(u8, usize), Stuck> {
        let b = |i: usize| self.byte(i);
        match b(at) {
            b'u' | b'U' => match b(at + 1) {
                b'L' if b(at + 2) == b'L' => Ok((sym, at + 3)),
                b'L' | b'l' => Ok((sym, at + 2)),
                _ => Ok((sym, at + 1)),
            },
            first @ (b'L' | b'l') => match b(at + 1) {
                b'u' | b'U' => Ok((sym, at + 2)),
                second if second == first => {
                    if matches!(b(at + 2), b'u' | b'U') {
                        Ok((sym, at + 3))
                    } else {
                        Err(Stuck {
                            at: at + 2,
                            accept: Some(sym),
                        })
                    }
                }
                _ => Ok((sym, at + 1)),
            },
            _ => Ok((sym, at)),
        }
    }

    // ------------------------------------------------------- the grammar

    fn parse_declarations(&mut self, mut sym: u8) -> Parse<u8> {
        while sym == tok::EXTENSION || tok::is_declaration_specifier(sym) {
            let mut common = Dcl::default();
            if sym == tok::EXTENSION {
                sym = self.get_sym()?;
            }
            sym = self.parse_declaration_specifiers(sym, &mut common)?;
            if sym == tok::STAR || sym == tok::ID || sym == tok::LPAREN {
                let mut dcl = common;
                let (next, name) = self.parse_declarator(sym, &mut dcl)?;
                sym = next;
                if sym == tok::ASM {
                    sym = self.get_sym()?;
                    if sym != tok::LPAREN {
                        return Err(self.error_sym("'(' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                    loop {
                        if sym != tok::STRING {
                            return Err(self.error_sym("<STRING> expected, got", sym));
                        }
                        sym = self.get_sym()?;
                        if sym != tok::STRING {
                            break;
                        }
                    }
                    if sym != tok::RPAREN {
                        return Err(self.error_sym("')' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                }
                if tok::is_attribute(sym) {
                    sym = self.parse_attributes(sym, &mut dcl)?;
                }
                if sym == tok::EQUAL {
                    sym = self.parse_initializer(sym)?;
                }
                self.declare(&name, &mut dcl)?;
                while sym == tok::COMMA {
                    sym = self.get_sym()?;
                    let mut dcl = common;
                    let (next, name) = self.parse_declarator(sym, &mut dcl)?;
                    sym = next;
                    if tok::is_attribute(sym) {
                        sym = self.parse_attributes(sym, &mut dcl)?;
                    }
                    if sym == tok::EQUAL {
                        sym = self.parse_initializer(sym)?;
                    }
                    self.declare(&name, &mut dcl)?;
                }
            } else if sym == tok::SEMICOLON {
                // `zend_ffi_cleanup_dcl`: a struct, union or enum no tag
                // and no declarator keeps is freed.
                if common.flags & (dcl::ENUM | dcl::STRUCT | dcl::UNION) != 0 {
                    self.cleanup(&common);
                }
            } else {
                return Err(self.error_sym("unexpected", sym));
            }
            if sym != tok::SEMICOLON {
                return Err(self.error_sym("';' expected, got", sym));
            }
            sym = self.get_sym()?;
        }
        Ok(sym)
    }

    fn parse_declaration_specifiers(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        loop {
            match sym {
                tok::TYPEDEF | tok::EXTERN | tok::STATIC | tok::AUTO | tok::REGISTER => {
                    if dcl.flags & dcl::STORAGE_CLASS != 0 {
                        return Err(self.error_sym("unexpected", sym));
                    }
                    dcl.flags |= match sym {
                        tok::TYPEDEF => dcl::TYPEDEF,
                        tok::EXTERN => dcl::EXTERN,
                        tok::STATIC => dcl::STATIC,
                        tok::AUTO => dcl::AUTO,
                        _ => dcl::REGISTER,
                    };
                    sym = self.get_sym()?;
                }
                tok::INLINE..=tok::INLINE3 => {
                    sym = self.get_sym()?;
                    dcl.flags |= dcl::INLINE;
                }
                tok::NORETURN => {
                    sym = self.get_sym()?;
                    dcl.flags |= dcl::NO_RETURN;
                }
                tok::ALIGNAS => {
                    sym = self.get_sym()?;
                    if sym != tok::LPAREN {
                        return Err(self.error_sym("'(' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                    if tok::is_specifier_qualifier(sym)
                        && self.lookahead(sym, Self::check_type_name_start)?
                    {
                        let mut align = Dcl::default();
                        sym = self.parse_type_name(sym, &mut align)?;
                        self.align_as_type(dcl, &mut align)?;
                    } else if tok::is_expression(sym, true) {
                        let mut val = Val::EMPTY;
                        sym = self.parse_conditional_expression(sym, &mut val)?;
                        align_as_val(dcl, val);
                    } else {
                        return Err(self.error_sym("unexpected", sym));
                    }
                    if sym != tok::RPAREN {
                        return Err(self.error_sym("')' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                }
                _ if tok::is_attribute(sym) => sym = self.parse_attributes(sym, dcl)?,
                _ if tok::is_qualifier(sym) => sym = self.parse_type_qualifier(sym, dcl)?,
                _ if tok::is_specifier(sym) => sym = self.parse_type_specifier(sym, dcl)?,
                _ => return Err(self.error_sym("unexpected", sym)),
            }
            if !(tok::is_declaration_specifier(sym)
                && (sym != tok::ID || dcl.flags & dcl::TYPE_SPECIFIERS == 0))
            {
                return Ok(sym);
            }
        }
    }

    fn parse_specifier_qualifier_list(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        if sym == tok::EXTENSION {
            sym = self.get_sym()?;
        }
        loop {
            if tok::is_specifier(sym) {
                sym = self.parse_type_specifier(sym, dcl)?;
            } else if tok::is_qualifier(sym) {
                sym = self.parse_type_qualifier(sym, dcl)?;
            } else if tok::is_attribute(sym) {
                sym = self.parse_attributes(sym, dcl)?;
            } else {
                return Err(self.error_sym("unexpected", sym));
            }
            let more = tok::is_specifier(sym) || tok::is_qualifier(sym) || tok::is_attribute(sym);
            if !(more
                && (sym != tok::ID
                    || self.is_typedef_name(&self.token())
                    || dcl.flags & dcl::TYPE_SPECIFIERS == 0))
            {
                return Ok(sym);
            }
        }
    }

    fn parse_type_qualifier_list(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        loop {
            if tok::is_qualifier(sym) {
                sym = self.parse_type_qualifier(sym, dcl)?;
            } else if tok::is_attribute(sym) {
                sym = self.parse_attributes(sym, dcl)?;
            } else {
                return Err(self.error_sym("unexpected", sym));
            }
            if !(tok::is_qualifier(sym) || tok::is_attribute(sym)) {
                return Ok(sym);
            }
        }
    }

    fn parse_type_qualifier(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        match sym {
            tok::CONST..=tok::CONST3 => {
                dcl.flags |= dcl::CONST;
                dcl.attr |= attr::CONST;
            }
            tok::RESTRICT..=tok::RESTRICT3 => dcl.flags |= dcl::RESTRICT,
            tok::VOLATILE..=tok::VOLATILE3 => dcl.flags |= dcl::VOLATILE,
            tok::ATOMIC => dcl.flags |= dcl::ATOMIC,
            _ => return Err(self.error_sym("unexpected", sym)),
        }
        self.get_sym()
    }

    fn parse_type_specifier(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        use dcl::{
            BOOL, CHAR, COMPLEX, DOUBLE, FLOAT, INT, LONG, LONG_LONG, SHORT, SIGNED,
            TYPE_SPECIFIERS, UNSIGNED,
        };
        let refuse = |this: &Self, allowed: u32| {
            if dcl.flags & (TYPE_SPECIFIERS & !allowed) != 0 {
                Err(this.error_sym("unexpected", sym))
            } else {
                Ok(())
            }
        };
        let flag = match sym {
            tok::VOID => {
                refuse(self, 0)?;
                dcl::VOID
            }
            tok::CHAR => {
                refuse(self, SIGNED | UNSIGNED)?;
                CHAR
            }
            tok::SHORT => {
                refuse(self, SIGNED | UNSIGNED | INT)?;
                SHORT
            }
            tok::INT => {
                refuse(self, SIGNED | UNSIGNED | SHORT | LONG | LONG_LONG)?;
                INT
            }
            tok::LONG => {
                if dcl.flags & LONG != 0 {
                    refuse(self, LONG | SIGNED | UNSIGNED | INT)?;
                    LONG_LONG
                } else {
                    refuse(self, LONG | SIGNED | UNSIGNED | INT | DOUBLE | COMPLEX)?;
                    LONG
                }
            }
            tok::FLOAT => {
                refuse(self, COMPLEX)?;
                FLOAT
            }
            tok::DOUBLE => {
                refuse(self, LONG | COMPLEX)?;
                DOUBLE
            }
            tok::SIGNED => {
                refuse(self, CHAR | SHORT | LONG | LONG_LONG | INT)?;
                SIGNED
            }
            tok::UNSIGNED => {
                refuse(self, CHAR | SHORT | LONG | LONG_LONG | INT)?;
                UNSIGNED
            }
            tok::BOOL => {
                refuse(self, 0)?;
                BOOL
            }
            tok::COMPLEX..=tok::COMPLEX4 => {
                refuse(self, FLOAT | DOUBLE | LONG)?;
                COMPLEX
            }
            tok::STRUCT | tok::UNION => {
                refuse(self, 0)?;
                return self.parse_struct_or_union_specifier(sym, dcl);
            }
            tok::ENUM => {
                refuse(self, 0)?;
                return self.parse_enum_specifier(sym, dcl);
            }
            tok::ID => {
                refuse(self, 0)?;
                let name = self.token();
                let sym = self.get_sym()?;
                dcl.flags |= dcl::TYPEDEF_NAME;
                self.resolve_typedef(&name, dcl)?;
                return Ok(sym);
            }
            _ => return Err(self.error_sym("unexpected", sym)),
        };
        let next = self.get_sym()?;
        dcl.flags |= flag;
        Ok(next)
    }

    fn parse_struct_or_union_specifier(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        dcl.flags |= if sym == tok::STRUCT {
            dcl::STRUCT
        } else {
            dcl::UNION
        };
        let mut sym = self.get_sym()?;
        if tok::is_attribute(sym) {
            sym = self.parse_attributes(sym, dcl)?;
        }
        if sym == tok::ID {
            let name = self.token();
            sym = self.get_sym()?;
            self.declare_tag(&name, dcl, true)?;
            if sym == tok::LBRACE {
                sym = self.parse_struct_contents(sym, dcl)?;
                self.declare_tag(&name, dcl, false)?;
            }
        } else if sym == tok::LBRACE {
            self.make_struct_type(dcl);
            sym = self.parse_struct_contents(sym, dcl)?;
        } else {
            return Err(self.error_sym("unexpected", sym));
        }
        Ok(sym)
    }

    fn parse_struct_contents(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        if sym != tok::LBRACE {
            return Err(self.error_sym("'{' expected, got", sym));
        }
        let mut sym = self.get_sym()?;
        if tok::is_specifier_qualifier(sym) {
            sym = self.parse_struct_declaration(sym, dcl)?;
            let mut alt;
            loop {
                let mark = self.mark();
                let mut sym2 = sym;
                if sym2 == tok::SEMICOLON {
                    sym2 = self.get_sym()?;
                    if tok::is_specifier_qualifier(sym2) {
                        alt = 3;
                    } else if sym2 == tok::RBRACE {
                        alt = 5;
                    } else {
                        return Err(self.error_sym("unexpected", sym2));
                    }
                } else if sym2 == tok::RBRACE {
                    alt = 6;
                } else {
                    return Err(self.error_sym("unexpected", sym2));
                }
                self.reset(mark);
                if alt != 3 {
                    break;
                }
                sym = self.get_sym()?;
                sym = self.parse_struct_declaration(sym, dcl)?;
            }
            if alt == 5 {
                sym = self.get_sym()?;
            }
        }
        if sym != tok::RBRACE {
            return Err(self.error_sym("'}' expected, got", sym));
        }
        sym = self.get_sym()?;
        if tok::is_attribute(sym) {
            sym = self.parse_attributes(sym, dcl)?;
        }
        self.adjust_struct_size(dcl);
        Ok(sym)
    }

    fn parse_struct_declaration(&mut self, sym: u8, struct_dcl: &mut Dcl) -> Parse<u8> {
        let mut common = Dcl::default();
        let mut sym = self.parse_specifier_qualifier_list(sym, &mut common)?;
        let base = common;
        if sym == tok::SEMICOLON || sym == tok::RBRACE {
            self.add_anonymous_field(struct_dcl, &mut common)?;
        } else if matches!(sym, tok::STAR | tok::ID | tok::LPAREN | tok::COLON) {
            sym = self.parse_struct_declarator(sym, struct_dcl, &mut common)?;
            while sym == tok::COMMA {
                sym = self.get_sym()?;
                let mut field = base;
                if tok::is_attribute(sym) {
                    sym = self.parse_attributes(sym, &mut field)?;
                }
                sym = self.parse_struct_declarator(sym, struct_dcl, &mut field)?;
            }
        } else {
            return Err(self.error_sym("unexpected", sym));
        }
        Ok(sym)
    }

    fn parse_struct_declarator(
        &mut self,
        sym: u8,
        struct_dcl: &mut Dcl,
        field: &mut Dcl,
    ) -> Parse<u8> {
        if matches!(sym, tok::STAR | tok::ID | tok::LPAREN) {
            let (mut sym, name) = self.parse_declarator(sym, field)?;
            if sym == tok::COLON {
                sym = self.get_sym()?;
                let mut bits = Val::EMPTY;
                sym = self.parse_conditional_expression(sym, &mut bits)?;
                if tok::is_attribute(sym) {
                    sym = self.parse_attributes(sym, field)?;
                }
                self.add_bit_field(struct_dcl, Some(&name), field, bits)?;
            } else if tok::is_attribute(sym)
                || matches!(sym, tok::COMMA | tok::SEMICOLON | tok::RBRACE)
            {
                if tok::is_attribute(sym) {
                    sym = self.parse_attributes(sym, field)?;
                }
                self.add_field(struct_dcl, &name, field)?;
            } else {
                return Err(self.error_sym("unexpected", sym));
            }
            Ok(sym)
        } else if sym == tok::COLON {
            let mut sym = self.get_sym()?;
            let mut bits = Val::EMPTY;
            sym = self.parse_conditional_expression(sym, &mut bits)?;
            self.add_bit_field(struct_dcl, None, field, bits)?;
            Ok(sym)
        } else {
            Err(self.error_sym("unexpected", sym))
        }
    }

    fn parse_enum_specifier(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        if sym != tok::ENUM {
            return Err(self.error_sym("'enum' expected, got", sym));
        }
        let mut sym = self.get_sym()?;
        dcl.flags |= dcl::ENUM;
        if tok::is_attribute(sym) {
            sym = self.parse_attributes(sym, dcl)?;
        }
        if sym == tok::ID {
            let name = self.token();
            sym = self.get_sym()?;
            if sym == tok::LBRACE {
                self.declare_tag(&name, dcl, false)?;
                sym = self.get_sym()?;
                sym = self.parse_enumerator_list(sym, dcl)?;
                if sym != tok::RBRACE {
                    return Err(self.error_sym("'}' expected, got", sym));
                }
                sym = self.get_sym()?;
                if tok::is_attribute(sym) {
                    sym = self.parse_attributes(sym, dcl)?;
                }
            } else if tok::is_declaration_specifier(sym)
                || matches!(
                    sym,
                    tok::STAR
                        | tok::LPAREN
                        | tok::SEMICOLON
                        | tok::COLON
                        | tok::LBRACK
                        | tok::RBRACE
                        | tok::COMMA
                        | tok::RPAREN
                        | tok::EOF
                )
            {
                self.declare_tag(&name, dcl, true)?;
            } else {
                return Err(self.error_sym("unexpected", sym));
            }
        } else if sym == tok::LBRACE {
            sym = self.get_sym()?;
            self.make_enum_type(dcl);
            sym = self.parse_enumerator_list(sym, dcl)?;
            if sym != tok::RBRACE {
                return Err(self.error_sym("'}' expected, got", sym));
            }
            sym = self.get_sym()?;
            if tok::is_attribute(sym) {
                sym = self.parse_attributes(sym, dcl)?;
            }
        } else {
            return Err(self.error_sym("unexpected", sym));
        }
        Ok(sym)
    }

    fn parse_enumerator_list(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        let mut range = EnumRange {
            min: 0,
            max: 0,
            last: -1,
        };
        let mut sym = self.parse_enumerator(sym, dcl, &mut range)?;
        let mut alt;
        loop {
            let mark = self.mark();
            let mut sym2 = sym;
            if sym2 == tok::COMMA {
                sym2 = self.get_sym()?;
                if sym2 == tok::ID {
                    alt = 251;
                } else if sym2 == tok::RBRACE {
                    alt = 253;
                } else {
                    return Err(self.error_sym("unexpected", sym2));
                }
            } else if sym2 == tok::RBRACE {
                alt = -1;
            } else {
                return Err(self.error_sym("unexpected", sym2));
            }
            self.reset(mark);
            if alt != 251 {
                break;
            }
            sym = self.get_sym()?;
            sym = self.parse_enumerator(sym, dcl, &mut range)?;
        }
        if alt == 253 {
            sym = self.get_sym()?;
        }
        Ok(sym)
    }

    fn parse_enumerator(&mut self, sym: u8, dcl: &mut Dcl, range: &mut EnumRange) -> Parse<u8> {
        if sym != tok::ID {
            return Err(self.error_sym("<ID> expected, got", sym));
        }
        let name = self.token();
        let mut sym = self.get_sym()?;
        let mut val = Val::EMPTY;
        if sym == tok::EQUAL {
            sym = self.get_sym()?;
            sym = self.parse_conditional_expression(sym, &mut val)?;
        }
        self.add_enum_val(dcl, &name, val, range)?;
        Ok(sym)
    }

    /// `declarator`: the name it declares and the next token.
    fn parse_declarator(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<(u8, Vec<u8>)> {
        let mut nested_dcl = Dcl::nested();
        let mut nested = false;
        if sym == tok::STAR {
            sym = self.parse_pointer(sym, dcl)?;
        }
        let name;
        if sym == tok::ID {
            name = self.token();
            sym = self.get_sym()?;
        } else if sym == tok::LPAREN {
            sym = self.get_sym()?;
            if tok::is_attribute(sym) {
                sym = self.parse_attributes(sym, &mut nested_dcl)?;
            }
            let (next, inner) = self.parse_declarator(sym, &mut nested_dcl)?;
            name = inner;
            sym = next;
            if sym != tok::RPAREN {
                return Err(self.error_sym("')' expected, got", sym));
            }
            sym = self.get_sym()?;
            nested = true;
        } else {
            return Err(self.error_sym("unexpected", sym));
        }
        if sym == tok::LBRACK || sym == tok::LPAREN {
            sym = self.parse_array_or_function_declarators(sym, dcl)?;
        }
        if nested {
            self.nested_declaration(dcl, &mut nested_dcl)?;
        }
        Ok((sym, name))
    }

    fn parse_abstract_declarator(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        let mut nested_dcl = Dcl::nested();
        let mut nested = false;
        if sym == tok::STAR {
            sym = self.parse_pointer(sym, dcl)?;
        }
        if sym == tok::LPAREN && self.lookahead(sym, Self::check_nested_declarator_start)? {
            sym = self.get_sym()?;
            if tok::is_attribute(sym) {
                sym = self.parse_attributes(sym, &mut nested_dcl)?;
            }
            sym = self.parse_abstract_declarator(sym, &mut nested_dcl)?;
            if sym != tok::RPAREN {
                return Err(self.error_sym("')' expected, got", sym));
            }
            sym = self.get_sym()?;
            nested = true;
        }
        if sym == tok::LBRACK || sym == tok::LPAREN {
            sym = self.parse_array_or_function_declarators(sym, dcl)?;
        }
        if nested {
            self.nested_declaration(dcl, &mut nested_dcl)?;
        }
        Ok(sym)
    }

    fn parse_parameter_declarator(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        let mut nested_dcl = Dcl::nested();
        let mut nested = false;
        if sym == tok::STAR {
            sym = self.parse_pointer(sym, dcl)?;
        }
        if sym == tok::LPAREN && self.lookahead(sym, Self::check_nested_declarator_start)? {
            sym = self.get_sym()?;
            if tok::is_attribute(sym) {
                sym = self.parse_attributes(sym, &mut nested_dcl)?;
            }
            sym = self.parse_parameter_declarator(sym, &mut nested_dcl)?;
            if sym != tok::RPAREN {
                return Err(self.error_sym("')' expected, got", sym));
            }
            sym = self.get_sym()?;
            nested = true;
        } else if sym == tok::ID {
            sym = self.get_sym()?;
        } else if !matches!(sym, tok::LBRACK | tok::LPAREN | tok::RPAREN | tok::COMMA) {
            return Err(self.error_sym("unexpected", sym));
        }
        if sym == tok::LBRACK || sym == tok::LPAREN {
            sym = self.parse_array_or_function_declarators(sym, dcl)?;
        }
        if nested {
            self.nested_declaration(dcl, &mut nested_dcl)?;
        }
        Ok(sym)
    }

    fn parse_pointer(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        if sym != tok::STAR {
            return Err(self.error_sym("'*' expected, got", sym));
        }
        loop {
            let mut sym = self.get_sym()?;
            self.make_pointer_type(dcl)?;
            if tok::is_qualifier(sym) || tok::is_attribute(sym) {
                sym = self.parse_type_qualifier_list(sym, dcl)?;
            }
            if sym != tok::STAR {
                return Ok(sym);
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one function per rule of ffi.g, or per callback of ffi.c, as long as the original"
    )]
    fn parse_array_or_function_declarators(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        let mut dummy = Dcl::default();
        let mut len = Val::EMPTY;
        let mut args: Option<Vec<(TypeId, bool)>> = None;
        let mut flags = 0;
        if sym == tok::LBRACK {
            let mut sym = self.get_sym()?;
            let mark = self.mark();
            let mut sym2 = sym;
            let alt105 = if sym2 == tok::STATIC {
                106
            } else if tok::is_qualifier(sym2) || tok::is_attribute(sym2) {
                109
            } else if sym2 == tok::STAR {
                sym2 = self.get_sym()?;
                if sym2 == tok::RBRACK {
                    114
                } else if tok::is_expression(sym2, true) {
                    115
                } else {
                    return Err(self.error_sym("unexpected", sym2));
                }
            } else if tok::is_expression(sym2, false) {
                115
            } else if sym2 == tok::RBRACK {
                116
            } else {
                return Err(self.error_sym("unexpected", sym2));
            };
            self.reset(mark);
            match alt105 {
                106 => {
                    sym = self.get_sym()?;
                    if tok::is_qualifier(sym) || tok::is_attribute(sym) {
                        sym = self.parse_type_qualifier_list(sym, &mut dummy)?;
                    }
                    sym = self.parse_conditional_expression(sym, &mut len)?;
                }
                109 => {
                    sym = self.parse_type_qualifier_list(sym, &mut dummy)?;
                    let mark = self.mark();
                    let mut sym2 = sym;
                    let alt109 = if sym2 == tok::STATIC {
                        110
                    } else if sym2 == tok::STAR {
                        sym2 = self.get_sym()?;
                        if sym2 == tok::RBRACK {
                            112
                        } else if tok::is_expression(sym2, true) {
                            113
                        } else {
                            return Err(self.error_sym("unexpected", sym2));
                        }
                    } else if tok::is_expression(sym2, false) {
                        113
                    } else if sym2 == tok::RBRACK {
                        116
                    } else {
                        return Err(self.error_sym("unexpected", sym2));
                    };
                    self.reset(mark);
                    match alt109 {
                        110 => {
                            sym = self.get_sym()?;
                            sym = self.parse_conditional_expression(sym, &mut len)?;
                        }
                        116 => flags |= attr::INCOMPLETE_ARRAY,
                        112 => {
                            sym = self.get_sym()?;
                            flags |= attr::VLA;
                        }
                        _ => sym = self.parse_conditional_expression(sym, &mut len)?,
                    }
                }
                116 => flags |= attr::INCOMPLETE_ARRAY,
                114 => {
                    sym = self.get_sym()?;
                    flags |= attr::VLA;
                }
                _ => sym = self.parse_conditional_expression(sym, &mut len)?,
            }
            if sym != tok::RBRACK {
                return Err(self.error_sym("']' expected, got", sym));
            }
            sym = self.get_sym()?;
            if sym == tok::LBRACK || sym == tok::LPAREN {
                sym = self.parse_array_or_function_declarators(sym, dcl)?;
            }
            dcl.attr |= flags;
            self.make_array_type(dcl, len)?;
            Ok(sym)
        } else if sym == tok::LPAREN {
            let mut sym = self.get_sym()?;
            if tok::is_specifier_qualifier(sym) {
                sym = self.parse_parameter_declaration(sym, &mut args)?;
                let mut alt;
                loop {
                    let mark = self.mark();
                    let mut sym2 = sym;
                    if sym2 == tok::COMMA {
                        sym2 = self.get_sym()?;
                        if tok::is_specifier_qualifier(sym2) {
                            alt = 120;
                        } else if sym2 == tok::ELLIPSIS {
                            alt = 122;
                        } else {
                            return Err(self.error_sym("unexpected", sym2));
                        }
                    } else if sym2 == tok::RPAREN {
                        alt = 125;
                    } else {
                        return Err(self.error_sym("unexpected", sym2));
                    }
                    self.reset(mark);
                    if alt != 120 {
                        break;
                    }
                    sym = self.get_sym()?;
                    sym = self.parse_parameter_declaration(sym, &mut args)?;
                }
                if alt == 122 {
                    sym = self.get_sym()?;
                    if sym != tok::ELLIPSIS {
                        return Err(self.error_sym("'...' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                    flags |= attr::VARIADIC;
                }
            } else if sym == tok::ELLIPSIS {
                sym = self.get_sym()?;
                flags |= attr::VARIADIC;
            }
            if sym != tok::RPAREN {
                return Err(self.error_sym("')' expected, got", sym));
            }
            sym = self.get_sym()?;
            if sym == tok::LBRACK || sym == tok::LPAREN {
                sym = self.parse_array_or_function_declarators(sym, dcl)?;
            }
            dcl.attr |= flags;
            self.make_func_type(dcl, args)?;
            Ok(sym)
        } else {
            Err(self.error_sym("unexpected", sym))
        }
    }

    fn parse_parameter_declaration(
        &mut self,
        sym: u8,
        args: &mut Option<Vec<(TypeId, bool)>>,
    ) -> Parse<u8> {
        let old_allow_vla = self.allow_vla;
        self.allow_vla = true;
        let mut param = Dcl::default();
        let sym = self.parse_specifier_qualifier_list(sym, &mut param)?;
        let sym = self.parse_parameter_declarator(sym, &mut param)?;
        self.add_arg(args, &mut param)?;
        self.allow_vla = old_allow_vla;
        Ok(sym)
    }

    fn parse_type_name(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        let sym = self.parse_specifier_qualifier_list(sym, dcl)?;
        self.parse_abstract_declarator(sym, dcl)
    }

    fn parse_attributes(&mut self, mut sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        loop {
            match sym {
                tok::ATTRIBUTE | tok::ATTRIBUTE2 => {
                    sym = self.get_sym()?;
                    for _ in 0..2 {
                        if sym != tok::LPAREN {
                            return Err(self.error_sym("'(' expected, got", sym));
                        }
                        sym = self.get_sym()?;
                    }
                    sym = self.parse_attrib(sym, dcl)?;
                    while sym == tok::COMMA {
                        sym = self.get_sym()?;
                        sym = self.parse_attrib(sym, dcl)?;
                    }
                    for _ in 0..2 {
                        if sym != tok::RPAREN {
                            return Err(self.error_sym("')' expected, got", sym));
                        }
                        sym = self.get_sym()?;
                    }
                }
                tok::DECLSPEC => {
                    sym = self.get_sym()?;
                    if sym != tok::LPAREN {
                        return Err(self.error_sym("'(' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                    loop {
                        if sym != tok::ID {
                            return Err(self.error_sym("<ID> expected, got", sym));
                        }
                        let name = self.token();
                        sym = self.get_sym()?;
                        if sym == tok::LPAREN {
                            sym = self.get_sym()?;
                            let mut val = Val::EMPTY;
                            sym = self.parse_conditional_expression(sym, &mut val)?;
                            self.add_msvc_attribute_value(dcl, &name, val)?;
                            if sym != tok::RPAREN {
                                return Err(self.error_sym("')' expected, got", sym));
                            }
                            sym = self.get_sym()?;
                        }
                        if sym != tok::ID {
                            break;
                        }
                    }
                    if sym != tok::RPAREN {
                        return Err(self.error_sym("')' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                }
                tok::CDECL => {
                    sym = self.get_sym()?;
                    self.set_abi(dcl, abi::CDECL)?;
                }
                tok::STDCALL => {
                    sym = self.get_sym()?;
                    self.set_abi(dcl, abi::STDCALL)?;
                }
                tok::FASTCALL => {
                    sym = self.get_sym()?;
                    self.set_abi(dcl, abi::FASTCALL)?;
                }
                tok::THISCALL => {
                    sym = self.get_sym()?;
                    self.set_abi(dcl, abi::THISCALL)?;
                }
                tok::VECTORCALL => {
                    sym = self.get_sym()?;
                    self.set_abi(dcl, abi::VECTORCALL)?;
                }
                _ => return Err(self.error_sym("unexpected", sym)),
            }
            if !tok::is_attribute(sym) {
                return Ok(sym);
            }
        }
    }

    fn parse_attrib(&mut self, sym: u8, dcl: &mut Dcl) -> Parse<u8> {
        match sym {
            tok::ID => {
                let name = self.token();
                let mut sym = self.get_sym()?;
                if sym == tok::COMMA || sym == tok::RPAREN {
                    self.add_attribute(dcl, &name)?;
                } else if sym == tok::LPAREN {
                    sym = self.get_sym()?;
                    let orig = self.attribute_parsing;
                    self.attribute_parsing = true;
                    let mut val = Val::EMPTY;
                    sym = self.parse_conditional_expression(sym, &mut val)?;
                    self.add_attribute_value(dcl, &name, 0, val)?;
                    let mut n = 0;
                    while sym == tok::COMMA {
                        sym = self.get_sym()?;
                        let mut val = Val::EMPTY;
                        sym = self.parse_conditional_expression(sym, &mut val)?;
                        n += 1;
                        self.add_attribute_value(dcl, &name, n, val)?;
                    }
                    self.attribute_parsing = orig;
                    if sym != tok::RPAREN {
                        return Err(self.error_sym("')' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                } else {
                    return Err(self.error_sym("unexpected", sym));
                }
                Ok(sym)
            }
            tok::CONST..=tok::CONST3 => self.get_sym(),
            _ => Ok(sym),
        }
    }

    fn parse_initializer(&mut self, sym: u8) -> Parse<u8> {
        if sym != tok::EQUAL {
            return Err(self.error_sym("'=' expected, got", sym));
        }
        let mut sym = self.get_sym()?;
        if tok::is_expression(sym, true) {
            let mut dummy = Val::EMPTY;
            sym = self.parse_conditional_expression(sym, &mut dummy)?;
        } else if sym == tok::LBRACE {
            sym = self.get_sym()?;
            if sym == tok::LBRACK || sym == tok::POINT {
                sym = self.parse_designation(sym)?;
            }
            sym = self.parse_initializer(sym)?;
            let mut alt;
            loop {
                let mark = self.mark();
                let mut sym2 = sym;
                if sym2 == tok::COMMA {
                    sym2 = self.get_sym()?;
                    if matches!(sym2, tok::LBRACK | tok::POINT | tok::EQUAL) {
                        alt = 344;
                    } else if sym2 == tok::RBRACE {
                        alt = 347;
                    } else {
                        return Err(self.error_sym("unexpected", sym2));
                    }
                } else if sym2 == tok::RBRACE {
                    alt = 348;
                } else {
                    return Err(self.error_sym("unexpected", sym2));
                }
                self.reset(mark);
                if alt != 344 {
                    break;
                }
                sym = self.get_sym()?;
                if sym == tok::LBRACK || sym == tok::POINT {
                    sym = self.parse_designation(sym)?;
                }
                sym = self.parse_initializer(sym)?;
            }
            if alt == 347 {
                sym = self.get_sym()?;
            }
            if sym != tok::RBRACE {
                return Err(self.error_sym("'}' expected, got", sym));
            }
            sym = self.get_sym()?;
        } else {
            return Err(self.error_sym("unexpected", sym));
        }
        Ok(sym)
    }

    fn parse_designation(&mut self, mut sym: u8) -> Parse<u8> {
        loop {
            if sym == tok::LBRACK {
                sym = self.get_sym()?;
                let mut dummy = Val::EMPTY;
                sym = self.parse_conditional_expression(sym, &mut dummy)?;
                if sym != tok::RBRACK {
                    return Err(self.error_sym("']' expected, got", sym));
                }
                sym = self.get_sym()?;
            } else if sym == tok::POINT {
                sym = self.get_sym()?;
                if sym != tok::ID {
                    return Err(self.error_sym("<ID> expected, got", sym));
                }
                sym = self.get_sym()?;
            } else {
                return Err(self.error_sym("unexpected", sym));
            }
            if sym != tok::LBRACK && sym != tok::POINT {
                break;
            }
        }
        if sym != tok::EQUAL {
            return Err(self.error_sym("'=' expected, got", sym));
        }
        self.get_sym()
    }

    fn parse_expr_list(&mut self, sym: u8) -> Parse<u8> {
        let mut dummy = Val::EMPTY;
        let mut sym = self.parse_conditional_expression(sym, &mut dummy)?;
        while sym == tok::COMMA {
            sym = self.get_sym()?;
            sym = self.parse_conditional_expression(sym, &mut dummy)?;
        }
        Ok(sym)
    }

    fn parse_expression(&mut self, sym: u8, val: &mut Val) -> Parse<u8> {
        let mut sym = self.parse_conditional_expression(sym, val)?;
        while sym == tok::COMMA {
            sym = self.get_sym()?;
            sym = self.parse_conditional_expression(sym, val)?;
        }
        Ok(sym)
    }

    fn parse_conditional_expression(&mut self, sym: u8, val: &mut Val) -> Parse<u8> {
        let mut sym = self.parse_binary(sym, val, 0)?;
        if sym == tok::QUERY {
            sym = self.get_sym()?;
            let mut op2 = Val::EMPTY;
            sym = self.parse_expression(sym, &mut op2)?;
            if sym != tok::COLON {
                return Err(self.error_sym("':' expected, got", sym));
            }
            sym = self.get_sym()?;
            let mut op3 = Val::EMPTY;
            sym = self.parse_conditional_expression(sym, &mut op3)?;
            expr_conditional(val, op2, op3);
        }
        Ok(sym)
    }

    /// `logical_or_expression` down to `multiplicative_expression`, one
    /// level per precedence, left to right within each as the grammar's
    /// loops are.
    fn parse_binary(&mut self, sym: u8, val: &mut Val, level: u8) -> Parse<u8> {
        if level > 9 {
            return self.parse_cast_expression(sym, val);
        }
        let mut sym = self.parse_binary(sym, val, level + 1)?;
        loop {
            let found = matches!(
                (level, sym),
                (0, tok::BAR_BAR)
                    | (1, tok::AND_AND)
                    | (2, tok::BAR)
                    | (3, tok::UPARROW)
                    | (4, tok::AND)
                    | (5, tok::EQUAL_EQUAL | tok::BANG_EQUAL)
                    | (
                        6,
                        tok::LESS | tok::GREATER | tok::LESS_EQUAL | tok::GREATER_EQUAL
                    )
                    | (7, tok::LESS_LESS | tok::GREATER_GREATER)
                    | (8, tok::PLUS | tok::MINUS)
                    | (9, tok::STAR | tok::SLASH | tok::PERCENT)
            );
            if !found {
                return Ok(sym);
            }
            let op = sym;
            sym = self.get_sym()?;
            let mut op2 = Val::EMPTY;
            sym = self.parse_binary(sym, &mut op2, level + 1)?;
            match op {
                tok::BAR_BAR => expr_bool_or(val, op2),
                tok::AND_AND => expr_bool_and(val, op2),
                tok::BAR => expr_int_math(val, op2, Op::Or),
                tok::UPARROW => expr_int_math(val, op2, Op::Xor),
                tok::AND => expr_int_math(val, op2, Op::And),
                tok::EQUAL_EQUAL => expr_cmp(val, op2, Cmp::Eq),
                tok::BANG_EQUAL => expr_cmp(val, op2, Cmp::Ne),
                tok::LESS => expr_cmp(val, op2, Cmp::Lt),
                tok::GREATER => expr_cmp(val, op2, Cmp::Gt),
                tok::LESS_EQUAL => expr_cmp(val, op2, Cmp::Le),
                tok::GREATER_EQUAL => expr_cmp(val, op2, Cmp::Ge),
                tok::LESS_LESS => expr_int_math(val, op2, Op::Shl),
                tok::GREATER_GREATER => expr_int_math(val, op2, Op::Shr),
                tok::PLUS => expr_math(val, op2, Op::Add),
                tok::MINUS => expr_math(val, op2, Op::Sub),
                tok::STAR => expr_math(val, op2, Op::Mul),
                tok::SLASH => expr_math(val, op2, Op::Div),
                _ => expr_int_math(val, op2, Op::Mod),
            }
        }
    }

    fn parse_cast_expression(&mut self, mut sym: u8, val: &mut Val) -> Parse<u8> {
        let mut cast = None;
        if sym == tok::LPAREN && self.lookahead(sym, Self::check_paren_type_name)? {
            sym = self.get_sym()?;
            let mut dcl = Dcl::default();
            sym = self.parse_type_name(sym, &mut dcl)?;
            if sym != tok::RPAREN {
                return Err(self.error_sym("')' expected, got", sym));
            }
            sym = self.get_sym()?;
            cast = Some(dcl);
        }
        sym = self.parse_unary_expression(sym, val)?;
        if let Some(mut dcl) = cast {
            self.expr_cast(val, &mut dcl)?;
        }
        Ok(sym)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one function per rule of ffi.g, or per callback of ffi.c, as long as the original"
    )]
    fn parse_unary_expression(&mut self, sym: u8, val: &mut Val) -> Parse<u8> {
        let mut dcl = Dcl::default();
        let mut sym = sym;
        match sym {
            tok::ID => {
                let (at, len) = (self.text, self.pos - self.text);
                let name = self.token();
                sym = self.get_sym()?;
                self.resolve_const(&name, at, len, val);
                while matches!(
                    sym,
                    tok::LBRACK
                        | tok::LPAREN
                        | tok::POINT
                        | tok::MINUS_GREATER
                        | tok::PLUS_PLUS
                        | tok::MINUS_MINUS
                ) {
                    match sym {
                        tok::LBRACK => {
                            sym = self.get_sym()?;
                            sym = self.parse_expr_list(sym)?;
                            if sym != tok::RBRACK {
                                return Err(self.error_sym("']' expected, got", sym));
                            }
                            sym = self.get_sym()?;
                        }
                        tok::LPAREN => {
                            sym = self.get_sym()?;
                            if tok::is_expression(sym, true) {
                                sym = self.parse_expr_list(sym)?;
                            }
                            if sym != tok::RPAREN {
                                return Err(self.error_sym("')' expected, got", sym));
                            }
                            sym = self.get_sym()?;
                        }
                        tok::POINT | tok::MINUS_GREATER => {
                            sym = self.get_sym()?;
                            if sym != tok::ID {
                                return Err(self.error_sym("<ID> expected, got", sym));
                            }
                            sym = self.get_sym()?;
                        }
                        _ => sym = self.get_sym()?,
                    }
                    val.error();
                }
            }
            tok::OCTNUMBER | tok::DECNUMBER | tok::HEXNUMBER => {
                // `strtoll(yy_text, NULL, base)` reads on past the token,
                // which a token the scanner put back has none of.
                let (base, skip) = match sym {
                    tok::OCTNUMBER => (8, 0),
                    tok::DECNUMBER => (10, 0),
                    _ => (16, 2),
                };
                let rest = self.buf.get(self.text + skip..).unwrap_or_default();
                let len = (self.pos - self.text).saturating_sub(skip);
                val_number(val, base, rest, len);
                sym = self.get_sym()?;
            }
            tok::FLOATNUMBER => {
                val_float_number(val, &self.token());
                sym = self.get_sym()?;
            }
            tok::STRING => {
                let (at, len) = (self.text, self.pos - self.text);
                val_string(val, self.buf, at, len);
                sym = self.get_sym()?;
            }
            tok::CHARACTER => {
                val_character(val, &self.token());
                sym = self.get_sym()?;
            }
            tok::LPAREN => {
                sym = self.get_sym()?;
                sym = self.parse_expression(sym, val)?;
                if sym != tok::RPAREN {
                    return Err(self.error_sym("')' expected, got", sym));
                }
                sym = self.get_sym()?;
            }
            tok::PLUS_PLUS | tok::MINUS_MINUS => {
                sym = self.get_sym()?;
                sym = self.parse_unary_expression(sym, val)?;
                val.error();
            }
            tok::AND | tok::STAR => {
                sym = self.get_sym()?;
                sym = self.parse_cast_expression(sym, val)?;
                val.error();
            }
            tok::PLUS | tok::MINUS | tok::TILDE | tok::BANG => {
                let op = sym;
                sym = self.get_sym()?;
                sym = self.parse_cast_expression(sym, val)?;
                match op {
                    tok::PLUS => expr_plus(val),
                    tok::MINUS => expr_neg(val),
                    tok::TILDE => expr_bw_not(val),
                    _ => expr_bool_not(val),
                }
            }
            tok::SIZEOF | tok::ALIGNOF2 | tok::ALIGNOF3 => {
                let sizeof = sym == tok::SIZEOF;
                sym = self.get_sym()?;
                if sym == tok::LPAREN && self.lookahead(sym, Self::check_paren_type_name)? {
                    sym = self.get_sym()?;
                    sym = self.parse_type_name(sym, &mut dcl)?;
                    if sym != tok::RPAREN {
                        return Err(self.error_sym("')' expected, got", sym));
                    }
                    sym = self.get_sym()?;
                    let ty = self.finalize(&mut dcl)?;
                    let ty = self.types.get(ty);
                    if sizeof {
                        val.kind = if ty.size > 0xffff_ffff {
                            ValKind::Uint64
                        } else {
                            ValKind::Uint32
                        };
                        val.raw = ty.size as u64;
                    } else {
                        val.kind = ValKind::Uint32;
                        val.raw = u64::from(ty.align);
                    }
                } else if tok::is_expression(sym, true) {
                    sym = self.parse_unary_expression(sym, val)?;
                    if sizeof {
                        expr_sizeof_val(val, self.buf);
                    } else {
                        expr_alignof_val(val);
                    }
                } else {
                    return Err(self.error_sym("unexpected", sym));
                }
            }
            tok::ALIGNOF => {
                sym = self.get_sym()?;
                if sym != tok::LPAREN {
                    return Err(self.error_sym("'(' expected, got", sym));
                }
                sym = self.get_sym()?;
                sym = self.parse_type_name(sym, &mut dcl)?;
                if sym != tok::RPAREN {
                    return Err(self.error_sym("')' expected, got", sym));
                }
                sym = self.get_sym()?;
                let ty = self.finalize(&mut dcl)?;
                val.kind = ValKind::Uint32;
                val.raw = u64::from(self.types.get(ty).align);
            }
            _ => return Err(self.error_sym("unexpected", sym)),
        }
        Ok(sym)
    }

    // ------------------------------------------------------- lookaheads

    /// A `synpred`: `check` tried from `sym`, the lexer put back after it.
    fn lookahead(&mut self, sym: u8, check: fn(&mut Self, u8) -> Parse<Option<u8>>) -> Parse<bool> {
        let mark = self.mark();
        let matched = check(self, sym)?.is_some();
        self.reset(mark);
        Ok(matched)
    }

    /// `check_nested_declarator_start`.
    fn check_nested_declarator_start(&mut self, sym: u8) -> Parse<Option<u8>> {
        if sym != tok::LPAREN {
            return Ok(None);
        }
        let sym = self.get_sym()?;
        if sym == tok::ID && !self.is_typedef_name(&self.token()) {
            return self.get_sym().map(Some);
        }
        if matches!(
            sym,
            tok::ATTRIBUTE
                | tok::ATTRIBUTE2
                | tok::DECLSPEC
                | tok::STAR
                | tok::LPAREN
                | tok::LBRACK
        ) {
            return self.get_sym().map(Some);
        }
        Ok(None)
    }

    /// `check_type_name_start`, whose list names `__restict` where the
    /// keyword is `__restrict`, as `ffi.g` spells it.
    fn check_type_name_start(&mut self, sym: u8) -> Parse<Option<u8>> {
        if sym == tok::ID && self.is_typedef_name(&self.token()) {
            return self.get_sym().map(Some);
        }
        let starts = (tok::VOID..=tok::COMPLEX4).contains(&sym)
            || matches!(
                sym,
                tok::STRUCT
                    | tok::UNION
                    | tok::ENUM
                    | tok::CONST
                    | tok::CONST2
                    | tok::CONST3
                    | tok::RESTRICT
                    | tok::RESTICT
                    | tok::RESTRICT3
                    | tok::VOLATILE
                    | 24
                    | tok::VOLATILE3
                    | tok::ATOMIC
                    | tok::ATTRIBUTE
                    | tok::ATTRIBUTE2
                    | tok::DECLSPEC
            );
        if starts {
            return self.get_sym().map(Some);
        }
        Ok(None)
    }

    /// `"(" type_name_start`, the `synpred` of a cast, `sizeof` and
    /// `__alignof`.
    fn check_paren_type_name(&mut self, sym: u8) -> Parse<Option<u8>> {
        if sym != tok::LPAREN {
            return Ok(None);
        }
        let sym = self.get_sym()?;
        self.check_type_name_start(sym)
    }

    // ------------------------------------------------- ffi.c's callbacks

    /// `zend_ffi_is_typedef_name`.
    fn is_typedef_name(&self, name: &[u8]) -> bool {
        if let Some(sym) = self
            .scope
            .symbols
            .as_ref()
            .and_then(|symbols| symbols.get(name))
        {
            return sym.kind == SymbolKind::Type;
        }
        predefined(name).is_some()
    }

    /// `zend_ffi_resolve_typedef`.
    fn resolve_typedef(&mut self, name: &[u8], dcl: &mut Dcl) -> Parse<()> {
        if let Some(sym) = self
            .scope
            .symbols
            .as_ref()
            .and_then(|symbols| symbols.get(name))
            && sym.kind == SymbolKind::Type
        {
            dcl.ty = Some(sym.ty);
            dcl.owned = false;
            if sym.is_const {
                dcl.attr |= attr::CONST;
            }
            return Ok(());
        }
        if let Some(ty) = predefined(name) {
            dcl.ty = Some(ty);
            dcl.owned = false;
            return Ok(());
        }
        Err(Bail(format!(
            "Undefined C type \"{}\" at line {}",
            String::from_utf8_lossy(name),
            self.line
        )))
    }

    /// `zend_ffi_resolve_const`: an enumerator's value, an attribute's
    /// argument as the name it is, or an error value.
    fn resolve_const(&self, name: &[u8], at: usize, len: usize, val: &mut Val) {
        if self.attribute_parsing {
            *val = Val {
                kind: ValKind::Name,
                raw: 0,
                at,
                len,
            };
            return;
        }
        if let Some(sym) = self
            .scope
            .symbols
            .as_ref()
            .and_then(|symbols| symbols.get(name))
            && sym.kind == SymbolKind::Const
        {
            val.set_i64(sym.value);
            val.kind = match self.types.get(sym.ty).kind {
                Kind::Int(Int::I64) => ValKind::Int64,
                Kind::Int(Int::U8 | Int::U16 | Int::U32) => ValKind::Uint32,
                Kind::Int(Int::U64) => ValKind::Uint64,
                _ => ValKind::Int32,
            };
            return;
        }
        val.kind = ValKind::Error;
    }

    /// `zend_ffi_finalize_type`: the type the specifiers spell, where no
    /// type was named.
    fn finalize(&mut self, dcl: &mut Dcl) -> Parse<TypeId> {
        use dcl::{
            BOOL, CHAR, COMPLEX, DOUBLE, FLOAT, INT, LONG, LONG_LONG, SHORT, SIGNED, UNSIGNED,
        };
        if let Some(ty) = dcl.ty {
            return Ok(ty);
        }
        let ty = match dcl.flags & dcl::TYPE_SPECIFIERS {
            dcl::VOID => TypeId::VOID,
            CHAR => TypeId::CHAR,
            f if f == CHAR | SIGNED => TypeId::SINT8,
            f if f == CHAR | UNSIGNED || f == BOOL => TypeId::UINT8,
            f if f == SHORT
                || f == SHORT | SIGNED
                || f == SHORT | INT
                || f == SHORT | SIGNED | INT =>
            {
                TypeId::SINT16
            }
            f if f == SHORT | UNSIGNED || f == SHORT | UNSIGNED | INT => TypeId::UINT16,
            f if f == INT || f == SIGNED || f == SIGNED | INT => TypeId::SINT32,
            f if f == UNSIGNED || f == UNSIGNED | INT => TypeId::UINT32,
            f if f == LONG || f == LONG | SIGNED || f == LONG | INT || f == LONG | SIGNED | INT => {
                TypeId::SINT64
            }
            f if f == LONG | UNSIGNED || f == LONG | UNSIGNED | INT => TypeId::UINT64,
            f if f == LONG_LONG | LONG
                || f == LONG_LONG | LONG | SIGNED
                || f == LONG_LONG | LONG | INT
                || f == LONG_LONG | LONG | SIGNED | INT =>
            {
                TypeId::SINT64
            }
            f if f == LONG_LONG | LONG | UNSIGNED || f == LONG_LONG | LONG | UNSIGNED | INT => {
                TypeId::UINT64
            }
            FLOAT => TypeId::FLOAT,
            DOUBLE => TypeId::DOUBLE,
            f if f == LONG | DOUBLE => TypeId::LONG_DOUBLE,
            f if f == FLOAT | COMPLEX || f == DOUBLE | COMPLEX || f == DOUBLE | LONG | COMPLEX => {
                return Err(Bail(format!(
                    "Unsupported type _Complex at line {}",
                    self.line
                )));
            }
            _ => {
                return Err(Bail(format!(
                    "Unsupported type specifier combination at line {}",
                    self.line
                )));
            }
        };
        dcl.ty = Some(ty);
        dcl.owned = false;
        dcl.flags &= !dcl::TYPE_SPECIFIERS;
        dcl.flags |= dcl::TYPEDEF_NAME;
        Ok(ty)
    }

    /// `zend_ffi_make_enum_type`.
    fn make_enum_type(&mut self, dcl: &mut Dcl) {
        let attr = self.default_attr | (dcl.attr & attr::ENUM_ATTRS);
        let (of, size) = if attr & attr::PACKED != 0 {
            (Int::U8, 1)
        } else {
            (Int::U32, 4)
        };
        let id = self.types.add(Type {
            kind: Kind::Enum { tag: None, of },
            size,
            align: size as u32,
            attr,
        });
        dcl.ty = Some(id);
        dcl.owned = true;
        dcl.attr &= !attr::ENUM_ATTRS;
    }

    /// `zend_ffi_add_enum_val`: one enumerator, its value, the type the
    /// values so far fit, and the constant it declares.
    #[expect(
        clippy::too_many_lines,
        reason = "one function per rule of ffi.g, or per callback of ffi.c, as long as the original"
    )]
    fn add_enum_val(
        &mut self,
        enum_dcl: &Dcl,
        name: &[u8],
        val: Val,
        range: &mut EnumRange,
    ) -> Parse<()> {
        let Some(enum_id) = enum_dcl.ty else {
            return Ok(());
        };
        let (packed, mut is_signed) = match &self.types.get(enum_id).kind {
            Kind::Enum { of, .. } => (
                self.types.get(enum_id).attr & attr::PACKED != 0,
                matches!(of, Int::I8 | Int::I16 | Int::I32 | Int::I64),
            ),
            _ => return Ok(()),
        };
        let shown = String::from_utf8_lossy(name);
        let mut overflow = false;
        let value: i64 = match val.kind {
            ValKind::Empty => {
                if is_signed {
                    overflow = range.last == i64::MAX;
                } else {
                    overflow = (range.min != 0 || range.max != 0)
                        && range.last.cast_unsigned() == u64::MAX;
                }
                range.last.wrapping_add(1)
            }
            ValKind::Char => {
                if !is_signed && val.ch() < 0 {
                    if range.max.cast_unsigned() > 0x7FFF_FFFF_FFFF_FFFF {
                        overflow = true;
                    } else {
                        is_signed = true;
                    }
                }
                i64::from(val.ch())
            }
            ValKind::Int32 | ValKind::Int64 => {
                if !is_signed && val.i64() < 0 {
                    if range.max.cast_unsigned() > 0x7FFF_FFFF_FFFF_FFFF {
                        overflow = true;
                    } else {
                        is_signed = true;
                    }
                }
                val.i64()
            }
            ValKind::Uint32 | ValKind::Uint64 => {
                if is_signed && val.u64() > 0x7FFF_FFFF_FFFF_FFFF {
                    overflow = true;
                }
                val.u64().cast_signed()
            }
            _ => {
                return Err(Bail(format!(
                    "Enumerator value \"{shown}\" must be an integer at line {}",
                    self.line
                )));
            }
        };
        if overflow {
            return Err(Bail(format!(
                "Overflow in enumeration values \"{shown}\" at line {}",
                self.line
            )));
        }
        let of = if is_signed {
            range.min = range.min.min(value);
            range.max = range.max.max(value);
            if packed && range.min >= -0x80 && range.max <= 0x7F {
                Int::I8
            } else if packed && range.min >= -0x8000 && range.max <= 0x7FFF {
                Int::I16
            } else if range.min >= -0x8000_0000 && range.max <= 0x7FFF_FFFF {
                Int::I32
            } else {
                Int::I64
            }
        } else {
            range.min = range
                .min
                .cast_unsigned()
                .min(value.cast_unsigned())
                .cast_signed();
            range.max = range
                .max
                .cast_unsigned()
                .max(value.cast_unsigned())
                .cast_signed();
            let max = range.max.cast_unsigned();
            if packed && max <= 0xFF {
                Int::U8
            } else if packed && max <= 0xFFFF {
                Int::U16
            } else if max <= 0xFFFF_FFFF {
                Int::U32
            } else {
                Int::U64
            }
        };
        let sym_type = of.id();
        let (size, align) = {
            let ty = self.types.get(sym_type);
            (ty.size, ty.align)
        };
        if let Some(ty) = self.types.get_mut(enum_id) {
            if let Kind::Enum { of: kind, .. } = &mut ty.kind {
                *kind = of;
            }
            ty.size = size;
            ty.align = align;
        }
        range.last = value;
        let symbols = self.scope.symbols.get_or_insert_with(Table::default);
        let added = symbols.add(
            name,
            Symbol {
                kind: SymbolKind::Const,
                is_const: false,
                ty: sym_type,
                addr: 0,
                value,
            },
        );
        if !added {
            return Err(Bail(format!(
                "Redeclaration of \"{shown}\" at line {}",
                self.line
            )));
        }
        Ok(())
    }

    /// `zend_ffi_make_struct_type`.
    fn make_struct_type(&mut self, dcl: &mut Dcl) {
        let mut attr = self.default_attr | (dcl.attr & attr::STRUCT_ATTRS);
        if dcl.flags & dcl::UNION != 0 {
            attr |= attr::UNION;
        }
        let id = self.types.add(Type {
            kind: Kind::Struct(Record::default()),
            size: 0,
            align: if dcl.align > 1 { dcl.align } else { 1 },
            attr,
        });
        dcl.ty = Some(id);
        dcl.owned = true;
        dcl.attr &= !attr::STRUCT_ATTRS;
        dcl.align = 0;
    }

    /// `zend_ffi_validate_prev_field_type`: a flexible array member is
    /// the struct's last.
    fn validate_prev_field_type(&self, struct_id: TypeId) -> Parse<()> {
        if let Some(record) = self.types.get(struct_id).record()
            && let Some((_, last)) = record.fields.last()
            && self.types.get(last.ty).attr & attr::INCOMPLETE_ARRAY != 0
        {
            return Err(Bail(format!(
                "Flexible array member not at end of struct at line {}",
                self.line
            )));
        }
        Ok(())
    }

    /// `zend_ffi_validate_field_type`.
    fn validate_field_type(&self, ty: TypeId, struct_id: TypeId) -> Parse<()> {
        if ty == struct_id {
            return Err(Bail(format!(
                "Struct/union can't contain an instance of itself at line {}",
                self.line
            )));
        }
        self.validate_var_type(ty, true)?;
        if self.types.get(struct_id).attr & attr::UNION != 0
            && self.types.get(ty).attr & attr::INCOMPLETE_ARRAY != 0
        {
            return Err(Bail(format!(
                "Flexible array member in union at line {}",
                self.line
            )));
        }
        self.validate_prev_field_type(struct_id)
    }

    /// The struct a declaration is building, and its record.
    fn struct_of(&self, dcl: &Dcl) -> Option<TypeId> {
        dcl.ty.filter(|ty| self.types.get(*ty).record().is_some())
    }

    /// `zend_ffi_add_field`.
    fn add_field(&mut self, struct_dcl: &Dcl, name: &[u8], field_dcl: &mut Dcl) -> Parse<()> {
        let Some(struct_id) = self.struct_of(struct_dcl) else {
            return Ok(());
        };
        let field_ty = self.finalize(field_dcl)?;
        self.validate_field_type(field_ty, struct_id)?;
        let (fsize, falign) = {
            let t = self.types.get(field_ty);
            (t.size, t.align)
        };
        let field_packed = field_dcl.attr & attr::PACKED != 0;
        let mut offset = 0;
        if let Some(st) = self.types.get_mut(struct_id) {
            let packed = st.attr & attr::PACKED != 0;
            if !packed && !field_packed {
                st.align = st.align.max(falign.max(field_dcl.align));
            }
            if st.attr & attr::UNION != 0 {
                st.size = st.size.max(fsize);
            } else {
                if !packed && !field_packed {
                    let field_align = falign.max(field_dcl.align) as usize;
                    st.size = st.size.div_ceil(field_align) * field_align;
                }
                offset = st.size;
                st.size += fsize;
            }
        }
        let field = Field {
            offset,
            is_const: field_dcl.attr & attr::CONST != 0,
            is_nested: false,
            first_bit: 0,
            bits: 0,
            ty: field_ty,
        };
        self.types.own(struct_id, field_ty, field_dcl.owned);
        field_dcl.owned = false;
        if let Some(Type {
            kind: Kind::Struct(record),
            ..
        }) = self.types.get_mut(struct_id)
        {
            if record.field(name).is_some() {
                return Err(Bail(format!(
                    "Duplicate field name \"{}\" at line {}",
                    String::from_utf8_lossy(name),
                    self.line
                )));
            }
            record.fields.push((Some(name.to_vec()), field));
        }
        Ok(())
    }

    /// `zend_ffi_add_anonymous_field`: an anonymous struct or union's
    /// fields laid into the outer one.
    fn add_anonymous_field(&mut self, struct_dcl: &Dcl, field_dcl: &mut Dcl) -> Parse<()> {
        let Some(struct_id) = self.struct_of(struct_dcl) else {
            return Ok(());
        };
        let field_ty = self.finalize(field_dcl)?;
        let Some(inner) = self.types.get(field_ty).record().cloned() else {
            return Err(Bail(format!(
                "Declaration does not declare anything at line {}",
                self.line
            )));
        };
        let (fsize, falign) = {
            let t = self.types.get(field_ty);
            (t.size, t.align)
        };
        let field_packed = field_dcl.attr & attr::PACKED != 0;
        let (packed, union) = {
            let st = self.types.get(struct_id);
            (st.attr & attr::PACKED != 0, st.attr & attr::UNION != 0)
        };
        if let Some(st) = self.types.get_mut(struct_id)
            && !packed
            && !field_packed
        {
            st.align = st.align.max(falign.max(field_dcl.align));
        }
        if !union {
            self.validate_prev_field_type(struct_id)?;
            if let Some(st) = self.types.get_mut(struct_id)
                && !packed
                && !field_packed
            {
                let field_align = falign.max(field_dcl.align) as usize;
                st.size = st.size.div_ceil(field_align) * field_align;
            }
        }
        let base = self.types.get(struct_id).size;
        for (key, field) in inner.fields {
            let mut new_field = field;
            if !union {
                new_field.offset = base + field.offset;
            }
            new_field.is_nested = true;
            if let Some(Type {
                kind: Kind::Struct(record),
                ..
            }) = self.types.get_mut(struct_id)
            {
                match key {
                    Some(name) => {
                        if record.field(&name).is_some() {
                            return Err(Bail(format!(
                                "Duplicate field name \"{}\" at line {}",
                                String::from_utf8_lossy(&name),
                                self.line
                            )));
                        }
                        record.fields.push((Some(name), new_field));
                    }
                    None => record.fields.push((None, field)),
                }
            }
            self.types.move_owned(field_ty, struct_id, field.ty);
        }
        if let Some(st) = self.types.get_mut(struct_id) {
            if union {
                st.size = st.size.max(fsize);
            } else {
                st.size += fsize;
            }
        }
        if field_dcl.owned {
            self.types.free(field_ty);
        }
        field_dcl.ty = None;
        Ok(())
    }

    /// `zend_ffi_add_bit_field`.
    #[expect(
        clippy::too_many_lines,
        reason = "one function per rule of ffi.g, or per callback of ffi.c, as long as the original"
    )]
    fn add_bit_field(
        &mut self,
        struct_dcl: &Dcl,
        name: Option<&[u8]>,
        field_dcl: &mut Dcl,
        bits: Val,
    ) -> Parse<()> {
        let Some(struct_id) = self.struct_of(struct_dcl) else {
            return Ok(());
        };
        let field_ty = self.finalize(field_dcl)?;
        self.validate_field_type(field_ty, struct_id)?;
        let shown = name.map_or_else(
            || String::from("<anonymous>"),
            |n| String::from_utf8_lossy(n).into_owned(),
        );
        let (code, fsize) = {
            let t = self.types.get(field_ty);
            (t.code(), t.size)
        };
        if !(4..=13).contains(&code) {
            return Err(Bail(format!(
                "Wrong type of bit field \"{shown}\" at line {}",
                self.line
            )));
        }
        let limit = (fsize * 8) as u64;
        if bits.is_signed() {
            if bits.i64() < 0 {
                return Err(Bail(format!(
                    "Negative width in bit-field \"{shown}\" at line {}",
                    self.line
                )));
            } else if bits.i64() == 0 {
                if name.is_some() {
                    return Err(Bail(format!(
                        "Zero width in bit-field \"{shown}\" at line {}",
                        self.line
                    )));
                }
                return Ok(());
            } else if bits.u64() > limit {
                return Err(Bail(format!(
                    "Width of \"{shown}\" exceeds its type at line {}",
                    self.line
                )));
            }
        } else if bits.is_unsigned() {
            if bits.u64() == 0 {
                if name.is_some() {
                    return Err(Bail(format!(
                        "Zero width in bit-field \"{shown}\" at line {}",
                        self.line
                    )));
                }
                return Ok(());
            } else if bits.u64() > limit {
                return Err(Bail(format!(
                    "Width of \"{shown}\" exceeds its type at line {}",
                    self.line
                )));
            }
        } else {
            return Err(Bail(format!(
                "Bit field \"{shown}\" width not an integer constant at line {}",
                self.line
            )));
        }
        let width = bits.u64();
        let mut field = Field {
            offset: 0,
            is_const: field_dcl.attr & attr::CONST != 0,
            is_nested: false,
            first_bit: 0,
            bits: width as u8,
            ty: field_ty,
        };
        let prev = self
            .types
            .get(struct_id)
            .record()
            .and_then(|record| record.fields.last().map(|(_, f)| *f));
        if let Some(st) = self.types.get_mut(struct_id) {
            let packed = st.attr & attr::PACKED != 0;
            if !packed {
                st.align = st.align.max(4);
            }
            if st.attr & attr::UNION != 0 {
                let need = if packed {
                    width.div_ceil(8) as usize
                } else {
                    (width.div_ceil(32) * 4) as usize
                };
                st.size = st.size.max(need);
            } else {
                if let Some(prev) = prev.filter(|prev| prev.bits != 0) {
                    field.offset = prev.offset;
                    field.first_bit = prev.first_bit.wrapping_add(prev.bits);
                } else {
                    field.offset = st.size;
                }
                let used = u64::from(field.first_bit) + width;
                st.size = field.offset
                    + if packed {
                        used.div_ceil(8) as usize
                    } else {
                        (used.div_ceil(32) * 4) as usize
                    };
            }
        }
        self.types.own(struct_id, field_ty, field_dcl.owned);
        field_dcl.owned = false;
        if let Some(Type {
            kind: Kind::Struct(record),
            ..
        }) = self.types.get_mut(struct_id)
        {
            match name {
                Some(name) => {
                    if record.field(name).is_some() {
                        return Err(Bail(format!(
                            "Duplicate field name \"{shown}\" at line {}",
                            self.line
                        )));
                    }
                    record.fields.push((Some(name.to_vec()), field));
                }
                None => record.fields.push((None, field)),
            }
        }
        Ok(())
    }

    /// `zend_ffi_adjust_struct_size`.
    fn adjust_struct_size(&mut self, dcl: &mut Dcl) {
        if let Some(id) = self.struct_of(dcl)
            && let Some(st) = self.types.get_mut(id)
        {
            if dcl.align > st.align {
                st.align = dcl.align;
            }
            if st.attr & attr::PACKED == 0 {
                let align = st.align.max(1) as usize;
                st.size = st.size.div_ceil(align) * align;
            }
        }
        dcl.align = 0;
    }

    /// `zend_ffi_make_pointer_type`.
    fn make_pointer_type(&mut self, dcl: &mut Dcl) -> Parse<()> {
        let attr = self.default_attr | (dcl.attr & attr::POINTER_ATTRS);
        let to = self.finalize(dcl)?;
        self.validate_vla(to)?;
        let id = self.types.pointer_to(to, attr);
        self.types.own(id, to, dcl.owned);
        dcl.ty = Some(id);
        dcl.owned = true;
        dcl.flags &= !dcl::TYPE_QUALIFIERS;
        dcl.attr &= !attr::POINTER_ATTRS;
        dcl.align = 0;
        Ok(())
    }

    /// `zend_ffi_validate_array_element_type`.
    fn validate_array_element_type(&self, ty: TypeId) -> Parse<()> {
        let t = self.types.get(ty);
        if matches!(t.kind, Kind::Func(_)) {
            return Err(Bail(format!(
                "Array of functions is not allowed at line {}",
                self.line
            )));
        }
        if matches!(t.kind, Kind::Array { .. }) && t.attr & attr::INCOMPLETE_ARRAY != 0 {
            return Err(Bail(format!(
                "Only the leftmost array can be undimensioned at line {}",
                self.line
            )));
        }
        self.validate_type(ty, false, true)
    }

    /// `zend_ffi_make_array_type`.
    fn make_array_type(&mut self, dcl: &mut Dcl, len: Val) -> Parse<()> {
        let element = self.finalize(dcl)?;
        let length: i64 = match len.kind {
            ValKind::Empty => 0,
            ValKind::Uint32 | ValKind::Uint64 => len.u64().cast_signed(),
            ValKind::Int32 | ValKind::Int64 => len.i64(),
            ValKind::Char => i64::from(len.ch()),
            _ => {
                return Err(Bail(format!(
                    "Unsupported array index type at line {}",
                    self.line
                )));
            }
        };
        if length < 0 {
            return Err(Bail(format!("Negative array index at line {}", self.line)));
        }
        self.validate_array_element_type(element)?;
        let (esize, ealign) = {
            let t = self.types.get(element);
            (t.size, t.align)
        };
        let id = self.types.add(Type {
            kind: Kind::Array {
                of: element,
                len: length,
            },
            size: (length as usize).wrapping_mul(esize),
            align: ealign,
            attr: self.default_attr | (dcl.attr & attr::ARRAY_ATTRS),
        });
        self.types.own(id, element, dcl.owned);
        dcl.ty = Some(id);
        dcl.owned = true;
        dcl.flags &= !dcl::TYPE_QUALIFIERS;
        dcl.attr &= !attr::ARRAY_ATTRS;
        dcl.align = 0;
        Ok(())
    }

    /// `zend_ffi_validate_func_ret_type`.
    fn validate_func_ret_type(&self, ty: TypeId) -> Parse<()> {
        match self.types.get(ty).kind {
            Kind::Func(_) => Err(Bail(format!(
                "Function returning function is not allowed at line {}",
                self.line
            ))),
            Kind::Array { .. } => Err(Bail(format!(
                "Function returning array is not allowed at line {}",
                self.line
            ))),
            _ => self.validate_incomplete_type(ty, true, false),
        }
    }

    /// `zend_ffi_make_func_type`.
    fn make_func_type(
        &mut self,
        dcl: &mut Dcl,
        mut args: Option<Vec<(TypeId, bool)>>,
    ) -> Parse<()> {
        let ret = self.finalize(dcl)?;
        if let Some(list) = &args {
            let mut no_args = false;
            for (arg, _) in list {
                if matches!(self.types.get(*arg).kind, Kind::Void) {
                    if list.len() != 1 {
                        return Err(Bail(format!(
                            "void type is not allowed at line {}",
                            self.line
                        )));
                    }
                    no_args = true;
                }
            }
            if no_args {
                args = None;
            }
        }
        self.validate_func_ret_type(ret)?;
        let abi = match dcl.abi {
            abi::DEFAULT | abi::CDECL | abi::SYSV => crate::ffi::DEFAULT_ABI,
            _ => {
                return Err(Bail(format!(
                    "Unsupported calling convention line {}",
                    self.line
                )));
            }
        };
        let owned = args.iter().flatten().copied().collect::<Vec<_>>();
        let args = args.map(|list| list.into_iter().map(|(arg, _)| arg).collect());
        let id = self.types.add(Type {
            kind: Kind::Func(Func { ret, args, abi }),
            size: 8,
            align: 1,
            attr: self.default_attr | (dcl.attr & attr::FUNC_ATTRS),
        });
        self.types.own(id, ret, dcl.owned);
        for (arg, is_owned) in owned {
            self.types.own(id, arg, is_owned);
        }
        dcl.ty = Some(id);
        dcl.owned = true;
        dcl.attr &= !attr::FUNC_ATTRS;
        dcl.align = 0;
        dcl.abi = 0;
        Ok(())
    }

    /// `zend_ffi_add_arg`: a parameter, an array adjusted to a pointer to
    /// its element and a function to a pointer to it.
    fn add_arg(&mut self, args: &mut Option<Vec<(TypeId, bool)>>, arg_dcl: &mut Dcl) -> Parse<()> {
        let list = args.get_or_insert_with(Vec::new);
        let ty = self.finalize(arg_dcl)?;
        match self.types.get(ty).kind {
            Kind::Array { of, .. } => {
                if arg_dcl.owned {
                    if let Some(t) = self.types.get_mut(ty) {
                        t.kind = Kind::Pointer(of);
                        t.size = 8;
                    }
                } else {
                    let attr = self.default_attr | (self.types.get(ty).attr & attr::POINTER_ATTRS);
                    let id = self.types.pointer_to(of, attr);
                    arg_dcl.ty = Some(id);
                    arg_dcl.owned = true;
                }
            }
            Kind::Func(_) => {
                let id = self.types.pointer_to(ty, self.default_attr);
                self.types.own(id, ty, arg_dcl.owned);
                arg_dcl.ty = Some(id);
                arg_dcl.owned = true;
            }
            _ => {}
        }
        if let Err(bail) = self.validate_incomplete_type(ty, true, true) {
            *args = None;
            return Err(bail);
        }
        list.push((arg_dcl.ty.unwrap_or(ty), arg_dcl.owned));
        Ok(())
    }

    /// `zend_ffi_declare`: a typedef, a variable or a function into the
    /// scope's symbols; a redeclaration of the same is allowed.
    fn declare(&mut self, name: &[u8], dcl: &mut Dcl) -> Parse<()> {
        self.scope.symbols.get_or_insert_with(Table::default);
        let ty = self.finalize(dcl)?;
        let storage = dcl.flags & dcl::STORAGE_CLASS;
        let is_const = dcl.attr & attr::CONST != 0;
        let shown = String::from_utf8_lossy(name).into_owned();
        if let Some(sym) = self
            .scope
            .symbols
            .as_ref()
            .and_then(|symbols| symbols.get(name))
        {
            let sym = sym.clone();
            if storage == dcl::TYPEDEF
                && sym.kind == SymbolKind::Type
                && self.types.is_same(sym.ty, ty)
                && sym.is_const == is_const
            {
                self.cleanup(dcl);
                return Ok(());
            }
            if storage == 0 || storage == dcl::EXTERN {
                if matches!(self.types.get(ty).kind, Kind::Func(_)) {
                    if sym.kind == SymbolKind::Func && self.types.same_types(sym.ty, ty) {
                        self.cleanup(dcl);
                        return Ok(());
                    }
                } else if sym.kind == SymbolKind::Var
                    && self.types.is_same(sym.ty, ty)
                    && sym.is_const == is_const
                {
                    self.cleanup(dcl);
                    return Ok(());
                }
            }
            return Err(Bail(format!(
                "Redeclaration of \"{shown}\" at line {}",
                self.line
            )));
        }
        if storage == dcl::TYPEDEF {
            self.validate_vla(ty)?;
            let mut ty = ty;
            if dcl.align != 0 && dcl.align > self.types.get(ty).align {
                if dcl.owned {
                    if let Some(t) = self.types.get_mut(ty) {
                        t.align = dcl.align;
                    }
                } else {
                    let mut copy = self.types.get(ty).clone();
                    copy.attr |= self.default_attr;
                    copy.align = dcl.align;
                    ty = self.types.add(copy);
                }
            }
            self.scope.symbols.get_or_insert_with(Table::default).add(
                name,
                Symbol {
                    kind: SymbolKind::Type,
                    is_const,
                    ty,
                    addr: 0,
                    value: 0,
                },
            );
            dcl.owned = false;
        } else {
            self.validate_type(ty, storage == dcl::EXTERN, true)?;
            if storage == 0 || storage == dcl::EXTERN {
                let kind = if matches!(self.types.get(ty).kind, Kind::Func(_)) {
                    SymbolKind::Func
                } else {
                    SymbolKind::Var
                };
                self.scope.symbols.get_or_insert_with(Table::default).add(
                    name,
                    Symbol {
                        kind,
                        is_const,
                        ty,
                        addr: 0,
                        value: 0,
                    },
                );
                dcl.owned = false;
            } else {
                // A useless declaration.
                self.cleanup(dcl);
            }
        }
        Ok(())
    }

    /// `zend_ffi_type_dtor(dcl->type)` where the declaration owns its
    /// type: the type, and what it owns in turn, no longer counted on
    /// the request's heap.
    ///
    /// ```php
    /// $m = memory_get_usage(); $f = FFI::cdef("static int *x[3];"); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($f); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $f = FFI::cdef("struct { struct { int a; } in; };"); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($f); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $f = FFI::cdef("struct s { struct { int *p; long q; }; int r; };"); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($f); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 152
    /// -152
    /// 96
    /// -96
    /// 1200
    /// -1200
    /// ```
    fn cleanup(&mut self, dcl: &Dcl) {
        if dcl.owned
            && let Some(ty) = dcl.ty
        {
            self.types.free(ty);
        }
    }

    /// `zend_ffi_declare_tag`: a struct, union or enum tag, made
    /// incomplete until its body completes it in place.
    fn declare_tag(&mut self, name: &[u8], dcl: &mut Dcl, incomplete: bool) -> Parse<()> {
        let shown = String::from_utf8_lossy(name).into_owned();
        if let Some(tag) = self
            .scope
            .tags
            .as_ref()
            .and_then(|tags| tags.get(name))
            .copied()
        {
            let complete = self.types.get(tag.ty).attr & attr::INCOMPLETE_TAG == 0;
            let (want, word) = if dcl.flags & dcl::STRUCT != 0 {
                (TagKind::Struct, "struct")
            } else if dcl.flags & dcl::UNION != 0 {
                (TagKind::Union, "union")
            } else {
                (TagKind::Enum, "enum")
            };
            if tag.kind != want {
                return Err(Bail(format!(
                    "\"{shown}\" defined as wrong kind of tag at line {}",
                    self.line
                )));
            }
            if !incomplete && complete {
                return Err(Bail(format!(
                    "Redefinition of \"{word} {shown}\" at line {}",
                    self.line
                )));
            }
            dcl.ty = Some(tag.ty);
            dcl.owned = false;
            if !incomplete && let Some(t) = self.types.get_mut(tag.ty) {
                t.attr &= !attr::INCOMPLETE_TAG;
            }
            return Ok(());
        }
        let kind = if dcl.flags & dcl::STRUCT != 0 {
            self.make_struct_type(dcl);
            TagKind::Struct
        } else if dcl.flags & dcl::UNION != 0 {
            self.make_struct_type(dcl);
            TagKind::Union
        } else {
            self.make_enum_type(dcl);
            TagKind::Enum
        };
        let Some(id) = dcl.ty else {
            return Ok(());
        };
        if let Some(t) = self.types.get_mut(id) {
            match &mut t.kind {
                Kind::Struct(record) => record.tag = Some(name.to_vec()),
                Kind::Enum { tag, .. } => *tag = Some(name.to_vec()),
                _ => {}
            }
            if incomplete {
                t.attr |= attr::INCOMPLETE_TAG;
            }
        }
        dcl.owned = false;
        self.scope
            .tags
            .get_or_insert_with(Table::default)
            .add(name, Tag { kind, ty: id });
        Ok(())
    }

    /// `zend_ffi_set_abi`.
    fn set_abi(&self, dcl: &mut Dcl, abi: u16) -> Parse<()> {
        if dcl.abi != abi::DEFAULT {
            return Err(Bail(format!(
                "Multiple calling convention specifiers at line {}",
                self.line
            )));
        }
        dcl.abi = abi;
        Ok(())
    }

    /// `zend_ffi_add_attribute`: an attribute with no arguments.
    fn add_attribute(&self, dcl: &mut Dcl, name: &[u8]) -> Parse<()> {
        let bare = strip_underscores(name);
        match bare {
            b"cdecl" => self.set_abi(dcl, abi::CDECL)?,
            b"fastcall" => self.set_abi(dcl, abi::FASTCALL)?,
            b"thiscall" => self.set_abi(dcl, abi::THISCALL)?,
            b"stdcall" => self.set_abi(dcl, abi::STDCALL)?,
            b"ms_abi" => self.set_abi(dcl, abi::MS)?,
            b"sysv_abi" => self.set_abi(dcl, abi::SYSV)?,
            b"vectorcall" => self.set_abi(dcl, abi::VECTORCALL)?,
            b"aligned" => dcl.align = BIGGEST_ALIGNMENT,
            b"packed" => dcl.attr |= attr::PACKED,
            b"ms_struct" => dcl.attr |= attr::MS_STRUCT,
            b"gcc_struct" => dcl.attr |= attr::GCC_STRUCT,
            b"const"
            | b"malloc"
            | b"deprecated"
            | b"nothrow"
            | b"leaf"
            | b"pure"
            | b"noreturn"
            | b"warn_unused_result" => {}
            _ => {
                return Err(Bail(format!(
                    "Unsupported attribute \"{}\" at line {}",
                    String::from_utf8_lossy(bare),
                    self.line
                )));
            }
        }
        Ok(())
    }

    /// `zend_ffi_add_attribute_value`: the `n`th argument of an attribute.
    fn add_attribute_value(&self, dcl: &mut Dcl, name: &[u8], n: u32, val: Val) -> Parse<()> {
        let bare = strip_underscores(name);
        match bare {
            b"regparam" => {
                if n == 0 && val.is_int() && val.i64() == 3 {
                    self.set_abi(dcl, abi::REGISTER)?;
                } else {
                    return Err(Bail(format!(
                        "Incorrect \"regparam\" value at line {}",
                        self.line
                    )));
                }
            }
            b"aligned" => {
                if n == 0
                    && val.is_int()
                    && val.i64() > 0
                    && val.i64() <= 0x8000_0000
                    && val.i64() & (val.i64() - 1) == 0
                {
                    dcl.align = val.i64() as u32;
                } else {
                    return Err(Bail(format!(
                        "Incorrect \"alignment\" value at line {}",
                        self.line
                    )));
                }
            }
            b"mode" => {
                if n == 0 && val.kind == ValKind::Name {
                    let text = self.buf.get(val.at..val.at + val.len).unwrap_or_default();
                    if mode(dcl, strip_underscores(text)) {
                        return Ok(());
                    }
                }
                return Err(Bail(format!(
                    "Unsupported \"mode\" value at line {}",
                    self.line
                )));
            }
            b"nonnull" | b"alloc_size" | b"format" | b"deprecated" => {}
            _ => {
                return Err(Bail(format!(
                    "Unsupported attribute \"{}\" at line {}",
                    String::from_utf8_lossy(bare),
                    self.line
                )));
            }
        }
        Ok(())
    }

    /// `zend_ffi_add_msvc_attribute_value`: `__declspec(align(n))`.
    fn add_msvc_attribute_value(&self, dcl: &mut Dcl, name: &[u8], val: Val) -> Parse<()> {
        if name == b"align" {
            if val.is_int()
                && val.i64() > 0
                && val.i64() <= 0x8000_0000
                && val.i64() & (val.i64() - 1) == 0
            {
                dcl.align = val.i64() as u32;
            } else {
                return Err(Bail(format!(
                    "Incorrect \"alignment\" value at line {}",
                    self.line
                )));
            }
        }
        Ok(())
    }

    /// `zend_ffi_nested_type`: the outer type laid into the hole the
    /// nested declarator left, where its `char` stands.
    fn nested_type(&mut self, ty: TypeId, owned: bool, nested: TypeId) -> Parse<()> {
        match self.types.get(nested).kind.clone() {
            Kind::Pointer(to) => {
                if to == TypeId::CHAR {
                    if let Some(t) = self.types.get_mut(nested) {
                        t.kind = Kind::Pointer(ty);
                    }
                    self.types.own(nested, ty, owned);
                    self.validate_vla(ty)
                } else {
                    self.nested_type(ty, owned, to)
                }
            }
            Kind::Array { of, len } => {
                let element = if of == TypeId::CHAR {
                    if let Some(t) = self.types.get_mut(nested) {
                        t.kind = Kind::Array { of: ty, len };
                    }
                    self.types.own(nested, ty, owned);
                    self.validate_array_element_type(ty)?;
                    ty
                } else {
                    self.nested_type(ty, owned, of)?;
                    of
                };
                let (esize, ealign) = {
                    let e = self.types.get(element);
                    (e.size, e.align)
                };
                if let Some(t) = self.types.get_mut(nested) {
                    t.size = (len as usize).wrapping_mul(esize);
                    t.align = ealign;
                }
                Ok(())
            }
            Kind::Func(func) => {
                if func.ret == TypeId::CHAR {
                    if let Some(Type {
                        kind: Kind::Func(f),
                        ..
                    }) = self.types.get_mut(nested)
                    {
                        f.ret = ty;
                    }
                    self.types.own(nested, ty, owned);
                    self.validate_func_ret_type(ty)
                } else {
                    self.nested_type(ty, owned, func.ret)
                }
            }
            _ => Ok(()),
        }
    }

    /// `zend_ffi_nested_declaration`.
    fn nested_declaration(&mut self, dcl: &mut Dcl, nested_dcl: &mut Dcl) -> Parse<()> {
        let ty = self.finalize(dcl)?;
        match nested_dcl.ty {
            None | Some(TypeId::CHAR) => {
                nested_dcl.ty = Some(ty);
                nested_dcl.owned = dcl.owned;
            }
            Some(nested) => self.nested_type(ty, dcl.owned, nested)?,
        }
        dcl.ty = nested_dcl.ty;
        dcl.owned = nested_dcl.owned;
        Ok(())
    }

    /// `zend_ffi_align_as_type`.
    fn align_as_type(&mut self, dcl: &mut Dcl, align_dcl: &mut Dcl) -> Parse<()> {
        let ty = self.finalize(align_dcl)?;
        dcl.align = align_dcl.align.max(self.types.get(ty).align);
        Ok(())
    }

    // ----------------------------------------------------- validations

    /// `zend_ffi_validate_vla`.
    fn validate_vla(&self, ty: TypeId) -> Parse<()> {
        if !self.allow_vla && self.types.get(ty).attr & attr::VLA != 0 {
            return Err(Bail(format!(
                "\"[*]\" is not allowed in other than function prototype scope at line {}",
                self.line
            )));
        }
        Ok(())
    }

    /// `zend_ffi_validate_incomplete_type`: an incomplete tag named as
    /// php names it, from the scope's tags or its symbols.
    fn validate_incomplete_type(
        &self,
        ty: TypeId,
        allow_incomplete_tag: bool,
        allow_incomplete_array: bool,
    ) -> Parse<()> {
        let t = self.types.get(ty);
        if !allow_incomplete_tag && t.attr & attr::INCOMPLETE_TAG != 0 {
            if let Some(tags) = &self.scope.tags {
                for (key, tag) in tags.iter() {
                    if tag.ty == ty {
                        let what = if matches!(t.kind, Kind::Enum { .. }) {
                            "enum"
                        } else if t.attr & attr::UNION != 0 {
                            "union"
                        } else {
                            "struct"
                        };
                        return Err(Bail(format!(
                            "Incomplete {what} \"{}\" at line {}",
                            String::from_utf8_lossy(key),
                            self.line
                        )));
                    }
                }
            }
            if let Some(symbols) = &self.scope.symbols {
                for (key, sym) in symbols.iter() {
                    if sym.ty == ty {
                        return Err(Bail(format!(
                            "Incomplete C type {} at line {}",
                            String::from_utf8_lossy(key),
                            self.line
                        )));
                    }
                }
            }
            return Err(Bail(format!("Incomplete type at line {}", self.line)));
        }
        if !allow_incomplete_array && t.attr & attr::INCOMPLETE_ARRAY != 0 {
            return Err(Bail(format!("\"[]\" is not allowed at line {}", self.line)));
        }
        if !self.allow_vla && t.attr & attr::VLA != 0 {
            return Err(Bail(format!(
                "\"[*]\" is not allowed in other than function prototype scope at line {}",
                self.line
            )));
        }
        Ok(())
    }

    /// `zend_ffi_validate_type`.
    fn validate_type(
        &self,
        ty: TypeId,
        allow_incomplete_tag: bool,
        allow_incomplete_array: bool,
    ) -> Parse<()> {
        if matches!(self.types.get(ty).kind, Kind::Void) {
            return Err(Bail(format!(
                "void type is not allowed at line {}",
                self.line
            )));
        }
        self.validate_incomplete_type(ty, allow_incomplete_tag, allow_incomplete_array)
    }

    /// `zend_ffi_validate_var_type`.
    fn validate_var_type(&self, ty: TypeId, allow_incomplete_array: bool) -> Parse<()> {
        if matches!(self.types.get(ty).kind, Kind::Func(_)) {
            return Err(Bail(format!(
                "function type is not allowed at line {}",
                self.line
            )));
        }
        self.validate_type(ty, false, allow_incomplete_array)
    }

    /// `zend_ffi_expr_cast`.
    fn expr_cast(&mut self, val: &mut Val, dcl: &mut Dcl) -> Parse<()> {
        let ty = self.finalize(dcl)?;
        let kind = self.types.get(ty).kind.clone();
        let float_kind = match kind {
            Kind::Float => Some(ValKind::Float),
            Kind::Double => Some(ValKind::Double),
            Kind::LongDouble => Some(ValKind::LongDouble),
            _ => None,
        };
        if let Some(target) = float_kind {
            if val.is_unsigned() {
                val.set_d(val.u64() as f64);
            } else if val.is_signed() {
                val.set_d(val.i64() as f64);
            } else if val.is_char() {
                val.set_d(f64::from(val.ch()));
            } else if !val.is_float() {
                val.error();
                return Ok(());
            }
            val.kind = target;
            return Ok(());
        }
        match kind {
            Kind::Int(Int::U8 | Int::U16 | Int::U32) | Kind::Bool | Kind::Char => {
                if val.is_int() {
                    val.kind = ValKind::Uint32;
                } else if val.is_float() {
                    val.kind = ValKind::Uint32;
                    val.raw = val.d() as u64;
                } else if val.is_char() {
                    val.kind = ValKind::Uint32;
                    val.raw = i64::from(val.ch()).cast_unsigned();
                } else {
                    val.error();
                }
            }
            Kind::Int(Int::I8 | Int::I16 | Int::I32) => {
                if val.is_int() {
                    val.kind = ValKind::Int32;
                } else if val.is_float() {
                    val.kind = ValKind::Int32;
                    val.raw = val.d() as u64;
                } else if val.is_char() {
                    val.kind = ValKind::Int32;
                    val.set_i64(i64::from(val.ch()));
                } else {
                    val.error();
                }
            }
            Kind::Int(Int::U64) => {
                if val.is_int() {
                    val.kind = ValKind::Uint64;
                } else if val.is_float() {
                    val.kind = ValKind::Uint64;
                    val.raw = val.d() as u64;
                } else if val.is_char() {
                    val.kind = ValKind::Uint64;
                    val.raw = i64::from(val.ch()).cast_unsigned();
                } else {
                    val.error();
                }
            }
            Kind::Int(Int::I64) => {
                if val.is_unsigned() {
                    val.kind = ValKind::Char;
                    val.set_ch(val.u64() as i8);
                } else if val.is_signed() {
                    val.kind = ValKind::Char;
                    val.set_ch(val.i64() as i8);
                } else if val.is_float() {
                    val.kind = ValKind::Char;
                    val.set_ch(val.d() as i8);
                } else if !val.is_char() {
                    val.error();
                }
            }
            _ => val.error(),
        }
        Ok(())
    }
}

/// An enumerator list's running `min`, `max` and `last`.
#[derive(Debug, Clone, Copy)]
struct EnumRange {
    min: i64,
    max: i64,
    last: i64,
}

/// `__name__` as `name`, as php reads an attribute's name.
fn strip_underscores(name: &[u8]) -> &[u8] {
    if name.len() > 4 && name.starts_with(b"__") && name.ends_with(b"__") {
        name.get(2..name.len() - 2).unwrap_or(name)
    } else {
        name
    }
}

/// `mode(QI)` and its kin: the integer or floating width the attribute
/// names, laid onto the declaration's specifiers; false where it names
/// none php supports.
fn mode(dcl: &mut Dcl, text: &[u8]) -> bool {
    use dcl::{
        CHAR, DOUBLE, FLOAT, INT, LONG, LONG_LONG, SHORT, SIGNED, TYPE_SPECIFIERS, UNSIGNED,
    };
    let [width, class] = text else {
        return false;
    };
    match class {
        b'I' => {
            if dcl.flags
                & (TYPE_SPECIFIERS & !(CHAR | SHORT | INT | LONG | LONG_LONG | SIGNED | UNSIGNED))
                != 0
            {
                return false;
            }
            let flag = match width {
                b'Q' => CHAR,
                b'H' => SHORT,
                b'S' => INT,
                b'D' => LONG,
                _ => return false,
            };
            dcl.flags &= !(CHAR | SHORT | INT | LONG | LONG_LONG);
            dcl.flags |= flag;
            true
        }
        b'F' => {
            if dcl.flags & (TYPE_SPECIFIERS & !(LONG | FLOAT | DOUBLE)) != 0 {
                return false;
            }
            let flag = match width {
                b'S' => FLOAT,
                b'D' => DOUBLE,
                _ => return false,
            };
            dcl.flags &= !(LONG | FLOAT | DOUBLE);
            dcl.flags |= flag;
            true
        }
        _ => false,
    }
}

/// `zend_ffi_align_as_val`.
fn align_as_val(dcl: &mut Dcl, val: Val) {
    match val.kind {
        ValKind::Int32 | ValKind::Uint32 | ValKind::Float => dcl.align = 4,
        ValKind::Int64 | ValKind::Uint64 | ValKind::Double => dcl.align = 8,
        ValKind::LongDouble => {
            dcl.align = crate::types::Types::default()
                .get(TypeId::LONG_DOUBLE)
                .align;
        }
        ValKind::Char | ValKind::Str => dcl.align = 1,
        _ => {}
    }
}

/// `yy_escape_string` into php's 64-byte buffer.
fn escape(bytes: &[u8], size: usize) -> String {
    let mut out = String::new();
    for &b in bytes {
        if out.len() + 8 > size {
            out.push_str("...");
            break;
        }
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'"' => out.push_str("\\\""),
            0x07 => out.push_str("\\\x07"),
            0x08 => out.push_str("\\\x08"),
            27 => out.push_str("\\\x1b"),
            0x0c => out.push_str("\\\x0c"),
            b'\n' => out.push_str("\\\n"),
            b'\r' => out.push_str("\\\r"),
            b'\t' => out.push_str("\\\t"),
            0x0b => out.push_str("\\\x0b"),
            b'?' => out.push_str("\\?"),
            32..=126 => out.push(char::from(b)),
            _ => {
                out.push('\\');
                out.push(char::from(b'0' + ((b >> 6) % 8)));
                out.push(char::from(b'0' + ((b >> 3) % 8)));
                out.push(char::from(b'0' + (b % 8)));
            }
        }
    }
    out
}

// ------------------------------------------------- constant expressions

/// `zend_ffi_expr_bool`.
fn expr_bool(val: &mut Val) {
    let truth = if val.is_unsigned() {
        val.u64() != 0
    } else if val.is_signed() {
        val.i64() != 0
    } else if val.is_float() {
        val.d() != 0.0
    } else if val.is_char() {
        val.ch() != 0
    } else {
        val.error();
        return;
    };
    val.kind = ValKind::Int32;
    val.set_i64(i64::from(truth));
}

/// `zend_ffi_expr_conditional`.
fn expr_conditional(val: &mut Val, op2: Val, op3: Val) {
    expr_bool(val);
    if val.kind == ValKind::Int32 {
        *val = if val.i64() != 0 { op2 } else { op3 };
    }
}

/// `zend_ffi_expr_bool_or`.
fn expr_bool_or(val: &mut Val, mut op2: Val) {
    expr_bool(val);
    expr_bool(&mut op2);
    if val.kind == ValKind::Int32 && op2.kind == ValKind::Int32 {
        val.set_i64(i64::from(val.i64() != 0 || op2.i64() != 0));
    } else {
        val.error();
    }
}

/// `zend_ffi_expr_bool_and`.
fn expr_bool_and(val: &mut Val, mut op2: Val) {
    expr_bool(val);
    expr_bool(&mut op2);
    if val.kind == ValKind::Int32 && op2.kind == ValKind::Int32 {
        val.set_i64(i64::from(val.i64() != 0 && op2.i64() != 0));
    } else {
        val.error();
    }
}

/// `zend_ffi_expr_math`: `+`, `-`, `*` and `/` under C's conversions.
fn expr_math(val: &mut Val, op2: Val, op: Op) {
    if val.is_unsigned() {
        if op2.is_unsigned() {
            val.kind = val.kind.max(op2.kind);
            val.raw = op.unsigned(val.u64(), op2.u64());
        } else if op2.is_signed() {
            val.raw = op.unsigned(val.u64(), op2.i64().cast_unsigned());
        } else if op2.is_float() {
            val.kind = op2.kind;
            val.set_d(op.float(val.u64() as f64, op2.d()));
        } else if op2.is_char() {
            val.raw = op.unsigned(val.u64(), i64::from(op2.ch()).cast_unsigned());
        } else {
            val.error();
        }
    } else if val.is_signed() {
        if op2.is_unsigned() {
            val.raw = op.unsigned(val.u64(), op2.u64());
        } else if op2.is_signed() {
            val.kind = val.kind.max(op2.kind);
            val.set_i64(op.signed(val.i64(), op2.i64()));
        } else if op2.is_float() {
            val.kind = op2.kind;
            val.set_d(op.float(val.i64() as f64, op2.d()));
        } else if op2.is_char() {
            val.set_i64(op.signed(val.i64(), i64::from(op2.ch())));
        } else {
            val.error();
        }
    } else if val.is_float() {
        if op2.is_unsigned() {
            val.set_d(op.float(val.d(), op2.u64() as f64));
        } else if op2.is_signed() {
            val.set_d(op.float(val.d(), op2.i64() as f64));
        } else if op2.is_float() {
            val.kind = val.kind.max(op2.kind);
            val.set_d(op.float(val.d(), op2.d()));
        } else if op2.is_char() {
            val.set_d(op.float(val.d(), f64::from(op2.ch())));
        } else {
            val.error();
        }
    } else if val.is_char() {
        if op2.is_unsigned() {
            val.kind = op2.kind;
            val.raw = op.unsigned(i64::from(val.ch()).cast_unsigned(), op2.u64());
        } else if op2.is_signed() {
            val.kind = ValKind::Int64;
            val.set_i64(op.signed(i64::from(val.ch()), op2.i64()));
        } else if op2.is_float() {
            val.kind = op2.kind;
            val.set_d(op.float(f64::from(val.ch()), op2.d()));
        } else if op2.is_char() {
            let ch = op.signed(i64::from(val.ch()), i64::from(op2.ch()));
            val.set_ch(ch as i8);
        } else {
            val.error();
        }
    } else {
        val.error();
    }
}

/// `zend_ffi_expr_int_math`: `|`, `^`, `&`, `<<`, `>>` and `%`.
fn expr_int_math(val: &mut Val, op2: Val, op: Op) {
    if val.is_unsigned() {
        if op2.is_unsigned() {
            val.kind = val.kind.max(op2.kind);
            val.raw = op.unsigned(val.u64(), op2.u64());
        } else if op2.is_signed() {
            val.raw = op.unsigned(val.u64(), op2.i64().cast_unsigned());
        } else if op2.is_float() {
            val.raw = op.unsigned(val.u64(), op2.d() as u64);
        } else if op2.is_char() {
            val.raw = op.unsigned(val.u64(), i64::from(op2.ch()).cast_unsigned());
        } else {
            val.error();
        }
    } else if val.is_signed() {
        if op2.is_unsigned() {
            val.set_i64(op.mixed_signed(val.i64(), op2.u64()));
        } else if op2.is_signed() {
            val.kind = val.kind.max(op2.kind);
            val.set_i64(op.signed(val.i64(), op2.i64()));
        } else if op2.is_float() {
            val.raw = op.unsigned(val.u64(), (op2.d() as i64).cast_unsigned());
        } else if op2.is_char() {
            val.set_i64(op.signed(val.i64(), i64::from(op2.ch())));
        } else {
            val.error();
        }
    } else if val.is_float() {
        if op2.is_unsigned() {
            val.kind = op2.kind;
            val.raw = op.unsigned(val.d() as u64, op2.u64());
        } else if op2.is_signed() {
            val.kind = op2.kind;
            val.set_i64(op.signed(val.d() as i64, op2.i64()));
        } else {
            val.error();
        }
    } else if val.is_char() {
        if op2.is_unsigned() {
            val.kind = op2.kind;
            val.raw = op.unsigned(i64::from(val.ch()).cast_unsigned(), op2.u64());
        } else if op2.is_signed() {
            val.kind = op2.kind;
            val.set_i64(op.mixed_signed(i64::from(val.ch()), op2.u64()));
        } else if op2.is_char() {
            let ch = op.signed(i64::from(val.ch()), i64::from(op2.ch()));
            val.set_ch(ch as i8);
        } else {
            val.error();
        }
    } else {
        val.error();
    }
}

/// `zend_ffi_expr_cmp`, reading each operand through the field php's
/// macro reads, a mismatched one included.
fn expr_cmp(val: &mut Val, op2: Val, cmp: Cmp) {
    let result = if val.is_unsigned() {
        if op2.is_int() {
            cmp.test(&val.u64(), &op2.u64())
        } else if op2.is_float() || op2.is_char() {
            // php's macro reads a `char` operand as the double too.
            cmp.test(&(val.u64() as f64), &op2.d())
        } else {
            val.error();
            return;
        }
    } else if val.is_signed() {
        if op2.is_int() {
            cmp.test(&val.i64(), &op2.i64())
        } else if op2.is_float() {
            cmp.test(&(val.i64() as f64), &op2.d())
        } else if op2.is_char() {
            cmp.test(&val.i64(), &i64::from(op2.ch()))
        } else {
            val.error();
            return;
        }
    } else if val.is_float() {
        if op2.is_unsigned() {
            cmp.test(&val.d(), &(op2.u64() as f64))
        } else if op2.is_signed() {
            cmp.test(&val.d(), &(op2.i64() as f64))
        } else if op2.is_float() {
            cmp.test(&val.d(), &op2.d())
        } else if op2.is_char() {
            cmp.test(&val.d(), &f64::from(op2.ch()))
        } else {
            val.error();
            return;
        }
    } else if val.is_char() {
        if op2.is_int() {
            cmp.test(&i64::from(val.ch()), &op2.i64())
        } else if op2.is_float() {
            cmp.test(&f64::from(val.ch()), &op2.d())
        } else if op2.is_char() {
            cmp.test(&val.ch(), &op2.ch())
        } else {
            val.error();
            return;
        }
    } else {
        val.error();
        return;
    };
    val.kind = ValKind::Int32;
    val.set_i64(i64::from(result));
}

/// `zend_ffi_expr_plus`.
fn expr_plus(val: &mut Val) {
    if !(val.is_int() || val.is_float() || val.is_char()) {
        val.error();
    }
}

/// `zend_ffi_expr_neg`.
fn expr_neg(val: &mut Val) {
    if val.is_unsigned() {
        val.raw = val.u64().wrapping_neg();
    } else if val.is_signed() {
        val.set_i64(val.i64().wrapping_neg());
    } else if val.is_float() {
        val.set_d(-val.d());
    } else if val.is_char() {
        val.set_ch(val.ch().wrapping_neg());
    } else {
        val.error();
    }
}

/// `zend_ffi_expr_bw_not`.
fn expr_bw_not(val: &mut Val) {
    if val.is_int() {
        val.raw = !val.raw;
    } else if val.is_char() {
        val.set_ch(!val.ch());
    } else {
        val.error();
    }
}

/// `zend_ffi_expr_bool_not`.
fn expr_bool_not(val: &mut Val) {
    expr_bool(val);
    if val.kind == ValKind::Int32 {
        val.set_i64(i64::from(val.i64() == 0));
    }
}

/// `zend_ffi_expr_sizeof_val`.
fn expr_sizeof_val(val: &mut Val, buf: &[u8]) {
    let size = match val.kind {
        ValKind::Uint32 | ValKind::Int32 | ValKind::Float => 4,
        ValKind::Uint64 | ValKind::Int64 | ValKind::Double => 8,
        ValKind::LongDouble => Types::default().get(TypeId::LONG_DOUBLE).size as u64,
        ValKind::Char => 1,
        ValKind::Str => {
            if buf
                .get(val.at..val.at + val.len)
                .is_some_and(|s| s.contains(&b'\\'))
            {
                val.error();
                return;
            }
            val.len as u64 + 1
        }
        _ => {
            val.error();
            return;
        }
    };
    val.kind = ValKind::Uint32;
    val.raw = size;
}

/// `zend_ffi_expr_alignof_val`.
fn expr_alignof_val(val: &mut Val) {
    let align = match val.kind {
        ValKind::Uint32 | ValKind::Int32 | ValKind::Float => 4,
        ValKind::Uint64 | ValKind::Int64 | ValKind::Double | ValKind::Str => 8,
        ValKind::LongDouble => u64::from(Types::default().get(TypeId::LONG_DOUBLE).align),
        ValKind::Char => 1,
        _ => {
            val.error();
            return;
        }
    };
    val.kind = ValKind::Uint32;
    val.raw = align;
}

/// `strtoull(text, NULL, base)`: the digits it opens with, `ULLONG_MAX`
/// past the range.
fn strtoull(text: &[u8], base: u32) -> u64 {
    let mut value: u64 = 0;
    let mut overflow = false;
    for &b in text {
        let Some(digit) = char::from(b).to_digit(base) else {
            break;
        };
        match value
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(u64::from(digit)))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
    }
    if overflow { u64::MAX } else { value }
}

/// `zend_ffi_val_number`: a decimal, octal or hexadecimal constant and
/// the kind its suffix gives it.
fn val_number(val: &mut Val, base: u32, text: &[u8], n: usize) {
    let at = |back: usize| {
        n.checked_sub(back)
            .and_then(|i| text.get(i))
            .copied()
            .unwrap_or(0)
    };
    let (mut u, mut l) = (false, 0);
    if matches!(at(1), b'u' | b'U') {
        u = true;
        if matches!(at(2), b'l' | b'L') {
            l = 1;
            if matches!(at(3), b'l' | b'L') {
                l = 2;
            }
        }
    } else if matches!(at(1), b'l' | b'L') {
        l = 1;
        if matches!(at(2), b'l' | b'L') {
            l = 2;
            if matches!(at(3), b'u' | b'U') {
                u = true;
            }
        } else if matches!(at(2), b'u' | b'U') {
            u = true;
        }
    }
    let value = strtoull(text, base);
    if u {
        val.raw = value;
        val.kind = if l == 0 {
            ValKind::Uint32
        } else {
            ValKind::Uint64
        };
    } else {
        val.set_i64(i64::try_from(value).unwrap_or(i64::MAX));
        val.kind = if l == 0 {
            ValKind::Int32
        } else {
            ValKind::Int64
        };
    }
}

/// `zend_ffi_val_float_number`: `strtold` and the kind the suffix gives.
fn val_float_number(val: &mut Val, text: &[u8]) {
    let last = text.last().copied().unwrap_or(0);
    let digits = if matches!(last, b'f' | b'F' | b'l' | b'L') {
        text.get(..text.len() - 1).unwrap_or_default()
    } else {
        text
    };
    let d = std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    val.set_d(d);
    val.kind = match last {
        b'f' | b'F' => ValKind::Float,
        b'l' | b'L' => ValKind::LongDouble,
        _ => ValKind::Double,
    };
}

/// `zend_ffi_val_string`: a plain string's contents; a prefixed one is
/// an error value.
fn val_string(val: &mut Val, buf: &[u8], at: usize, len: usize) {
    if buf.get(at) != Some(&b'"') {
        val.error();
        return;
    }
    val.kind = ValKind::Str;
    val.at = at + 1;
    val.len = len.saturating_sub(2);
}

/// `zend_ffi_val_character`: a character constant's value, its escapes
/// read as php reads them.
fn val_character(val: &mut Val, text: &[u8]) {
    let len = text.len();
    let b = |i: usize| text.get(i).copied().unwrap_or(0);
    if b(0) != b'\'' {
        val.error();
        return;
    }
    val.kind = ValKind::Char;
    if len == 3 {
        val.set_ch(b(1) as i8);
        return;
    }
    if b(1) != b'\\' {
        val.error();
        return;
    }
    let escaped = b(2);
    match escaped {
        b'a' => {}
        b'b' if len == 4 => val.set_ch(0x08),
        b'f' if len == 4 => val.set_ch(0x0c),
        b'n' if len == 4 => val.set_ch(b'\n' as i8),
        b'r' if len == 4 => val.set_ch(b'\r' as i8),
        b't' if len == 4 => val.set_ch(b'\t' as i8),
        b'v' if len == 4 => val.set_ch(0x0b),
        b'0'..=b'7' => {
            let mut n = u32::from(escaped - b'0');
            if (b'0'..=b'7').contains(&b(3)) {
                n = n * 8 + u32::from(b(3) - b'0');
                if (b'0'..=b'7').contains(&b(4)) && len == 6 {
                    n = n * 8 + u32::from(b(4) - b'0');
                } else if len != 5 {
                    val.error();
                }
            } else if len != 4 {
                val.error();
            }
            if n <= 0xff {
                val.set_ch(n as u8 as i8);
            } else {
                val.error();
            }
        }
        b'x' => {
            let digit = |c: u8| match c {
                b'0'..=b'9' => Some(u32::from(c - b'0')),
                b'A'..=b'F' => Some(u32::from(c - b'A')),
                b'a'..=b'f' => Some(u32::from(c - b'a')),
                _ => None,
            };
            let Some(mut n) = digit(b(3)) else {
                val.error();
                return;
            };
            if let Some(next) = digit(b(4)).filter(|_| len == 6) {
                n = n * 16 + next;
            } else if len != 5 {
                val.error();
                return;
            }
            val.set_ch(n as u8 as i8);
        }
        _ if len == 4 => val.set_ch(escaped as i8),
        _ => val.error(),
    }
}
