//! A negative control (harness-design.md §4.5 condition 2; #1068): an
//! extension whose natives stand on the attributes, some under a `#[cfg]`
//! the target's set decides. Its natives are linked into no engine, so
//! every subject reads missing; what the coverage half must not name is a
//! native the build compiled out: under `windows`, in an impl, an inline
//! `mod` or a `mod name;` file a false `#[cfg]` stands on, or in a file
//! whose `#![cfg]` is false. One under `feature` stays a subject, since
//! the target's set does not say which features a crate is built with.
#![forbid(unsafe_code)]

use php_ext::attr::{php_class, php_function};

#[cfg(windows)]
mod platform;
mod elsewhere;

/// On every target.
#[php_function]
pub fn fixture_everywhere() {}

/// On every unix target, which every host the lane runs on is.
#[cfg(unix)]
#[php_function]
pub fn fixture_unix() {}

/// On no host the lane runs on.
#[php_function]
#[cfg(all(windows, not(unix)))]
pub fn fixture_windows() {}

/// Where php declares it on Darwin alone.
#[cfg(target_vendor = "apple")]
#[php_function]
pub fn fixture_apple() {}

/// Where php declares it on Linux and FreeBSD.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[php_function]
pub fn fixture_linux() {}

/// Undecided by the target's set, so a subject.
#[cfg(feature = "fixture")]
#[php_function]
pub fn fixture_feature() {}

#[php_class]
pub struct Pattern;

#[cfg(windows)]
#[php_class]
impl Pattern {
    fn replace(this: This<Self>, subject: String) -> String {
        subject
    }
}

#[cfg(windows)]
mod inline {
    #[php_function]
    pub fn fixture_inline() {}
}
