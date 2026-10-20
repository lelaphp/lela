//! `Z/Zend/zend_virtual_cwd.c`'s path resolution as the natives reach
//! it: `virtual_file_ex` over `tsrm_realpath_r` and the realpath cache,
//! which the engine holds ([`Environment::virtual_file_ex`]), and
//! `expand_filepath` (`Z/main/fopen_wrappers.c:787`) over it.

use php_ext::request::{Request, Resolve};

/// `expand_filepath_ex(path, real, relative_to, len)` under
/// `CWD_FILEPATH` (`Z/main/fopen_wrappers.c:787`): an absolute path as
/// it is, a relative one against `relative_to`, the working directory
/// the caller hands it; `None` for the empty path and wherever
/// `virtual_file_ex` fails.
pub(super) fn expand_filepath(
    req: &mut Request,
    path: &[u8],
    relative_to: &[u8],
) -> Option<Vec<u8>> {
    if path.is_empty() {
        return None;
    }
    let cwd: &[u8] = if path.first() == Some(&b'/') {
        b""
    } else {
        relative_to
    };
    req.environment_mut()
        .virtual_file_ex(cwd, path, Resolve::FilePath)
}
