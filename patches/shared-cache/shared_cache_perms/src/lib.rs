#![deny(missing_docs)]
// `fs-err` depends on this crate, so it uses `std::fs` directly.
#![allow(clippy::disallowed_methods)]

//! Keep cache directories that several users share usable by all of them,
//! whatever their umask.
//!
//! Nothing happens until [`activate`] is called. After that, entries created
//! under one of the registered roots (and not under an excluded path) get the
//! permissions the active [`Mode`] adds. With [`Mode::world`], directories get
//! mode `0777` and files `0666` (`0777` when the owner can execute them), so
//! every user on the host can read and replace them.
//!
//! Directories and files that other processes may open concurrently are
//! created under a temporary name, fixed up, and then renamed into place
//! without replacing anything, so another user never sees them with the
//! umask's permissions. Everything outside the registered roots is left
//! alone.
//!
//! Patched copies of `fs-err`, `tempfile`, rattler and `cacache` call into
//! this crate where they create files and directories.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

/// What sharing does to the permissions of what is created in a shared cache.
///
/// Every permission change this crate makes, and the git configuration from
/// [`git_config`], take their bits from the active mode, so this is the one
/// place that decides them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    /// Bits added to every directory.
    pub dir_bits: u32,
    /// Bits added to every file.
    pub file_bits: u32,
    /// Bits added to a file whose owner can execute it.
    pub exec_bits: u32,
    /// git's `core.sharedRepository` for repositories in the cache.
    pub git_shared_repository: &'static str,
}

impl Mode {
    /// Share with every user on the host: directories `0777`, files `0666`,
    /// and execute for everyone on files the owner can execute. There is no
    /// sticky bit, so users can replace each other's lock and temporary files.
    pub const fn world() -> Self {
        Self {
            dir_bits: 0o777,
            file_bits: 0o666,
            exec_bits: 0o111,
            git_shared_repository: "0666",
        }
    }
}

struct Shared {
    mode: Mode,
    uid: u32,
    roots: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();

/// Share everything created under `roots`, except under `excluded`, as
/// `mode` says from now on. Returns `false` if sharing was already activated.
pub fn activate(
    mode: Mode,
    roots: impl IntoIterator<Item = PathBuf>,
    excluded: impl IntoIterator<Item = PathBuf>,
) -> bool {
    let shared = Shared {
        mode,
        uid: imp::euid(),
        roots: roots.into_iter().filter_map(|p| normalize(&p)).collect(),
        excluded: excluded.into_iter().filter_map(|p| normalize(&p)).collect(),
    };
    SHARED.set(shared).is_ok()
}

/// Returns `true` once [`activate`] has been called.
pub fn is_active() -> bool {
    SHARED.get().is_some()
}

/// git configuration (key, value) for commands that write repositories in the
/// cache, empty unless sharing is active: `core.sharedRepository` from the
/// mode, and `safe.directory=*`, since git refuses to work in a repository
/// that another user owns.
pub fn git_config() -> Vec<(&'static str, &'static str)> {
    match SHARED.get() {
        Some(shared) => vec![
            ("core.sharedRepository", shared.mode.git_shared_repository),
            ("safe.directory", "*"),
        ],
        None => Vec::new(),
    }
}

/// Returns `true` if sharing is active and `path` lies under a shared root.
pub fn applies_to(path: &Path) -> bool {
    shared_for(path).is_some()
}

fn shared_for(path: &Path) -> Option<&'static Shared> {
    let shared = SHARED.get()?;
    let path = normalize(path)?;
    let under = |p: &PathBuf| path.starts_with(p);
    (shared.roots.iter().any(under) && !shared.excluded.iter().any(under)).then_some(shared)
}

/// Makes `path` absolute and resolves `.` and `..` lexically.
fn normalize(path: &Path) -> Option<PathBuf> {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir().ok()?.join(path);
        &absolute
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Some(out)
}

/// Creates the directory `path` with permission bits `mode` (before the umask,
/// usually `0o777`). Under a shared root, the directory appears with the
/// shared permissions in one step.
pub fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    match shared_for(path) {
        Some(shared) => imp::create_dir(shared, path, mode),
        None => imp::plain_create_dir(path, mode, false),
    }
}

/// Like [`create_dir`], but also creates missing parents, like
/// [`std::fs::create_dir_all`]. Parents above the shared root are created as
/// usual.
pub fn create_dir_all(path: &Path, mode: u32) -> io::Result<()> {
    // Even an excluded path can have shared parents, so with sharing active
    // every missing component decides for itself.
    if SHARED.get().is_none() || path.is_dir() {
        return imp::plain_create_dir(path, mode, true);
    }
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || dir.is_dir() {
            break;
        }
        missing.push(dir);
        current = dir.parent();
    }
    for dir in missing.into_iter().rev() {
        let result = match shared_for(dir) {
            Some(shared) => imp::create_dir(shared, dir, mode),
            None => imp::plain_create_dir(dir, mode, false),
        };
        match result {
            Err(_) if dir.is_dir() => {}
            other => other?,
        }
    }
    Ok(())
}

/// Opens `path` with `options`, whose `create` and `create_new` flags the
/// caller passes along (they cannot be read back from [`OpenOptions`]).
///
/// Under a shared root, a file that does not exist yet is created under a
/// temporary name, given the shared permissions, and renamed into place without
/// replacing anything, so another user never opens it with the umask's
/// permissions.
pub fn open(
    options: &OpenOptions,
    path: &Path,
    create: bool,
    create_new: bool,
) -> io::Result<File> {
    match shared_for(path) {
        Some(shared) if create || create_new => imp::open(shared, options, path, create_new),
        _ => options.open(path),
    }
}

/// Gives a file that was just created at `path` the shared permissions, if `path` is under a shared root and the file is ours.
pub fn fix_file(file: &File, path: &Path) -> io::Result<()> {
    match shared_for(path) {
        Some(shared) => imp::fix_file(shared, file),
        None => Ok(()),
    }
}

/// Like [`fix_file`] for a path, file or directory. Symbolic links are left
/// alone.
pub fn fix_path(path: &Path) -> io::Result<()> {
    match shared_for(path) {
        Some(shared) => imp::fix_path(shared, path),
        None => Ok(()),
    }
}

/// Applies [`fix_path`] to `path` and, if it is a directory, to everything
/// inside it, for trees written by code that does not go through this crate
/// (archive extraction, build backends). Entries owned by other users are
/// skipped.
pub fn fix_tree(path: &Path) -> io::Result<()> {
    match shared_for(path) {
        Some(shared) => imp::fix_tree(shared, path),
        None => Ok(()),
    }
}

#[cfg(unix)]
mod imp {
    use std::{
        ffi::{CString, OsString},
        fs::{self, File, OpenOptions},
        io,
        os::{
            fd::AsRawFd,
            unix::{
                ffi::OsStrExt,
                fs::{DirBuilderExt, MetadataExt},
            },
        },
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{Mode, Shared};

    pub(super) fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// The mode an entry should have: its current mode plus the bits `shared`
    /// adds for its kind.
    pub(super) fn wanted_mode(shared: &Mode, mode: u32, is_dir: bool) -> u32 {
        let perm = mode & 0o7777;
        if is_dir {
            perm | shared.dir_bits
        } else if perm & 0o100 != 0 {
            perm | shared.file_bits | shared.exec_bits
        } else {
            perm | shared.file_bits
        }
    }

    fn cstr(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
    }

    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn fix(
        shared: &Shared,
        meta: &fs::Metadata,
        chmod: impl FnOnce(u32) -> io::Result<()>,
    ) -> io::Result<()> {
        // Only the owner can change an entry; entries of other users were
        // fixed by whoever created them.
        if meta.uid() != shared.uid || meta.file_type().is_symlink() {
            return Ok(());
        }
        let mode = meta.mode() & 0o7777;
        let want = wanted_mode(&shared.mode, mode, meta.is_dir());
        if want != mode {
            chmod(want)?;
        }
        Ok(())
    }

    pub(super) fn fix_file(shared: &Shared, file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        fix(
            shared,
            &file.metadata()?,
            // SAFETY: `fd` is an open descriptor.
            |mode| check(unsafe { libc::fchmod(fd, mode as libc::mode_t) }),
        )
    }

    pub(super) fn fix_path(shared: &Shared, path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        let c = cstr(path)?;
        fix(
            shared,
            &meta,
            // SAFETY: `c` is a valid C string.
            |mode| check(unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) }),
        )
    }

    pub(super) fn fix_tree(shared: &Shared, path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        let _ = fix_path(shared, path);
        if meta.is_dir() {
            let mut stack = vec![path.to_path_buf()];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let child = entry.path();
                    let _ = fix_path(shared, &child);
                    if entry.file_type().is_ok_and(|t| t.is_dir()) {
                        stack.push(child);
                    }
                }
            }
        }
        Ok(())
    }

    /// A unique name next to `path` (short, so it never exceeds `NAME_MAX`).
    fn temp_sibling(path: &Path) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = OsString::from(format!(
            ".sgtmp-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        path.with_file_name(name)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (cstr(from)?, cstr(to)?);
        // SAFETY: both paths are valid C strings.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(target_vendor = "apple")]
    fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (cstr(from)?, cstr(to)?);
        // SAFETY: both paths are valid C strings.
        check(unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) })
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }

    fn is_unsupported(err: &io::Error) -> bool {
        matches!(
            err.raw_os_error(),
            Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
        ) || err.raw_os_error() == Some(libc::EOPNOTSUPP)
    }

    fn already_exists() -> io::Error {
        io::Error::from_raw_os_error(libc::EEXIST)
    }

    pub(super) fn plain_create_dir(path: &Path, mode: u32, recursive: bool) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(recursive)
            .mode(mode)
            .create(path)
    }

    pub(super) fn create_dir(shared: &Shared, path: &Path, mode: u32) -> io::Result<()> {
        // Report an existing entry like mkdir does, even where we could not
        // create the temporary directory.
        if fs::symlink_metadata(path).is_ok() {
            return Err(already_exists());
        }
        let temp = temp_sibling(path);
        plain_create_dir(&temp, mode, false)?;
        let _ = fix_path(shared, &temp);
        match rename_noreplace(&temp, path) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = fs::remove_dir(&temp);
                if err.raw_os_error() == Some(libc::EEXIST) {
                    Err(already_exists())
                } else if is_unsupported(&err) {
                    plain_create_dir(path, mode, false)?;
                    let _ = fix_path(shared, path);
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    pub(super) fn open(
        shared: &Shared,
        options: &OpenOptions,
        path: &Path,
        create_new: bool,
    ) -> io::Result<File> {
        let mut existing = options.clone();
        existing.create(false).create_new(false);
        let mut fresh = options.clone();
        fresh.create(false).create_new(true);
        // Retry if the file disappears between failing to create it and
        // opening it; give up on the atomic creation after a few rounds.
        for _ in 0..8 {
            if create_new {
                if fs::symlink_metadata(path).is_ok() {
                    return Err(already_exists());
                }
            } else {
                match existing.open(path) {
                    Ok(file) => return Ok(file),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
            let temp = temp_sibling(path);
            let file = fresh.open(&temp)?;
            let _ = fix_file(shared, &file);
            match rename_noreplace(&temp, path) {
                Ok(()) => return Ok(file),
                Err(err) => {
                    let _ = fs::remove_file(&temp);
                    if err.raw_os_error() == Some(libc::EEXIST) {
                        if create_new {
                            return Err(already_exists());
                        }
                    } else if is_unsupported(&err) {
                        break;
                    } else {
                        return Err(err);
                    }
                }
            }
        }
        let file = options.open(path)?;
        let _ = fix_file(shared, &file);
        Ok(file)
    }
}

#[cfg(not(unix))]
mod imp {
    use std::{
        fs::{self, File, OpenOptions},
        io,
        path::Path,
    };

    use super::Shared;

    pub(super) fn euid() -> u32 {
        0
    }
    pub(super) fn plain_create_dir(path: &Path, _mode: u32, recursive: bool) -> io::Result<()> {
        fs::DirBuilder::new().recursive(recursive).create(path)
    }
    pub(super) fn create_dir(_: &Shared, path: &Path, mode: u32) -> io::Result<()> {
        plain_create_dir(path, mode, false)
    }
    pub(super) fn open(
        _: &Shared,
        options: &OpenOptions,
        path: &Path,
        _create_new: bool,
    ) -> io::Result<File> {
        options.open(path)
    }
    pub(super) fn fix_file(_: &Shared, _: &File) -> io::Result<()> {
        Ok(())
    }
    pub(super) fn fix_path(_: &Shared, _: &Path) -> io::Result<()> {
        Ok(())
    }
    pub(super) fn fix_tree(_: &Shared, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    use super::*;

    /// One shared root per test process (activation is process-wide), with
    /// umask 022 like most users have.
    fn root() -> &'static Path {
        static ROOT: OnceLock<PathBuf> = OnceLock::new();
        ROOT.get_or_init(|| {
            // SAFETY: umask has no preconditions.
            unsafe { libc::umask(0o022) };
            let base = std::env::temp_dir()
                .join(format!("shared-cache-perms-test-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            let root = base.join("cache");
            assert!(activate(Mode::world(), [root.clone()], [root.join("envs")]));
            root
        })
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn leftovers(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".sgtmp-")
            })
            .collect()
    }

    #[test]
    fn wanted_modes() {
        let world = Mode::world();
        assert_eq!(imp::wanted_mode(&world, 0o644, false), 0o666);
        assert_eq!(imp::wanted_mode(&world, 0o755, false), 0o777);
        assert_eq!(imp::wanted_mode(&world, 0o600, false), 0o666);
        assert_eq!(imp::wanted_mode(&world, 0o700, false), 0o777);
        assert_eq!(imp::wanted_mode(&world, 0o755, true), 0o777);
        assert_eq!(imp::wanted_mode(&world, 0o700, true), 0o777);
    }

    #[test]
    fn directories_are_world_writable() {
        let root = root();
        let deep = root.join("dirs/a/b/c");
        create_dir_all(&deep, 0o777).unwrap();
        for dir in [
            root.to_path_buf(),
            root.join("dirs"),
            root.join("dirs/a"),
            deep.clone(),
        ] {
            assert_eq!(mode(&dir), 0o777, "{}", dir.display());
        }
        // The parent of the root is not shared.
        assert_eq!(mode(root.parent().unwrap()), 0o755);

        let err = create_dir(&deep, 0o777).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        create_dir_all(&deep, 0o777).unwrap();
        assert!(leftovers(&root.join("dirs/a/b")).is_empty());
    }

    #[test]
    fn files_are_world_read_write() {
        let dir = root().join("files");
        create_dir_all(&dir, 0o777).unwrap();
        let path = dir.join("x.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        let file = open(&options, &path, true, false).unwrap();
        drop(file);
        assert_eq!(mode(&path), 0o666);

        // An existing file is opened, not replaced.
        let inode = fs::metadata(&path).unwrap().ino();
        open(&options, &path, true, false).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);

        let mut new = OpenOptions::new();
        new.write(true).create_new(true);
        let err = open(&new, &path, false, true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        let fresh = dir.join("fresh");
        open(&new, &fresh, false, true).unwrap();
        assert_eq!(mode(&fresh), 0o666);
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn concurrent_creation_yields_one_entry() {
        let dir = root().join("race/x/y");
        let lock = dir.join(".lock");
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (dir, lock) = (dir.clone(), lock.clone());
                std::thread::spawn(move || {
                    create_dir_all(&dir, 0o777).unwrap();
                    let mut options = OpenOptions::new();
                    options.read(true).write(true).create(true);
                    open(&options, &lock, true, false)
                        .unwrap()
                        .metadata()
                        .unwrap()
                        .ino()
                })
            })
            .collect();
        let inodes: Vec<u64> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(inodes.iter().all(|&i| i == inodes[0]));
        assert_eq!(mode(&lock), 0o666);
        assert_eq!(mode(&dir), 0o777);
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn excluded_and_outside_paths_are_untouched() {
        let root = root();
        let envs = root.join("envs/default");
        create_dir_all(&envs, 0o777).unwrap();
        assert_eq!(mode(&root.join("envs")) & 0o777, 0o755);
        assert_eq!(mode(&envs) & 0o777, 0o755);
        let outside = root.parent().unwrap().join("outside/a");
        create_dir_all(&outside, 0o777).unwrap();
        assert_eq!(mode(&outside), 0o755);
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        let file = envs.join("f");
        open(&options, &file, true, false).unwrap();
        assert_eq!(mode(&file), 0o644);
        assert!(!applies_to(&root.join("envs/../envs/x")));
        assert!(applies_to(&root.join("envs/../pkgs")));
    }

    #[test]
    fn fix_tree_fixes_foreign_writes() {
        let dir = root().join("tree");
        create_dir_all(&dir, 0o777).unwrap();
        let tree = dir.join("pkg");
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::write(tree.join("bin/tool"), b"x").unwrap();
        fs::set_permissions(tree.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(tree.join("data"), b"x").unwrap();
        fs::set_permissions(tree.join("data"), fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("data", tree.join("link")).unwrap();
        fix_tree(&tree).unwrap();
        assert_eq!(mode(&tree), 0o777);
        assert_eq!(mode(&tree.join("bin")), 0o777);
        assert_eq!(mode(&tree.join("bin/tool")), 0o777);
        assert_eq!(mode(&tree.join("data")), 0o666);
    }
}
