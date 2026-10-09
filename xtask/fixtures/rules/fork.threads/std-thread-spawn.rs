//! A timer thread started beside `fork::spawn`, through a grouped import.
use std::thread::{self, JoinHandle};

pub fn timer() -> JoinHandle<()> {
    thread::spawn(|| {})
}
