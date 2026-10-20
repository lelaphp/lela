//! The two halves php lays over libxml2's SAX2 push parser: `compat.c`,
//! the expat-compatible layer that turns each SAX callback into an expat
//! handler call or the default handler's text, and `xml.c`'s handlers,
//! which call the program's callables and build what
//! `xml_parse_into_struct` answers. Both are [`Compat`], the [`Sax`] one
//! parse is served through; the parser's state is read off the object
//! at each callback, since a program's handler may change it.

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::to_long;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{At, AttributeNs, Node, Sax};

use crate::{Hook, Parser, parser_mut};

/// `XML_MAXLEVEL`: the depth past which `xml_parse_into_struct` builds
/// nothing.
const MAX_LEVEL: i32 = 255;

/// `XML_ERROR_EXTERNAL_ENTITY_HANDLING`.
const EXTERNAL_ENTITY_HANDLING: i32 = 21;

/// libxml's `xmlEntityType`s `get_entity` tells apart.
const INTERNAL_GENERAL: i32 = 1;
const EXTERNAL_GENERAL_PARSED: i32 = 2;
const INTERNAL_PARAMETER: i32 = 4;
const INTERNAL_PREDEFINED: i32 = 6;

/// One parse of the parser `object` stands for.
pub(crate) struct Compat {
    pub(crate) object: PhpObject,
}

impl Compat {
    fn parser<'a>(&self, req: &'a mut Request) -> Option<&'a mut Parser> {
        parser_mut(req, self.object)
    }

    /// Whether `compat.c`'s `h_*` for `hook` is set: `XML_Set*Handler`
    /// was called once for it.
    fn registered(&self, req: &mut Request, hook: Hook) -> bool {
        self.parser(req).is_some_and(|p| p.registered.has(hook))
    }

    /// The position the parser stands at, kept on the object for the
    /// getters a handler calls while the parser is out of it.
    fn note(&self, req: &mut Request, at: &At) {
        if let Some(p) = self.parser(req) {
            p.engine.note(at.position());
        }
    }

    /// `qualify_namespace`: the URI, the separator's first byte and the
    /// name, or the name alone.
    fn qualify(&self, req: &mut Request, name: &[u8], uri: Option<&[u8]>) -> Vec<u8> {
        let Some(uri) = uri else {
            return name.to_vec();
        };
        let separator = self
            .parser(req)
            .and_then(|p| p.separator.as_ref()?.first().copied());
        let mut out = uri.to_vec();
        out.extend(separator);
        out.extend_from_slice(name);
        out
    }

    // ---- compat.c

    /// `parser->h_default(parser->user, text)`, where it is set.
    fn emit_default(&self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown> {
        if !self.registered(req, Hook::Default) {
            return Ok(());
        }
        self.default_handler(req, at, text)
    }

    /// `start_element_emit_default`.
    fn start_default(&self, req: &mut Request, at: &At) -> Result<(), Thrown> {
        if self.registered(req, Hook::Default) {
            let tag = at.start_tag();
            return self.default_handler(req, at, &tag);
        }
        Ok(())
    }

    /// `external_entity_ref_handler`: the handler's answer, and the parser
    /// stopped where it is falsy.
    fn external_entity_ref(
        &self,
        req: &mut Request,
        at: &At,
        names: &[u8],
        ids: [Option<&[u8]>; 2],
    ) -> Result<(), Thrown> {
        if !self.registered(req, Hook::ExternalEntityRef) {
            return Ok(());
        }
        if self.external_entity_ref_handler(req, at, names, ids)? == 0 {
            at.stop(EXTERNAL_ENTITY_HANDLING);
        }
        Ok(())
    }

    // ---- xml.c's handlers

    /// The program's handler for `hook`, served with the parser first and
    /// `arguments` after it, and its answer; nothing where none is set or
    /// a throwable is pending (`zend_call_known_fcc` under `EG(exception)`).
    fn serve(
        &self,
        req: &mut Request,
        at: &At,
        hook: Hook,
        arguments: Vec<Owner<Value>>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let callable: Option<Callable> = self
            .parser(req)
            .and_then(|p| p.handlers.get(hook))
            .map(|h| (*h.callable).clone());
        let Some(callable) = callable.filter(|_| !at.pending()) else {
            release_all(req, arguments);
            return Ok(None);
        };
        self.note(req, at);
        let mut all = Vec::with_capacity(arguments.len() + 1);
        all.push(self.object.as_value());
        all.extend(arguments.iter().map(|argument| **argument));
        let answered = call(req, &callable, &all);
        release_all(req, arguments);
        answered.map(Some)
    }

    /// `xml_decode_tag`: the target encoding's text, upper-cased where
    /// case folding is on.
    fn decode_tag(&self, req: &mut Request, tag: &[u8]) -> Vec<u8> {
        let Some(p) = self.parser(req) else {
            return tag.to_vec();
        };
        let mut out = p.options.target.decode(tag);
        if p.options.case_folding {
            out.make_ascii_uppercase();
        }
        out
    }

    fn decode(&self, req: &mut Request, text: &[u8]) -> Vec<u8> {
        match self.parser(req) {
            Some(p) => p.options.target.decode(text),
            None => text.to_vec(),
        }
    }

    /// `xml_xmlchar_zval`: the decoded text, `false` for none.
    fn text(&self, req: &mut Request, text: Option<&[u8]>) -> Result<Owner<Value>, Thrown> {
        match text {
            Some(t) => {
                let decoded = self.decode(req, t);
                Ok(PhpString::new(req, &decoded)?.into_value())
            }
            None => Ok(Owner::bool(false)),
        }
    }

    fn offset(&self, req: &mut Request) -> usize {
        self.parser(req).map_or(0, |p| p.options.skip_tagstart)
    }

    /// Whether `xml_parse_into_struct` is building: `parser->data` set and
    /// no throwable pending.
    fn building(&self, req: &mut Request, at: &At) -> bool {
        !at.pending() && self.parser(req).is_some_and(|p| p.into.is_some())
    }

    /// `xml_startElementHandler`.
    fn start(
        &self,
        req: &mut Request,
        at: &At,
        name: &[u8],
        attributes: &[(Vec<u8>, &[u8])],
    ) -> Result<(), Thrown> {
        let level = match self.parser(req) {
            Some(p) => {
                p.level += 1;
                p.level
            }
            None => return Ok(()),
        };
        let tag = self.decode_tag(req, name);
        let offset = self.offset(req);
        if self
            .parser(req)
            .is_some_and(|p| p.handlers.get(Hook::StartElement).is_some())
        {
            let stripped = PhpString::new(req, stripped(&tag, offset))?;
            let attrs = self.attributes(req, attributes)?;
            let answered = self.serve(
                req,
                at,
                Hook::StartElement,
                vec![stripped.into_value(), attrs.into_value()],
            )?;
            release_all(req, answered);
        }
        if !self.building(req, at) {
            return Ok(());
        }
        if level > MAX_LEVEL {
            if level == MAX_LEVEL + 1 {
                docref(
                    req,
                    Level::Warning,
                    "Maximum depth exceeded - Results truncated",
                );
            }
            return Ok(());
        }
        let stripped = stripped(&tag, offset).to_vec();
        self.add_to_info(req, &stripped)?;
        let mut entry = PhpArray::new(req)?;
        set_str(req, &mut entry, b"tag", &stripped)?;
        set_str(req, &mut entry, b"type", b"open")?;
        set(req, &mut entry, b"level", Owner::int(i64::from(level)))?;
        if let Some(p) = self.parser(req) {
            if let Some(slot) = usize::try_from(level - 1)
                .ok()
                .and_then(|at| p.ltags.get_mut(at))
            {
                *slot = Some(tag.clone());
            }
            p.lastwasopen = true;
        }
        if !attributes.is_empty() {
            let attrs = self.attributes(req, attributes)?;
            set(req, &mut entry, b"attributes", attrs.into_value())?;
        }
        let Some(mut data) = self.data(req)? else {
            entry.give_up(req);
            return Ok(());
        };
        if data.append(req, entry.into_value()).is_ok()
            && let Some(p) = self.parser(req)
        {
            p.ctag_index = data.next_free() - 1;
        }
        Ok(())
    }

    /// The attributes as the handler and the struct see them: each name
    /// decoded and folded, each value decoded, keyed as a symbol table.
    fn attributes(
        &self,
        req: &mut Request,
        attributes: &[(Vec<u8>, &[u8])],
    ) -> Result<Owner<PhpArray>, Thrown> {
        let mut array = PhpArray::new(req)?;
        for (name, value) in attributes {
            let key = self.decode_tag(req, name);
            let decoded = self.decode(req, value);
            let key = PhpString::new(req, &key)?;
            let key = Key::from_string(req, key);
            let value = PhpString::new(req, &decoded)?;
            array.set(req, key, value.into_value())?;
        }
        Ok(array)
    }

    /// `xml_endElementHandler`.
    fn end(&self, req: &mut Request, at: &At, name: &[u8]) -> Result<(), Thrown> {
        let tag = self.decode_tag(req, name);
        let offset = self.offset(req);
        if self
            .parser(req)
            .is_some_and(|p| p.handlers.get(Hook::EndElement).is_some())
        {
            let stripped = PhpString::new(req, stripped(&tag, offset))?;
            let answered = self.serve(req, at, Hook::EndElement, vec![stripped.into_value()])?;
            release_all(req, answered);
        }
        if self.building(req, at) {
            let (lastwasopen, level) = self
                .parser(req)
                .map_or((false, 0), |p| (p.lastwasopen, p.level));
            if lastwasopen {
                self.with_ctag(req, |req, ctag| set_str(req, ctag, b"type", b"complete"))?;
            } else {
                let stripped = stripped(&tag, offset).to_vec();
                self.add_to_info(req, &stripped)?;
                if let Some(mut data) = self.data(req)? {
                    let mut entry = PhpArray::new(req)?;
                    set_str(req, &mut entry, b"tag", &stripped)?;
                    set_str(req, &mut entry, b"type", b"close")?;
                    set(req, &mut entry, b"level", Owner::int(i64::from(level)))?;
                    data.append(req, entry.into_value())?;
                }
            }
            if let Some(p) = self.parser(req) {
                p.lastwasopen = false;
            }
        }
        if let Some(p) = self.parser(req) {
            if p.level <= MAX_LEVEL
                && let Some(slot) = usize::try_from(p.level - 1)
                    .ok()
                    .and_then(|at| p.ltags.get_mut(at))
            {
                *slot = None;
            }
            p.level -= 1;
        }
        Ok(())
    }

    /// `xml_characterDataHandler`.
    fn cdata(&self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown> {
        if self
            .parser(req)
            .is_some_and(|p| p.handlers.get(Hook::CharacterData).is_some())
        {
            let value = self.text(req, Some(text))?;
            let answered = self.serve(req, at, Hook::CharacterData, vec![value])?;
            release_all(req, answered);
        }
        if !self.building(req, at) {
            return Ok(());
        }
        let decoded = self.decode(req, text);
        let (skipwhite, lastwasopen, level) = self.parser(req).map_or((false, false, 0), |p| {
            (p.options.skip_white, p.lastwasopen, p.level)
        });
        let doprint = skipwhite && decoded.iter().any(|b| !matches!(b, b' ' | b'\t' | b'\n'));
        let keep = doprint || !skipwhite;
        if lastwasopen {
            return self.with_ctag(req, |req, ctag| {
                if let Some(held) = string_at(req, **ctag, b"value")? {
                    let mut joined = held;
                    joined.extend_from_slice(&decoded);
                    set_str(req, ctag, b"value", &joined)
                } else if keep {
                    set_str(req, ctag, b"value", &decoded)
                } else {
                    Ok(())
                }
            });
        }
        let Some(mut data) = self.data(req)? else {
            return Ok(());
        };
        // The last element, where it is a `cdata` entry with a value, takes
        // the text.
        if let Some((key, last)) = last(data)
            && let Some(entry) = last.deref().as_array()
            && string_at(req, entry, b"type")?.as_deref() == Some(b"cdata")
            && let Some(held) = string_at(req, entry, b"value")?
        {
            let mut joined = held;
            joined.extend_from_slice(&decoded);
            let mut entry = separate_owned(req, entry)?;
            set_str(req, &mut entry, b"value", &joined)?;
            return data.set(req, Owner::take(req, key), entry.into_value());
        }
        if level > 0 && level <= MAX_LEVEL && keep {
            let tag = self
                .parser(req)
                .and_then(|p| p.ltags.get(usize::try_from(level - 1).ok()?)?.clone())
                .unwrap_or_default();
            let offset = self.offset(req);
            let stripped = stripped(&tag, offset).to_vec();
            self.add_to_info(req, &stripped)?;
            let mut entry = PhpArray::new(req)?;
            set_str(req, &mut entry, b"tag", &stripped)?;
            set_str(req, &mut entry, b"value", &decoded)?;
            set_str(req, &mut entry, b"type", b"cdata")?;
            set(req, &mut entry, b"level", Owner::int(i64::from(level)))?;
            data.append(req, entry.into_value())?;
        } else if level == MAX_LEVEL + 1 {
            docref(
                req,
                Level::Warning,
                "Maximum depth exceeded - Results truncated",
            );
        }
        Ok(())
    }

    /// `xml_defaultHandler`.
    fn default_handler(&self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown> {
        let value = self.text(req, Some(text))?;
        let answered = self.serve(req, at, Hook::Default, vec![value])?;
        release_all(req, answered);
        Ok(())
    }

    /// `xml_externalEntityRefHandler`: the handler's answer as an int, `0`
    /// where none is set or it answered nothing.
    fn external_entity_ref_handler(
        &self,
        req: &mut Request,
        at: &At,
        names: &[u8],
        [system_id, public_id]: [Option<&[u8]>; 2],
    ) -> Result<i64, Thrown> {
        let arguments = vec![
            self.text(req, Some(names))?,
            self.text(req, Some(b""))?,
            self.text(req, system_id)?,
            self.text(req, public_id)?,
        ];
        match self.serve(req, at, Hook::ExternalEntityRef, arguments)? {
            Some(answer) => {
                let n = to_long(req, &answer);
                answer.give_up(req);
                n
            }
            None => Ok(0),
        }
    }

    /// A handler of plain text arguments served, `xml_processingInstructionHandler`
    /// and its kin.
    fn plain(
        &self,
        req: &mut Request,
        at: &At,
        hook: Hook,
        texts: &[Option<&[u8]>],
    ) -> Result<(), Thrown> {
        if self
            .parser(req)
            .is_none_or(|p| p.handlers.get(hook).is_none())
            || at.pending()
        {
            return Ok(());
        }
        let mut arguments = Vec::with_capacity(texts.len());
        for t in texts {
            arguments.push(self.text(req, *t)?);
        }
        let answered = self.serve(req, at, hook, arguments)?;
        release_all(req, answered);
        Ok(())
    }

    /// `xml_add_to_info`: the tag's list in `$index` takes the entry's
    /// number.
    fn add_to_info(&self, req: &mut Request, name: &[u8]) -> Result<(), Thrown> {
        let Some((info, curtag)) = self.parser(req).and_then(|p| {
            let info = p.into.as_ref()?.info.as_deref().copied()?;
            let n = p.curtag;
            Some((info, n))
        }) else {
            return Ok(());
        };
        let Some(mut index) = info.separate_array(req)? else {
            return Ok(());
        };
        let mut list = match index.find(name).and_then(|v| v.deref().as_array()) {
            Some(list) => separate_owned(req, list)?,
            None => PhpArray::new(req)?,
        };
        list.append(req, Owner::int(i64::from(curtag)))?;
        index.set_str(req, name, list.into_value())?;
        if let Some(p) = self.parser(req) {
            p.curtag += 1;
        }
        Ok(())
    }

    /// `xml_get_separated_data`: `$values` separated, `None` where it holds
    /// no array.
    fn data(&self, req: &mut Request) -> Result<Option<PhpArray>, Thrown> {
        let Some(data) = self.parser(req).and_then(|p| Some(*p.into.as_ref()?.data)) else {
            return Ok(None);
        };
        data.separate_array(req)
    }

    /// `xml_get_ctag`: the entry `ctag_index` names, changed by `f` and
    /// written back.
    fn with_ctag(
        &self,
        req: &mut Request,
        f: impl FnOnce(&mut Request, &mut Owner<PhpArray>) -> Result<(), Thrown>,
    ) -> Result<(), Thrown> {
        let Some(mut data) = self.data(req)? else {
            return Ok(());
        };
        let index = self.parser(req).map_or(0, |p| p.ctag_index);
        let key = Key::Int(index);
        let Some(entry) = data.get(&key).and_then(|v| v.deref().as_array()) else {
            return Ok(());
        };
        let mut entry = separate_owned(req, entry)?;
        f(req, &mut entry)?;
        data.set(req, Owner::take(req, key), entry.into_value())
    }
}

/// `xml_stripped_tag`: the tag past `offset` bytes, empty where it is no
/// longer than that.
fn stripped(tag: &[u8], offset: usize) -> &[u8] {
    tag.get(offset..).unwrap_or_default()
}

/// A string element of `array`.
fn string_at(req: &mut Request, array: PhpArray, name: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let key = PhpString::new(req, name)?;
    let found = match array.get(&Key::Str(*key)).map(|v| v.deref()) {
        Some(v) => match v.as_ref() {
            ValueRef::Str(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        },
        None => None,
    };
    key.give_up(req);
    Ok(found)
}

/// The last element of `array`, `ZEND_HASH_REVERSE_FOREACH`'s first: at a
/// bucket no earlier than the count less one, the elements before it
/// taking a bucket each.
fn last(array: PhpArray) -> Option<(Key, Value)> {
    let start = u32::try_from(array.len().checked_sub(1)?).ok()?;
    let (mut at, mut key, mut value) = array.live_from(start)?;
    while let Some((next, k, v)) = array.live_from(at.checked_add(1)?) {
        (at, key, value) = (next, k, v);
    }
    Some((key, value))
}

fn set_str(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    text: &[u8],
) -> Result<(), Thrown> {
    let value = PhpString::new(req, text)?;
    set(req, array, name, value.into_value())
}

fn set(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name, value)
}

/// An array read out of another taken as one owner of its own and
/// separated, so writing it changes only the copy written back.
fn separate_owned(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    let mut owned = Owner::take(req, array);
    owned.separate(req)?;
    Ok(owned)
}

fn release_all(req: &mut Request, values: impl IntoIterator<Item = Owner<Value>>) {
    for v in values {
        v.give_up(req);
    }
}

impl Sax for Compat {
    fn start_element(
        &mut self,
        req: &mut Request,
        at: &At,
        name: &[u8],
        attributes: &[(&[u8], &[u8])],
    ) -> Result<(), Thrown> {
        if !self.registered(req, Hook::StartElement) {
            return self.start_default(req, at);
        }
        let attrs: Vec<(Vec<u8>, &[u8])> =
            attributes.iter().map(|(n, v)| (n.to_vec(), *v)).collect();
        self.start(req, at, name, &attrs)
    }

    fn start_element_ns(
        &mut self,
        req: &mut Request,
        at: &At,
        tag: AttributeNs<'_>,
        namespaces: &[(Option<&[u8]>, &[u8])],
        attributes: &[AttributeNs<'_>],
    ) -> Result<(), Thrown> {
        if !namespaces.is_empty() && self.registered(req, Hook::StartNamespaceDecl) {
            for (prefix, uri) in namespaces {
                self.plain(req, at, Hook::StartNamespaceDecl, &[*prefix, Some(uri)])?;
            }
        }
        if !self.registered(req, Hook::StartElement) {
            return self.start_default(req, at);
        }
        let name = self.qualify(req, tag.local, tag.uri);
        let mut attrs = Vec::with_capacity(attributes.len());
        for a in attributes {
            let n = match a.prefix {
                Some(_) => self.qualify(req, a.local, a.uri),
                None => a.local.to_vec(),
            };
            attrs.push((n, a.value));
        }
        self.start(req, at, &name, &attrs)
    }

    fn end_element(&mut self, req: &mut Request, at: &At, name: &[u8]) -> Result<(), Thrown> {
        if !self.registered(req, Hook::StartElement) {
            let text = [b"</".as_slice(), name, b">"].concat();
            return self.emit_default(req, at, &text);
        }
        self.end(req, at, name)
    }

    fn end_element_ns(
        &mut self,
        req: &mut Request,
        at: &At,
        tag: AttributeNs<'_>,
    ) -> Result<(), Thrown> {
        if !self.registered(req, Hook::StartElement) {
            let text = match tag.prefix {
                Some(p) => [b"</".as_slice(), p, b":", tag.local, b">"].concat(),
                None => [b"</".as_slice(), tag.local, b">"].concat(),
            };
            return self.emit_default(req, at, &text);
        }
        let name = self.qualify(req, tag.local, tag.uri);
        self.end(req, at, &name)
    }

    fn characters(&mut self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown> {
        if !self.registered(req, Hook::CharacterData) {
            return self.emit_default(req, at, text);
        }
        self.cdata(req, at, text)
    }

    fn processing_instruction(
        &mut self,
        req: &mut Request,
        at: &At,
        target: &[u8],
        data: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        if !self.registered(req, Hook::ProcessingInstruction) {
            // `spprintf("<?%s %s?>")`, which prints a null string as `(null)`.
            let text = [
                b"<?".as_slice(),
                target,
                b" ",
                data.unwrap_or(b"(null)"),
                b"?>",
            ]
            .concat();
            return self.emit_default(req, at, &text);
        }
        self.plain(req, at, Hook::ProcessingInstruction, &[Some(target), data])
    }

    fn comment(&mut self, req: &mut Request, at: &At, text: &[u8]) -> Result<(), Thrown> {
        let text = [b"<!--".as_slice(), text, b"-->"].concat();
        self.emit_default(req, at, &text)
    }

    fn unparsed_entity_decl(
        &mut self,
        req: &mut Request,
        at: &At,
        [name, public_id, system_id, notation]: [Option<&[u8]>; 4],
    ) -> Result<(), Thrown> {
        if !self.registered(req, Hook::UnparsedEntityDecl) {
            return Ok(());
        }
        let texts = [name, None, system_id, public_id, notation];
        self.plain(req, at, Hook::UnparsedEntityDecl, &texts)
    }

    fn notation_decl(
        &mut self,
        req: &mut Request,
        at: &At,
        [name, public_id, system_id]: [Option<&[u8]>; 3],
    ) -> Result<(), Thrown> {
        if !self.registered(req, Hook::NotationDecl) {
            return Ok(());
        }
        self.plain(
            req,
            at,
            Hook::NotationDecl,
            &[name, None, system_id, public_id],
        )
    }

    fn entity(
        &mut self,
        req: &mut Request,
        at: &At,
        name: &[u8],
        found: Option<Node>,
    ) -> Result<(), Thrown> {
        if found.is_some() && !at.in_content() {
            return Ok(());
        }
        let kind = found.map(Node::entity_type);
        match kind {
            None | Some(INTERNAL_GENERAL | INTERNAL_PARAMETER | INTERNAL_PREDEFINED) => {
                let cdata = self.registered(req, Hook::CharacterData);
                let predefined_to_cdata = kind == Some(INTERNAL_PREDEFINED) && cdata;
                if self.registered(req, Hook::Default) && !predefined_to_cdata {
                    let text = [b"&".as_slice(), name, b";"].concat();
                    return self.default_handler(req, at, &text);
                }
                if let (true, Some(entity)) = (cdata, found) {
                    let content = entity.entity_content().unwrap_or_default();
                    return self.cdata(req, at, &content);
                }
                Ok(())
            }
            Some(EXTERNAL_GENERAL_PARSED) => {
                let Some(entity) = found else {
                    return Ok(());
                };
                let (system_id, public_id) =
                    (entity.entity_system_id(), entity.entity_external_id());
                let names = entity.name();
                self.external_entity_ref(
                    req,
                    at,
                    &names,
                    [system_id.as_deref(), public_id.as_deref()],
                )
            }
            Some(_) => Ok(()),
        }
    }
}

/// `$values` and `$index` as `xml_parse_into_struct` keeps them: the
/// caller's references, an owner of each.
pub(crate) struct Collected {
    pub(crate) data: Owner<PhpRef>,
    pub(crate) info: Option<Owner<PhpRef>>,
}

impl Collected {
    pub(crate) fn release(self, req: &mut Request) {
        self.data.give_up(req);
        if let Some(info) = self.info {
            info.give_up(req);
        }
    }

    /// Both references, named to the cycle collector.
    pub(crate) fn visit(&self, visit: &mut dyn FnMut(Value)) {
        visit(self.data.as_value());
        if let Some(info) = &self.info {
            visit(info.as_value());
        }
    }
}
