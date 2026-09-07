pub mod abi;

pub mod raising {
    pub enum Level {
        Warning,
        Notice,
    }
    pub fn warning(_level: Level) {}
}
