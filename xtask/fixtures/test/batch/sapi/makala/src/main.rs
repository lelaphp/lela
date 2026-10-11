//! A host whose binary says whether php_heap's `loud` is on: off in its own build.

fn main() {
    println!("{}", php_heap::loud());
}
