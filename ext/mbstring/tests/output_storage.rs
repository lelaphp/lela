//! #2831 non-shipped experiment at the existing native face and Capture.
//! No codec is registered and no product execution path is switched.
#![cfg(debug_assertions)]

use php_ext::abi::ExtensionBuilder;
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::test::{Capture, Engine, LeakSensor};
use php_ext::values::{FreshString, Owner, PhpString};

#[php_function]
fn storage_profile(req: &mut Request, shrink: bool) -> Result<Owner<PhpString>, Thrown> {
    let baseline = req.memory().usage(false);
    let mut bytes = FreshString::new(req, 2050)?;
    bytes.bytes_mut()[..7].copy_from_slice(b"payload");
    let initial = req.memory().usage(false) - baseline;
    // libmbfl: old_capacity + max(old_capacity / 2, needed).
    bytes = bytes.extend(req, 2050 + 1500)?;
    let grown = req.memory().usage(false) - baseline;
    let content = bytes.bytes()[..7] == *b"payload";
    if shrink {
        bytes = bytes.truncate(req, 7)?;
    } else {
        bytes.set_len(7);
    }
    let retained = req.memory().usage(false) - baseline;
    let finished = bytes.finish();
    let correct = content && finished.as_bytes() == b"payload";
    finished.give_up(req);
    let released = req.memory().usage(false) - baseline;
    PhpString::new(
        req,
        format!("{initial},{grown},{retained},{released},{correct}").as_bytes(),
    )
}

#[php_function]
fn storage_free(req: &mut Request) -> Result<i64, Thrown> {
    let baseline = req.memory().usage(false);
    let bytes = FreshString::new(req, 2050)?;
    bytes.free(req);
    Ok(i64::try_from(req.memory().usage(false) - baseline).unwrap_or(-1))
}

#[php_function]
fn storage_growth_schedule(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let baseline = req.memory().usage(false);
    let mut bytes = FreshString::new(req, 2050)?;
    let initial = req.memory().usage(false) - baseline;
    bytes = bytes.extend(req, 3075)?;
    let first_growth = req.memory().usage(false) - baseline;
    bytes = bytes.extend(req, 4612)?;
    let second_growth = req.memory().usage(false) - baseline;
    bytes.bytes_mut()[..4100].fill(b'a');
    bytes.set_len(4100);
    let retained = req.memory().usage(false) - baseline;
    bytes.finish().give_up(req);
    let released = req.memory().usage(false) - baseline;
    PhpString::new(
        req,
        format!("{initial},{first_growth},{second_growth},{retained},{released}").as_bytes(),
    )
}

#[php_function]
fn storage_identity(req: &mut Request, input: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let baseline = req.memory().usage(false);
    let empty = FreshString::new(req, 0)?.finish();
    let fresh_empty = req.memory().usage(false) - baseline;
    empty.give_up(req);
    let one = FreshString::new(req, 1)?.finish();
    let fresh_one = req.memory().usage(false) - baseline;
    one.give_up(req);
    let empty = PhpString::existing_interned(req, b"")?;
    let interned_empty = req.memory().usage(false) - baseline;
    empty.give_up(req);
    let one = PhpString::existing_interned(req, b"a")?;
    let interned_one = req.memory().usage(false) - baseline;
    one.give_up(req);
    let reused = Owner::take(req, input);
    let taken = req.memory().usage(false) - baseline;
    reused.give_up(req);
    PhpString::new(
        req,
        format!("{fresh_empty},{fresh_one},{interned_empty},{interned_one},{taken}").as_bytes(),
    )
}

#[php_function]
fn storage_limit(req: &mut Request, grow: bool) -> Result<Owner<PhpString>, Thrown> {
    let bytes = FreshString::new(req, if grow { 2050 } else { 8 * 1024 * 1024 })?;
    let bytes = if grow {
        bytes.extend(req, 16 * 1024 * 1024)?
    } else {
        bytes
    };
    Ok(bytes.finish())
}

fn engine() -> Engine {
    Engine::builder()
        .extensions(&[
            php_ext_pcre::extension(),
            php_ext_mbstring::extension(),
            ExtensionBuilder::default()
                .functions(vec![
                    storage_profile::declared(),
                    storage_free::declared(),
                    storage_limit::declared(),
                    storage_growth_schedule::declared(),
                    storage_identity::declared(),
                ])
                .build("mbstring_storage_proof"),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build()
}

#[test]
fn fresh_empty_and_one_byte_strings_are_not_automatically_interned() {
    let served = engine().serve_capture("<?php echo storage_identity('a');", Capture::default());
    println!("{}", String::from_utf8_lossy(&served.stdout));
    assert_eq!(served.fatal, None);
    assert_eq!(served.stdout, b"32,32,0,0,0");
}

#[test]
fn the_scalar_growth_schedule_reaches_the_oracles_retained_bin() {
    let served =
        engine().serve_capture("<?php echo storage_growth_schedule();", Capture::default());
    println!("{}", String::from_utf8_lossy(&served.stdout));
    assert_eq!(served.fatal, None);
    assert_eq!(served.stdout, b"2560,4096,8192,8192,0");
}

#[test]
fn final_length_retains_capacity_but_truncate_does_not() {
    let served = engine().serve_capture(
        "<?php echo storage_profile(false), \"\n\", storage_profile(true), \"\n\", storage_free();",
        Capture::default(),
    );
    println!("{}", String::from_utf8_lossy(&served.stdout));
    assert_eq!(served.fatal, None);
    assert_eq!(
        served.stdout,
        b"2560,4096,4096,0,true\n2560,4096,32,0,true\n0"
    );
}

#[test]
fn initial_and_growth_limits_end_the_request_without_a_live_owner() {
    for grow in [false, true] {
        let source = format!(
            "<?php ini_set('memory_limit', '8M'); storage_limit({grow}); echo 'unreachable';"
        );
        let served = engine().serve_capture(&source, Capture::default());
        println!("grow={grow}: {:?}", served.fatal);
        assert!(served.fatal.is_some());
        assert!(!String::from_utf8_lossy(&served.stdout).contains("unreachable"));
    }
}
