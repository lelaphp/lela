//! How `build.rs` reads the libxml2 version, `build/libxml2.rs`, where
//! `cargo test` reaches it: the release that renamed the `XPath` value
//! stack's functions is the cut, whatever the platform links.

#[path = "../build/libxml2.rs"]
mod libxml2;

use libxml2::exports_xpath_value_push;

#[test]
fn releases_before_2_14_export_value_push() {
    for version in ["2.9", "2.9.13", "2.9.14", "2.10.3", "2.12.10", "2.13.8"] {
        assert!(!exports_xpath_value_push(version), "{version}");
    }
}

#[test]
fn releases_from_2_14_export_xml_xpath_value_push() {
    for version in ["2.14.0", "2.14.6", "2.15.2", "2.15.2\n", "2.16", "3.0.0"] {
        assert!(exports_xpath_value_push(version), "{version}");
    }
}

#[test]
fn a_suffixed_release_reads_by_its_numbers() {
    assert!(exports_xpath_value_push("2.14.0-rc1"));
    assert!(!exports_xpath_value_push("2.13.0rc2"));
}

#[test]
fn an_unreadable_version_is_the_floor() {
    for version in ["", "2", "x.y", "2.x"] {
        assert!(!exports_xpath_value_push(version), "{version:?}");
    }
}
