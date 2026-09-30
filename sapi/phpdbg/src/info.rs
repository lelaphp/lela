//! `info` (`Z/sapi/phpdbg/phpdbg_info.c`) and `back`
//! (`phpdbg_frame.c`): what the request holds, as phpdbg lists it, and
//! the values in the short and flat forms phpdbg prints them in.

use php_sapi::tier::{self, FrameRef, Key, Request, Shown};

use crate::cmd::Param;
use crate::out;
use crate::prompt::{Here, Ret};
use crate::session::Session;

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `PHPDBG_INFO(break)`.
pub fn breaks(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    s.breaks.print();
    Ret::Success
}

/// `PHPDBG_INFO(files)`.
pub fn files(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let files = here
        .standing()
        .map_or_else(Vec::new, |(req, _)| tier::included_files(req));
    out::notice(format!("Included files: {}", files.len()).as_bytes());
    for file in files {
        out::writeln(format!("File: {}", lossy(&file)).as_bytes());
    }
    Ret::Success
}

/// `phpdbg_print_class_name`'s type word.
fn class_word(shape: tier::Shape) -> &'static str {
    match shape {
        tier::Shape::Interface => "Interface",
        tier::Shape::AbstractClass => "Abstract Class",
        tier::Shape::Class => "Class",
    }
}

/// `PHPDBG_INFO(classes)`.
pub fn classes(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let classes = here
        .standing()
        .map_or_else(Vec::new, |(req, _)| tier::user_classes(req));
    out::notice(format!("User Classes ({})", classes.len()).as_bytes());
    for class in classes {
        out::writeln(
            format!(
                "User {} {} ({})",
                class_word(class.shape),
                lossy(&class.name),
                class.methods.len()
            )
            .as_bytes(),
        );
        if let Some(parent) = &class.unlinked_parent {
            out::writeln(
                format!(
                    "|-------- User Class {} (not yet linked because declaration for parent was not encountered when declaring the class)",
                    lossy(parent)
                )
                .as_bytes(),
            );
        }
        out::writeln(
            format!("|---- in {} on line {}", lossy(&class.file), class.lines.0).as_bytes(),
        );
    }
    Ret::Success
}

/// `PHPDBG_INFO(funcs)`.
pub fn funcs(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let functions = here
        .standing()
        .map_or_else(Vec::new, |(req, _)| tier::user_functions(req));
    out::notice(format!("User Functions ({})", functions.len()).as_bytes());
    for function in functions {
        out::write(format!("|-------- {}", lossy(&function.name)).as_bytes());
        out::writeln(
            format!(" in {} on line {}", lossy(&function.file), function.lines.0).as_bytes(),
        );
    }
    Ret::Success
}

/// `PHPDBG_INFO(error)`.
pub fn error(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    match here.standing().and_then(|(req, _)| tier::last_error(req)) {
        Some((message, file, line)) => out::writeln(
            format!(
                "Last error: {} at {} line {line}",
                lossy(&message),
                lossy(&file)
            )
            .as_bytes(),
        ),
        None => out::notice(b"No error found!"),
    }
    Ret::Success
}

/// `zend_get_type_by_const`.
fn type_name(value: &Shown) -> &'static str {
    match value {
        Shown::Undef => "undefined",
        Shown::Null => "null",
        Shown::Bool(_) => "bool",
        Shown::Int(_) => "int",
        Shown::Float(_) => "float",
        Shown::Str { .. } => "string",
        Shown::Array { .. } => "array",
        Shown::Object { .. } => "object",
        Shown::Resource { .. } => "resource",
        Shown::Reference { .. } => "reference",
    }
}

/// `VARIABLEINFO` over one row: the address column php prints a pointer
/// in, the owners, the type, the name, and the value where the type has
/// a short form.
fn variable_row(name: &[u8], refs: u32, value: &Shown, sigil: &str, address: usize) {
    let (isref, value) = match value {
        Shown::Reference { value, .. } => ("&", value.as_ref()),
        other => ("", other),
    };
    let head = format!(
        "{:<18} {:<7} {:<9} {isref}{sigil}{}",
        format!("0x{address:012x}"),
        refs,
        type_name(value),
        lossy(name)
    );
    let tail = match value {
        Shown::Str { bytes, .. } => {
            let shown = bytes.get(..255).unwrap_or(bytes);
            format!(
                "\nstring ({}) \"{}{}\"",
                bytes.len(),
                lossy(shown),
                if bytes.len() > 255 { "..." } else { "" }
            )
        }
        Shown::Bool(true) => String::from("\nbool (true)"),
        Shown::Bool(false) => String::from("\nbool (false)"),
        Shown::Int(n) => format!("\nint ({n})"),
        Shown::Float(f) => format!("\ndouble ({f:.6})"),
        Shown::Object { class, .. } => format!("\n|-----(instanceof)----> ({})\n", lossy(class)),
        Shown::Resource { .. } => String::from("\n|-------(typeof)------> (unknown)\n"),
        _ => String::new(),
    };
    out::writeln(format!("{head}{tail}").as_bytes());
}

/// `phpdbg_print_symbols(0)`.
pub fn vars(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    let Some((req, frame)) = here.standing().filter(|_| executing) else {
        out::error(b"No active op array!");
        return Ret::Success;
    };
    let rows = tier::variables(req, frame);
    let title = match tier::serves(req, frame) {
        tier::Serves::Function {
            name,
            class: Some(class),
        } => format!(
            "Variables in {}::{}() ({})",
            lossy(&class),
            lossy(&name),
            rows.len()
        ),
        tier::Serves::Function { name, class: None } => {
            format!("Variables in {}() ({})", lossy(&name), rows.len())
        }
        tier::Serves::Code => {
            let file = tier::position(req, frame)
                .map(|p| p.file)
                .unwrap_or_default();
            format!("Variables in {} ({})", lossy(&file), rows.len())
        }
    };
    out::notice(title.as_bytes());
    if !rows.is_empty() {
        out::out(b"Address            Refs    Type      Variable\n");
        for (at, row) in rows.iter().enumerate() {
            variable_row(&row.name, row.refs, &row.value, "$", 0x1000 + at * 16);
        }
    }
    Ret::Success
}

/// `phpdbg_print_symbols(1)`.
pub fn globals(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    let Some((req, _)) = here.standing().filter(|_| executing) else {
        out::error(b"No active op array!");
        return Ret::Success;
    };
    let rows = tier::superglobals(req);
    out::notice(format!("Superglobal variables ({})", rows.len()).as_bytes());
    if !rows.is_empty() {
        out::out(b"Address            Refs    Type      Variable\n");
        for (at, row) in rows.iter().enumerate() {
            variable_row(&row.name, row.refs, &row.value, "$", 0x2000 + at * 16);
        }
    }
    Ret::Success
}

/// `PHPDBG_INFO(constants)`.
pub fn constants(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let rows = here
        .standing()
        .map_or_else(Vec::new, |(req, _)| tier::user_constants(req));
    out::notice(format!("User-defined constants ({})", rows.len()).as_bytes());
    if !rows.is_empty() {
        out::out(b"Address            Refs    Type      Constant\n");
        for (at, row) in rows.iter().enumerate() {
            variable_row(&row.name, row.refs, &row.value, "", 0x3000 + at * 16);
        }
    }
    Ret::Success
}

/// `PHPDBG_INFO(literal)`.
pub fn literal(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    let Some((req, frame)) = here.standing().filter(|_| executing) else {
        out::error(b"Not executing!");
        return Ret::Success;
    };
    let literals = tier::literals(req, frame);
    let count = literals.len().saturating_sub(1);
    let title = match tier::serves(req, frame) {
        tier::Serves::Function {
            name,
            class: Some(class),
        } => format!(
            "Literal Constants in {}::{}() ({count})",
            lossy(&class),
            lossy(&name)
        ),
        tier::Serves::Function { name, class: None } => {
            format!("Literal Constants in {}() ({count})", lossy(&name))
        }
        tier::Serves::Code => {
            let file = tier::position(req, frame)
                .map(|p| p.file)
                .unwrap_or_default();
            format!("Literal Constants in {} ({count})", lossy(&file))
        }
    };
    out::notice(title.as_bytes());
    for (at, literal) in literals.iter().enumerate() {
        if matches!(literal, Shown::Null) {
            continue;
        }
        out::write(format!("|-------- C{at} -------> [").as_bytes());
        out::write(&flat(literal));
        out::out(b"]\n");
    }
    Ret::Success
}

/// `%.*G` with 14 places and php's exponent, as `phpdbg_short_zval_print`
/// and `zend_print_zval` write a float under the default precision.
fn float_text(f: f64) -> String {
    if f.is_nan() {
        return String::from("NAN");
    }
    if f.is_infinite() {
        return String::from(if f > 0.0 { "INF" } else { "-INF" });
    }
    let text = format!("{f:.13e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-5..14).contains(&exponent) {
        let places = usize::try_from(13 - exponent).unwrap_or(0);
        let fixed = format!("{f:.places$}");
        return if fixed.contains('.') {
            fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            fixed
        };
    }
    let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
    format!(
        "{mantissa}E{}{}",
        if exponent < 0 { '-' } else { '+' },
        exponent.abs()
    )
}

/// `zend_print_flat_zval_r`: a scalar as its string, an array as
/// `Array (` and its rows `[key] => value` joined by `,`, then `)`.
pub fn flat(value: &Shown) -> Vec<u8> {
    let mut out = Vec::new();
    flat_into(&mut out, value);
    out
}

fn flat_into(out: &mut Vec<u8>, value: &Shown) {
    match value {
        Shown::Undef | Shown::Null | Shown::Bool(false) => {}
        Shown::Bool(true) => out.push(b'1'),
        Shown::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Shown::Float(f) => out.extend_from_slice(float_text(*f).as_bytes()),
        Shown::Str { bytes, .. } => out.extend_from_slice(bytes),
        Shown::Array { entries, .. } => {
            out.extend_from_slice(b"Array (");
            for (at, (key, value)) in entries.iter().enumerate() {
                if at > 0 {
                    out.push(b',');
                }
                out.push(b'[');
                match key {
                    Key::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
                    Key::Str(s) => out.extend_from_slice(s),
                }
                out.extend_from_slice(b"] => ");
                flat_into(out, value);
            }
            out.push(b')');
        }
        Shown::Object { class, .. } => {
            out.extend_from_slice(class);
            out.extend_from_slice(b" Object ()");
        }
        Shown::Resource { handle } => {
            out.extend_from_slice(format!("Resource id #{handle}").as_bytes());
        }
        Shown::Reference { value, .. } => flat_into(out, value),
    }
}

/// `phpdbg_short_zval_print(zv, 40)`.
fn short(value: &Shown) -> String {
    const MAXLEN: usize = 40;
    match value {
        Shown::Undef => String::new(),
        Shown::Null => String::from("null"),
        Shown::Bool(false) => String::from("false"),
        Shown::Bool(true) => String::from("true"),
        Shown::Int(n) => n.to_string(),
        Shown::Float(f) => {
            let text = float_text(*f);
            if f.is_finite() && !text.contains('.') {
                format!("{text}.0")
            } else {
                text
            }
        }
        Shown::Str { bytes, .. } => {
            let mut escaped = Vec::new();
            for b in bytes {
                match b {
                    b'\\' | b'"' => escaped.extend_from_slice(&[b'\\', *b]),
                    b'\n' => escaped.extend_from_slice(b"\\n"),
                    b'\t' => escaped.extend_from_slice(b"\\t"),
                    0 => escaped.extend_from_slice(b"\\000"),
                    _ => escaped.push(*b),
                }
            }
            for b in &mut escaped {
                if *b < 32 {
                    *b = b' ';
                }
            }
            if escaped.len() <= MAXLEN - 2 {
                format!("\"{}\"", lossy(&escaped))
            } else {
                format!("\"{}\"+", lossy(&escaped[..MAXLEN - 3]))
            }
        }
        Shown::Resource { handle } => format!("Rsrc #{handle}"),
        Shown::Array { len, .. } => format!("array({len})"),
        Shown::Object { class, .. } => {
            if class.len() <= MAXLEN {
                lossy(class)
            } else {
                format!("{}+", lossy(&class[..MAXLEN - 1]))
            }
        }
        Shown::Reference { value, .. } => short(value),
    }
}

/// `phpdbg_dump_prototype`.
fn prototype(row: &tier::TraceRow) -> String {
    let mut text = match &row.class {
        Some((class, join)) => format!("{}{}{}(", lossy(class), lossy(join), lossy(&row.function)),
        None => format!("{}(", lossy(&row.function)),
    };
    let args: Vec<String> = row.args.iter().map(short).collect();
    text.push_str(&args.join(", "));
    text.push(')');
    text
}

/// `phpdbg_dump_backtrace`: a row per frame, each at the site the row
/// before named, the first at the current one, and `{main}` last.
pub fn backtrace(req: &mut Request, frame: FrameRef, limit: usize) {
    let rows = tier::backtrace(req, i64::try_from(limit).unwrap_or(0));
    let position = tier::position(req, frame);
    let mut file = position.as_ref().map(|p| p.file.clone());
    let mut line = position.map_or(0, |p| i64::from(p.line));
    let mut last_function = Vec::new();
    let mut at = 0;
    for row in &rows {
        match &file {
            Some(file) => {
                out::out(
                    format!(
                        "frame #{at}: {} at {}:{line}\n",
                        prototype(row),
                        lossy(file)
                    )
                    .as_bytes(),
                );
                at += 1;
            }
            None => out::out(format!(" => {} (internal function)\n", prototype(row)).as_bytes()),
        }
        file.clone_from(&row.file);
        if let Some(row_line) = row.line {
            line = row_line;
        }
        last_function.clone_from(&row.function);
    }
    let last = match file {
        None => format!(" => {} (internal function)", lossy(&last_function)),
        Some(file) => format!("frame #{at}: {{main}} at {}:{line}", lossy(&file)),
    };
    out::writeln(last.as_bytes());
}
