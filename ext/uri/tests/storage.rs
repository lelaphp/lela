//! A non-shipped storage experiment at the existing native face and Capture
//! seams (#2832). It stores the actual component bytes on the request heap.
//! It is not a parser or a proposed public allocator/URL representation.
#![deny(unsafe_code)]

use std::ops::Range;

use php_ext::abi::{Callable, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::test::{Capture, Engine, LeakSensor};
use php_ext::values::{Emalloc, Owner, PhpString, Value};

struct Bytes {
    allocation: Emalloc,
    component: Range<usize>,
}

impl Bytes {
    fn new(req: &mut Request, content: &[u8]) -> Result<Self, Thrown> {
        let mut allocation = Emalloc::new(req, content.len())?;
        allocation.bytes_mut().copy_from_slice(content);
        Ok(Self {
            allocation,
            component: 0..content.len(),
        })
    }

    fn range(&self, start: usize, len: usize) -> Option<&[u8]> {
        self.allocation.bytes().get(start..start.checked_add(len)?)
    }

    fn read(&self) -> Option<&[u8]> {
        self.range(
            self.component.start,
            self.component.end.checked_sub(self.component.start)?,
        )
    }

    fn free(self, req: &mut Request) {
        self.allocation.free(req);
    }
}

struct RawAndNormalized {
    raw: Bytes,
    normalized: Option<Bytes>,
}

impl RawAndNormalized {
    fn prepare(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.normalized.is_none() {
            // Content pinned by semantics.php. No normalization algorithm here.
            self.normalized = Some(Bytes::new(req, b"example.com")?);
        }
        Ok(())
    }

    fn duplicate(&self, req: &mut Request) -> Result<Self, Thrown> {
        Ok(Self {
            raw: Bytes::new(req, self.raw.read().unwrap_or_default())?,
            normalized: None,
        })
    }

    fn free(self, req: &mut Request) {
        self.raw.free(req);
        if let Some(bytes) = self.normalized {
            bytes.free(req);
        }
    }
}

#[php_function]
fn storage_probe(req: &mut Request, callback: Callable) -> Result<Owner<PhpString>, Thrown> {
    let before = req.memory_usage(false);
    let mut value = RawAndNormalized {
        raw: Bytes::new(req, b"EXAMPLE.COM")?,
        normalized: None,
    };
    let raw = req.memory_usage(false) - before;
    let checks = value.raw.range(usize::MAX, 1).is_none()
        && (value.raw.allocation.bytes().as_ptr() as usize)
            .is_multiple_of(std::mem::align_of::<u64>())
        && value.raw.range(12, 0).is_none()
        && value.raw.range(11, 0) == Some(b"".as_slice())
        && value.raw.read() == Some(b"EXAMPLE.COM".as_slice());
    let prepared = (|| {
        value.prepare(req)?;
        let normalized = req.memory_usage(false) - before;
        value.prepare(req)?;
        let again = req.memory_usage(false) - before;
        let copy = value.duplicate(req)?;
        let clone_has_no_normalized = copy.normalized.is_none();
        copy.free(req);
        // End the component borrow by creating a genuine PHP string owner
        // before callback/re-entry, with no moving-buffer pointer/handle table.
        let context = PhpString::new(req, value.raw.read().unwrap_or_default())?;
        Ok::<_, Thrown>((context, normalized, again, clone_has_no_normalized))
    })();
    let before_free = req.memory_usage(false);
    value.free(req);
    let (context, normalized, again, clone_has_no_normalized) = prepared?;
    let freed = before_free - req.memory_usage(false) == again;
    let returned = call(req, &callback, &[Value::string(*context)]);
    context.give_up(req);
    let returned = returned?;
    returned.give_up(req);
    PhpString::new(
        req,
        format!("{raw} {normalized} {again} {checks} {clone_has_no_normalized} {freed}").as_bytes(),
    )
}

#[php_function]
fn storage_failure(req: &mut Request, exhaust: bool) -> Result<(), Thrown> {
    let first = Bytes::new(req, b"initialized")?;
    // A later allocation failure must give back the initialized prefix. A
    // semantic refusal does too. This runs only in the isolated proof extension.
    let failure = if exhaust {
        match Emalloc::new(req, 256 * 1024 * 1024) {
            Ok(block) => {
                block.free(req);
                None
            }
            Err(thrown) => Some(thrown),
        }
    } else {
        Some(throw(req, "Exception", "storage refused"))
    };
    first.free(req);
    match failure {
        Some(thrown) => Err(thrown),
        None => Ok(()),
    }
}

#[php_function]
fn retained_pool_probe(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let before = req.memory_usage(false);
    let mut pool = Emalloc::new(req, 8192)?;
    pool.bytes_mut()[..11].copy_from_slice(b"example.com");
    let held = req.memory_usage(false) - before;
    // Moving the owner never stores pointers into its own Rust value.
    let pool = pool;
    let valid = pool.bytes().get(..11) == Some(b"example.com".as_slice());
    // Reuse cleared space, not a dummy compatibility charge beside a Vec.
    let mut pool = pool;
    pool.bytes_mut()[..11].copy_from_slice(b"another.com");
    let retained = req.memory_usage(false) - before;
    pool.free(req);
    let freed = req.memory_usage(false) == before;
    PhpString::new(req, format!("{held} {retained} {valid} {freed}").as_bytes())
}

#[php_function]
fn consumer_storage_probe(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let before = req.memory_usage(false);
    let fields = php_ext_uri::Parser::Rfc3986.parse_to_struct(
        req,
        b"https://example.com/a?b#c",
        php_ext_uri::ReadAs::NormalizedAscii,
        false,
    )?;
    let retained = req.memory_usage(false) - before;
    let actual_contents = fields.is_some_and(|fields| {
        fields.host.as_deref() == Some(b"example.com") && fields.path.as_deref() == Some(b"/a")
    });
    // The existing Vec-backed consumer answer retains real bytes, but none on
    // the request heap. This is a falsification, not a compatibility charge.
    PhpString::new(req, format!("{retained} {actual_contents}").as_bytes())
}

fn engine() -> Engine {
    let proof = ExtensionBuilder::default()
        .functions(vec![
            storage_probe::declared(),
            storage_failure::declared(),
            retained_pool_probe::declared(),
            consumer_storage_probe::declared(),
        ])
        .build("uri_storage_proof");
    Engine::builder()
        .extensions(&[php_ext_lexbor::extension(), php_ext_uri::extension(), proof])
        .sense_leaks(LeakSensor::Armed)
        .build()
}

#[test]
fn actual_component_bytes_lazy_clone_release_and_short_reentrant_borrows() {
    let out = engine().serve_capture(
        "<?php echo storage_probe(function ($context) { echo \"reentry\\n\", $context, \"\\n\"; }), \"\\n\"; echo retained_pool_probe(), \"\\n\";",
        Capture::default(),
    );
    assert_eq!(
        out.stdout,
        b"reentry\nEXAMPLE.COM\n16 32 32 true true true\n8192 8192 true true\n"
    );
}

#[test]
fn initialized_prefix_is_released_on_refusal() {
    let out = engine().serve_capture(
        "<?php try { storage_failure(false); } catch (Exception $e) { echo $e->getMessage(); }",
        Capture::default(),
    );
    assert_eq!(out.stdout, b"storage refused");
}

#[test]
fn callback_refusal_releases_the_copied_context_owner() {
    let out = engine().serve_capture(
        "<?php try { storage_probe(function ($context) { throw new Exception($context); }); } catch (Exception $e) { echo $e->getMessage(); }",
        Capture::default(),
    );
    assert_eq!(out.stdout, b"EXAMPLE.COM");
}

#[test]
fn current_consumer_components_are_system_owned_not_php_struct_storage() {
    let out = engine().serve_capture("<?php echo consumer_storage_probe();", Capture::default());
    assert_eq!(out.stdout, b"0 true");
}

#[test]
fn allocation_failure_and_next_request_do_not_retain_components() {
    let engine = engine();
    let out = engine.serve_capture("<?php storage_failure(true);", Capture::default());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Allowed memory size"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("request ended with"));
    let out = engine.serve_capture("<?php echo retained_pool_probe();", Capture::default());
    assert_eq!(out.stdout, b"8192 8192 true true");
}
