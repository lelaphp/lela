//! The procedural `xmlwriter_*` functions and the `XMLWriter` methods php
//! aliases to them (`@alias` in `php_xmlwriter.stub.php`): each pair the
//! same [`Op`] on the emitter, which `call` serves.

use php_ext::abi::{Instance, OrFalse, Path, Static, This, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::raising::{Thrown, arg_value_error, throw_error, type_error};
use php_ext::request::{Request, Stream};
use php_ext::values::{Owner, PhpString, Resource, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::Emitter;

/// The object a procedural function is handed, `XMLWriter $writer`.
type Handle = Instance<XMLWriter>;
/// A `string` argument.
type Text = PhpString;
/// A `?string` argument.
type Nullable = Option<PhpString>;
/// What a call of the emitter answers, `bool`.
type Took = Result<bool, Thrown>;

use crate::{
    Op, Opened, XMLWriter, call, create_static, flush, new_writer, open_uri, open_warned,
    output_memory, replace,
};
/// `xmlwriter_open_uri(string $uri): XMLWriter|false`.
///
/// ```php
/// $f = sys_get_temp_dir() . '/xmlwriter_open_uri.xml';
/// $w = xmlwriter_open_uri($f); var_dump(get_class($w));
/// xmlwriter_write_element($w, 'a', 'b'); unset($w);
/// echo file_get_contents($f), "\n"; unlink($f);
/// var_dump(xmlwriter_open_uri('/nonexistent/dir/x.xml'));
/// try { xmlwriter_open_uri(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(9) "XMLWriter"
/// <a>b</a>
///
/// Warning: xmlwriter_open_uri(): Unable to resolve file path in %s on line 6
/// bool(false)
/// xmlwriter_open_uri(): Argument #1 ($uri) must not be empty
/// ```
#[php_function]
fn xmlwriter_open_uri(
    req: &mut Request,
    uri: Path,
) -> Result<OrFalse<Owner<Instance<XMLWriter>>>, Thrown> {
    match open_warned(req, uri)? {
        Some(writer) => new_writer(req, writer).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `xmlwriter_open_memory(): XMLWriter|false`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_text($w, 'a&b');
/// var_dump(get_class($w), xmlwriter_output_memory($w));
/// ```
/// ```expect
/// string(9) "XMLWriter"
/// string(3) "a&b"
/// ```
#[php_function]
fn xmlwriter_open_memory(req: &mut Request) -> Result<OrFalse<Owner<Instance<XMLWriter>>>, Thrown> {
    match Emitter::memory() {
        Some(writer) => new_writer(req, writer).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `xmlwriter_set_indent(XMLWriter $writer, bool $enable): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_set_indent($w, true));
/// xmlwriter_start_element($w, 'a'); xmlwriter_write_element($w, 'b', 'c'); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w);
/// ```
/// ```expect
/// bool(true)
/// <a>
///  <b>c</b>
/// </a>
/// ```
#[php_function]
fn xmlwriter_set_indent(req: &mut Request, writer: Handle, enable: bool) -> Took {
    call(req, &writer, Op::SetIndent(enable))
}
/// `xmlwriter_set_indent_string(XMLWriter $writer, string $indentation): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_set_indent($w, true); var_dump(xmlwriter_set_indent_string($w, '--'));
/// xmlwriter_start_element($w, 'a'); xmlwriter_write_element($w, 'b'); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w);
/// ```
/// ```expect
/// bool(true)
/// <a>
/// --<b/>
/// </a>
/// ```
#[php_function]
fn xmlwriter_set_indent_string(req: &mut Request, writer: Handle, indentation: Text) -> Took {
    call(req, &writer, Op::SetIndentString(indentation))
}
/// `xmlwriter_start_comment(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_start_comment($w)); xmlwriter_text($w, 'c'); var_dump(xmlwriter_end_comment($w), xmlwriter_end_comment($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// <!--c-->
/// ```
#[php_function]
fn xmlwriter_start_comment(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::StartComment)
}
/// `xmlwriter_end_comment(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_comment($w)); xmlwriter_start_comment($w); var_dump(xmlwriter_end_comment($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// <!---->
/// ```
#[php_function]
fn xmlwriter_end_comment(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndComment)
}
/// `xmlwriter_start_attribute(XMLWriter $writer, string $name): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a');
/// var_dump(xmlwriter_start_attribute($w, 'x')); xmlwriter_text($w, '1"'); xmlwriter_end_attribute($w); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// try { xmlwriter_start_attribute($w, '1x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// <a x="1&quot;"/>
/// xmlwriter_start_attribute(): Argument #2 ($name) must be a valid attribute name, "1x" given
/// ```
#[php_function]
fn xmlwriter_start_attribute(req: &mut Request, writer: Handle, name: Text) -> Took {
    call(req, &writer, Op::StartAttribute(name))
}
/// `xmlwriter_end_attribute(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_attribute($w));
/// xmlwriter_start_element($w, 'a'); xmlwriter_start_attribute($w, 'x'); var_dump(xmlwriter_end_attribute($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// <a x=""
/// ```
#[php_function]
fn xmlwriter_end_attribute(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndAttribute)
}
/// `xmlwriter_write_attribute(XMLWriter $writer, string $name, string $value): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a');
/// var_dump(xmlwriter_write_attribute($w, 'x', '<&>')); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// var_dump(xmlwriter_write_attribute($w, 'y', 'z'));
/// ```
/// ```expect
/// bool(true)
/// <a x="&lt;&amp;&gt;"/>
/// bool(false)
/// ```
#[php_function]
fn xmlwriter_write_attribute(req: &mut Request, writer: Handle, name: Text, value: Text) -> Took {
    call(req, &writer, Op::WriteAttribute(name, value))
}
/// `xmlwriter_start_attribute_ns(XMLWriter $writer, ?string $prefix, string $name, ?string $namespace): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a');
/// var_dump(xmlwriter_start_attribute_ns($w, 'p', 'x', 'urn:p')); xmlwriter_text($w, 'v'); xmlwriter_end_attribute($w); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <a p:x="v" xmlns:p="urn:p"/>
/// ```
#[php_function]
fn xmlwriter_start_attribute_ns(
    req: &mut Request,
    writer: Handle,
    prefix: Nullable,
    name: Text,
    namespace: Nullable,
) -> Took {
    call(req, &writer, Op::StartAttributeNs(prefix, name, namespace))
}
/// `xmlwriter_write_attribute_ns(XMLWriter $writer, ?string $prefix, string $name, ?string $namespace, string $value): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a');
/// var_dump(xmlwriter_write_attribute_ns($w, 'p', 'x', 'urn:p', 'v'), xmlwriter_write_attribute_ns($w, null, 'y', null, 'w')); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <a p:x="v" y="w" xmlns:p="urn:p"/>
/// ```
#[php_function]
fn xmlwriter_write_attribute_ns(
    req: &mut Request,
    writer: Handle,
    prefix: Nullable,
    name: Text,
    namespace: Nullable,
    value: Text,
) -> Took {
    call(
        req,
        &writer,
        Op::WriteAttributeNs(prefix, name, namespace, value),
    )
}
/// `xmlwriter_start_element(XMLWriter $writer, string $name): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_start_element($w, 'a')); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// try { xmlwriter_start_element($w, 'a b'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// <a/>
/// xmlwriter_start_element(): Argument #2 ($name) must be a valid element name, "a b" given
/// ```
#[php_function]
fn xmlwriter_start_element(req: &mut Request, writer: Handle, name: Text) -> Took {
    call(req, &writer, Op::StartElement(name))
}
/// `xmlwriter_end_element(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_element($w)); xmlwriter_start_element($w, 'a'); xmlwriter_text($w, 't'); var_dump(xmlwriter_end_element($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// <a>t</a>
/// ```
#[php_function]
fn xmlwriter_end_element(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndElement)
}
/// `xmlwriter_full_end_element(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a'); var_dump(xmlwriter_full_end_element($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <a></a>
/// ```
#[php_function]
fn xmlwriter_full_end_element(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::FullEndElement)
}
/// `xmlwriter_start_element_ns(XMLWriter $writer, ?string $prefix, string $name, ?string $namespace): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_start_element_ns($w, 'p', 'a', 'urn:p')); xmlwriter_start_element_ns($w, null, 'b', 'urn:d'); xmlwriter_end_element($w); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <p:a xmlns:p="urn:p"><b xmlns="urn:d"/></p:a>
/// ```
#[php_function]
fn xmlwriter_start_element_ns(
    req: &mut Request,
    writer: Handle,
    prefix: Nullable,
    name: Text,
    namespace: Nullable,
) -> Took {
    call(req, &writer, Op::StartElementNs(prefix, name, namespace))
}
/// `xmlwriter_write_element(XMLWriter $writer, string $name, ?string $content = null): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_write_element($w, 'a'), xmlwriter_write_element($w, 'b', 'x<y'));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <a/><b>x&lt;y</b>
/// ```
#[php_function]
fn xmlwriter_write_element(
    req: &mut Request,
    writer: Handle,
    name: Text,
    #[php(default = null)] content: Nullable,
) -> Took {
    call(req, &writer, Op::WriteElement(name, content))
}
/// `xmlwriter_write_element_ns(XMLWriter $writer, ?string $prefix, string $name, ?string $namespace, ?string $content = null): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_write_element_ns($w, 'p', 'a', 'urn:p'), xmlwriter_write_element_ns($w, 'p', 'b', 'urn:p', 'c'));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <p:a xmlns:p="urn:p"/><p:b xmlns:p="urn:p">c</p:b>
/// ```
#[php_function]
fn xmlwriter_write_element_ns(
    req: &mut Request,
    writer: Handle,
    prefix: Nullable,
    name: Text,
    namespace: Nullable,
    #[php(default = null)] content: Nullable,
) -> Took {
    call(
        req,
        &writer,
        Op::WriteElementNs(prefix, name, namespace, content),
    )
}
/// `xmlwriter_start_pi(XMLWriter $writer, string $target): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_start_pi($w, 'php')); xmlwriter_text($w, 'echo 1;'); var_dump(xmlwriter_end_pi($w));
/// echo xmlwriter_output_memory($w), "\n";
/// try { xmlwriter_start_pi($w, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <?php echo 1;?>
/// xmlwriter_start_pi(): Argument #2 ($target) must be a valid PI target, "" given
/// ```
#[php_function]
fn xmlwriter_start_pi(req: &mut Request, writer: Handle, target: Text) -> Took {
    call(req, &writer, Op::StartPi(target))
}
/// `xmlwriter_end_pi(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_pi($w)); xmlwriter_start_pi($w, 't'); var_dump(xmlwriter_end_pi($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <?t?>
/// ```
#[php_function]
fn xmlwriter_end_pi(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndPi)
}
/// `xmlwriter_write_pi(XMLWriter $writer, string $target, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_write_pi($w, 'xml-stylesheet', 'href="a"'));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <?xml-stylesheet href="a"?>
/// ```
#[php_function]
fn xmlwriter_write_pi(req: &mut Request, writer: Handle, target: Text, content: Text) -> Took {
    call(req, &writer, Op::WritePi(target, content))
}
/// `xmlwriter_start_cdata(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a'); var_dump(xmlwriter_start_cdata($w)); xmlwriter_text($w, '<x>'); xmlwriter_end_cdata($w); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <a><![CDATA[<x>]]></a>
/// ```
#[php_function]
fn xmlwriter_start_cdata(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::StartCdata)
}
/// `xmlwriter_end_cdata(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_cdata($w)); xmlwriter_start_element($w, 'a'); xmlwriter_start_cdata($w); var_dump(xmlwriter_end_cdata($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// <a><![CDATA[]]>
/// ```
#[php_function]
fn xmlwriter_end_cdata(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndCdata)
}
/// `xmlwriter_write_cdata(XMLWriter $writer, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a'); var_dump(xmlwriter_write_cdata($w, 'a]]>b'), xmlwriter_write_cdata($w, 'c')); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <a><![CDATA[a]]>b]]><![CDATA[c]]></a>
/// ```
#[php_function]
fn xmlwriter_write_cdata(req: &mut Request, writer: Handle, content: Text) -> Took {
    call(req, &writer, Op::WriteCdata(content))
}
/// `xmlwriter_text(XMLWriter $writer, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a'); var_dump(xmlwriter_text($w, "<\"&'>")); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <a>&lt;&quot;&amp;'&gt;</a>
/// ```
#[php_function]
fn xmlwriter_text(req: &mut Request, writer: Handle, content: Text) -> Took {
    call(req, &writer, Op::Text(content))
}
/// `xmlwriter_write_raw(XMLWriter $writer, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_element($w, 'a'); var_dump(xmlwriter_write_raw($w, '<b>&</b>')); xmlwriter_end_element($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <a><b>&</b></a>
/// ```
#[php_function]
fn xmlwriter_write_raw(req: &mut Request, writer: Handle, content: Text) -> Took {
    call(req, &writer, Op::WriteRaw(content))
}
/// `xmlwriter_start_document(XMLWriter $writer, ?string $version = "1.0", ?string $encoding = null, ?string $standalone = null): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_start_document($w, '1.0', 'UTF-8', 'yes'), xmlwriter_start_document($w));
/// xmlwriter_write_element($w, 'a'); xmlwriter_end_document($w);
/// echo xmlwriter_output_memory($w);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <?xml version="1.0" encoding="UTF-8" standalone="yes"?>
/// <?xml version="1.0"?>
/// <a/>
/// ```
#[php_function]
fn xmlwriter_start_document(
    req: &mut Request,
    writer: Handle,
    #[php(default = "1.0")] version: Nullable,
    #[php(default = null)] encoding: Nullable,
    #[php(default = null)] standalone: Nullable,
) -> Took {
    call(
        req,
        &writer,
        Op::StartDocument(version, encoding, standalone),
    )
}
/// `xmlwriter_end_document(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_document($w)); xmlwriter_start_document($w); xmlwriter_start_element($w, 'a'); var_dump(xmlwriter_end_document($w));
/// echo xmlwriter_output_memory($w);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// <?xml version="1.0"?>
/// <a/>
/// ```
#[php_function]
fn xmlwriter_end_document(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndDocument)
}
/// `xmlwriter_write_comment(XMLWriter $writer, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_write_comment($w, ' c '), xmlwriter_write_comment($w, 'a--b'));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!-- c --><!--a--b-->
/// ```
#[php_function]
fn xmlwriter_write_comment(req: &mut Request, writer: Handle, content: Text) -> Took {
    call(req, &writer, Op::WriteComment(content))
}
/// `xmlwriter_start_dtd(XMLWriter $writer, string $qualifiedName, ?string $publicId = null, ?string $systemId = null): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_start_dtd($w, 'html', '-//W3C//DTD XHTML 1.0//EN', 'x.dtd')); var_dump(xmlwriter_end_dtd($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0//EN" "x.dtd">
/// ```
#[php_function]
fn xmlwriter_start_dtd(
    req: &mut Request,
    writer: Handle,
    qualifiedName: Text,
    #[php(default = null)] publicId: Nullable,
    #[php(default = null)] systemId: Nullable,
) -> Took {
    call(
        req,
        &writer,
        Op::StartDtd(qualifiedName, publicId, systemId),
    )
}
/// `xmlwriter_end_dtd(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_dtd($w)); xmlwriter_start_dtd($w, 'r'); var_dump(xmlwriter_end_dtd($w));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!DOCTYPE r>
/// ```
#[php_function]
fn xmlwriter_end_dtd(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndDtd)
}
/// `xmlwriter_write_dtd(XMLWriter $writer, string $name, ?string $publicId = null, ?string $systemId = null, ?string $content = null): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_write_dtd($w, 'r', null, 'r.dtd', '<!ELEMENT r EMPTY>'));
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <!DOCTYPE r SYSTEM "r.dtd" [<!ELEMENT r EMPTY>]>
/// ```
#[php_function]
fn xmlwriter_write_dtd(
    req: &mut Request,
    writer: Handle,
    name: Text,
    #[php(default = null)] publicId: Nullable,
    #[php(default = null)] systemId: Nullable,
    #[php(default = null)] content: Nullable,
) -> Took {
    call(
        req,
        &writer,
        Op::WriteDtd(name, publicId, systemId, content),
    )
}
/// `xmlwriter_start_dtd_element(XMLWriter $writer, string $qualifiedName): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_dtd($w, 'r'); var_dump(xmlwriter_start_dtd_element($w, 'r')); xmlwriter_text($w, 'EMPTY'); var_dump(xmlwriter_end_dtd_element($w)); xmlwriter_end_dtd($w);
/// echo xmlwriter_output_memory($w), "\n";
/// try { xmlwriter_start_dtd_element($w, '1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!DOCTYPE r [<!ELEMENT r EMPTY>]>
/// xmlwriter_start_dtd_element(): Argument #2 ($qualifiedName) must be a valid element name, "1" given
/// ```
#[php_function]
fn xmlwriter_start_dtd_element(req: &mut Request, writer: Handle, qualifiedName: Text) -> Took {
    call(req, &writer, Op::StartDtdElement(qualifiedName))
}
/// `xmlwriter_end_dtd_element(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_dtd_element($w));
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
fn xmlwriter_end_dtd_element(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndDtdElement)
}
/// `xmlwriter_write_dtd_element(XMLWriter $writer, string $name, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_dtd($w, 'r'); var_dump(xmlwriter_write_dtd_element($w, 'r', '(#PCDATA)')); xmlwriter_end_dtd($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <!DOCTYPE r [<!ELEMENT r (#PCDATA)>]>
/// ```
#[php_function]
fn xmlwriter_write_dtd_element(
    req: &mut Request,
    writer: Handle,
    name: Text,
    content: Text,
) -> Took {
    call(req, &writer, Op::WriteDtdElement(name, content))
}
/// `xmlwriter_start_dtd_attlist(XMLWriter $writer, string $name): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_dtd($w, 'r'); var_dump(xmlwriter_start_dtd_attlist($w, 'r')); xmlwriter_text($w, 'a CDATA #IMPLIED'); var_dump(xmlwriter_end_dtd_attlist($w)); xmlwriter_end_dtd($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!DOCTYPE r [<!ATTLIST r a CDATA #IMPLIED>]>
/// ```
#[php_function]
fn xmlwriter_start_dtd_attlist(req: &mut Request, writer: Handle, name: Text) -> Took {
    call(req, &writer, Op::StartDtdAttlist(name))
}
/// `xmlwriter_end_dtd_attlist(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_dtd_attlist($w));
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
fn xmlwriter_end_dtd_attlist(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndDtdAttlist)
}
/// `xmlwriter_write_dtd_attlist(XMLWriter $writer, string $name, string $content): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_dtd($w, 'r'); var_dump(xmlwriter_write_dtd_attlist($w, 'r', 'id ID #REQUIRED')); xmlwriter_end_dtd($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// <!DOCTYPE r [<!ATTLIST r id ID #REQUIRED>]>
/// ```
#[php_function]
fn xmlwriter_write_dtd_attlist(
    req: &mut Request,
    writer: Handle,
    name: Text,
    content: Text,
) -> Took {
    call(req, &writer, Op::WriteDtdAttlist(name, content))
}
/// `xmlwriter_start_dtd_entity(XMLWriter $writer, string $name, bool $isParam): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_dtd($w, 'r'); var_dump(xmlwriter_start_dtd_entity($w, 'e', true)); xmlwriter_text($w, 'v'); var_dump(xmlwriter_end_dtd_entity($w)); xmlwriter_end_dtd($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!DOCTYPE r [<!ENTITY % e "v">]>
/// ```
#[php_function]
fn xmlwriter_start_dtd_entity(
    req: &mut Request,
    writer: Handle,
    name: Text,
    isParam: bool,
) -> Took {
    call(req, &writer, Op::StartDtdEntity(name, isParam))
}
/// `xmlwriter_end_dtd_entity(XMLWriter $writer): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); var_dump(xmlwriter_end_dtd_entity($w));
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
fn xmlwriter_end_dtd_entity(req: &mut Request, writer: Handle) -> Took {
    call(req, &writer, Op::EndDtdEntity)
}
/// `xmlwriter_write_dtd_entity(XMLWriter $writer, string $name, string $content, bool $isParam = false, ?string $publicId = null, ?string $systemId = null, ?string $notationData = null): bool`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_start_dtd($w, 'r');
/// var_dump(xmlwriter_write_dtd_entity($w, 'e', 'v'), xmlwriter_write_dtd_entity($w, 'f', '', false, null, 'f.gif', 'gif')); xmlwriter_end_dtd($w);
/// echo xmlwriter_output_memory($w), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// <!DOCTYPE r [<!ENTITY e "v"><!ENTITY f SYSTEM "f.gif" NDATA gif>]>
/// ```
#[allow(clippy::too_many_arguments)]
#[php_function]
fn xmlwriter_write_dtd_entity(
    req: &mut Request,
    writer: Handle,
    name: Text,
    content: Text,
    #[php(default = false)] isParam: bool,
    #[php(default = null)] publicId: Nullable,
    #[php(default = null)] systemId: Nullable,
    #[php(default = null)] notationData: Nullable,
) -> Took {
    call(
        req,
        &writer,
        Op::WriteDtdEntity(name, content, isParam, publicId, systemId, notationData),
    )
}
/// `xmlwriter_output_memory(XMLWriter $writer, bool $flush = true): string`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_text($w, 'a');
/// var_dump(xmlwriter_output_memory($w, false), xmlwriter_output_memory($w), xmlwriter_output_memory($w));
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "a"
/// string(0) ""
/// ```
#[php_function]
fn xmlwriter_output_memory(
    req: &mut Request,
    writer: Instance<XMLWriter>,
    #[php(default = true)] flush: bool,
) -> Result<Owner<PhpString>, Thrown> {
    output_memory(req, writer.object(), flush)
}

/// `xmlwriter_flush(XMLWriter $writer, bool $empty = true): string|int`.
///
/// ```php
/// $w = xmlwriter_open_memory(); xmlwriter_text($w, 'ab'); var_dump(xmlwriter_flush($w, false), xmlwriter_flush($w));
/// $f = sys_get_temp_dir() . '/xmlwriter_flush.xml'; $u = xmlwriter_open_uri($f); xmlwriter_text($u, 'abc');
/// var_dump(xmlwriter_flush($u), file_get_contents($f)); unlink($f);
/// ```
/// ```expect
/// string(2) "ab"
/// string(2) "ab"
/// int(3)
/// string(3) "abc"
/// ```
#[php_function]
fn xmlwriter_flush(
    req: &mut Request,
    writer: Instance<XMLWriter>,
    #[php(default = true)] empty: bool,
) -> Result<Union<Owner<PhpString>, i64>, Thrown> {
    flush(req, writer.object(), empty, false)
}

#[php_class]
impl XMLWriter {
    /// `openUri(string $uri): bool`, `xmlwriter_open_uri` on the object.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/xmlwriter_openuri.xml';
    /// $w = new XMLWriter; var_dump($w->openUri($f)); $w->text('t'); $w->flush();
    /// echo file_get_contents($f), "\n"; unlink($f);
    /// var_dump($w->openUri('/nonexistent/dir/x.xml'));
    /// ```
    /// ```expect
    /// bool(true)
    /// t
    ///
    /// Warning: XMLWriter::openUri(): Unable to resolve file path in %s on line 5
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn openUri(req: &mut Request, this: This<Self>, uri: Path) -> Took {
        let Some(writer) = open_warned(req, uri)? else {
            return Ok(false);
        };
        replace(req, this.object(), writer);
        Ok(true)
    }

    /// `toUri(string $uri): static`.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/xmlwriter_touri.xml';
    /// class W extends XMLWriter {}
    /// $w = W::toUri($f); var_dump(get_class($w)); $w->text('t'); unset($w);
    /// echo file_get_contents($f), "\n"; unlink($f);
    /// try { XMLWriter::toUri('/nonexistent/dir/x.xml'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { XMLWriter::toUri(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "W"
    /// t
    /// XMLWriter::toUri(): Argument #1 ($uri) must resolve to a valid file path
    /// XMLWriter::toUri(): Argument #1 ($uri) must not be empty
    /// ```
    fn toUri(req: &mut Request, uri: Path) -> Result<Owner<Static>, Thrown> {
        if uri.0.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        match open_uri(req, uri.0.as_bytes())? {
            Opened::Unresolved => Err(arg_value_error(req, 1, "resolve to a valid file path")),
            Opened::Failed => Err(throw_error(req, "Error", UNCONSTRUCTED)),
            Opened::Emitter(writer) => create_static(req, writer),
        }
    }

    /// `openMemory(): bool`, `xmlwriter_open_memory` on the object.
    ///
    /// ```php
    /// $w = new XMLWriter; var_dump($w->openMemory()); $w->text('a'); var_dump($w->openMemory(), $w->outputMemory());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// string(0) ""
    /// ```
    #[php(tentative)]
    fn openMemory(req: &mut Request, this: This<Self>) -> bool {
        match Emitter::memory() {
            Some(writer) => {
                replace(req, this.object(), writer);
                true
            }
            None => false,
        }
    }

    /// `toMemory(): static`.
    ///
    /// ```php
    /// class W extends XMLWriter { function __construct() { echo "made\n"; } }
    /// $w = W::toMemory(); $w->text('a'); var_dump(get_class($w), $w->outputMemory());
    /// ```
    /// ```expect
    /// made
    /// string(1) "W"
    /// string(1) "a"
    /// ```
    fn toMemory(req: &mut Request) -> Result<Owner<Static>, Thrown> {
        match Emitter::memory() {
            Some(writer) => create_static(req, writer),
            None => Err(throw_error(req, "Error", UNCONSTRUCTED)),
        }
    }

    /// `toStream($stream): static`: a emitter over a stream it never
    /// closes.
    ///
    /// ```php
    /// $s = fopen('php://memory', 'w+'); $w = XMLWriter::toStream($s);
    /// $w->startElement('a'); $w->text('b'); $w->endElement(); var_dump($w->flush());
    /// rewind($s); var_dump(stream_get_contents($s));
    /// try { XMLWriter::toStream(false); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $kept = XMLWriter::toStream(fopen('php://memory', 'w+')); $kept->text('x'); var_dump($kept->flush());
    /// ```
    /// ```expect
    /// int(8)
    /// string(8) "<a>b</a>"
    /// XMLWriter::toStream(): Argument #1 ($stream) must be of type resource, false given
    /// int(1)
    /// ```
    fn toStream(req: &mut Request, stream: Resource) -> Result<Owner<Static>, Thrown> {
        let Some(stream) = Stream::from_value(req, &Value::resource(stream)) else {
            let message = "XMLWriter::toStream(): supplied resource is not a valid stream resource";
            return Err(type_error(req, message));
        };
        let (made, raised, thrown) = Emitter::stream(req, stream);
        Errors::settle(req, raised, thrown)?;
        match made {
            Some(writer) => create_static(req, writer),
            None => Err(throw_error(req, "Error", UNCONSTRUCTED)),
        }
    }

    #[php(tentative)]
    fn setIndent(req: &mut Request, this: This<Self>, enable: bool) -> Took {
        call(req, &this, Op::SetIndent(enable))
    }

    #[php(tentative)]
    fn setIndentString(req: &mut Request, this: This<Self>, indentation: Text) -> Took {
        call(req, &this, Op::SetIndentString(indentation))
    }

    #[php(tentative)]
    fn startComment(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::StartComment)
    }

    #[php(tentative)]
    fn endComment(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndComment)
    }

    #[php(tentative)]
    fn startAttribute(req: &mut Request, this: This<Self>, name: Text) -> Took {
        call(req, &this, Op::StartAttribute(name))
    }

    #[php(tentative)]
    fn endAttribute(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndAttribute)
    }

    #[php(tentative)]
    fn writeAttribute(req: &mut Request, this: This<Self>, name: Text, value: Text) -> Took {
        call(req, &this, Op::WriteAttribute(name, value))
    }

    #[php(tentative)]
    fn startAttributeNs(
        req: &mut Request,
        this: This<Self>,
        prefix: Nullable,
        name: Text,
        namespace: Nullable,
    ) -> Took {
        call(req, &this, Op::StartAttributeNs(prefix, name, namespace))
    }

    #[php(tentative)]
    fn writeAttributeNs(
        req: &mut Request,
        this: This<Self>,
        prefix: Nullable,
        name: Text,
        namespace: Nullable,
        value: Text,
    ) -> Took {
        call(
            req,
            &this,
            Op::WriteAttributeNs(prefix, name, namespace, value),
        )
    }

    #[php(tentative)]
    fn startElement(req: &mut Request, this: This<Self>, name: Text) -> Took {
        call(req, &this, Op::StartElement(name))
    }

    #[php(tentative)]
    fn endElement(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndElement)
    }

    #[php(tentative)]
    fn fullEndElement(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::FullEndElement)
    }

    #[php(tentative)]
    fn startElementNs(
        req: &mut Request,
        this: This<Self>,
        prefix: Nullable,
        name: Text,
        namespace: Nullable,
    ) -> Took {
        call(req, &this, Op::StartElementNs(prefix, name, namespace))
    }

    #[php(tentative)]
    fn writeElement(
        req: &mut Request,
        this: This<Self>,
        name: Text,
        #[php(default = null)] content: Nullable,
    ) -> Took {
        call(req, &this, Op::WriteElement(name, content))
    }

    #[php(tentative)]
    fn writeElementNs(
        req: &mut Request,
        this: This<Self>,
        prefix: Nullable,
        name: Text,
        namespace: Nullable,
        #[php(default = null)] content: Nullable,
    ) -> Took {
        call(
            req,
            &this,
            Op::WriteElementNs(prefix, name, namespace, content),
        )
    }

    #[php(tentative)]
    fn startPi(req: &mut Request, this: This<Self>, target: Text) -> Took {
        call(req, &this, Op::StartPi(target))
    }

    #[php(tentative)]
    fn endPi(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndPi)
    }

    #[php(tentative)]
    fn writePi(req: &mut Request, this: This<Self>, target: Text, content: Text) -> Took {
        call(req, &this, Op::WritePi(target, content))
    }

    #[php(tentative)]
    fn startCdata(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::StartCdata)
    }

    #[php(tentative)]
    fn endCdata(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndCdata)
    }

    #[php(tentative)]
    fn writeCdata(req: &mut Request, this: This<Self>, content: Text) -> Took {
        call(req, &this, Op::WriteCdata(content))
    }

    #[php(tentative)]
    fn text(req: &mut Request, this: This<Self>, content: Text) -> Took {
        call(req, &this, Op::Text(content))
    }

    #[php(tentative)]
    fn writeRaw(req: &mut Request, this: This<Self>, content: Text) -> Took {
        call(req, &this, Op::WriteRaw(content))
    }

    #[php(tentative)]
    fn startDocument(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "1.0")] version: Nullable,
        #[php(default = null)] encoding: Nullable,
        #[php(default = null)] standalone: Nullable,
    ) -> Took {
        call(req, &this, Op::StartDocument(version, encoding, standalone))
    }

    #[php(tentative)]
    fn endDocument(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndDocument)
    }

    #[php(tentative)]
    fn writeComment(req: &mut Request, this: This<Self>, content: Text) -> Took {
        call(req, &this, Op::WriteComment(content))
    }

    #[php(tentative)]
    fn startDtd(
        req: &mut Request,
        this: This<Self>,
        qualifiedName: Text,
        #[php(default = null)] publicId: Nullable,
        #[php(default = null)] systemId: Nullable,
    ) -> Took {
        call(req, &this, Op::StartDtd(qualifiedName, publicId, systemId))
    }

    #[php(tentative)]
    fn endDtd(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndDtd)
    }

    #[php(tentative)]
    fn writeDtd(
        req: &mut Request,
        this: This<Self>,
        name: Text,
        #[php(default = null)] publicId: Nullable,
        #[php(default = null)] systemId: Nullable,
        #[php(default = null)] content: Nullable,
    ) -> Took {
        call(req, &this, Op::WriteDtd(name, publicId, systemId, content))
    }

    #[php(tentative)]
    fn startDtdElement(req: &mut Request, this: This<Self>, qualifiedName: Text) -> Took {
        call(req, &this, Op::StartDtdElement(qualifiedName))
    }

    #[php(tentative)]
    fn endDtdElement(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndDtdElement)
    }

    #[php(tentative)]
    fn writeDtdElement(req: &mut Request, this: This<Self>, name: Text, content: Text) -> Took {
        call(req, &this, Op::WriteDtdElement(name, content))
    }

    #[php(tentative)]
    fn startDtdAttlist(req: &mut Request, this: This<Self>, name: Text) -> Took {
        call(req, &this, Op::StartDtdAttlist(name))
    }

    #[php(tentative)]
    fn endDtdAttlist(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndDtdAttlist)
    }

    #[php(tentative)]
    fn writeDtdAttlist(req: &mut Request, this: This<Self>, name: Text, content: Text) -> Took {
        call(req, &this, Op::WriteDtdAttlist(name, content))
    }

    #[php(tentative)]
    fn startDtdEntity(req: &mut Request, this: This<Self>, name: Text, isParam: bool) -> Took {
        call(req, &this, Op::StartDtdEntity(name, isParam))
    }

    #[php(tentative)]
    fn endDtdEntity(req: &mut Request, this: This<Self>) -> Took {
        call(req, &this, Op::EndDtdEntity)
    }

    #[allow(clippy::too_many_arguments)]
    #[php(tentative)]
    fn writeDtdEntity(
        req: &mut Request,
        this: This<Self>,
        name: Text,
        content: Text,
        #[php(default = false)] isParam: bool,
        #[php(default = null)] publicId: Nullable,
        #[php(default = null)] systemId: Nullable,
        #[php(default = null)] notationData: Nullable,
    ) -> Took {
        call(
            req,
            &this,
            Op::WriteDtdEntity(name, content, isParam, publicId, systemId, notationData),
        )
    }

    #[php(tentative)]
    fn outputMemory(
        req: &mut Request,
        this: This<Self>,
        #[php(default = true)] flush: bool,
    ) -> Result<Owner<PhpString>, Thrown> {
        output_memory(req, this.object(), flush)
    }

    #[php(tentative)]
    fn flush(
        req: &mut Request,
        this: This<Self>,
        #[php(default = true)] empty: bool,
    ) -> Result<Union<Owner<PhpString>, i64>, Thrown> {
        flush(req, this.object(), empty, false)
    }
}

/// What `zend_throw_error` says where libxml constructed nothing.
const UNCONSTRUCTED: &str = "Could not construct libxml writer";
