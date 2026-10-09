//! A named thread started through `std::thread::Builder`, the shape
//! `fork::spawn` itself takes, from outside the home.
use std::thread::Builder;

pub fn named() -> bool {
    Builder::new().name("php-timer".into()).spawn(|| {}).is_ok()
}
