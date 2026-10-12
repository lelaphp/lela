use php_ext::attr::mirrors;

pub fn extension() {}

/// The RNG ext/standard draws from.
pub struct Rng;

impl Rng {
    /// Named as the header declares it: `PHPAPI` on
    /// `php_random_default_algo`.
    #[mirrors("ext/random/php_random.h", "php_random_default_algo")]
    pub fn default_engine() -> Rng {
        Rng
    }

    /// Named as the header does not declare it: `php_random_default_engine`
    /// is its `static inline`, which carries no `PHPAPI`.
    #[mirrors("ext/random/php_random.h", "php_random_default_engine")]
    pub fn engine() -> Rng {
        Rng
    }
}

/// Named in a header the tree lacks.
#[mirrors("ext/random/php_rng.h", "php_random_default_algo")]
pub fn absent() {}
