use crate::{Kind, Plain};

impl Kind {
    pub const COUNT: usize = 2;

    pub fn name(self) -> &'static str {
        "kind"
    }

    fn private(self) {}
}

impl Plain {
    pub fn new() -> Plain {
        Plain
    }
}
