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

/// A copy of a file that others can still write (an image in the working
/// directory of a preview container), made in a directory only texrun
/// writes. It is inspected through its own descriptor and then renamed into
/// place ([`Staged::store`]), so what was checked is what is stored, and the
/// stored file shares no inode with the original. Removed when dropped
/// unless stored.
#[derive(Debug)]
pub(crate) struct Staged {
    dir: OwnedFd,
    name: String,
    file: File,
    len: u64,
    stored: bool,
}

impl Staged {
    /// Copies at most `limit` bytes of the regular file `from` in
    /// `from_dir` (opened with `O_NOFOLLOW`) to a new file in `to_dir`.
    /// `None` if `from` is missing or not a regular file.
    pub(crate) fn copy(
        from_dir: &OwnedFd,
        from: &str,
        to_dir: &OwnedFd,
        limit: u64,
    ) -> io::Result<Option<Self>> {
        let source = match rustix::fs::openat(from_dir, from, READ_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT | Errno::LOOP | Errno::MLINK) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if FileType::from_raw_mode(rustix::fs::fstat(&source)?.st_mode) != FileType::RegularFile {
            return Ok(None);
        }
        let name = format!(".{:016x}.tmp", random_u64());
        let mut staged = Self {
            dir: to_dir.try_clone()?,
            file: File::from(create_new(to_dir, &name, Mode::from_raw_mode(0o644))?),
            name,
            len: 0,
            stored: false,
        };
        staged.len = io::copy(&mut File::from(source).take(limit), &mut staged.file)?;
        Ok(Some(staged))
    }

    /// The number of bytes copied.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// The first `N` bytes of the copy.
    pub(crate) fn header<const N: usize>(&self) -> Option<[u8; N]> {
        use std::os::unix::fs::FileExt;
        let mut header = [0u8; N];
        self.file.read_exact_at(&mut header, 0).ok()?;
        Some(header)
    }

    /// Renames the copy to `to` in its directory, replacing a file (or
    /// symlink) of that name.
    pub(crate) fn store(mut self, to: &str) -> io::Result<()> {
        rustix::fs::renameat(&self.dir, self.name.as_str(), &self.dir, to)?;
        self.stored = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.stored {
            remove(&self.dir, &self.name);
        }
    }
}

/// Creates the new file `name` in `dir` (`O_EXCL`, never through a
/// symlink) with permissions `mode` (not reduced by the umask).
fn create_new(dir: &OwnedFd, name: &str, mode: Mode) -> io::Result<OwnedFd> {
    let flags = OFlags::RDWR
        .union(OFlags::CREATE)
        .union(OFlags::EXCL)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    let fd = rustix::fs::openat(dir, name, flags, mode)?;
    rustix::fs::fchmod(&fd, mode)?;
    Ok(fd)
}

/// Copies the regular file `source` (the PDF, a path the caller checked) to
/// the new file `name` in `dir`, readable by everyone (mode 0644: the
/// container user may be another uid). Its last component is not followed
/// if it is a symlink. Returns the number of bytes copied.
pub(crate) fn copy_in(source: &Path, dir: &OwnedFd, name: &str) -> io::Result<u64> {
    let fd = rustix::fs::open(source, READ_FLAGS, Mode::empty()).map_err(map)?;
    if FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(io::Error::other("not a regular file"));
    }
    let target = create_new(dir, name, Mode::from_raw_mode(0o644))?;
    io::copy(&mut File::from(fd), &mut File::from(target))
}

/// Deepest directory level [`remove_tree`] descends to. The tools only
/// create a few levels of caches in their `HOME`.
const MAX_REMOVE_DEPTH: usize = 32;

/// A private directory created below a held parent directory with `mkdirat`
/// and opened with `O_NOFOLLOW`, removed (through the descriptors) when
/// dropped.
#[derive(Debug)]
pub(crate) struct ScratchDir {
    parent: OwnedFd,
    name: String,
    dir: OwnedFd,
    path: std::path::PathBuf,
}

impl ScratchDir {
    /// Creates `<parent>/<prefix><random>` (mode 0700) through the held
    /// directory `parent`, whose path is `parent_path` (only used to build
    /// [`ScratchDir::path`]). A name that exists already (whatever it is) is
    /// never reused.
    pub(crate) fn create(parent: OwnedFd, parent_path: &Path, prefix: &str) -> io::Result<Self> {
        for _ in 0..64 {
            let name = format!("{prefix}{:016x}", random_u64());
            match rustix::fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(0o700)) {
                Ok(()) => {}
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(e.into()),
            }
            let dir = rustix::fs::openat(&parent, name.as_str(), DIR_FLAGS, Mode::empty())
                .map_err(map)?;
            let path = parent_path.join(&name);
            return Ok(Self {
                parent,
                name,
                dir,
                path,
            });
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no unused scratch directory name found",
        ))
    }

    /// A path of the directory (for `HOME` and messages; not followed by
    /// texrun itself).
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the subdirectory `name` and opens it (`O_NOFOLLOW`).
    pub(crate) fn subdir(&self, name: &str) -> io::Result<OwnedFd> {
        self.subdir_with_mode(name, Mode::from_raw_mode(0o700))
    }

    /// [`ScratchDir::subdir`] with permissions `mode` (not reduced by the
    /// umask).
    pub(crate) fn subdir_with_mode(&self, name: &str, mode: Mode) -> io::Result<OwnedFd> {
        rustix::fs::mkdirat(&self.dir, name, mode)?;
        let fd = rustix::fs::openat(&self.dir, name, DIR_FLAGS, Mode::empty()).map_err(map)?;
        rustix::fs::fchmod(&fd, mode)?;
        Ok(fd)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Ok(name) = std::ffi::CString::new(self.name.as_str()) {
            remove_tree(&self.parent, &name, 0);
        }
    }
}

/// A random value for a directory name (the standard library's per-process
/// random hash keys, mixed with a counter).
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

/// Removes `name` in `dir` and, if it is a directory, everything below it,
/// best effort. Symlinks are removed, never followed; directories are
/// opened with `O_NOFOLLOW` below the held descriptor.
fn remove_tree(dir: &OwnedFd, name: &std::ffi::CStr, depth: usize) {
    if rustix::fs::unlinkat(dir, name, AtFlags::empty()).is_ok() {
        return;
    }
    if depth < MAX_REMOVE_DEPTH
        && let Ok(sub) = rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty())
        && let Ok(mut entries) = rustix::fs::Dir::read_from(&sub)
    {
        let mut names = Vec::new();
        while let Some(Ok(entry)) = entries.read() {
            let entry_name = entry.file_name();
            if !matches!(entry_name.to_bytes(), b"." | b"..") {
                names.push(entry_name.to_owned());
            }
        }
        for n in names {
            remove_tree(&sub, &n, depth + 1);
        }
    }
    let _ = rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR);
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

    #[test]
    fn scratch_dirs_are_private_and_removed_through_descriptors() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"x").unwrap();
        let a = ScratchDir::create(open_dir(root.path()).unwrap(), root.path(), ".s-").unwrap();
        let b = ScratchDir::create(open_dir(root.path()).unwrap(), root.path(), ".s-").unwrap();
        assert_ne!(a.path(), b.path());
        assert!(
            a.path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".s-")
        );
        let mode = fs::metadata(a.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        let work = a.subdir("work").unwrap();
        assert!(a.subdir("work").is_err(), "never reused");
        fs::write(a.path().join("work/f"), b"data").unwrap();
        fs::create_dir_all(a.path().join("home/.cache/deep")).unwrap();
        fs::write(a.path().join("home/.cache/deep/c"), b"c").unwrap();
        // A symlink inside is removed, not followed.
        std::os::unix::fs::symlink(outside.path(), a.path().join("home/link")).unwrap();
        assert_eq!(regular_file_len(&work, "f"), Some(4));

        drop(a);
        drop(b);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        assert!(outside.path().join("keep").exists());
    }

    #[test]
    fn copies_go_through_descriptors_and_never_follow_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let dst_fd = open_dir(dst.path()).unwrap();
        fs::write(src.path().join("doc.pdf"), b"%PDF").unwrap();
        assert_eq!(
            copy_in(&src.path().join("doc.pdf"), &dst_fd, "in.pdf").unwrap(),
            4
        );
        let mode = fs::metadata(dst.path().join("in.pdf"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
        // Never over an existing name (or through a symlink there).
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("x"), dst.path().join("link")).unwrap();
        assert!(copy_in(&src.path().join("doc.pdf"), &dst_fd, "link").is_err());
        assert!(copy_in(&src.path().join("doc.pdf"), &dst_fd, "in.pdf").is_err());
        assert!(!outside.path().join("x").exists());
        assert!(
            copy_in(src.path(), &dst_fd, "dir.pdf").is_err(),
            "a directory"
        );

        // Never through a symlink as the source either.
        std::os::unix::fs::symlink(src.path().join("doc.pdf"), src.path().join("link.pdf"))
            .unwrap();
        assert!(copy_in(&src.path().join("link.pdf"), &dst_fd, "l.pdf").is_err());
    }

    #[test]
    fn a_staged_copy_is_what_is_stored() {
        use std::os::unix::fs::MetadataExt;

        let work = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let work_fd = open_dir(work.path()).unwrap();
        let out_fd = open_dir(out.path()).unwrap();
        fs::write(work.path().join("render.png"), b"0123456789").unwrap();

        let staged = Staged::copy(&work_fd, "render.png", &out_fd, 100)
            .unwrap()
            .unwrap();
        assert_eq!(staged.len(), 10);
        assert_eq!(staged.header::<4>(), Some(*b"0123"));
        // Whatever happens to the original after the copy (also through a
        // descriptor someone kept open) does not reach the copy.
        let mut original = fs::OpenOptions::new()
            .write(true)
            .open(work.path().join("render.png"))
            .unwrap();
        std::io::Write::write_all(&mut original, b"XXXX").unwrap();
        assert_eq!(staged.header::<4>(), Some(*b"0123"));
        // Stored over a symlink: the link is replaced, its target untouched.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("y"), out.path().join("page-001.png"))
            .unwrap();
        staged.store("page-001.png").unwrap();
        let stored = out.path().join("page-001.png");
        assert_eq!(fs::read(&stored).unwrap(), b"0123456789");
        assert!(!outside.path().join("y").exists());
        let (a, b) = (
            fs::metadata(&stored).unwrap(),
            fs::metadata(work.path().join("render.png")).unwrap(),
        );
        assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()), "a separate inode");
        std::io::Write::write_all(&mut original, b"YYYY").unwrap();
        assert_eq!(fs::read(&stored).unwrap(), b"0123456789");

        // At most `limit` bytes; dropped copies are removed.
        let short = Staged::copy(&work_fd, "render.png", &out_fd, 3)
            .unwrap()
            .unwrap();
        assert_eq!(short.len(), 3);
        drop(short);
        // A missing file or a symlink is no image.
        assert!(
            Staged::copy(&work_fd, "missing.png", &out_fd, 100)
                .unwrap()
                .is_none()
        );
        std::os::unix::fs::symlink(work.path().join("render.png"), work.path().join("l.png"))
            .unwrap();
        assert!(
            Staged::copy(&work_fd, "l.png", &out_fd, 100)
                .unwrap()
                .is_none()
        );
        let names: Vec<_> = fs::read_dir(out.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["page-001.png"]);
    }

    #[test]
    fn a_moved_scratch_dir_is_still_removed() {
        let root = tempfile::tempdir().unwrap();
        let scratch =
            ScratchDir::create(open_dir(root.path()).unwrap(), root.path(), ".s-").unwrap();
        let work = scratch.subdir("work").unwrap();
        // Something replaces the work directory by a symlink: the held
        // descriptor still names the real one.
        let outside = tempfile::tempdir().unwrap();
        fs::rename(scratch.path().join("work"), scratch.path().join("old")).unwrap();
        std::os::unix::fs::symlink(outside.path(), scratch.path().join("work")).unwrap();
        fs::write(scratch.path().join("old/page.png"), b"png").unwrap();
        assert_eq!(regular_file_len(&work, "page.png"), Some(3));
        drop(scratch);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
