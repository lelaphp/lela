//! What the concern files share: a source compiled under the bundle
//! php's `-n` compiles under, its unit, its strings and its dump, and
//! the refusal of one that does not compile.

use super::*;

/// `source` parsed as a file under the lexer's defaults and compiled
/// through [`compile`], the parse and the compile in one arena.
pub(super) fn compile_parsed(
    source: &[u8],
    options: &Options<'_>,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    let arena = Arena::new();
    let parsed = php_syntax::parse(&arena, source, &php_syntax::Options::default());
    compile(&arena, &parsed, options, strings)
}

/// A dump read as text by the tests, which hold it to php's
/// words; a byte that is no character reads as U+FFFD here alone.
pub(super) trait Text {
    fn text(&self) -> String;
}

impl Text for Vec<u8> {
    fn text(&self) -> String {
        String::from_utf8_lossy(self).into_owned()
    }
}

/// A unit whose constant strings are one engine's, which is what the
/// unit cache holds (design §6.3).
pub(super) fn unit_of(source: &[u8]) -> Unit {
    unit_and_strings(source).0
}

/// php's dump of a one-line file's main op array, the file unnamed
/// as `Options::default` leaves it: the header with these counts,
/// then the oplines.
pub(super) fn main_dump(lines: usize, vars: u32, tmps: u32, oplines: &str) -> String {
    format!(
        "\n$_main:\n     ; (lines={lines}, args=0, vars={vars}, tmps={tmps})\n\
         \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n{oplines}"
    )
}

/// The dump of one source, read while the table its strings were
/// interned into still stands, under the frameless table of the
/// bundle the source compiled against, as the engine hands it to
/// `compile-identity`.
pub(super) fn dump_of(source: &[u8]) -> String {
    let (unit, _strings) = unit_and_strings(source);
    let options = bundled();
    let functions = options.functions.rows();
    let names: Vec<Name> = (options.frameless.iter())
        .filter_map(|entry| functions.get(entry.function as usize))
        .map(|decl| decl.name.clone())
        .collect();
    unit.dump(&names).text()
}

pub(super) fn unit_and_strings(source: &[u8]) -> (Unit, php_heap::Strings) {
    let mut strings = php_heap::Strings::new();
    let unit = compile_parsed(source, &bundled(), &mut strings).expect("the source compiles");
    (unit, strings)
}

/// The natives the tests call that are in no row of
/// `special::TABLE`: php's `-n` has each, so its dump reads them off
/// `CG(function_table)` as `INIT_FCALL` and `DO_ICALL`.
pub(super) const CALLED: [&[u8]; 6] = [
    b"var_dump",
    b"extension_loaded",
    b"str_repeat",
    b"set_time_limit",
    b"memory_get_usage",
    b"exit",
];

/// A native's parameter as its stub declares it, by value, with its
/// default where it has one.
pub(super) fn parameter(name: &str, ty: Type, default: Option<ConstExpr>) -> Parameter {
    Parameter {
        default: default.map_or(
            php_unit::decl::ParameterDefault::None,
            php_unit::decl::ParameterDefault::Expr,
        ),
        ..php_unit::decl::parameter(name.as_bytes(), Some(ty))
    }
}

/// The natives with a frameless entry the tests call, as
/// `ext/standard` declares them, in `basic_functions.stub.php`'s
/// order, which is `zend_flf_functions`' among them: the name, the
/// parameters as php's stub has them with a default where the stub
/// has one, and the arities of `frameless_function_infos_<name>`;
/// and `join` and `strchr`, aliases php registers with none.
pub(super) fn frameless_natives() -> Vec<FunctionDecl> {
    let string = |name| parameter(name, Type::String, None);
    let either = |name| parameter(name, Type::Union(vec![Type::Array, Type::String]), None);
    let nullable = |name, ty| parameter(name, Type::Nullable(Box::new(ty)), Some(ConstExpr::Null));
    let long = |name, value| {
        let radix = php_unit::decl::Radix::Decimal;
        parameter(name, Type::Int, Some(ConstExpr::Long { value, radix }))
    };
    let haystack =
        |more: Vec<Parameter>| [vec![string("haystack"), string("needle")], more].concat();
    let implode = || vec![either("separator"), nullable("array", Type::Array)];
    let before = || {
        haystack(vec![parameter(
            "before_needle",
            Type::Bool,
            Some(ConstExpr::Bool(false)),
        )])
    };
    let characters = Some(ConstExpr::Str(b" ".as_slice().into()));
    let count = Parameter {
        by_ref: true,
        ..parameter("count", Type::Mixed, Some(ConstExpr::Null))
    };
    [
        (
            "trim",
            vec![
                string("string"),
                parameter("characters", Type::String, characters),
            ],
            vec![1, 2],
        ),
        ("implode", implode(), vec![1, 2]),
        ("join", implode(), vec![]),
        (
            "dirname",
            vec![string("path"), long("levels", 1)],
            vec![1, 2],
        ),
        ("strstr", before(), vec![2, 3]),
        ("strchr", before(), vec![]),
        ("strpos", haystack(vec![long("offset", 0)]), vec![2, 3]),
        ("str_contains", haystack(vec![]), vec![2]),
        ("str_starts_with", haystack(vec![]), vec![2]),
        (
            "substr",
            vec![
                string("string"),
                parameter("offset", Type::Int, None),
                nullable("length", Type::Int),
            ],
            vec![2, 3],
        ),
        (
            "strtr",
            vec![
                string("string"),
                either("from"),
                nullable("to", Type::String),
            ],
            vec![2, 3],
        ),
        (
            "str_replace",
            vec![
                either("search"),
                either("replace"),
                either("subject"),
                count,
            ],
            vec![3],
        ),
    ]
    .into_iter()
    .map(|(name, parameters, frameless)| FunctionDecl {
        parameters,
        frameless,
        ..php_unit::decl::native_function(name.as_bytes())
    })
    .collect()
}

/// The options of an engine whose bundle declares every name of
/// `special::TABLE` and every name of [`CALLED`] as a native, which
/// php's `-n` has, and then [`frameless_natives`]: what
/// `zend_compile_call` reads as `CG(function_table)`, so the tests
/// compile under the table php compiles under; and `INF` and
/// `NAN` among its persistent constants, as `ext/standard` declares
/// them (`REGISTER_DOUBLE_CONSTANT("INF", ZEND_INFINITY,
/// CONST_PERSISTENT)`).
pub(super) fn bundled() -> Options<'static> {
    static NATIVES: std::sync::LazyLock<Vec<FunctionDecl>> = std::sync::LazyLock::new(|| {
        special::TABLE
            .iter()
            .map(|(name, _)| *name)
            .chain(CALLED)
            .map(php_unit::decl::native_function)
            .chain(frameless_natives())
            .collect()
    });
    static NAMES: std::sync::LazyLock<Names<Folded>> =
        std::sync::LazyLock::new(|| Names::over(&NATIVES));
    static FRAMELESS: std::sync::LazyLock<Vec<php_unit::FramelessEntry>> =
        std::sync::LazyLock::new(|| frameless_table(NATIVES.iter()));
    static PERSISTENT: std::sync::LazyLock<Vec<ConstDecl>> = std::sync::LazyLock::new(|| {
        [(&b"INF"[..], f64::INFINITY), (b"NAN", f64::NAN)]
            .into_iter()
            .map(|(name, value)| ConstDecl {
                name: Name(name.into()),
                value: php_unit::decl::ConstExpr::Double(value),
                attributes: Vec::new(),
            })
            .collect()
    });
    static CONSTANT_NAMES: std::sync::LazyLock<Names<Exact>> =
        std::sync::LazyLock::new(|| Names::over(&PERSISTENT));
    Options {
        functions: Indexed::new(&NATIVES, &NAMES),
        frameless: &FRAMELESS,
        persistent: Indexed::new(&PERSISTENT, &CONSTANT_NAMES),
        ..Options::default()
    }
}

/// `rows` as the engine hands a table to the compiler, held for the
/// rest of the test process.
pub(super) fn indexed<R: php_link::Named, C: php_link::Case>(
    rows: Vec<R>,
) -> Indexed<'static, R, C> {
    let rows: &'static [R] = Vec::leak(rows);
    Indexed::new(rows, Box::leak(Box::new(Names::over(rows))))
}

/// The one constant a single-expression `echo` leaves in the table.
pub(super) fn echoed_literal(source: &[u8]) -> php_heap::Zval {
    let (unit, _strings) = unit_and_strings(source);
    unit.main
        .literals
        .first()
        .copied()
        .expect("the echoed literal")
}

/// The text of the one constant a single-expression `echo` leaves,
/// read while the table it was interned into still stands.
pub(super) fn echoed_string(source: &[u8]) -> Vec<u8> {
    let (unit, _strings) = unit_and_strings(source);
    literal_text(&unit, 0).expect("a string literal")
}

/// The text of the constant at `at`, for a string; nothing otherwise.
pub(super) fn literal_text(unit: &Unit, at: usize) -> Option<Vec<u8>> {
    unit.main
        .literals
        .get(at)
        .and_then(|z| z.as_str())
        .map(|s| s.bytes().to_vec())
}

/// The constant a single-expression `echo` leaves, where the whole
/// expression folded: the first slot, of two with the return's.
pub(super) fn folded(source: &[u8]) -> Option<php_heap::Zval> {
    let (unit, _strings) = unit_and_strings(source);
    (unit.main.oplines.len() == 2 && unit.main.oplines[0].opcode == Opcode::Echo)
        .then(|| unit.main.literals.first().copied())
        .flatten()
}

/// Whether the source compiles to the ordinary call of `name`: an
/// `INIT_FCALL` first, over that lowered name, and a `DO_ICALL`.
pub(super) fn ordinary_call(source: &[u8], name: &[u8]) -> bool {
    let (unit, _strings) = unit_and_strings(source);
    let oplines = &unit.main.oplines;
    oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcall)
        && literal_text(&unit, 0).as_deref() == Some(name)
        && oplines.iter().any(|op| op.opcode == Opcode::DoIcall)
}

/// The oplines and the live-range table of one source's main op
/// array, the header left off: what the oracle's dump prints after
/// `; return  [] RANGE[0..0]`.
pub(super) fn oplines_of(source: &[u8]) -> String {
    let dump = dump_of(source);
    dump.split_once("RANGE[0..0]\n")
        .map(|(_, rest)| rest.to_owned())
        .expect("the main op array's header")
}

/// The message one source refuses with, or nothing.
pub(super) fn refused_message(source: &[u8]) -> Option<String> {
    refused(source).map(|e| e.message.into_owned())
}

/// The compile error one source refuses with, or nothing.
pub(super) fn refused(source: &[u8]) -> Option<CompileError> {
    let mut strings = php_heap::Strings::new();
    compile_parsed(source, &bundled(), &mut strings).err()
}
