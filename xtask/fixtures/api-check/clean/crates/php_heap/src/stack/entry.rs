pub struct Entry;

pub trait Frame {
    const DEPTH: usize;
    fn current(&self) -> Entry;
}
