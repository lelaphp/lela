//! The `tidy` class and the `tidy_*` functions: `tidy.c` from
//! `php_tidy_quick_repair` to `PHP_METHOD(tidy, parseString)`. Each
//! method php aliases to a function (`@alias` in `tidy.stub.php`) and the
//! function share one body here, handed the object and the argument
//! position the object takes up, which a method reports one lower.

use php_ext::abi::{Instance, ObjectHandlers, OrFalse, Path, Refusals, Std, This, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{numeric_string, to_long, to_string};
use php_ext::decl::FunctionDecl;
use php_ext::raising::{
    Level, Thrown, arg_null_deprecated, arg_type_error, argument_error, docref, fatal, throw,
    throw_error, throwing_warnings,
};
use php_ext::request::{Opening, OwnedStream as _, Request, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};

use crate::ffi::{self, Base, OptType, Status};
use crate::node::{self, tidyNode};
use crate::{
    DocId, adopt, default_config, forget, initialized, load_config, set_initialized, with_doc,
};

/// `array|string|null $config`.
type Config = Option<Union<PhpArray, PhpString>>;
/// What a `tidy` argument or `$this` is.
type Tidy = Instance<class::tidy>;

/// `PHPTidyObj` past its `zend_object`: the document the object names,
/// and for a `tidyNode` its node.
#[derive(Default)]
pub(crate) struct Obj {
    pub(crate) doc: Option<DocId>,
    pub(crate) node: Option<ffi::Node>,
}

/// `tidy_object_handlers_doc` and `tidy_object_handlers_node`:
/// `std_object_handlers` with no clone, the cast of each class, the
/// document made by `create_object` for a `tidy` and given up by
/// `free_obj` for both.
pub(crate) struct Handlers {
    node: bool,
}

pub(crate) static DOC: Handlers = Handlers { node: false };
pub(crate) static NODE: Handlers = Handlers { node: true };

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `tidy_object_new` for a `tidy`: a document, its error buffer, the
    /// two options php sets and the default configuration.
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        if self.node {
            return Ok(());
        }
        let Some(mut doc) = ffi::Doc::new(req)? else {
            return Err(fatal(req, "Could not set Tidy error buffer"));
        };
        if let Err(thrown) = default_config(req, &mut doc) {
            doc.release(req);
            return Err(thrown);
        }
        let id = adopt(req, doc)?;
        if let Some(obj) = object.native_mut::<Obj>(req) {
            obj.doc = Some(id);
        }
        Ok(())
    }

    /// `tidy_object_free_storage`: `zend_object_std_dtor`, then the
    /// document one owner fewer.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        if let Some(id) = object.native_mut::<Obj>(req).and_then(|obj| obj.doc.take()) {
            forget(req, id);
        }
    }

    /// `tidy_doc_cast_handler` and `tidy_node_cast_handler`: zero for a
    /// number, `true` for a bool, and for a string the document as libtidy saves it
    /// or the node's text, its last byte dropped.
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        Ok(Some(match to {
            TypeTag::Int | TypeTag::Number => Owner::int(0),
            TypeTag::Float => Owner::float(0.0),
            TypeTag::Bool => Owner::bool(true),
            TypeTag::String => {
                let bytes = if self.node {
                    node::text(req, object)?.unwrap_or_default()
                } else {
                    let id = doc_id(req, object)?;
                    with_doc(req, id, |req, doc| doc.save(req))?.unwrap_or_default()
                };
                PhpString::new(req, fixed(&bytes))?.into_value()
            }
            _ => return Ok(None),
        }))
    }
}

/// `FIX_BUFFER` and the length php hands on, `size - 1`: the buffer
/// without its last byte.
pub(crate) fn fixed(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.len().saturating_sub(1)]
}

/// The document an object names.
pub(crate) fn doc_id(req: &mut Request, object: PhpObject) -> Result<DocId, Thrown> {
    let id = object.native_mut::<Obj>(req).and_then(|obj| obj.doc);
    id.ok_or_else(|| fatal(req, "InternalError: a tidy object without its document"))
}

/// `body` over the object's document.
fn on_doc<R: Default>(
    req: &mut Request,
    object: PhpObject,
    body: impl FnOnce(&mut Request, &mut ffi::Doc) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let id = doc_id(req, object)?;
    Ok(with_doc(req, id, body)?.unwrap_or_default())
}

/// A C string's text in a message: the bytes to the first NUL.
pub(crate) fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(php_ext::coerce::c_str(bytes)).into_owned()
}

fn new_string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, bytes)
}

/// `php_tidy_file_to_mem`: the file's bytes through the engine's
/// wrappers, warning about nothing; `None` where it did not open.
fn file_to_mem(
    req: &mut Request,
    path: &[u8],
    use_path: bool,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let opening = Opening {
        use_path,
        ..Opening::default()
    };
    let Some(stream) = req.open_stream_with(path, b"rb", opening)? else {
        return Ok(None);
    };
    let data = match stream.copy_to_mem(req, None) {
        Ok(data) => data,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    stream.free(req);
    Ok(Some(data))
}

/// `php_tidy_set_tidy_opt`: one option of a configuration array, refused
/// where libtidy knows no such option, where it is internal, and where a
/// string libtidy does not take is given.
fn set_opt(
    req: &mut Request,
    id: DocId,
    name: &[u8],
    value: Value,
    arg: u32,
) -> Result<bool, Thrown> {
    let name_shown = shown(name);
    let Some(opt) = with_doc(req, id, |_, doc| Ok(doc.option(name)))?.flatten() else {
        let text = format!("Unknown Tidy configuration option \"{name_shown}\"");
        return Err(argument_error(req, "ValueError", arg, &text));
    };
    if opt.is_internal() {
        let text = format!("Attempting to set read-only option \"{name_shown}\"");
        return Err(argument_error(req, "ValueError", arg, &text));
    }
    let value = value.deref();
    let as_text = match opt.kind() {
        OptType::String => true,
        OptType::Integer => matches!(value.as_ref(), ValueRef::Str(_) | ValueRef::Object(_)),
        OptType::Boolean => false,
    };
    if !as_text {
        let n = to_long(req, &value)?;
        let set = with_doc(req, id, |req, doc| match opt.kind() {
            OptType::Boolean => doc.set_flag(req, opt, n),
            _ => doc.set_int(req, opt, n),
        })?;
        return Ok(set.unwrap_or(false));
    }
    let text = to_string(req, &value)?;
    let number = (opt.kind() == OptType::Integer)
        .then(|| numeric_string(text.as_bytes()))
        .flatten();
    let set = match with_doc(req, id, |req, doc| {
        match number.as_ref().map(Value::as_ref) {
            Some(ValueRef::Int(n)) => doc.set_int(req, opt, n),
            Some(ValueRef::Float(d)) => doc.set_int(req, opt, lval_cap(d)),
            _ => doc.set_value(req, opt, text.as_bytes()),
        }
    }) {
        Ok(set) => set.unwrap_or(false),
        Err(thrown) => {
            text.give_up(req);
            return Err(thrown);
        }
    };
    let refused = !set && number.is_none();
    let value_shown = shown(text.as_bytes());
    text.give_up(req);
    if refused {
        let text = format!("option \"{name_shown}\" does not accept \"{value_shown}\" as a value");
        return Err(argument_error(req, "TypeError", arg, &text));
    }
    Ok(set)
}

/// `zend_dval_to_lval_cap`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn lval_cap(d: f64) -> i64 {
    if !d.is_finite() {
        0
    } else if d >= i64::MAX as f64 || d < i64::MIN as f64 {
        if d > 0.0 { i64::MAX } else { i64::MIN }
    } else {
        d as i64
    }
}

/// `php_tidy_apply_config`: an array's options set one by one until one
/// fails, or a file loaded where `open_basedir` lets it be read.
fn apply_config(req: &mut Request, id: DocId, config: Config, arg: u32) -> Result<bool, Thrown> {
    match config {
        None => Ok(true),
        Some(Union::B(path)) => {
            if req.check_open_basedir(path.as_bytes(), true).is_err() {
                return Ok(false);
            }
            with_doc(req, id, |req, doc| load_config(req, doc, path.as_bytes()))?;
            Ok(true)
        }
        Some(Union::A(options)) => {
            let keyed = "be of type array with keys as string";
            if options.is_packed() {
                return Err(arg_type_error(req, arg, keyed));
            }
            let mut failed = false;
            // `ZEND_HASH_FOREACH_STR_KEY_VAL(ht_options, opt_name, opt_val)`,
            // whose conversions may serve `__toString`; the argument holds
            // the array.
            walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
                let Key::Str(name) = row.key() else {
                    return Err(arg_type_error(req, arg, keyed));
                };
                failed = failed || !set_opt(req, id, name.as_bytes(), *row.value(), arg)?;
                Ok(std::ops::ControlFlow::Continue(()))
            })?;
            Ok(!failed)
        }
    }
}

/// `tidy_doc_update_properties`: `value` the saved document and `errorBuffer`
/// the errors, each where libtidy wrote any.
fn update_properties(req: &mut Request, object: PhpObject, id: DocId) -> Result<(), Thrown> {
    let read = with_doc(req, id, |req, doc| Ok((doc.save(req)?, doc.errors())))?;
    let Some((output, errors)) = read else {
        return Ok(());
    };
    for (name, bytes) in [(&b"value"[..], Some(output)), (b"errorBuffer", errors)] {
        if let Some(bytes) = bytes.filter(|b| !b.is_empty()) {
            let bytes = fixed(&bytes);
            let value = PhpString::filled(req, bytes.len(), |out| {
                out.copy_from_slice(bytes);
            })?;
            php_ext::call::write_prop(req, &object, name, value.into_value())?;
        }
    }
    Ok(())
}

/// `php_tidy_parse_string`: the encoding set, the document marked
/// initialised, `input` parsed and the properties updated; `false`
/// beside php's warning where libtidy refused the encoding or the input.
fn parse_string(
    req: &mut Request,
    object: PhpObject,
    input: &[u8],
    encoding: Option<&[u8]>,
) -> Result<bool, Thrown> {
    let id = doc_id(req, object)?;
    if let Some(encoding) = encoding {
        let set = with_doc(req, id, |req, doc| doc.set_encoding(req, encoding))?;
        if set.unwrap_or(-1) < 0 {
            let text = format!("Could not set encoding \"{}\"", shown(encoding));
            docref(req, Level::Warning, &text);
            return Ok(false);
        }
    }
    set_initialized(req, id);
    let parsed = with_doc(req, id, |req, doc| {
        Ok((doc.parse(req, input)?, doc.errors()))
    })?;
    match parsed {
        Some((answer, errors)) if answer < 0 => {
            let text = errors.map_or_else(|| String::from("(null)"), |e| shown(&e));
            docref(req, Level::Warning, &text);
            Ok(false)
        }
        _ => update_properties(req, object, id).map(|()| true),
    }
}

/// `ZEND_SIZE_T_UINT_OVFL`: a length libtidy's `uint` cannot hold.
fn too_long(bytes: &[u8]) -> bool {
    u32::try_from(bytes.len()).is_err()
}

/// The `Deprecated` a `?string $encoding` passed `null` meets, where php
/// parses it with `Z_PARAM_STRING`.
fn null_encoding(req: &mut Request, encoding: Option<&PhpString>) {
    if encoding.is_none() && req.argument_count() >= 3 {
        arg_null_deprecated(req, 3, "?string");
    }
}

/// `php_tidy_quick_repair`: `data` repaired over a document of its own,
/// the configuration and the encoding applied first; a failure of either
/// answers `false` unless the repair still writes, as php's
/// `RETVAL_STRINGL` overwrites its `RETVAL_FALSE`.
fn quick_repair(
    req: &mut Request,
    data: &[u8],
    config: Config,
    encoding: Option<&[u8]>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(mut doc) = ffi::Doc::new(req)? else {
        return Err(fatal(req, "Could not set Tidy error buffer"));
    };
    if let Err(thrown) = default_config(req, &mut doc) {
        doc.release(req);
        return Err(thrown);
    }
    let id = adopt(req, doc)?;
    let repaired = repair(req, id, data, config, encoding);
    forget(req, id);
    repaired
}

fn repair(
    req: &mut Request,
    id: DocId,
    data: &[u8],
    config: Config,
    encoding: Option<&[u8]>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    if apply_config(req, id, config, 2)?
        && let Some(encoding) = encoding.filter(|e| !e.is_empty())
    {
        let set = with_doc(req, id, |req, doc| doc.set_encoding(req, encoding))?;
        if set.unwrap_or(-1) < 0 {
            let text = format!("Could not set encoding \"{}\"", shown(encoding));
            docref(req, Level::Warning, &text);
        }
    }
    let repaired = with_doc(req, id, |req, doc| {
        if doc.parse(req, data)? < 0 {
            return Ok(Err(doc.errors()));
        }
        if doc.clean_repair(req)? < 0 {
            return Ok(Ok(None));
        }
        doc.save(req).map(|out| Ok(Some(out)))
    })?;
    match repaired {
        Some(Ok(Some(out))) => new_string(req, fixed(&out)).map(OrFalse::Value),
        Some(Err(errors)) => {
            let text = errors.map_or_else(|| String::from("(null)"), |e| shown(&e));
            docref(req, Level::Warning, &text);
            Ok(OrFalse::False)
        }
        _ => Ok(OrFalse::False),
    }
}

/// `tidy_repair_file` and `tidy::repairFile`: nothing where the file
/// does not open, and php's `ValueError` for one too long.
fn repair_file(
    req: &mut Request,
    filename: Path,
    config: Config,
    encoding: Option<PhpString>,
    use_include_path: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    null_encoding(req, encoding.as_ref());
    let Some(data) = file_to_mem(req, filename.0.as_bytes(), use_include_path)? else {
        return Ok(OrFalse::False);
    };
    let answer = if too_long(data.as_bytes()) {
        Err(argument_error(
            req,
            "ValueError",
            1,
            "File content is too long",
        ))
    } else {
        quick_repair(
            req,
            data.as_bytes(),
            config,
            encoding.as_ref().map(PhpString::as_bytes),
        )
    };
    data.give_up(req);
    answer
}

/// `tidy_repair_string` and `tidy::repairString`.
fn repair_string(
    req: &mut Request,
    string: PhpString,
    config: Config,
    encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    null_encoding(req, encoding.as_ref());
    if too_long(string.as_bytes()) {
        return Err(argument_error(req, "ValueError", 1, "is too long"));
    }
    quick_repair(
        req,
        string.as_bytes(),
        config,
        encoding.as_ref().map(PhpString::as_bytes),
    )
}

/// `tidy_ce_doc`.
fn doc_class(req: &mut Request) -> Result<php_ext::values::ClassId, Thrown> {
    let class = lookup_class(req, b"tidy")?;
    class.ok_or_else(|| fatal(req, "InternalError: tidy is not linked"))
}

/// `object_init_ex(return_value, tidy_ce_doc)` and what the function
/// asks of it; the object given up and `false` where that fails.
fn parse_new(
    req: &mut Request,
    input: &[u8],
    config: Config,
    encoding: Option<&[u8]>,
) -> Result<OrFalse<Owner<Tidy>>, Thrown> {
    let class = doc_class(req)?;
    let object = new_object(req, class)?;
    let parsed = match doc_id(req, *object) {
        Ok(id) => match apply_config(req, id, config, 2) {
            Ok(true) => parse_string(req, *object, input, encoding),
            other => other,
        },
        Err(thrown) => Err(thrown),
    };
    match parsed {
        Ok(true) => Ok(OrFalse::Value(object.into_instance())),
        Ok(false) => {
            object.give_up(req);
            Ok(OrFalse::False)
        }
        Err(thrown) => {
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// `Cannot load "%s" into memory%s`.
fn cannot_load(path: &[u8], use_include_path: bool) -> String {
    let using = if use_include_path {
        " (using include path)"
    } else {
        ""
    };
    format!("Cannot load \"{}\" into memory{using}", shown(path))
}

/// `tidy_parse_string(string $string, array|string|null $config = null,
/// ?string $encoding = null): tidy|false`.
///
/// ```php
/// $t = tidy_parse_string('<p>x', ['show-body-only' => true], 'utf8'); echo $t, "\n";
/// try { tidy_parse_string('', [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(tidy_parse_string('', null, 'bogus'));
/// ```
/// ```expect
/// <p>x</p>
///
/// tidy_parse_string(): Argument #2 ($config) must be of type array with keys as string
///
/// Warning: tidy_parse_string(): Could not set encoding "bogus" in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn tidy_parse_string(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] config: Config,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<Tidy>>, Thrown> {
    if too_long(string.as_bytes()) {
        return Err(argument_error(req, "ValueError", 1, "is too long"));
    }
    parse_new(
        req,
        string.as_bytes(),
        config,
        encoding.as_ref().map(PhpString::as_bytes),
    )
}

/// `tidy_get_error_buffer(tidy $tidy): string|false`.
///
/// ```php
/// var_dump(tidy_get_error_buffer(new tidy), tidy_get_error_buffer(tidy_parse_string('<p>')));
/// ```
/// ```expect
/// bool(false)
/// string(217) "line 1 column 1 - Warning: missing <!DOCTYPE> declaration
/// line 1 column 1 - Warning: inserting implicit <body>
/// line 1 column 1 - Warning: inserting missing 'title' element
/// line 1 column 1 - Warning: trimming empty <p>"
/// ```
#[php_function]
fn tidy_get_error_buffer(
    req: &mut Request,
    tidy: Tidy,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let errors = on_doc(req, tidy.object(), |_, doc| Ok(doc.errors()))?;
    match errors {
        Some(errors) => new_string(req, fixed(&errors)).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `tidy_get_output(tidy $tidy): string`.
///
/// ```php
/// var_dump(tidy_get_output(new tidy), tidy_get_output(tidy_parse_string('<i>a', ['show-body-only' => true])));
/// ```
/// ```expect
/// string(0) ""
/// string(8) "<i>a</i>"
/// ```
#[php_function]
fn tidy_get_output(req: &mut Request, tidy: Tidy) -> Result<Owner<PhpString>, Thrown> {
    let output = on_doc(req, tidy.object(), |req, doc| doc.save(req))?;
    new_string(req, fixed(&output))
}

/// `tidy_parse_file(string $filename, array|string|null $config = null,
/// ?string $encoding = null, bool $useIncludePath = false): tidy|false`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'tidy'); file_put_contents($f, '<b>x');
/// echo tidy_parse_file($f, ['show-body-only' => 1]), "\n"; unlink($f);
/// var_dump(tidy_parse_file('/nonexistent/x.html'));
/// ```
/// ```expect
/// <b>x</b>
///
/// Warning: tidy_parse_file(): Cannot load "/nonexistent/x.html" into memory in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn tidy_parse_file(
    req: &mut Request,
    filename: Path,
    #[php(default = null)] config: Config,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = false)] useIncludePath: bool,
) -> Result<OrFalse<Owner<Tidy>>, Thrown> {
    let path = filename.0.as_bytes();
    let Some(contents) = file_to_mem(req, path, useIncludePath)? else {
        let text = cannot_load(path, useIncludePath);
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    };
    let answer = if too_long(contents.as_bytes()) {
        Err(throw(req, "ValueError", "File content is too long"))
    } else {
        let encoding = encoding.as_ref().map(PhpString::as_bytes);
        parse_new(req, contents.as_bytes(), config, encoding)
    };
    contents.give_up(req);
    answer
}

/// `tidy_clean_repair`, `tidy::cleanRepair`.
fn clean_repair(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let id = doc_id(req, object)?;
    if with_doc(req, id, |req, doc| doc.clean_repair(req))?.unwrap_or(-1) >= 0 {
        update_properties(req, object, id)?;
        return Ok(true);
    }
    Ok(false)
}

/// `tidy_clean_repair(tidy $tidy): bool`.
///
/// ```php
/// $t = tidy_parse_string('<b>a', ['show-body-only' => 1]); var_dump($t->value, tidy_clean_repair($t), $t->value);
/// ```
/// ```expect
/// string(8) "<b>a</b>"
/// bool(true)
/// string(8) "<b>a</b>"
/// ```
#[php_function]
fn tidy_clean_repair(req: &mut Request, tidy: Tidy) -> Result<bool, Thrown> {
    clean_repair(req, tidy.object())
}

/// `tidy_repair_string(string $string, array|string|null $config = null,
/// ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(tidy_repair_string('<p>a', ['show-body-only' => true, 'newline' => 'LF']));
/// var_dump(tidy_repair_string('<p>a', 'nonexistent.tcfg', 'utf8') !== false);
/// ```
/// ```expect
/// string(9) "<p>a</p>
/// "
///
/// Warning: tidy_repair_string(): Could not load the Tidy configuration file "nonexistent.tcfg" in %s on line 3
/// bool(true)
/// ```
#[php_function]
fn tidy_repair_string(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] config: Config,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    repair_string(req, string, config, encoding)
}

/// `tidy_repair_file(string $filename, array|string|null $config = null,
/// ?string $encoding = null, bool $useIncludePath = false): string|false`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'tidy'); file_put_contents($f, '<i>a');
/// var_dump(tidy_repair_file($f, ['show-body-only' => true]), tidy_repair_file('/nonexistent')); unlink($f);
/// try { tidy_repair_file(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(8) "<i>a</i>"
/// bool(false)
/// Path must not be empty
/// ```
#[php_function]
fn tidy_repair_file(
    req: &mut Request,
    filename: Path,
    #[php(default = null)] config: Config,
    #[php(default = null)] encoding: Option<PhpString>,
    #[php(default = false)] useIncludePath: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    repair_file(req, filename, config, encoding, useIncludePath)
}

/// `tidy_diagnose`, `tidy::diagnose`: no diagnostics for a document never
/// parsed.
fn diagnose(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let id = doc_id(req, object)?;
    if !initialized(req, id) {
        return Ok(false);
    }
    if with_doc(req, id, |req, doc| doc.diagnose(req))?.unwrap_or(-1) >= 0 {
        update_properties(req, object, id)?;
        return Ok(true);
    }
    Ok(false)
}

/// `tidy_diagnose(tidy $tidy): bool`.
///
/// ```php
/// var_dump(tidy_diagnose(new tidy));
/// $t = tidy_parse_string('<!DOCTYPE html><title>t</title><p>x'); var_dump(tidy_diagnose($t)); echo $t->errorBuffer, "\n";
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// line 1 column 32 - Warning: inserting implicit <body>
/// Info: Document content looks like HTML5
/// Tidy found 1 warning and 0 errors!
/// ```
#[php_function]
fn tidy_diagnose(req: &mut Request, tidy: Tidy) -> Result<bool, Thrown> {
    diagnose(req, tidy.object())
}

/// `tidy_get_release(): string`, `tidyReleaseDate()`.
///
/// ```php
/// var_dump(tidy_get_release(), (new tidy)->getRelease());
/// ```
/// ```expect
/// string(10) "2021/07/10"
/// string(10) "2021/07/10"
/// ```
#[php_function]
fn tidy_get_release(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    new_string(req, ffi::release_date().as_bytes())
}

/// `tidy_get_opt_doc`, `tidy::getOptDoc`.
fn opt_doc(
    req: &mut Request,
    object: PhpObject,
    option: PhpString,
    arg: u32,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name = option.as_bytes();
    let found = on_doc(req, object, |_, doc| {
        Ok(doc.option(name).map(|opt| doc.option_doc(opt)))
    })?;
    match found {
        None => Err(invalid_option(req, name, arg)),
        Some(Some(text)) => new_string(req, &text).map(OrFalse::Value),
        Some(None) => Ok(OrFalse::False),
    }
}

/// `is an invalid configuration option, "%s" given`.
fn invalid_option(req: &mut Request, name: &[u8], arg: u32) -> Thrown {
    let text = format!(
        "is an invalid configuration option, \"{}\" given",
        shown(name)
    );
    argument_error(req, "ValueError", arg, &text)
}

/// `tidy_get_opt_doc(tidy $tidy, string $option): string|false`.
///
/// ```php
/// var_dump(tidy_get_opt_doc(new tidy, 'ncr'));
/// try { tidy_get_opt_doc(new tidy, 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(73) "This option specifies if Tidy should allow numeric character references. "
/// tidy_get_opt_doc(): Argument #2 ($option) is an invalid configuration option, "x" given
/// ```
#[php_function]
fn tidy_get_opt_doc(
    req: &mut Request,
    tidy: Tidy,
    option: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    opt_doc(req, tidy.object(), option, 2)
}

/// One option's value as `php_tidy_get_opt_val` reads it.
fn opt_value(req: &mut Request, doc: &ffi::Doc, opt: ffi::Opt) -> Result<Owner<Value>, Thrown> {
    Ok(match opt.kind() {
        OptType::String => {
            let text = doc.get_value(opt).unwrap_or_default();
            new_string(req, &text)?.into_value()
        }
        #[allow(clippy::cast_possible_wrap)]
        OptType::Integer => Owner::int(doc.get_int(opt) as i64),
        OptType::Boolean => Owner::bool(doc.get_bool(opt)),
    })
}

/// `tidy_get_config`, `tidy::getConfig`: every option by name.
fn get_config(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let id = doc_id(req, object)?;
    let rows = with_doc(req, id, |req, doc| {
        let mut rows = Vec::new();
        for opt in doc.options() {
            rows.push((opt.name(), opt_value(req, doc, opt)?));
        }
        Ok(rows)
    })?;
    let mut out = PhpArray::new(req)?;
    for (name, value) in rows.unwrap_or_default() {
        out.set_symtable_str(req, &name, value)?;
    }
    Ok(out)
}

/// `tidy_get_config(tidy $tidy): array`.
///
/// ```php
/// $c = tidy_get_config(tidy_parse_string('', ['indent' => true, 'wrap' => 10, 'alt-text' => 'x']));
/// var_dump(count($c) > 90, $c['indent'], $c['wrap'], $c['alt-text'], $c['tidy-mark'], $c['force-output']);
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// int(10)
/// string(1) "x"
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn tidy_get_config(req: &mut Request, tidy: Tidy) -> Result<Owner<PhpArray>, Thrown> {
    get_config(req, tidy.object())
}

/// One of `tidyStatus` and its kin, of the object's document.
fn status(req: &mut Request, object: PhpObject, which: Status) -> Result<i64, Thrown> {
    on_doc(req, object, |_, doc| Ok(doc.status(which)))
}

/// `TIDY_FETCH_INITIALIZED_OBJECT`'s refusal.
fn initialized_status(req: &mut Request, object: PhpObject, which: Status) -> Result<i64, Thrown> {
    let id = doc_id(req, object)?;
    if !initialized(req, id) {
        return Err(throw_error(req, "Error", "tidy object is not initialized"));
    }
    status(req, object, which)
}

/// `tidy_get_status(tidy $tidy): int`.
///
/// ```php
/// var_dump(tidy_get_status(tidy_parse_string('<!DOCTYPE html><title>t</title>')), tidy_get_status(tidy_parse_string('<p>')), tidy_get_status(tidy_parse_string('<bogus>')));
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// ```
#[php_function]
fn tidy_get_status(req: &mut Request, tidy: Tidy) -> Result<i64, Thrown> {
    status(req, tidy.object(), Status::Overall)
}

/// `tidy_get_html_ver(tidy $tidy): int`.
///
/// ```php
/// var_dump(tidy_get_html_ver(tidy_parse_string('<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN"><title>t</title>')));
/// try { tidy_get_html_ver(new tidy); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// tidy object is not initialized
/// ```
#[php_function]
fn tidy_get_html_ver(req: &mut Request, tidy: Tidy) -> Result<i64, Thrown> {
    initialized_status(req, tidy.object(), Status::HtmlVersion)
}

/// `tidy_is_xhtml(tidy $tidy): bool`.
///
/// ```php
/// var_dump(tidy_is_xhtml(tidy_parse_string('<html xmlns="http://www.w3.org/1999/xhtml"><title>t</title></html>')), tidy_is_xhtml(tidy_parse_string('<p>')));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn tidy_is_xhtml(req: &mut Request, tidy: Tidy) -> Result<bool, Thrown> {
    initialized_status(req, tidy.object(), Status::Xhtml).map(|n| n != 0)
}

/// `tidy_is_xml(tidy $tidy): bool`.
///
/// ```php
/// var_dump(tidy_is_xml(tidy_parse_string('<a>b</a>', ['input-xml' => true])), tidy_is_xml(tidy_parse_string('<p>')));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn tidy_is_xml(req: &mut Request, tidy: Tidy) -> Result<bool, Thrown> {
    initialized_status(req, tidy.object(), Status::Xml).map(|n| n != 0)
}

/// `tidy_error_count(tidy $tidy): int`.
///
/// ```php
/// var_dump(tidy_error_count(tidy_parse_string('<bogus>')), tidy_error_count(new tidy));
/// ```
/// ```expect
/// int(1)
/// int(0)
/// ```
#[php_function]
fn tidy_error_count(req: &mut Request, tidy: Tidy) -> Result<i64, Thrown> {
    status(req, tidy.object(), Status::Errors)
}

/// `tidy_warning_count(tidy $tidy): int`.
///
/// ```php
/// var_dump(tidy_warning_count(tidy_parse_string('<p>')));
/// ```
/// ```expect
/// int(4)
/// ```
#[php_function]
fn tidy_warning_count(req: &mut Request, tidy: Tidy) -> Result<i64, Thrown> {
    status(req, tidy.object(), Status::Warnings)
}

/// `tidy_access_count(tidy $tidy): int`.
///
/// ```php
/// $t = tidy_parse_string('<img src="a.png">', ['accessibility-check' => 1]); $t->diagnose(); var_dump(tidy_access_count($t) > 0);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn tidy_access_count(req: &mut Request, tidy: Tidy) -> Result<i64, Thrown> {
    status(req, tidy.object(), Status::Access)
}

/// `tidy_config_count(tidy $tidy): int`.
///
/// ```php
/// var_dump(tidy_config_count(tidy_parse_string('', ['doctype' => 'auto'])));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function]
fn tidy_config_count(req: &mut Request, tidy: Tidy) -> Result<i64, Thrown> {
    status(req, tidy.object(), Status::Config)
}

/// `string|int|bool`.
type OptAnswer = Union<Owner<PhpString>, Union<i64, bool>>;

/// `tidy_getopt`, `tidy::getOpt`.
fn getopt(
    req: &mut Request,
    object: PhpObject,
    option: PhpString,
    arg: u32,
) -> Result<OptAnswer, Thrown> {
    let name = option.as_bytes();
    let id = doc_id(req, object)?;
    let found = with_doc(req, id, |req, doc| match doc.option(name) {
        Some(opt) => opt_value(req, doc, opt).map(Some),
        None => Ok(None),
    })?
    .flatten();
    let Some(value) = found else {
        return Err(invalid_option(req, name, arg));
    };
    let answer = match value.as_ref() {
        ValueRef::Str(s) => Union::A(Owner::take(req, s)),
        ValueRef::Int(n) => Union::B(Union::A(n)),
        _ => Union::B(Union::B(value.as_bool().unwrap_or(false))),
    };
    value.give_up(req);
    Ok(answer)
}

/// `tidy_getopt(tidy $tidy, string $option): string|int|bool`.
///
/// ```php
/// $t = new tidy; var_dump(tidy_getopt($t, 'tidy-mark'), tidy_getopt($t, 'tab-size'), tidy_getopt($t, 'error-file'), $t->getOpt('force-output'));
/// try { tidy_getopt($t, 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// int(8)
/// string(0) ""
/// bool(true)
/// tidy_getopt(): Argument #2 ($option) is an invalid configuration option, "x" given
/// ```
#[php_function]
fn tidy_getopt(req: &mut Request, tidy: Tidy, option: PhpString) -> Result<OptAnswer, Thrown> {
    getopt(req, tidy.object(), option, 2)
}

/// `php_tidy_create_node`: the node asked for, `null` where the tree has
/// none.
fn base_node(
    req: &mut Request,
    object: PhpObject,
    which: Base,
) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
    let id = doc_id(req, object)?;
    let found = with_doc(req, id, |_, doc| Ok(doc.base(which)))?.flatten();
    match found {
        Some(at) => node::create(req, id, at).map(|object| Some(object.into_instance())),
        None => Ok(None),
    }
}

/// `tidy_get_root(tidy $tidy): ?tidyNode`.
///
/// ```php
/// $r = tidy_get_root(tidy_parse_string('<p>x')); var_dump($r->type === TIDY_NODETYPE_ROOT, $r->name, count($r->child), tidy_get_root(new tidy)?->type);
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// int(1)
/// int(0)
/// ```
#[php_function]
fn tidy_get_root(
    req: &mut Request,
    tidy: Tidy,
) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
    base_node(req, tidy.object(), Base::Root)
}

/// `tidy_get_html(tidy $tidy): ?tidyNode`.
///
/// ```php
/// var_dump(tidy_get_html(tidy_parse_string('<p>x'))->name, tidy_get_html(new tidy));
/// ```
/// ```expect
/// string(4) "html"
/// NULL
/// ```
#[php_function]
fn tidy_get_html(
    req: &mut Request,
    tidy: Tidy,
) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
    base_node(req, tidy.object(), Base::Html)
}

/// `tidy_get_head(tidy $tidy): ?tidyNode`.
///
/// ```php
/// var_dump(tidy_get_head(tidy_parse_string('<p>x'))->name);
/// ```
/// ```expect
/// string(4) "head"
/// ```
#[php_function]
fn tidy_get_head(
    req: &mut Request,
    tidy: Tidy,
) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
    base_node(req, tidy.object(), Base::Head)
}

/// `tidy_get_body(tidy $tidy): ?tidyNode`.
///
/// ```php
/// var_dump(tidy_get_body(tidy_parse_string('<p>x'))->child[0]->name, tidy_get_body(tidy_parse_string('<frameset></frameset>')));
/// ```
/// ```expect
/// string(1) "p"
/// NULL
/// ```
#[php_function]
fn tidy_get_body(
    req: &mut Request,
    tidy: Tidy,
) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
    base_node(req, tidy.object(), Base::Body)
}

/// The functions in `tidy.stub.php`'s order.
/// A function's declaration and its entry.
type Entry = (
    FunctionDecl,
    fn(&mut Request) -> Result<Owner<Value>, Thrown>,
);

pub(crate) fn functions() -> Vec<Entry> {
    vec![
        tidy_parse_string::declared(),
        tidy_get_error_buffer::declared(),
        tidy_get_output::declared(),
        tidy_parse_file::declared(),
        tidy_clean_repair::declared(),
        tidy_repair_string::declared(),
        tidy_repair_file::declared(),
        tidy_diagnose::declared(),
        tidy_get_release::declared(),
        tidy_get_opt_doc::declared(),
        tidy_get_config::declared(),
        tidy_get_status::declared(),
        tidy_get_html_ver::declared(),
        tidy_is_xhtml::declared(),
        tidy_is_xml::declared(),
        tidy_error_count::declared(),
        tidy_warning_count::declared(),
        tidy_access_count::declared(),
        tidy_config_count::declared(),
        tidy_getopt::declared(),
        tidy_get_root::declared(),
        tidy_get_html::declared(),
        tidy_get_head::declared(),
        tidy_get_body::declared(),
    ]
}

/// The `tidy` class apart, so `$tidy` can name the
/// functions' parameter.
pub(crate) mod class {
    use super::{
        Base, Config, DOC, Instance, Level, Obj, OptAnswer, OrFalse, Owner, Path, PhpArray,
        PhpString, Request, Status, This, Thrown, argument_error, base_node, cannot_load,
        clean_repair, construct, diagnose, docref, ffi, file_to_mem, get_config, getopt,
        initialized_status, new_string, opt_doc, parse_with, php_class, repair_file, repair_string,
        status, throw, tidyNode, too_long,
    };

    /// `class tidy`: one `PHPTidyObj` each, its document made with it,
    /// uncloneable, cast to the document libtidy saves.
    ///
    /// ```php
    /// $t = new tidy; var_dump($t, (string) $t, (int) $t, (bool) $t);
    /// try { clone $t; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// class MyTidy extends tidy {} $m = new MyTidy; var_dump($m->parseString('<i>x'), $m->body()->child[0]->name);
    /// ```
    /// ```expect
    /// object(tidy)#1 (2) {
    ///   ["errorBuffer"]=>
    ///   NULL
    ///   ["value"]=>
    ///   NULL
    /// }
    /// string(0) ""
    /// int(0)
    /// bool(true)
    /// Trying to clone an uncloneable object of class tidy
    /// bool(true)
    /// string(1) "i"
    /// ```
    #[allow(non_camel_case_types)]
    #[php_class(size = 80, payload = Obj, handlers = DOC)]
    pub(crate) struct tidy;

    #[php_class]
    #[allow(non_snake_case)]
    impl tidy {
        #[php(property)]
        const errorBuffer: Option<PhpString> = None;
        #[php(property)]
        const value: Option<PhpString> = None;

        /// `tidy::__construct(...)`: a file parsed where one is named, a warning of the
        /// configuration's thrown as `Exception`.
        ///
        /// ```php
        /// $f = tempnam(sys_get_temp_dir(), 'tidy'); file_put_contents($f, '<b>x');
        /// echo new tidy($f, ['show-body-only' => true]), "\n"; unlink($f);
        /// try { new tidy('/nonexistent/x'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
        /// ```
        /// ```expect
        /// <b>x</b>
        /// Cannot load "/nonexistent/x" into memory
        /// ```
        fn __construct(
            req: &mut Request,
            this: This<Self>,
            #[php(default = null)] filename: Option<Path>,
            #[php(default = null)] config: Config,
            #[php(default = null)] encoding: Option<PhpString>,
            #[php(default = false)] useIncludePath: bool,
        ) -> Result<(), Thrown> {
            let Some(Path(filename)) = filename else {
                return Ok(());
            };
            let object = this.object();
            let Some(contents) = file_to_mem(req, filename.as_bytes(), useIncludePath)? else {
                let text = cannot_load(filename.as_bytes(), useIncludePath);
                return Err(throw(req, "Exception", &text));
            };
            let answer = construct(req, object, *contents, config, encoding.as_ref());
            contents.give_up(req);
            answer
        }

        /// `getOpt(string $option): string|int|bool`, `tidy_getopt`.
        ///
        /// ```php
        /// $t = new tidy; var_dump($t->getOpt('tab-size'), $t->getOpt('tidy-mark'));
        /// ```
        /// ```expect
        /// int(8)
        /// bool(false)
        /// ```
        #[php(tentative)]
        fn getOpt(
            req: &mut Request,
            this: This<Self>,
            option: PhpString,
        ) -> Result<OptAnswer, Thrown> {
            getopt(req, this.object(), option, 1)
        }

        /// `cleanRepair(): bool`, `tidy_clean_repair`.
        ///
        /// ```php
        /// $t = tidy_parse_string('<b>a', ['show-body-only' => 1]); var_dump($t->cleanRepair(), $t->value);
        /// ```
        /// ```expect
        /// bool(true)
        /// string(8) "<b>a</b>"
        /// ```
        #[php(tentative)]
        fn cleanRepair(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
            clean_repair(req, this.object())
        }

        /// `parseFile(string $filename, array|string|null $config = null,
        /// ?string $encoding = null, bool $useIncludePath = false): bool`.
        ///
        /// ```php
        /// $t = new tidy; var_dump($t->parseFile('/nonexistent/y'));
        /// ```
        /// ```expect
        /// Warning: tidy::parseFile(): Cannot load "/nonexistent/y" into memory in %s on line 2
        /// bool(false)
        /// ```
        #[php(tentative)]
        fn parseFile(
            req: &mut Request,
            this: This<Self>,
            filename: Path,
            #[php(default = null)] config: Config,
            #[php(default = null)] encoding: Option<PhpString>,
            #[php(default = false)] useIncludePath: bool,
        ) -> Result<bool, Thrown> {
            let path = filename.0.as_bytes();
            let Some(contents) = file_to_mem(req, path, useIncludePath)? else {
                let text = cannot_load(path, useIncludePath);
                docref(req, Level::Warning, &text);
                return Ok(false);
            };
            let answer = if too_long(contents.as_bytes()) {
                Err(throw(req, "ValueError", "File content is too long"))
            } else {
                parse_with(
                    req,
                    this.object(),
                    contents.as_bytes(),
                    config,
                    encoding.as_ref(),
                )
            };
            contents.give_up(req);
            answer
        }

        /// `parseString(string $string, array|string|null $config = null,
        /// ?string $encoding = null): bool`.
        ///
        /// ```php
        /// $t = new tidy; var_dump($t->parseString('<p>a', ['show-body-only' => true]), $t->value);
        /// try { $t->parseString('', ['bogus' => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
        /// ```
        /// ```expect
        /// bool(true)
        /// string(9) "<p>a</p>
        /// "
        /// tidy::parseString(): Argument #2 ($config) Unknown Tidy configuration option "bogus"
        /// ```
        #[php(tentative)]
        fn parseString(
            req: &mut Request,
            this: This<Self>,
            string: PhpString,
            #[php(default = null)] config: Config,
            #[php(default = null)] encoding: Option<PhpString>,
        ) -> Result<bool, Thrown> {
            if too_long(string.as_bytes()) {
                return Err(argument_error(req, "ValueError", 1, "is too long"));
            }
            parse_with(
                req,
                this.object(),
                string.as_bytes(),
                config,
                encoding.as_ref(),
            )
        }

        /// `static repairString(...): string|false`, `tidy_repair_string`.
        ///
        /// ```php
        /// var_dump(tidy::repairString('<u>a', ['show-body-only' => 1, 'newline' => 'LF']));
        /// ```
        /// ```expect
        /// string(8) "<u>a</u>"
        /// ```
        #[php(tentative)]
        fn repairString(
            req: &mut Request,
            string: PhpString,
            #[php(default = null)] config: Config,
            #[php(default = null)] encoding: Option<PhpString>,
        ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
            repair_string(req, string, config, encoding)
        }

        /// `static repairFile(...): string|false`, `tidy_repair_file`.
        ///
        /// ```php
        /// var_dump(tidy::repairFile('/nonexistent'));
        /// ```
        /// ```expect
        /// bool(false)
        /// ```
        #[php(tentative)]
        fn repairFile(
            req: &mut Request,
            filename: Path,
            #[php(default = null)] config: Config,
            #[php(default = null)] encoding: Option<PhpString>,
            #[php(default = false)] useIncludePath: bool,
        ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
            repair_file(req, filename, config, encoding, useIncludePath)
        }

        /// `diagnose(): bool`, `tidy_diagnose`.
        ///
        /// ```php
        /// $t = new tidy; var_dump($t->diagnose(), $t->parseString('<p>x'), $t->diagnose());
        /// ```
        /// ```expect
        /// bool(false)
        /// bool(true)
        /// bool(true)
        /// ```
        #[php(tentative)]
        fn diagnose(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
            diagnose(req, this.object())
        }

        /// `getRelease(): string`, `tidy_get_release`.
        ///
        /// ```php
        /// var_dump((new tidy)->getRelease());
        /// ```
        /// ```expect
        /// string(10) "2021/07/10"
        /// ```
        #[php(tentative)]
        fn getRelease(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
            let _ = this;
            new_string(req, ffi::release_date().as_bytes())
        }

        /// `getConfig(): array`, `tidy_get_config`.
        ///
        /// ```php
        /// var_dump((new tidy)->getConfig()['wrap']);
        /// ```
        /// ```expect
        /// int(68)
        /// ```
        #[php(tentative)]
        fn getConfig(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
            get_config(req, this.object())
        }

        /// `getStatus(): int`, `tidy_get_status`.
        ///
        /// ```php
        /// var_dump((new tidy)->getStatus(), tidy_parse_string('<bogus>')->getStatus());
        /// ```
        /// ```expect
        /// int(0)
        /// int(2)
        /// ```
        #[php(tentative)]
        fn getStatus(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
            status(req, this.object(), Status::Overall)
        }

        /// `getHtmlVer(): int`, `tidy_get_html_ver`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN" "http://www.w3.org/TR/html4/strict.dtd"><title>t</title>')->getHtmlVer());
        /// ```
        /// ```expect
        /// int(0)
        /// ```
        #[php(tentative)]
        fn getHtmlVer(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
            initialized_status(req, this.object(), Status::HtmlVersion)
        }

        /// `getOptDoc(string $option): string|false`, `tidy_get_opt_doc`.
        ///
        /// ```php
        /// try { (new tidy)->getOptDoc('x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
        /// ```
        /// ```expect
        /// tidy::getOptDoc(): Argument #1 ($option) is an invalid configuration option, "x" given
        /// ```
        #[php(tentative)]
        fn getOptDoc(
            req: &mut Request,
            this: This<Self>,
            option: PhpString,
        ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
            opt_doc(req, this.object(), option, 1)
        }

        /// `isXhtml(): bool`, `tidy_is_xhtml`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<p>')->isXhtml());
        /// ```
        /// ```expect
        /// bool(false)
        /// ```
        #[php(tentative)]
        fn isXhtml(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
            initialized_status(req, this.object(), Status::Xhtml).map(|n| n != 0)
        }

        /// `isXml(): bool`, `tidy_is_xml`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<p>')->isXml());
        /// ```
        /// ```expect
        /// bool(false)
        /// ```
        #[php(tentative)]
        fn isXml(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
            initialized_status(req, this.object(), Status::Xml).map(|n| n != 0)
        }

        /// `root(): ?tidyNode`, `tidy_get_root`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<p>')->root()->type);
        /// ```
        /// ```expect
        /// int(0)
        /// ```
        #[php(tentative)]
        fn root(
            req: &mut Request,
            this: This<Self>,
        ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
            base_node(req, this.object(), Base::Root)
        }

        /// `head(): ?tidyNode`, `tidy_get_head`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<p>')->head()->name);
        /// ```
        /// ```expect
        /// string(4) "head"
        /// ```
        #[php(tentative)]
        fn head(
            req: &mut Request,
            this: This<Self>,
        ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
            base_node(req, this.object(), Base::Head)
        }

        /// `html(): ?tidyNode`, `tidy_get_html`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<p>')->html()->name);
        /// ```
        /// ```expect
        /// string(4) "html"
        /// ```
        #[php(tentative)]
        fn html(
            req: &mut Request,
            this: This<Self>,
        ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
            base_node(req, this.object(), Base::Html)
        }

        /// `body(): ?tidyNode`, `tidy_get_body`.
        ///
        /// ```php
        /// var_dump(tidy_parse_string('<p>x')->body()->name);
        /// ```
        /// ```expect
        /// string(4) "body"
        /// ```
        #[php(tentative)]
        fn body(
            req: &mut Request,
            this: This<Self>,
        ) -> Result<Option<Owner<Instance<tidyNode>>>, Thrown> {
            base_node(req, this.object(), Base::Body)
        }
    }
}

/// `tidy::parseFile` and `tidy::parseString` past their input: the
/// configuration, then the parse.
fn parse_with(
    req: &mut Request,
    object: PhpObject,
    input: &[u8],
    config: Config,
    encoding: Option<&PhpString>,
) -> Result<bool, Thrown> {
    let id = doc_id(req, object)?;
    Ok(apply_config(req, id, config, 2)?
        && parse_string(req, object, input, encoding.map(PhpString::as_bytes))?)
}

/// `tidy::__construct` past the file: the configuration under
/// `EH_THROW`, then the parse, whose warnings stand.
fn construct(
    req: &mut Request,
    object: PhpObject,
    contents: PhpString,
    config: Config,
    encoding: Option<&PhpString>,
) -> Result<(), Thrown> {
    if too_long(contents.as_bytes()) {
        return Err(throw(req, "ValueError", "File content is too long"));
    }
    let id = doc_id(req, object)?;
    if !throwing_warnings(req, "Exception", |req| apply_config(req, id, config, 2))? {
        return Ok(());
    }
    parse_string(
        req,
        object,
        contents.as_bytes(),
        encoding.map(PhpString::as_bytes),
    )?;
    Ok(())
}
