pub struct Cache;

impl Cache {
    pub fn new() -> Cache {
        Cache
    }
    pub fn get_or_build(&self) {}
}

pub trait Extension {
    fn name(&self) -> &str;
}
