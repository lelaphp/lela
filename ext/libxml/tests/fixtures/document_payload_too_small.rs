// Negative control for #2829. The runner also substitutes 104 for 80 and
// requires that positive control to compile, ruling out missing dependencies.
use php_ext::attr::php_class;
use std::path::PathBuf;

#[derive(Default)]
struct Payload {
    words: [u64; 3],
    path: Option<PathBuf>,
}

#[php_class(size = 80, payload = Payload)]
struct DocumentPayloadFit;
