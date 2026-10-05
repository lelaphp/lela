pub fn extension() {}

/// The compiled pattern its dependents compile through, a row of the
/// fixture's design §4.10 that writes it with `compile` alone.
pub struct Regex;

impl Regex {
    pub fn compile() -> Regex {
        Regex
    }

    /// A member the row's list does not write.
    pub fn study(&self) {}
}

/// An export no row writes.
pub fn unlisted() {}
