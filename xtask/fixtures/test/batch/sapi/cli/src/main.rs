//! A host whose binary says whether php_heap's `loud` is on: on in its own build.

fn main() {
    println!("{}", php_heap::loud());
}
