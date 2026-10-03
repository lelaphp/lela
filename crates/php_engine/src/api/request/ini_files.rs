//! What `php_init_config` found (`Z/main/php_ini.c:409-723`): the
//! php.ini a host's search opened and the files its scan read, each a
//! path and its bytes, which the builder reads with the one INI grammar
//! after the host's defaults and before its `ini_entries` (design §4.3
//! seam 4, revision 57), and what `php_ini_loaded_file()`,
//! `php_ini_scanned_files()` and phpinfo read of them after.

/// One ini file a host found: its path as php records it and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniFile {
    pub(crate) path: Vec<u8>,
    pub(crate) bytes: Vec<u8>,
}

impl IniFile {
    /// The file at `path`, read as `bytes`.
    #[must_use]
    pub fn new(path: Vec<u8>, bytes: Vec<u8>) -> IniFile {
        IniFile { path, bytes }
    }
}

/// php's `PHP_CONFIG_FILE_PATH` the search ended at, `php_ini_opened_path`,
/// `php_ini_scanned_path` and the files of `php_ini_scanned_files`: none
/// opened and none scanned for a builder handed nothing, as under `-n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniFiles {
    ini_path: Vec<u8>,
    pub(crate) loaded: Option<IniFile>,
    scan_dir: Option<Vec<u8>>,
    pub(crate) scanned: Vec<IniFile>,
}

impl Default for IniFiles {
    fn default() -> IniFiles {
        IniFiles::new(crate::PHP_INI_PATH.as_bytes(), None, None, Vec::new())
    }
}

impl IniFiles {
    /// What a host's search found: the configured path, the file opened,
    /// the scan path where one was scanned, and the scanned files in the
    /// order read.
    #[must_use]
    pub fn new(
        ini_path: &[u8],
        loaded: Option<IniFile>,
        scan_dir: Option<&[u8]>,
        scanned: Vec<IniFile>,
    ) -> IniFiles {
        IniFiles {
            ini_path: ini_path.to_vec(),
            loaded,
            scan_dir: scan_dir.map(<[u8]>::to_vec),
            scanned,
        }
    }

    /// `PHP_CONFIG_FILE_PATH`, phpinfo's "Configuration File (php.ini) Path".
    #[must_use]
    pub fn ini_path(&self) -> &[u8] {
        &self.ini_path
    }

    /// `php_ini_opened_path`, the file read, expanded as php records it.
    #[must_use]
    pub fn loaded(&self) -> Option<&[u8]> {
        self.loaded.as_ref().map(|file| file.path.as_slice())
    }

    /// `php_ini_scanned_path`, the scan path, where one was scanned.
    #[must_use]
    pub fn scan_dir(&self) -> Option<&[u8]> {
        self.scan_dir.as_deref()
    }

    /// The scanned files that parsed, in the order read.
    pub fn scanned(&self) -> impl Iterator<Item = &[u8]> {
        self.scanned.iter().map(|file| file.path.as_slice())
    }
}
