//! Private session directory: ownership checks, mode repair, and atomic writes.
//!
//! The directory fd is opened with `O_NOFOLLOW`, so a symlink swapped in for
//! the leaf is refused. Files are created with `O_EXCL` in the same directory
//! and published with `renameat`.

use anyhow::{Context, Result, bail};
use std::ffi::{CStr, CString};
use std::fs::{File, Permissions, TryLockError};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Largest session file this process will read or write.
pub const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct PrivateDir {
    file: File,
    path: PathBuf,
    readonly: bool,
}

#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

#[derive(Debug)]
pub enum ExclusiveError {
    Exists,
    Failed(anyhow::Error),
}

impl PrivateDir {
    /// Open `path` as a private directory owned by the effective uid.
    ///
    /// When `readonly` is false, missing parents are created and a directory
    /// whose mode has group or other bits is repaired to `0700`. When
    /// `readonly` is true, nothing is created and modes are left unchanged.
    pub fn open(path: &Path, readonly: bool) -> Result<Self> {
        if !readonly {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating directory {}", path.display()))?;
        }

        let c_path = cstring_bytes(path.as_os_str().as_bytes())
            .with_context(|| format!("opening {}", path.display()))?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `c_path` is a NUL-terminated path and the flags request a
        // directory open that does not follow a symlink at the leaf.
        let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            // Linux returns ENOTDIR (not ELOOP) for O_DIRECTORY|O_NOFOLLOW on a
            // symlink, because the link itself is not a directory.
            if err.raw_os_error() == Some(libc::ELOOP)
                || (err.raw_os_error() == Some(libc::ENOTDIR) && path_is_symlink(path))
            {
                bail!("refusing symlink at {}", path.display());
            }
            if err.raw_os_error() == Some(libc::ENOTDIR) {
                bail!("not a directory: {}", path.display());
            }
            return Err(err).with_context(|| format!("opening {}", path.display()));
        }
        // SAFETY: `fd` is a freshly opened descriptor owned exclusively here.
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        if !meta.is_dir() {
            bail!("not a directory: {}", path.display());
        }
        if meta.uid() != euid() {
            bail!(
                "refusing directory {} not owned by the current user",
                path.display()
            );
        }
        if !readonly {
            let mode = meta.mode() & 0o777;
            if (mode & 0o077) != 0 {
                file.set_permissions(Permissions::from_mode(0o700))
                    .with_context(|| format!("repairing mode of {}", path.display()))?;
            }
        }

        Ok(Self {
            file,
            path: path.to_path_buf(),
            readonly,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hold an exclusive lock until the guard is dropped, leaving `leaf` in place.
    pub fn try_lock(&self, leaf: &str) -> Result<Option<FileLock>> {
        if self.readonly {
            bail!("directory {} is read-only", self.path.display());
        }
        validate_leaf(leaf)?;
        let leaf_c = cstring_bytes(leaf.as_bytes()).context("file name")?;
        let flags =
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: `leaf_c` is a single component relative to the opened directory.
        let fd = unsafe { libc::openat(self.dir_fd(), leaf_c.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("opening lock file {leaf}"));
        }
        // SAFETY: `fd` is a freshly opened descriptor owned exclusively here.
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().with_context(|| format!("stat {leaf}"))?;
        if !meta.is_file() {
            bail!("refusing non-regular file {leaf}");
        }
        if meta.uid() != euid() {
            bail!("refusing foreign-owned file {leaf}");
        }
        if (meta.mode() & 0o077) != 0 {
            file.set_permissions(Permissions::from_mode(0o600))
                .with_context(|| format!("repairing mode of {leaf}"))?;
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(FileLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err).with_context(|| format!("locking {leaf}")),
        }
    }

    /// Atomically replace `leaf` with `bytes`.
    ///
    /// The temporary name is hidden (`.<leaf>.<pid>.<nanos>.tmp`) so directory
    /// listings that skip dotfiles never observe it. A leaf that already
    /// starts with `.` keeps a single leading dot, so the name never contains
    /// `..`. Any failure before
    /// `renameat` unlinks that temporary.
    pub fn atomic_write(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        self.atomic_write_inner(leaf, bytes, false)
    }

    /// Create `leaf` only when that name is absent, then write `bytes`.
    ///
    /// `O_CREAT|O_EXCL|O_NOFOLLOW` claims the directory entry. An existing
    /// name, including a symlink, is left untouched. The new fd is `fchmod`'d
    /// to `0600` by [`write_temp`], so the mode does not follow the umask. A
    /// crash after the create and before the write can leave an empty file;
    /// presence is the claim.
    pub fn create_exclusive(
        &self,
        leaf: &str,
        bytes: &[u8],
    ) -> std::result::Result<(), ExclusiveError> {
        if self.readonly {
            return Err(ExclusiveError::Failed(anyhow::Error::msg(format!(
                "directory {} is read-only",
                self.path.display()
            ))));
        }
        validate_leaf(leaf).map_err(ExclusiveError::Failed)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
            return Err(ExclusiveError::Failed(anyhow::Error::msg(format!(
                "refusing to write {leaf}: exceeds {MAX_SNAPSHOT_BYTES} bytes"
            ))));
        }
        let mut file = self.open_exclusive(leaf)?;
        write_temp(&mut file, bytes).map_err(ExclusiveError::Failed)?;
        drop(file);
        self.file.sync_all().map_err(|err| {
            ExclusiveError::Failed(anyhow::Error::msg(format!(
                "syncing directory {}: {err}",
                self.path.display()
            )))
        })?;
        Ok(())
    }

    fn open_exclusive(&self, leaf: &str) -> std::result::Result<File, ExclusiveError> {
        let leaf_c = cstring_bytes(leaf.as_bytes()).map_err(ExclusiveError::Failed)?;
        let flags =
            libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_WRONLY;
        // SAFETY: `leaf_c` is a single path component and `dir_fd` refers to
        // the opened directory. `O_NOFOLLOW|O_EXCL` refuses a pre-existing name.
        let fd = unsafe { libc::openat(self.dir_fd(), leaf_c.as_ptr(), flags, 0o600) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                return Err(ExclusiveError::Exists);
            }
            return Err(ExclusiveError::Failed(anyhow::Error::msg(format!(
                "creating {leaf}: {err}"
            ))));
        }
        // SAFETY: `fd` is a freshly created descriptor owned exclusively here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(test)]
    pub fn atomic_write_fail_before_rename(&self, leaf: &str, bytes: &[u8]) -> Result<()> {
        self.atomic_write_inner(leaf, bytes, true)
    }

    fn atomic_write_inner(&self, leaf: &str, bytes: &[u8], fail_before_rename: bool) -> Result<()> {
        if self.readonly {
            bail!("directory {} is read-only", self.path.display());
        }
        validate_leaf(leaf)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
            bail!("refusing to write {leaf}: exceeds {MAX_SNAPSHOT_BYTES} bytes");
        }

        let tmp = temp_name(leaf);
        validate_leaf(&tmp)?;
        let tmp_c = cstring_bytes(tmp.as_bytes()).context("temp file name")?;
        let flags =
            libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_WRONLY;
        // SAFETY: `tmp_c` is a single path component and `dir_fd` refers to the
        // opened directory. `O_NOFOLLOW|O_EXCL` refuses a pre-existing symlink.
        let fd = unsafe { libc::openat(self.dir_fd(), tmp_c.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("creating temp file {tmp}"));
        }
        // SAFETY: `fd` is a freshly created regular file owned exclusively here.
        let mut file = unsafe { File::from_raw_fd(fd) };

        if let Err(err) = write_temp(&mut file, bytes) {
            drop(file);
            self.unlink_cstr(&tmp_c);
            return Err(err);
        }
        drop(file);
        if fail_before_rename {
            self.unlink_cstr(&tmp_c);
            bail!("injected failure before rename");
        }

        let leaf_c = cstring_bytes(leaf.as_bytes()).context("file name")?;
        // SAFETY: both names are single components in `dir_fd`; rename replaces
        // the destination directory entry without following a final symlink.
        let rc = unsafe {
            libc::renameat(
                self.dir_fd(),
                tmp_c.as_ptr(),
                self.dir_fd(),
                leaf_c.as_ptr(),
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            self.unlink_cstr(&tmp_c);
            return Err(err).with_context(|| format!("renaming {tmp} to {leaf}"));
        }
        self.file
            .sync_all()
            .with_context(|| format!("syncing directory {}", self.path.display()))?;
        Ok(())
    }

    /// Read a regular file owned by the effective uid.
    ///
    /// Missing leaves are `Ok(None)`. Symlinks, fifos, and other non-regular
    /// files are errors. Group or other permissions are repaired to `0600`
    /// unless this directory was opened read-only.
    pub fn read_regular(&self, leaf: &str) -> Result<Option<Vec<u8>>> {
        validate_leaf(leaf)?;
        let leaf_c = cstring_bytes(leaf.as_bytes()).context("file name")?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: `leaf_c` is a single component. `O_NOFOLLOW` refuses a
        // symlink and `O_NONBLOCK` keeps a fifo from blocking in `open`.
        let fd = unsafe { libc::openat(self.dir_fd(), leaf_c.as_ptr(), flags) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            if err.raw_os_error() == Some(libc::ELOOP) {
                bail!("refusing symlink {leaf}");
            }
            return Err(err).with_context(|| format!("opening {leaf}"));
        }
        // SAFETY: `fd` is a freshly opened descriptor owned exclusively here.
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata().with_context(|| format!("stat {leaf}"))?;
        if !meta.is_file() {
            bail!("refusing non-regular file {leaf}");
        }
        if meta.uid() != euid() {
            bail!("refusing foreign-owned file {leaf}");
        }
        if !self.readonly && (meta.mode() & 0o077) != 0 {
            file.set_permissions(Permissions::from_mode(0o600))
                .with_context(|| format!("repairing mode of {leaf}"))?;
        }
        if meta.len() > MAX_SNAPSHOT_BYTES {
            bail!("file {leaf} exceeds {MAX_SNAPSHOT_BYTES} bytes");
        }
        let mut buf = Vec::new();
        let read = file
            .take(MAX_SNAPSHOT_BYTES.saturating_add(1))
            .read_to_end(&mut buf)
            .with_context(|| format!("reading {leaf}"))?;
        if u64::try_from(read).unwrap_or(u64::MAX) > MAX_SNAPSHOT_BYTES {
            bail!("file {leaf} exceeds {MAX_SNAPSHOT_BYTES} bytes");
        }
        Ok(Some(buf))
    }

    pub fn exists_regular(&self, leaf: &str) -> bool {
        match self.read_metadata(leaf) {
            Ok(Some(meta)) => meta.is_file() && meta.uid() == euid(),
            _ => false,
        }
    }

    pub fn unlink(&self, leaf: &str) -> Result<()> {
        if self.readonly {
            bail!("directory {} is read-only", self.path.display());
        }
        validate_leaf(leaf)?;
        let leaf_c = cstring_bytes(leaf.as_bytes()).context("file name")?;
        // SAFETY: `leaf_c` is a single component in the opened directory.
        let rc = unsafe { libc::unlinkat(self.dir_fd(), leaf_c.as_ptr(), 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(());
            }
            return Err(err).with_context(|| format!("deleting {leaf}"));
        }
        Ok(())
    }

    pub fn list_names(&self) -> Result<Vec<String>> {
        // SAFETY: `dir_fd` is a live directory descriptor. The duplicate is
        // what `fdopendir` consumes.
        let fd = unsafe { libc::dup(self.dir_fd()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicating directory fd");
        }
        // SAFETY: `fd` is an owned duplicate of a directory descriptor.
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            // SAFETY: `fdopendir` did not take ownership, so close the duplicate.
            unsafe { libc::close(fd) };
            return Err(std::io::Error::last_os_error()).context("opening directory stream");
        }
        let stream = DirStream(dir);
        let mut names = Vec::new();
        loop {
            // SAFETY: `__errno_location` returns this thread's errno cell.
            unsafe {
                *libc::__errno_location() = 0;
            }
            // SAFETY: `stream` owns a live `DIR` for this loop.
            let ent = unsafe { libc::readdir(stream.0) };
            if ent.is_null() {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error().unwrap_or(0) != 0 {
                    return Err(err).context("reading directory");
                }
                break;
            }
            // SAFETY: `readdir` returned a non-null entry whose `d_name` is
            // NUL-terminated for the lifetime of the next `readdir` call.
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
            let Ok(name) = name.to_str() else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            names.push(name.to_string());
        }
        drop(stream);
        names.sort();
        Ok(names)
    }

    fn read_metadata(&self, leaf: &str) -> Result<Option<std::fs::Metadata>> {
        validate_leaf(leaf)?;
        let leaf_c = cstring_bytes(leaf.as_bytes()).context("file name")?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        // SAFETY: same constraints as `read_regular`: one component, no follow.
        let fd = unsafe { libc::openat(self.dir_fd(), leaf_c.as_ptr(), flags) };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(err).with_context(|| format!("opening {leaf}"));
        }
        // SAFETY: `fd` is freshly opened and owned exclusively here.
        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Some(
            file.metadata().with_context(|| format!("stat {leaf}"))?,
        ))
    }

    fn dir_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn unlink_cstr(&self, name: &CStr) {
        // SAFETY: `name` is a single component previously created in this dir.
        unsafe {
            libc::unlinkat(self.dir_fd(), name.as_ptr(), 0);
        }
    }
}

struct DirStream(*mut libc::DIR);

impl Drop for DirStream {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from `fdopendir` and is closed once.
            unsafe {
                libc::closedir(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

fn path_is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}
fn euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and does not follow user data.
    unsafe { libc::geteuid() }
}

fn cstring_bytes(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).context("path component contains a NUL byte")
}

fn validate_leaf(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
        || name.contains("..")
    {
        bail!("invalid file name '{name}'");
    }
    Ok(())
}

fn temp_name(leaf: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let body = leaf.strip_prefix('.').unwrap_or(leaf);
    format!(".{body}.{}.{nanos}.tmp", std::process::id())
}

fn write_temp(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.set_permissions(Permissions::from_mode(0o600))
        .context("setting file mode to 0600")?;
    file.write_all(bytes).context("writing file")?;
    file.sync_all().context("syncing file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    fn writable_dir() -> (tempfile::TempDir, PrivateDir) {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("sessions");
        let store = PrivateDir::open(&path, false).unwrap();
        (parent, store)
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn lock_guard_is_exclusive_and_preserves_leaf_after_drop() {
        let (_parent, store) = writable_dir();
        store.atomic_write("lock", b"sentinel").unwrap();
        let path = store.path().join("lock");
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        let guard = store.try_lock("lock").unwrap().unwrap();
        assert_eq!(mode_of(&path), 0o600);
        let other = PrivateDir::open(store.path(), false).unwrap();
        drop(store);
        assert!(other.try_lock("lock").unwrap().is_none());
        drop(guard);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
        let _guard = other.try_lock("lock").unwrap().unwrap();
        assert!(other.try_lock("lock").unwrap().is_none());
    }

    #[test]
    fn lock_refuses_unsafe_leaves_and_readonly_directory() {
        let (parent, store) = writable_dir();
        let outside = parent.path().join("outside");
        std::fs::write(&outside, b"sentinel").unwrap();
        symlink(&outside, store.path().join("link")).unwrap();
        assert!(store.try_lock("link").is_err());
        assert!(store.try_lock("../outside").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
        std::fs::create_dir(store.path().join("dir")).unwrap();
        assert!(store.try_lock("dir").is_err());
        let readonly = PrivateDir::open(store.path(), true).unwrap();
        assert!(readonly.try_lock("missing").is_err());
        assert!(!store.path().join("missing").exists());
    }

    #[test]
    fn lock_refuses_fifo_promptly() {
        let (_parent, store) = writable_dir();
        let fifo = store.path().join("pipe");
        let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path in a directory we created.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
        let started = Instant::now();
        let err = store.try_lock("pipe").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{err:#}").contains("non-regular"), "{err:#}");
    }

    #[test]
    fn symlink_leaf_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, Permissions::from_mode(0o755)).unwrap();
        let link = parent.path().join("link");
        symlink(&real, &link).unwrap();

        let err = PrivateDir::open(&link, false).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
        assert_eq!(mode_of(&real), 0o755);
    }

    #[test]
    fn fifo_is_refused_promptly() {
        let (_parent, store) = writable_dir();
        let fifo = store.path().join("pipe");
        let c_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path in a directory we created.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());

        let started = Instant::now();
        let result = store.read_regular("pipe");
        assert!(started.elapsed() < Duration::from_secs(1));
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("non-regular"), "{err:#}");
    }

    #[test]
    fn repairs_group_or_other_permissions_to_0600() {
        let (_parent, store) = writable_dir();
        let path = store.path().join("note.toml");
        std::fs::write(&path, b"hello").unwrap();
        for mode in [0o644, 0o666, 0o640, 0o604] {
            std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
            let data = store.read_regular("note.toml").unwrap().unwrap();
            assert_eq!(data, b"hello");
            assert_eq!(mode_of(&path), 0o600);
        }
    }

    #[test]
    fn directory_mode_repaired_to_0700() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("sessions");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();

        let _store = PrivateDir::open(&path, false).unwrap();
        assert_eq!(mode_of(&path), 0o700);
    }

    #[test]
    fn oversized_file_is_refused() {
        let (_parent, store) = writable_dir();
        let path = store.path().join("big.toml");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_SNAPSHOT_BYTES + 1).unwrap();

        let err = store.read_regular("big.toml").unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
    }

    #[test]
    fn missing_leaf_is_none() {
        let (_parent, store) = writable_dir();
        assert!(store.read_regular("missing.toml").unwrap().is_none());
    }

    #[test]
    fn readonly_repairs_nothing() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("sessions");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
        let file = path.join("session.toml");
        std::fs::write(&file, b"hello").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();

        let store = PrivateDir::open(&path, true).unwrap();
        assert!(store.readonly);
        assert_eq!(mode_of(&path), 0o755);
        let data = store.read_regular("session.toml").unwrap().unwrap();
        assert_eq!(data, b"hello");
        assert_eq!(mode_of(&file), 0o644);
    }

    #[test]
    fn temp_cleaned_on_pre_rename_failure() {
        let (_parent, store) = writable_dir();
        store.atomic_write("keep.toml", b"old").unwrap();

        let err = store
            .atomic_write_fail_before_rename("keep.toml", b"new")
            .unwrap_err();
        assert!(format!("{err:#}").contains("before rename"), "{err:#}");
        assert_eq!(store.read_regular("keep.toml").unwrap().unwrap(), b"old");

        let names = store.list_names().unwrap();
        assert!(
            names.iter().all(|name| Path::new(name)
                .extension()
                .is_none_or(|ext| !ext.eq_ignore_ascii_case("tmp"))),
            "{names:?}"
        );
    }

    #[test]
    fn atomic_replace_is_old_or_new() {
        let (_parent, store) = writable_dir();
        store.atomic_write("session.toml", b"old-content").unwrap();
        assert_eq!(
            store.read_regular("session.toml").unwrap().unwrap(),
            b"old-content"
        );

        store.atomic_write("session.toml", b"new-content").unwrap();
        let got = store.read_regular("session.toml").unwrap().unwrap();
        assert_eq!(got, b"new-content");
        assert_eq!(mode_of(&store.path().join("session.toml")), 0o600);

        let names = store.list_names().unwrap();
        assert!(names.iter().all(|name| !name.contains(".tmp")), "{names:?}");
        assert!(names.contains(&"session.toml".to_string()));
    }

    #[test]
    fn create_exclusive_refuses_to_replace_or_follow_symlink() {
        let (parent, store) = writable_dir();
        let outside = parent.path().join("outside");
        std::fs::write(&outside, b"secret").unwrap();
        symlink(&outside, store.path().join("leaf")).unwrap();
        assert!(store.create_exclusive("leaf", b"new").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"secret");

        store.create_exclusive("note", b"first").unwrap();
        assert!(matches!(
            store.create_exclusive("note", b"second"),
            Err(ExclusiveError::Exists)
        ));
        assert_eq!(store.read_regular("note").unwrap().unwrap(), b"first");
        assert_eq!(mode_of(&store.path().join("note")), 0o600);
    }
}
