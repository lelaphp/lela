use crate::Kind;

impl Kind {
    pub const COUNT: usize = 2;

    pub fn name(self) -> &'static str {
        "kind"
    }

    fn private(self) {}
}
