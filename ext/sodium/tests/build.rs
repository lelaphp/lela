//! How `build.rs` reads the libsodium version, `build/libsodium.rs`,
//! where `cargo test` reaches it: the release that added AEGIS is the
//! cut, whatever the platform links.

#[path = "../build/libsodium.rs"]
mod libsodium;

use libsodium::has_aead_aegis;

#[test]
fn releases_before_1_0_19_have_no_aegis() {
    for version in ["1.0.17", "1.0.18", "1.0.18\n", "0.9.99"] {
        assert!(!has_aead_aegis(version), "{version}");
    }
}

#[test]
fn releases_from_1_0_19_have_aegis() {
    for version in ["1.0.19", "1.0.20", "1.0.22", "1.1.0", "2.0.0"] {
        assert!(has_aead_aegis(version), "{version}");
    }
}

#[test]
fn a_suffixed_release_reads_by_its_numbers() {
    assert!(has_aead_aegis("1.0.19-stable"));
    assert!(!has_aead_aegis("1.0.18-stable"));
}

#[test]
fn an_unreadable_version_is_the_floor() {
    for version in ["", "1", "1.0", "x.y.z"] {
        assert!(!has_aead_aegis(version), "{version:?}");
    }
}
