//! Actual request bytes and native payload fit, not a typed allocator proposal.
//! Byte records have real contents; no system value has a representative charge.

use php_ext::abi::{ExtensionBuilder, This};
use php_ext::attr::{php_class, php_function};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::test::{Capture, Engine, LeakSensor};
use php_ext::values::{Emalloc, PhpString};
use std::path::PathBuf;

#[php_function]
fn document_bytes(req: &mut Request) -> Result<bool, Thrown> {
    let before = req.memory().usage(false);
    let mut node_claim = Emalloc::new(req, 24)?;
    assert_eq!(node_claim.bytes(), &[0; 24]);
    assert_eq!(node_claim.as_mut_ptr() as usize % 8, 0);
    for (part, word) in node_claim.bytes_mut().chunks_exact_mut(8).zip([1u64, 2, 3]) {
        part.copy_from_slice(&word.to_ne_bytes());
    }
    let mut document_claim = Emalloc::new(req, 48)?;
    document_claim.bytes_mut()[..24].copy_from_slice(node_claim.bytes());
    assert!(req.memory().usage(false) > before);
    let grown = document_claim.resize(req, 64)?;
    assert_eq!(&grown.bytes()[..24], node_claim.bytes());
    assert_eq!(&grown.bytes()[48..], &[0; 16]);
    grown.free(req);
    node_claim.free(req);
    Ok(req.memory().usage(false) == before)
}

#[php_function]
fn document_refusal(req: &mut Request) -> Result<bool, Thrown> {
    let mut initialized = Emalloc::new(req, 24)?;
    initialized.bytes_mut().copy_from_slice(&[1; 24]);
    let second = Emalloc::new(req, 4 * 1024 * 1024);
    // Partial construction cleans its initialized member before propagating
    // the actual heap refusal. No synthetic compatibility charge exists.
    initialized.free(req);
    second.map(|block| {
        block.free(req);
        true
    })
}

#[php_function]
fn document_drop_seen(path: PhpString) -> bool {
    PathBuf::from(String::from_utf8_lossy(path.as_bytes()).into_owned()).is_file()
}

#[derive(Default)]
struct ClaimPayload {
    words: [u64; 3],
    dropped: Option<PathBuf>,
}

impl Drop for ClaimPayload {
    fn drop(&mut self) {
        if let Some(path) = &self.dropped {
            std::fs::write(path, b"typed payload dropped").expect("test receipt writable");
        }
    }
}

// `size` is the complete PHP object, including its 56-byte header. The macro
// rejected sizes 24 and 48 with E0080. Use 56+48 for this proof, not for PHP DOM.
// Enlarging PHP-visible storage is NOT an approved compatibility fix.
#[php_class(size = 104, payload = ClaimPayload)]
struct DocumentClaimProbe;

#[php_class]
impl DocumentClaimProbe {
    fn initialize(req: &mut Request, this: This<Self>, path: PhpString) -> Result<bool, Thrown> {
        let path = PathBuf::from(String::from_utf8_lossy(path.as_bytes()).into_owned());
        let payload = this.payload_mut(req)?;
        payload.words = [1, 2, 3];
        payload.dropped = Some(path);
        Ok(true)
    }
}

fn engine() -> Engine {
    let extension = ExtensionBuilder::default()
        .functions(vec![
            document_bytes::declared(),
            document_refusal::declared(),
            document_drop_seen::declared(),
        ])
        .class::<DocumentClaimProbe>()
        .build("document_proof");
    Engine::builder()
        .extensions(&[extension])
        .sense_leaks(LeakSensor::Armed)
        .build()
}

#[test]
fn real_byte_records_are_initialized_grown_and_individually_freed() {
    let served = engine().serve_capture("<?php var_dump(document_bytes());", Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(served.stdout, b"bool(true)\n");
}

#[test]
fn real_payload_is_initialized_and_dropped_on_free_and_request_end() {
    assert_eq!(std::mem::size_of::<ClaimPayload>(), 48);
    assert_eq!(std::mem::align_of::<ClaimPayload>(), 8);
    for release in ["unset($p);", ""] {
        let path = std::env::temp_dir().join(format!(
            "t2829-payload-{}-{}.txt",
            std::process::id(),
            if release.is_empty() { "end" } else { "free" }
        ));
        assert!(!path.exists(), "do not overwrite an existing test receipt");
        let source = format!(
            "<?php $p = new DocumentClaimProbe(); var_dump($p->initialize('{path}')); var_dump(document_drop_seen('{path}')); {release} var_dump(document_drop_seen('{path}'));",
            path = path.display()
        );
        let served = engine().serve_capture(&source, Capture::default());
        assert_eq!(served.fatal, None);
        let expected = if release.is_empty() {
            b"bool(true)\nbool(false)\nbool(false)\n".as_slice()
        } else {
            b"bool(true)\nbool(false)\nbool(true)\n".as_slice()
        };
        assert_eq!(served.stdout, expected);
        assert_eq!(
            std::fs::read(&path).expect("drop receipt"),
            b"typed payload dropped"
        );
        std::fs::remove_file(path).expect("remove this test's receipt");
    }
}

#[test]
fn an_actual_request_allocation_refusal_ends_the_request_without_a_panic() {
    let served = engine().serve_capture(
        "<?php ini_set('memory_limit', '2M'); document_refusal(); echo 'unreachable';",
        Capture::default(),
    );
    assert!(served.fatal.is_some());
    assert!(!served.stdout.ends_with(b"unreachable"));
}
