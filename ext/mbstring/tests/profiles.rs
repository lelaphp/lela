//! #2831 retained byte/state experiments at Capture; the oracle captures live in data/.
use std::fmt::Write as _;

use php_ext::test::{Capture, Engine, LeakSensor};

#[test]
fn operation_specific_states_and_reentry_match_the_captured_oracle() {
    let engine = Engine::builder()
        .extensions(&[php_ext_pcre::extension(), php_ext_mbstring::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(include_str!("profiles.php"), Capture::default());
    println!("{}", String::from_utf8_lossy(&served.stdout));
    assert_eq!(served.fatal, None);
    let mut observed_hex = String::new();
    for byte in &served.stdout {
        assert!(write!(observed_hex, "{byte:02x}").is_ok());
    }
    assert_eq!(
        observed_hex,
        include_str!("../../../data/mbstring-2831/profiles.hex").trim()
    );
}

#[test]
fn a_warning_handler_throw_releases_partial_conversion_owners() {
    let engine = Engine::builder()
        .extensions(&[php_ext_pcre::extension(), php_ext_mbstring::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(include_str!("profiles_throw.php"), Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(
        served.stdout,
        include_bytes!("../../../data/mbstring-2831/profiles_throw.stdout")
    );
}
