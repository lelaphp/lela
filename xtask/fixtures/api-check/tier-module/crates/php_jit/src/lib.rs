use php_engine::tier;
use php_engine::tier as t;

pub fn enter() {
    tier::unlisted();
    tier::listed();
    t::renamed();
    php_engine::tier::in_place();
    let _: tier::Flow = tier::Flow::Exit;
}

pub fn scoped() {
    use php_engine::tier as inside;
    inside::scoped();
}

mod own {
    mod tier {
        pub fn mine() {}
    }

    pub fn local() {
        tier::mine();
    }
}
