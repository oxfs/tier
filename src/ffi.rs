//! Narrow C boundary: Samba owns data descriptors; this module owns their locks.
use crate::{Reader, Store, Writer, config::Config};
use anyhow::{Result, ensure};
use std::{
    ffi::{CStr, OsStr},
    os::unix::{
        ffi::OsStrExt,
        fs::MetadataExt,
        io::{AsRawFd, IntoRawFd},
    },
    path::{Path, PathBuf},
    ptr,
};

enum Handle {
    Read(Reader),
    Write(Writer),
}
fn error(error: anyhow::Error) -> i32 {
    let number = error
        .chain()
        .find_map(|e| {
            e.downcast_ref::<std::io::Error>().map(|e| {
                e.raw_os_error().unwrap_or(match e.kind() {
                    std::io::ErrorKind::NotFound => libc::ENOENT,
                    std::io::ErrorKind::AlreadyExists => libc::EEXIST,
                    std::io::ErrorKind::WouldBlock => libc::EAGAIN,
                    std::io::ErrorKind::PermissionDenied => libc::EACCES,
                    std::io::ErrorKind::InvalidInput => libc::EINVAL,
                    _ => libc::EIO,
                })
            })
        })
        .unwrap_or(libc::EIO);
    if ![libc::ENOENT, libc::EEXIST, libc::EAGAIN].contains(&number) {
        eprintln!("tier: {error:#}");
    }
    number
}
fn call(f: impl FnOnce() -> Result<i32>) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            unsafe {
                *libc::__errno_location() = error(e);
            }
            -1
        }
        Err(_) => {
            unsafe {
                *libc::__errno_location() = libc::EIO;
            }
            -1
        }
    }
}
unsafe fn path<'a>(value: *const libc::c_char) -> &'a Path {
    Path::new(OsStr::from_bytes(
        unsafe { CStr::from_ptr(value) }.to_bytes(),
    ))
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_connect(config: *const libc::c_char, out: *mut *mut Store) -> i32 {
    call(|| {
        let store = Store::new(Config::load(unsafe { path(config) })?)?;
        store.verify_storage()?;
        unsafe {
            *out = Box::into_raw(Box::new(store));
        }
        Ok(0)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_disconnect(store: *mut Store) {
    if !store.is_null() {
        drop(unsafe { Box::from_raw(store) });
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn tier_verify_storage(store: *const Store) -> i32 {
    call(|| {
        unsafe { &*store }.verify_storage()?;
        Ok(0)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn tier_open(
    store: *const Store,
    name: *const libc::c_char,
    flags: i32,
    metadata: bool,
    owner: *mut *mut libc::c_void,
) -> i32 {
    call(|| {
        let store = unsafe { &*store };
        let relative = unsafe { path(name) };
        unsafe {
            *owner = ptr::null_mut();
        }
        if metadata || flags & libc::O_DIRECTORY != 0 || relative.as_os_str().is_empty() {
            // O_PATH descriptors cannot accidentally receive content mutations.
            return Ok(store.metadata_fd(relative)?.into_raw_fd());
        }
        let handle = if flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_CREAT | libc::O_TRUNC) != 0
        {
            Handle::Write(store.open_write(
                relative,
                flags & libc::O_CREAT != 0,
                flags & libc::O_EXCL != 0,
                flags & libc::O_TRUNC != 0,
            )?)
        } else {
            Handle::Read(store.read(relative)?)
        };
        let fd = match &handle {
            Handle::Read(r) => r.as_raw_fd(),
            Handle::Write(w) => w.as_raw_fd(),
        };
        let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            *owner = Box::into_raw(Box::new(handle)).cast();
        }
        Ok(fd)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_release(owner: *mut libc::c_void) {
    if !owner.is_null() {
        drop(unsafe { Box::from_raw(owner.cast::<Handle>()) });
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn tier_stat(
    store: *const Store,
    name: *const libc::c_char,
    out: *mut libc::stat,
) -> i32 {
    call(|| {
        let (m, archived) = unsafe { &*store }.stat(unsafe { path(name) })?;
        let s = unsafe { &mut *out };
        *s = unsafe { std::mem::zeroed() };
        s.st_dev = m.dev();
        s.st_ino = m.ino();
        s.st_mode = m.mode();
        s.st_nlink = m.nlink() as _;
        s.st_uid = m.uid();
        s.st_gid = m.gid();
        s.st_size = m.size() as _;
        s.st_blksize = m.blksize() as _;
        s.st_blocks = m.blocks() as _;
        s.st_atime = m.atime();
        s.st_atime_nsec = m.atime_nsec();
        s.st_mtime = m.mtime();
        s.st_mtime_nsec = m.mtime_nsec();
        s.st_ctime = m.ctime();
        s.st_ctime_nsec = m.ctime_nsec();
        if let Some(a) = archived {
            s.st_size = a.size as _;
            s.st_mode = libc::S_IFREG | (a.mode & 0o555);
            s.st_mtime = a.modified_secs;
            s.st_mtime_nsec = a.modified_nanos;
        }
        Ok(0)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_identity(
    store: *const Store,
    name: *const libc::c_char,
    out: *mut u64,
) -> i32 {
    call(|| {
        unsafe {
            *out = (&*store).logical_id(path(name))?;
        }
        Ok(0)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_list(
    store: *const Store,
    name: *const libc::c_char,
    out: *mut *mut Vec<PathBuf>,
) -> i32 {
    call(|| {
        let mut entries = vec![PathBuf::from("."), PathBuf::from("..")];
        entries.extend(unsafe { &*store }.list(unsafe { path(name) })?);
        unsafe {
            *out = Box::into_raw(Box::new(entries));
        }
        Ok(0)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_entry(
    entries: *const Vec<PathBuf>,
    index: usize,
    out: *mut libc::c_char,
    size: usize,
) -> i32 {
    call(|| {
        let Some(name) = (unsafe { &*entries }).get(index) else {
            return Ok(0);
        };
        let bytes = name.as_os_str().as_bytes();
        ensure!(bytes.len() < size, "directory name exceeds buffer");
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast(), bytes.len());
            *out.add(bytes.len()) = 0;
        }
        Ok(1)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_list_free(entries: *mut Vec<PathBuf>) {
    if !entries.is_null() {
        drop(unsafe { Box::from_raw(entries) });
    }
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_mkdir(store: *const Store, name: *const libc::c_char) -> i32 {
    call(|| {
        unsafe { &*store }.mkdir(unsafe { path(name) })?;
        Ok(0)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_remove(
    store: *const Store,
    name: *const libc::c_char,
    directory: bool,
) -> i32 {
    call(|| {
        unsafe { &*store }.remove(unsafe { path(name) }, directory)?;
        Ok(0)
    })
}
#[unsafe(no_mangle)]
unsafe extern "C" fn tier_rename(
    store: *const Store,
    old: *const libc::c_char,
    new: *const libc::c_char,
) -> i32 {
    call(|| {
        unsafe { &*store }.rename(unsafe { path(old) }, unsafe { path(new) })?;
        Ok(0)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn tier_setattr(
    store: *const Store,
    name: *const libc::c_char,
    mode: i32,
    times: *const libc::timespec,
) -> i32 {
    call(|| {
        let store = unsafe { &*store };
        let relative = unsafe { path(name) };
        let directory = store.stat(relative)?.0.is_dir();
        let writer = if directory {
            None
        } else {
            Some(store.open_write(relative, false, false, false)?)
        };
        let dir = if directory {
            Some(store.metadata_fd(relative)?)
        } else {
            None
        };
        let fd = writer
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .unwrap_or_else(|| dir.as_ref().unwrap().as_raw_fd());
        if mode >= 0 && unsafe { libc::fchmod(fd, mode as _) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if !times.is_null() && unsafe { libc::futimens(fd, times) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(0)
    })
}
