//! Bounded actual-byte storage proof for #2827, not a Rust recognizer.
//! Uses only the already locked native face; no typed allocator or raw cast.

use php_ext::abi::ExtensionBuilder;
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::test::{Capture, Engine};
use php_ext::values::Emalloc;

enum Cleanup {
    Immediate,
    RequestEnd,
    ForgetForNegativeControl,
    RefuseLargeAllocation,
}

#[php_function]
fn storage_proof(req: &mut Request, stop: i64, cleanup: i64) -> Result<i64, Thrown> {
    let cleanup = match cleanup {
        0 => Cleanup::Immediate,
        1 => Cleanup::RequestEnd,
        2 => Cleanup::ForgetForNegativeControl,
        3 => Cleanup::RefuseLargeAllocation,
        _ => unreachable!("only the four test inputs call this adapter"),
    };
    let mut blocks = Vec::new();
    // A populated compiled record, continuation offsets and retained output.
    // These are the contents, not charges for a second system-owned copy.
    for (stage, size) in [432, 80, 122].into_iter().enumerate() {
        let mut block = match Emalloc::new(req, size) {
            Ok(block) => block,
            Err(thrown) => {
                for block in blocks {
                    Emalloc::free(block, req);
                }
                return Err(thrown);
            }
        };
        assert!(block.bytes().iter().all(|&byte| byte == 0));
        assert_eq!(block.as_mut_ptr() as usize % 8, 0);
        match stage {
            0 => {
                block.bytes_mut()[4..7].copy_from_slice(&[b'=', 6, 5]);
                block.bytes_mut()[32..38].copy_from_slice(b"GIF89a");
                assert_eq!(&block.bytes()[32..38], b"GIF89a");
            }
            1 => {
                block.bytes_mut()[0..4].copy_from_slice(&6_i32.to_le_bytes());
                assert_eq!(
                    i32::from_le_bytes(block.bytes()[0..4].try_into().unwrap()),
                    6
                );
                block = match block.resize(req, 160) {
                    Ok(block) => block,
                    Err(thrown) => {
                        for block in blocks {
                            Emalloc::free(block, req);
                        }
                        return Err(thrown);
                    }
                };
                assert_eq!(&block.bytes()[0..4], &6_i32.to_le_bytes());
                assert!(block.bytes()[80..].iter().all(|&byte| byte == 0));
            }
            2 => {
                block.bytes_mut()[..121].fill(b'x');
                assert_eq!(block.bytes()[121], 0);
            }
            _ => unreachable!(),
        }
        blocks.push(block);
        if stop == i64::try_from(stage).unwrap() {
            break;
        }
    }
    let populated = i64::try_from(blocks.iter().map(Emalloc::len).sum::<usize>()).unwrap();
    match cleanup {
        Cleanup::RefuseLargeAllocation => {
            let refused = Emalloc::new(req, 17_280_432);
            for block in blocks {
                block.free(req);
            }
            match refused {
                Ok(block) => block.free(req),
                Err(thrown) => return Err(thrown),
            }
        }
        Cleanup::Immediate => {
            for block in blocks {
                block.free(req);
            }
        }
        Cleanup::RequestEnd => req.state::<Storage>().0.extend(blocks),
        Cleanup::ForgetForNegativeControl => {}
    }
    Ok(populated)
}

#[derive(Default)]
struct Storage(Vec<Emalloc>);

fn release(req: &mut Request) {
    // End the state borrow before freeing through the request.
    let blocks = std::mem::take(&mut req.state::<Storage>().0);
    for block in blocks {
        block.free(req);
    }
}

fn engine() -> Engine {
    let proof = ExtensionBuilder::default()
        .functions(vec![storage_proof::declared()])
        .request_state::<Storage>()
        .rshutdown(release)
        .build("fileinfo_storage_proof");
    Engine::builder().extensions(&[proof]).build()
}

#[test]
fn populated_bytes_are_reclaimed_after_each_partial_stage() {
    let served = engine().serve_capture(
        "<?php foreach ([0, 1, 2] as $stage) { $m = memory_get_usage(); $n = storage_proof($stage, 0); $d = memory_get_usage() - $m; var_dump($n, $d); }",
        Capture::default(),
    );
    assert_eq!(served.exit_code, 0);
    assert_eq!(
        served.stdout,
        b"int(432)\nint(0)\nint(592)\nint(0)\nint(714)\nint(0)\n"
    );
}

#[test]
fn request_end_releases_the_actual_retained_bytes() {
    let engine = engine();
    for _ in 0..3 {
        let served = engine.serve_capture(
            "<?php $m = memory_get_usage(); var_dump(storage_proof(2, 1), memory_get_usage() - $m >= 714);",
            Capture::default(),
        );
        assert_eq!(served.exit_code, 0, "{served:?}");
        assert_eq!(served.stdout, b"int(714)\nbool(true)\n");
    }
}

#[test]
fn merely_dropping_the_holder_is_not_request_cleanup() {
    let served = engine().serve_capture("<?php storage_proof(2, 2);", Capture::default());
    assert_eq!(served.exit_code, 255);
    assert!(
        served
            .fatal
            .as_deref()
            .unwrap()
            .contains("3 blocks of its heap never freed, 736 bytes")
    );
}

#[test]
fn a_refused_allocation_releases_the_initialized_prefix() {
    let served = engine().serve_capture(
        "<?php ini_set('memory_limit', '8M'); storage_proof(2, 3);",
        Capture::default(),
    );
    assert_eq!(served.exit_code, 255);
    let fatal = served.fatal.as_deref().unwrap();
    assert!(
        fatal.contains("Allowed memory size of 8388608 bytes exhausted"),
        "{fatal}"
    );
    assert!(!fatal.contains("blocks of its heap never freed"), "{fatal}");
}
