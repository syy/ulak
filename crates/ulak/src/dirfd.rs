//! Reaching a name below a directory without ever following a symlink.
//!
//! A pulled file lands through directories ulak does not control: any
//! local process may swap a checked directory for a symlink between the
//! check and the write, and a pathname-based `rename` or `link` then
//! follows it out of the workspace. Checking twice does not close that
//! window; only holding each directory open does. Every step here is
//! relative to a descriptor already held, and every name is opened with
//! `O_NOFOLLOW`, so the directory that was checked is the directory that
//! is written — the same discipline `stream.py` keeps on the server.
//! Measured: a parent swapped by `renamex_np(RENAME_SWAP)` during `land`
//! received the pulled bytes outside the workspace.
//!
//! This is the crate's only unsafe code outside tests (stream.rs's tests
//! swap directories through `libc`): std has no stable `openat` family
//! (`std::fs::Dir` is unstable), so the calls go through `libc`. Each
//! wrapper checks the return value and takes ownership of a descriptor
//! at most once.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// An open directory, the base for every name below it.
pub struct Dir(OwnedFd);

/// What stands under a name, read without following it.
pub enum Entry {
    Absent,
    /// Present; the metadata when it could be opened as itself (a
    /// symlink or a socket cannot, and is present all the same).
    Present(Option<std::fs::Metadata>),
}

fn name(name: &OsStr) -> io::Result<CString> {
    if name.is_empty() || name.as_bytes().contains(&b'/') || name == "." || name == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a single path component",
        ));
    }
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a name"))
}

fn fd(raw: libc::c_int) -> io::Result<OwnedFd> {
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-negative return from open/openat is a new descriptor
    // nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn ok(ret: libc::c_int) -> io::Result<()> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn not_a_directory(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
}

impl Dir {
    /// The anchor itself. Symlinks ABOVE it are the user's own layout
    /// (macOS's `/tmp` is one) and are followed to reach its parent; the
    /// anchor's own name is opened with `O_NOFOLLOW`, because the anchor is
    /// canonical (config.rs) and a symlink standing there now means the
    /// project directory was replaced after it was chosen. Measured: a
    /// root swapped for a symlink sent the pull elsewhere.
    pub fn open(path: &Path) -> io::Result<Dir> {
        let (Some(parent), Some(last)) = (path.parent(), path.file_name()) else {
            return Dir::open_followed(path);
        };
        let parent = Dir::open_followed(parent)?;
        let c = name(last)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: valid descriptor and NUL-terminated name.
        fd(unsafe { libc::openat(parent.0.as_raw_fd(), c.as_ptr(), flags) }).map(Dir)
    }

    fn open_followed(path: &Path) -> io::Result<Dir> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a path"))?;
        // SAFETY: `path` is a valid NUL-terminated string for the call.
        fd(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })
        .map(Dir)
    }

    /// The child directory `name`, created when absent. None when the name
    /// holds a symlink or anything but a directory. The bool says whether
    /// this call created it.
    pub fn child(&self, name_: &OsStr) -> io::Result<Option<(Dir, bool)>> {
        let c = name(name_)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let open = || {
            // SAFETY: valid descriptor and NUL-terminated name.
            fd(unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags) })
        };
        match open() {
            Ok(dir) => return Ok(Some((Dir(dir), false))),
            Err(e) if not_a_directory(&e) => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // SAFETY: valid descriptor and NUL-terminated name.
        match ok(unsafe { libc::mkdirat(self.0.as_raw_fd(), c.as_ptr(), 0o777) }) {
            Ok(()) => {}
            // Made by someone else meanwhile: open whatever is there now.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        match open() {
            Ok(dir) => Ok(Some((Dir(dir), true))),
            Err(e) if not_a_directory(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// What `name` holds, opened with `O_NOFOLLOW | O_NONBLOCK` so neither
    /// a symlink nor a FIFO is traversed or waited on.
    pub fn entry(&self, name_: &OsStr) -> io::Result<Entry> {
        let c = name(name_)?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: valid descriptor and NUL-terminated name.
        match fd(unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags) }) {
            Ok(owned) => Ok(Entry::Present(File::from(owned).metadata().ok())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Entry::Absent),
            Err(_) => Ok(Entry::Present(None)),
        }
    }

    /// A new regular file `name`, refused when anything stands there.
    pub fn create(&self, name_: &OsStr, mode: u32) -> io::Result<File> {
        let c = name(name_)?;
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: valid descriptor and NUL-terminated name; the mode is
        // passed as the variadic third argument open(2) expects.
        fd(unsafe { libc::openat(self.0.as_raw_fd(), c.as_ptr(), flags, mode as libc::c_uint) })
            .map(File::from)
    }

    /// `rename(2)` from a name here to a name in `to`, both by descriptor.
    pub fn rename(&self, from: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
        let (a, b) = (name(from)?, name(to_name)?);
        // SAFETY: valid descriptors and NUL-terminated names.
        ok(unsafe { libc::renameat(self.0.as_raw_fd(), a.as_ptr(), to.0.as_raw_fd(), b.as_ptr()) })
    }

    /// `link(2)`, which refuses an existing destination name.
    pub fn link(&self, from: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
        let (a, b) = (name(from)?, name(to_name)?);
        // SAFETY: valid descriptors and NUL-terminated names; flag 0 never
        // follows a symlink source.
        ok(unsafe {
            libc::linkat(
                self.0.as_raw_fd(),
                a.as_ptr(),
                to.0.as_raw_fd(),
                b.as_ptr(),
                0,
            )
        })
    }

    pub fn unlink(&self, name_: &OsStr) -> io::Result<()> {
        let c = name(name_)?;
        // SAFETY: valid descriptor and NUL-terminated name.
        ok(unsafe { libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), 0) })
    }

    pub fn rmdir(&self, name_: &OsStr) -> io::Result<()> {
        let c = name(name_)?;
        // SAFETY: valid descriptor and NUL-terminated name.
        ok(unsafe { libc::unlinkat(self.0.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) })
    }

    /// Another descriptor for this same directory.
    pub fn try_clone(&self) -> io::Result<Dir> {
        Ok(Dir(self.0.try_clone()?))
    }

    /// The directory's own mode, set through the descriptor held, never
    /// through its name.
    pub fn set_mode(&self, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        File::from(self.0.try_clone()?).set_permissions(std::fs::Permissions::from_mode(mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every step accepts a real directory and refuses a symlink in its
    /// place — including one the test plants where a directory would
    /// otherwise be created.
    #[test]
    fn a_directory_is_reached_and_a_symlink_never_is() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = Dir::open(root.path()).unwrap();
        let (made, created) = dir.child(OsStr::new("made")).unwrap().unwrap();
        assert!(created && root.path().join("made").is_dir());
        let (_, created) = dir.child(OsStr::new("made")).unwrap().unwrap();
        assert!(!created);
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        assert!(dir.child(OsStr::new("link")).unwrap().is_none());
        std::fs::write(root.path().join("file"), b"x").unwrap();
        assert!(dir.child(OsStr::new("file")).unwrap().is_none());
        assert!(matches!(
            dir.entry(OsStr::new("link")).unwrap(),
            Entry::Present(None)
        ));
        assert!(matches!(
            dir.entry(OsStr::new("gone")).unwrap(),
            Entry::Absent
        ));
        assert!(matches!(
            dir.entry(OsStr::new("file")).unwrap(),
            Entry::Present(Some(_))
        ));

        let mut f = made.create(OsStr::new("t"), 0o600).unwrap();
        std::io::Write::write_all(&mut f, b"bytes").unwrap();
        assert!(
            made.create(OsStr::new("t"), 0o600).is_err(),
            "O_EXCL refuses an existing name"
        );
        made.link(OsStr::new("t"), &dir, OsStr::new("linked"))
            .unwrap();
        assert!(
            made.link(OsStr::new("t"), &dir, OsStr::new("linked"))
                .is_err()
        );
        made.rename(OsStr::new("t"), &dir, OsStr::new("renamed"))
            .unwrap();
        assert_eq!(
            std::fs::read(root.path().join("renamed")).unwrap(),
            b"bytes"
        );
        dir.unlink(OsStr::new("linked")).unwrap();
        assert!(dir.rmdir(OsStr::new("made")).is_ok());
        assert!(dir.child(OsStr::new("../x")).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    /// The anchor is reached through the symlinks above it — a tempdir
    /// on macOS lives under `/var`, itself a symlink — but a symlink
    /// standing in the anchor's own place is refused.
    #[test]
    fn an_anchor_replaced_by_a_symlink_is_refused_and_one_above_it_is_not() {
        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let real = base.path().join("real");
        std::fs::create_dir(real.join("")).unwrap();
        std::os::unix::fs::symlink(base.path(), outside.path().join("above")).unwrap();
        assert!(Dir::open(&outside.path().join("above").join("real")).is_ok());
        let anchor = base.path().join("project");
        std::os::unix::fs::symlink(outside.path(), &anchor).unwrap();
        assert!(Dir::open(&anchor).is_err());
        assert!(Dir::open(Path::new("/")).is_ok());
    }
}
