//! Descriptor-based file operations for storing the images.
//!
//! The output root is shared with the engine, and a process left over from
//! the compile could replace a directory with a symlink at any time. The
//! same approach as the workspace crate's artifact collection is used:
//! directories below the output root are created with `mkdirat` and opened
//! one component at a time with `O_NOFOLLOW`, files are inspected with
//! `fstatat(AT_SYMLINK_NOFOLLOW)` / `O_NOFOLLOW` opens, and images are moved
//! with `renameat` between the held descriptors. A path is never followed
//! again after it was checked.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use texrun_core::WorkspacePath;

/// A directory without following a final symlink.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A file for reading without following a final symlink or blocking on a
/// FIFO swapped in at that name.
const READ_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

fn unsafe_entry() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "a file or symlink with that name exists",
    )
}

fn map(e: Errno) -> io::Error {
    match e {
        // `O_NOFOLLOW` on a symlink, or a non-directory with `O_DIRECTORY`.
        Errno::LOOP | Errno::MLINK | Errno::NOTDIR => unsafe_entry(),
        e => e.into(),
    }
}

/// Opens a directory chosen by the caller (the output root, or a directory
/// texrun created itself). Like the destination of the workspace crate's
/// collection, the path itself may be a symlink to a directory; only what is
/// below it is checked strictly.
pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::open(path, flags, Mode::empty())?)
}

/// Creates (if needed) and opens `<root>/<subdir>` one component at a time,
/// refusing symlinks and non-directories.
pub(crate) fn ensure_subdir(root: &OwnedFd, subdir: &WorkspacePath) -> io::Result<OwnedFd> {
    let mut dir: Option<OwnedFd> = None;
    for comp in subdir.as_str().split('/') {
        let parent = dir.as_ref().unwrap_or(root);
        match rustix::fs::mkdirat(parent, comp, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        dir = Some(rustix::fs::openat(parent, comp, DIR_FLAGS, Mode::empty()).map_err(map)?);
    }
    Ok(dir.expect("workspace paths are non-empty"))
}

/// The size of the regular file `name` in `dir`, or `None` if it is missing
/// or anything else than a regular file.
pub(crate) fn regular_file_len(dir: &OwnedFd, name: &str) -> Option<u64> {
    let st = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    (FileType::from_raw_mode(st.st_mode) == FileType::RegularFile)
        .then(|| u64::try_from(st.st_size).ok())
        .flatten()
}

/// The first `N` bytes of the regular file `name` in `dir`.
pub(crate) fn read_header<const N: usize>(dir: &OwnedFd, name: &str) -> Option<[u8; N]> {
    let fd = rustix::fs::openat(dir, name, READ_FLAGS, Mode::empty()).ok()?;
    let st = rustix::fs::fstat(&fd).ok()?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return None;
    }
    let mut header = [0u8; N];
    File::from(fd).read_exact(&mut header).ok()?;
    Some(header)
}

/// Removes `name` from `dir` if it exists (never follows a symlink).
pub(crate) fn remove(dir: &OwnedFd, name: &str) {
    let _ = rustix::fs::unlinkat(dir, name, AtFlags::empty());
}

/// Moves `from_dir/from` to `to_dir/to`, replacing a file (or symlink) of
/// that name; the target of a replaced symlink is not touched.
pub(crate) fn rename(from_dir: &OwnedFd, from: &str, to_dir: &OwnedFd, to: &str) -> io::Result<()> {
    Ok(rustix::fs::renameat(from_dir, from, to_dir, to)?)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn ensure_subdir_refuses_symlinks_and_files() {
        let root = tempfile::tempdir().unwrap();
        let root_fd = open_dir(root.path()).unwrap();
        let sub = WorkspacePath::new("a/b").unwrap();
        ensure_subdir(&root_fd, &sub).unwrap();
        assert!(root.path().join("a/b").is_dir());
        assert!(ensure_subdir(&root_fd, &sub).is_ok(), "idempotent");

        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let err = ensure_subdir(&root_fd, &WorkspacePath::new("link/x").unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(!outside.path().join("x").exists());

        fs::write(root.path().join("file"), "").unwrap();
        let err = ensure_subdir(&root_fd, &WorkspacePath::new("file").unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn file_helpers_do_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let fd = open_dir(dir.path()).unwrap();
        fs::write(dir.path().join("f"), b"0123456789").unwrap();
        std::os::unix::fs::symlink(dir.path().join("f"), dir.path().join("l")).unwrap();
        assert_eq!(regular_file_len(&fd, "f"), Some(10));
        assert_eq!(regular_file_len(&fd, "l"), None);
        assert_eq!(regular_file_len(&fd, "missing"), None);
        assert_eq!(read_header::<4>(&fd, "f"), Some(*b"0123"));
        assert_eq!(read_header::<4>(&fd, "l"), None);
        assert_eq!(read_header::<20>(&fd, "f"), None, "too short");

        let other = tempfile::tempdir().unwrap();
        let other_fd = open_dir(other.path()).unwrap();
        // Renaming over a symlink replaces the link, not its target.
        std::os::unix::fs::symlink(dir.path().join("f"), other.path().join("t")).unwrap();
        fs::write(dir.path().join("g"), b"new").unwrap();
        rename(&fd, "g", &other_fd, "t").unwrap();
        assert_eq!(fs::read(other.path().join("t")).unwrap(), b"new");
        assert_eq!(fs::read(dir.path().join("f")).unwrap(), b"0123456789");

        remove(&fd, "f");
        remove(&fd, "missing");
        assert!(!dir.path().join("f").exists());
    }
}
