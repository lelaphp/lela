//! Bounded identity/claim experiment for #2829, not a runtime implementation.
//! The approved test surface is #2824's identity/reclamation contract. PHP GC
//! observations are separately captured through the pinned oracle; this model
//! contains no PHP owners. All allocations here are actual system allocations.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Name {
    request: u64,
    slot: usize,
    generation: u8,
}

impl Name {
    fn new(request: u64, slot: usize, generation: u8) -> Self {
        Self {
            request,
            slot,
            generation,
        }
    }
}

#[derive(Clone, Copy)]
enum Claim {
    Document,
    Dom,
    SimpleXml,
    Descendant,
    Attribute,
    Template,
    Declaration,
    Job,
}

impl Claim {
    fn index(self) -> usize {
        match self {
            Self::Document => 0,
            Self::Dom => 1,
            Self::SimpleXml => 2,
            Self::Descendant => 3,
            Self::Attribute => 4,
            Self::Template => 5,
            Self::Declaration => 6,
            Self::Job => 7,
        }
    }
}

#[derive(Clone, Default)]
struct Record {
    document: Option<Name>,
    parent: Option<Name>,
    children: Vec<Name>,
    attributes: Vec<Name>,
    template: Option<Name>,
    entity: Option<Name>,   // a declaration reference, NOT an owned child
    canonical: Option<u64>, // weak PHP object identity, NOT a PHP owner
    claims: [u32; 8],
    text: Vec<u8>,
}

struct Slot {
    generation: u8,
    record: Option<Box<Record>>,
}

struct Store {
    request: u64, // supplied once by the host, never reused during the process
    slots: Vec<Slot>,
    freed: Vec<Name>,
}

impl Store {
    fn new(request: u64) -> Self {
        Self {
            request,
            slots: Vec::new(),
            freed: Vec::new(),
        }
    }

    fn read(&self, name: Name) -> Option<&Record> {
        if name.request != self.request {
            return None;
        }
        let slot = self.slots.get(name.slot)?;
        if slot.generation != name.generation {
            return None;
        }
        slot.record.as_deref()
    }

    fn write(&mut self, name: Name) -> &mut Record {
        assert!(self.read(name).is_some(), "invalid name");
        self.slots[name.slot]
            .record
            .as_deref_mut()
            .expect("validated above")
    }

    fn make(&mut self, text: &[u8], document: Option<Name>) -> Name {
        assert!(document.is_none_or(|d| self.read(d).is_some()));
        let reusable = self
            .slots
            .iter()
            .position(|s| s.record.is_none() && s.generation < u8::MAX);
        let slot = reusable.unwrap_or(self.slots.len());
        if slot == self.slots.len() {
            self.slots.push(Slot {
                generation: 0,
                record: None,
            });
        } else {
            self.slots[slot].generation += 1;
        }
        let name = Name::new(self.request, slot, self.slots[slot].generation);
        self.slots[slot].record = Some(Box::new(Record {
            text: text.to_vec(),
            document,
            ..Record::default()
        }));
        name
    }

    fn claim(&mut self, name: Name, claim: Claim) {
        let count = &mut self.write(name).claims[claim.index()];
        *count = count
            .checked_add(1)
            .expect("claim exhaustion must refuse, never wrap");
    }

    fn release(&mut self, name: Name, claim: Claim) {
        let count = &mut self.write(name).claims[claim.index()];
        *count = count.checked_sub(1).expect("unbalanced claim");
        self.reclaim();
    }

    fn canonical(&mut self, name: Name, object: u64) -> u64 {
        *self.write(name).canonical.get_or_insert(object)
    }

    fn owned(&self, name: Name) -> Vec<Name> {
        let r = self.read(name).expect("live record");
        r.children
            .iter()
            .chain(&r.attributes)
            .copied()
            .chain(r.template)
            .collect()
    }

    fn contains(&self, ancestor: Name, wanted: Name) -> bool {
        ancestor == wanted
            || self
                .owned(ancestor)
                .iter()
                .any(|n| self.contains(*n, wanted))
    }

    fn unlink(&mut self, name: Name) {
        if let Some(parent) = self.read(name).expect("live").parent {
            let p = self.write(parent);
            p.children.retain(|n| *n != name);
            p.attributes.retain(|n| *n != name);
            if p.template == Some(name) {
                p.template = None;
            }
            self.write(name).parent = None;
        }
    }

    fn attach(&mut self, parent: Name, child: Name) -> bool {
        if self.contains(child, parent) {
            return false;
        }
        self.unlink(child);
        self.write(parent).children.push(child);
        self.write(child).parent = Some(parent);
        true
    }

    fn adopt(&mut self, name: Name, document: Name) {
        self.unlink(name);
        let mut todo = vec![name];
        while let Some(n) = todo.pop() {
            todo.extend(self.owned(n));
            self.write(n).document = Some(document);
        }
        self.reclaim();
    }

    fn copy(&mut self, name: Name, document: Name) -> Name {
        let source = self.read(name).expect("live source").clone();
        let made = self.make(&source.text, Some(document));
        for child in source.children {
            let copied = self.copy(child, document);
            assert!(self.attach(made, copied));
        }
        for attribute in source.attributes {
            let copied = self.copy(attribute, document);
            self.write(made).attributes.push(copied);
            self.write(copied).parent = Some(made);
        }
        if let Some(template) = source.template {
            let copied = self.copy(template, document);
            self.write(made).template = Some(copied);
            self.write(copied).parent = Some(made);
        }
        self.write(made).entity = source.entity;
        made
    }

    fn reclaim(&mut self) {
        let mut todo = Vec::new();
        for (slot, s) in self.slots.iter().enumerate() {
            if s.record
                .as_ref()
                .is_some_and(|r| r.claims.iter().any(|n| *n != 0))
            {
                todo.push(Name::new(self.request, slot, s.generation));
            }
        }
        let mut kept = BTreeSet::new();
        while let Some(n) = todo.pop() {
            if kept.insert(n) {
                let r = self.read(n).expect("owned edge must resolve");
                todo.extend(self.owned(n));
                todo.extend(r.document); // storage/dictionary claim, not a parent claim
            }
        }
        for (slot, s) in self.slots.iter_mut().enumerate() {
            let name = Name::new(self.request, slot, s.generation);
            if !kept.contains(&name) && s.record.take().is_some() {
                self.freed.push(name); // Box and its real text/edge buffers dropped here
            }
        }
        for s in &mut self.slots {
            if let Some(r) = s.record.as_mut()
                && r.parent.is_some_and(|p| !kept.contains(&p))
            {
                r.parent = None;
            }
        }
    }

    fn teardown(mut self) -> Vec<Name> {
        for slot in &mut self.slots {
            if let Some(r) = slot.record.as_mut() {
                r.claims = [0; 8];
            }
        }
        self.reclaim();
        assert!(self.slots.iter().all(|s| s.record.is_none()));
        self.freed
    }
}

#[test]
fn names_survive_adoption_but_copies_and_requests_are_distinct() {
    let mut s = Store::new(1);
    let old = s.make(b"document", None);
    s.claim(old, Claim::Document);
    let new = s.make(b"document", None);
    s.claim(new, Claim::Document);
    let node = s.make(b"element", Some(old));
    s.claim(node, Claim::Dom);
    assert!(s.attach(old, node));
    let attr = s.make(b"attribute", Some(old));
    s.write(node).attributes.push(attr);
    s.write(attr).parent = Some(node);
    s.claim(attr, Claim::Attribute);
    assert_eq!(s.canonical(node, 4), s.canonical(node, 5));
    s.unlink(node);
    assert!(s.read(node).is_some());
    s.adopt(node, new);
    assert_eq!(s.read(attr).expect("held attribute").document, Some(new));
    s.release(old, Claim::Document);
    assert!(s.read(old).is_none());
    let imported = s.copy(node, new);
    let cloned = s.copy(node, new);
    assert_ne!(imported, node);
    assert_ne!(cloned, imported);
    assert_eq!(s.read(imported).expect("copy").canonical, None);
    let mut other = Store::new(2);
    other.make(b"document", None);
    assert!(other.read(node).is_none());
    s.release(node, Claim::Dom);
    assert!(s.read(node).is_none()); // attribute does not retain its detached ancestor
    assert!(s.read(attr).is_some());
    s.release(attr, Claim::Attribute);
    assert!(s.read(attr).is_none());
    assert!(s.read(imported).is_none()); // copying a name did not claim it
    s.release(new, Claim::Document);
    assert!(s.slots.iter().all(|n| n.record.is_none()));
}

#[test]
fn independent_claims_templates_and_entity_references_reclaim_promptly() {
    let mut s = Store::new(9);
    let doc = s.make(b"document", None);
    s.claim(doc, Claim::Document);
    let ancestor = s.make(b"detached", Some(doc));
    let child = s.make(b"descendant", Some(doc));
    assert!(s.attach(ancestor, child));
    s.claim(child, Claim::Descendant);
    s.claim(child, Claim::SimpleXml);
    s.claim(child, Claim::SimpleXml);
    s.claim(child, Claim::Job);
    s.reclaim();
    assert!(s.read(ancestor).is_none());
    let template = s.make(b"template", Some(doc));
    let fragment = s.make(b"fragment", Some(doc));
    s.write(template).template = Some(fragment);
    s.write(fragment).parent = Some(template);
    s.claim(template, Claim::Template);
    let declaration = s.make(b"entity declaration", Some(doc));
    s.claim(declaration, Claim::Declaration);
    let reference = s.make(b"entity reference", Some(doc));
    s.write(reference).entity = Some(declaration);
    s.claim(reference, Claim::Dom);
    s.release(reference, Claim::Dom);
    assert!(s.read(declaration).is_some()); // reference never freed its declaration
    s.release(template, Claim::Template);
    assert!(s.read(fragment).is_none());
    s.release(child, Claim::Descendant);
    s.release(child, Claim::SimpleXml);
    s.release(child, Claim::SimpleXml);
    assert!(s.read(child).is_some());
    s.release(child, Claim::Job);
    assert!(s.read(child).is_none());
    s.release(declaration, Claim::Declaration);
    s.release(doc, Claim::Document);
    assert_eq!(s.freed.len(), 7);
}

#[test]
fn generation_exhaustion_retires_slots_instead_of_resurrecting_names() {
    let mut s = Store::new(7);
    let first = s.make(b"node", None);
    for _ in 0..=u8::MAX {
        s.reclaim();
        let fresh = s.make(b"node", None);
        assert_ne!(first, fresh);
        assert!(s.read(first).is_none());
    }
    assert_eq!(s.slots[0].generation, u8::MAX);
    assert!(s.slots[0].record.is_none());
    assert_eq!(s.slots.len(), 2);
    s.teardown();
}

#[test]
fn cycles_are_rejected_and_teardown_ignores_surviving_claims() {
    let mut s = Store::new(10);
    let doc = s.make(b"document", None);
    let child = s.make(b"child", Some(doc));
    s.claim(doc, Claim::Document);
    assert!(s.attach(doc, child));
    assert!(!s.attach(child, doc));
    s.claim(child, Claim::Job);
    assert_eq!(s.teardown().len(), 2);
}

#[test]
fn reusing_request_provenance_is_a_falsified_proposal() {
    let mut first = Store::new(12);
    let stale = first.make(b"old request", None);
    first.teardown();
    let mut second = Store::new(12);
    let resurrected = second.make(b"new request", None);
    assert_eq!(stale, resurrected);
    assert!(second.read(stale).is_some());
    // Therefore caller-chosen/reused request numbers are rejected. A real
    // issuer must checked-increment and refuse at exhaustion, never wrap.
    second.teardown();
}

#[test]
fn copying_entity_names_without_rebinding_is_a_falsified_proposal() {
    let mut s = Store::new(13);
    let source = s.make(b"source", None);
    s.claim(source, Claim::Document);
    let target = s.make(b"target", None);
    s.claim(target, Claim::Document);
    let declaration = s.make(b"declaration", Some(source));
    assert!(s.attach(source, declaration));
    let reference = s.make(b"reference", Some(source));
    s.write(reference).entity = Some(declaration);
    let copied = s.copy(reference, target);
    s.claim(copied, Claim::Dom);
    s.release(source, Claim::Document);
    let weak = s
        .read(copied)
        .expect("held copy")
        .entity
        .expect("entity name");
    assert!(s.read(weak).is_none());
    // Rebinding dictionaries/entities/namespaces is necessary. This mechanical
    // copy operation is NOT a complete PHP import implementation.
    s.teardown();
}

#[test]
fn representative_records_have_actual_storage_and_partial_failure_cleanup() {
    use std::mem::{align_of, size_of};
    assert!(align_of::<Name>() <= 8);
    assert!(align_of::<Record>() <= 8);
    // Current claim allocations are 24 and 48 bytes: inline this real record
    // into either would NOT fit. Emalloc bytes do not authorize typed writes.
    assert!(size_of::<Record>() > 48);
    println!(
        "Name={} align={} Record={} align={} Slot={}",
        size_of::<Name>(),
        align_of::<Name>(),
        size_of::<Record>(),
        align_of::<Record>(),
        size_of::<Slot>()
    );
    for refuse_at in 0..4 {
        let mut s = Store::new(11);
        for step in 0..4 {
            if step == refuse_at {
                break;
            } // deterministic allocation refusal
            s.make(&[b'x'; 128], None);
        }
        assert_eq!(s.teardown().len(), refuse_at);
    }
}
