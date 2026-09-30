//! Small filesystem helpers shared by materialization and collection.

use std::fs::Metadata;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

use rustix::fs::{FileType, OFlags, Stat};
use unicode_normalization::UnicodeNormalization;

/// Identity of a filesystem object: `(st_dev, st_ino)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    /// From a `stat` result. Field types differ between platforms and
    /// backends; the casts only normalize the representation.
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::unnecessary_cast,
        clippy::useless_conversion
    )]
    pub(crate) fn of(st: &Stat) -> Self {
        Self {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        }
    }

    pub(crate) fn of_metadata(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
        }
    }
}

/// File type of a `stat` result.
pub(crate) fn file_type(st: &Stat) -> FileType {
    FileType::from_raw_mode(st.st_mode)
}

/// `fstat` of an open descriptor.
pub(crate) fn fstat(fd: impl AsFd) -> std::io::Result<Stat> {
    Ok(rustix::fs::fstat(fd)?)
}

/// Flags for opening a directory without following a final symlink.
pub(crate) const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Flags for reading a file without following a final symlink or blocking
/// on a FIFO swapped in at that name.
pub(crate) const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

/// Folded form of a name for exclusion matching: NFKC, then lowercase,
/// then NFKC again (lowercasing can produce unnormalized sequences). This
/// approximates what case- and normalization-insensitive filesystems treat
/// as the same name, e.g. `LATEXM\u{212A}RC` (Kelvin sign) folds to
/// `latexmkrc`.
pub(crate) fn fold(name: &str) -> String {
    name.nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .nfkc()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_handles_case_and_compatibility_characters() {
        assert_eq!(fold("LATEXMKRC"), "latexmkrc");
        assert_eq!(fold("latexm\u{212A}rc"), "latexmkrc");
        assert_eq!(fold(".LATEXM\u{212A}RC"), ".latexmkrc");
        // Fullwidth letters and the long s are compatibility variants.
        assert_eq!(fold("\u{FF54}arget"), "target");
        assert_eq!(fold("bibe\u{0072}.conf"), "biber.conf");
        assert_eq!(fold("\u{017F}"), "s");
        assert_eq!(fold("main.tex"), "main.tex");
    }
}
