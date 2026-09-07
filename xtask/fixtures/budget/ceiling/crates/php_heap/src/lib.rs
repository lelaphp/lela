//! Three counted lines under a ceiling of two: the fixture is one line over.
#![forbid(unsafe_code)]

/// A comment is not counted.
pub fn one() {}
pub fn two() {}

#[cfg(test)]
mod tests {
    #[test]
    fn not_counted() {}
}
