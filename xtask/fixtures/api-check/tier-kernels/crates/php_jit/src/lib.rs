use php_engine::ops::{add_slow, sub_slow};
use php_engine::tier::Tier;

kernels! {
    Tier => mod wrapped {
        Sub => sub(a: u64) -> u64 = ops::sub_slow;
        Plain => plain(a: u64) -> u64;
    }
}
