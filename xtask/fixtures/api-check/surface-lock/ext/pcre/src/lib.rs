pub fn extension() {}

/// The compiled pattern its dependents compile through, with a line in
/// the fixture's `surface.lock`.
pub struct Regex;

impl Regex {
    pub fn compile() -> Regex {
        Regex
    }

    /// A member with no line in the lock.
    pub fn study(&self) {}
}

/// An export with no line in the lock.
pub fn unlisted() {}
