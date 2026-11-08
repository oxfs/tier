mod shutdown;
use crate::core::Engine;
use anyhow::{Result, anyhow};
use fuser::*;
use libc::{EACCES, EBUSY, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOTDIR, ENOTEMPTY};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::File,
    os::unix::fs::FileExt,
    path::Path,
    time::{Duration, SystemTime},
};
const TTL: Duration = Duration::from_secs(0);
fn errno(error: std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(EIO)
}
fn storage_errno(error: anyhow::Error) -> i32 {
    let code = error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .unwrap_or(EIO);
    if matches!(code, EIO | libc::ENOSPC | libc::EROFS) {
        tracing::warn!(errno = code, error = %format_args!("{error:#}"), "storage operation failed");
    }
    code
}
struct Handle {
    ino: u64,
    data: File,
    writable: bool,
    detached: bool,
    dirty: bool,
    append: bool,
    synchronous: bool,
    error: Option<i32>,
    writer: Option<crate::core::Writer>,
}
struct Fs {
    engine: Engine,
    paths: HashMap<u64, String>,
    inodes: HashMap<String, u64>,
    ids: HashMap<u64, String>,
    handles: HashMap<u64, Handle>,
    next: u64,
}
impl Fs {
    fn alloc(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    fn child(&self, parent: u64, name: &OsStr) -> Result<String, i32> {
        let name = name.to_str().ok_or(EINVAL)?;
        if !crate::core::safe_name(name) {
            return Err(EINVAL);
        }
        Ok(format!(
            "{}/{}",
            self.paths.get(&parent).ok_or(ENOENT)?.trim_end_matches('/'),
            name
        ))
    }
    fn attr(&mut self, path: &str) -> Result<FileAttr, i32> {
        let node = self.engine.stat(path).map_err(|_| ENOENT)?;
        let ino = match self.inodes.get(&node.id) {
            Some(&ino) => ino,
            None => {
                let ino = self.alloc();
                self.inodes.insert(node.id.clone(), ino);
                self.ids.insert(ino, node.id.clone());
                self.paths.insert(ino, path.to_owned());
                ino
            }
        };
        self.paths.entry(ino).or_insert_with(|| path.to_owned());
        let size = self
            .handles
            .values()
            .find(|h| h.ino == ino && h.dirty && !h.detached)
            .and_then(|h| h.data.metadata().ok())
            .map(|s| s.len())
            .unwrap_or(node.size);
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(node.mtime);
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind: if node.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if node.is_dir { 0o770 } else { 0o660 },
            nlink: if node.is_dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
    fn open_file(&mut self, ino: u64, flags: i32) -> Result<u64, i32> {
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if write
            && self
                .handles
                .values()
                .any(|h| h.ino == ino && h.writable && !h.detached)
        {
            return Err(EBUSY);
        }
        let writer = if write {
            let engine = &self.engine;
            let node = engine
                .stat(self.paths.get(&ino).ok_or(ENOENT)?)
                .map_err(|_| ENOENT)?;
            let pending: bool = engine
                .db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pending_writes WHERE node=?)",
                    [&node.id],
                    |row| row.get(0),
                )
                .map_err(|_| EIO)?;
            if pending {
                return Err(EIO);
            }
            Some(crate::core::Writer::new(node.id))
        } else {
            None
        };
        let truncate = write && flags & libc::O_TRUNC != 0;
        let path = self.paths.get(&ino).ok_or(ENOENT)?;
        let data = if let Some(handle) = self
            .handles
            .values()
            .find(|handle| handle.ino == ino && !handle.detached)
        {
            handle.data.try_clone().map_err(errno)?
        } else {
            let mut file = tempfile::tempfile().map_err(errno)?;
            if !truncate {
                self.engine
                    .copy_to(path, &mut file)
                    .map_err(storage_errno)?;
            }
            file
        };
        if truncate {
            self.engine.begin_write(path).map_err(storage_errno)?;
            data.set_len(0).map_err(errno)?;
        }
        let id = self.alloc();
        self.handles.insert(
            id,
            Handle {
                ino,
                data,
                writable: write,
                detached: false,
                dirty: truncate,
                error: None,
                writer,
                append: flags & libc::O_APPEND != 0,
                synchronous: flags & (libc::O_SYNC | libc::O_DSYNC) != 0,
            },
        );
        Ok(id)
    }
    fn save(&mut self, fh: u64) -> Result<(), i32> {
        let h = self.handles.get_mut(&fh).ok_or(EINVAL)?;
        if let Some(error) = h.error {
            return Err(error);
        }
        if h.dirty {
            let path = self.paths.get(&h.ino).ok_or(libc::ESTALE)?;
            self.engine.put_from(path, &mut h.data).map_err(|error| {
                let code = storage_errno(error.context(format!("publish {path}")));
                h.writer.take();
                h.error = Some(code);
                code
            })?;
            h.dirty = false;
        }
        Ok(())
    }
    fn save_before_detach(&mut self, ino: u64) -> Result<(), i32> {
        let writer = self
            .handles
            .iter()
            .find(|(_, h)| h.ino == ino && !h.detached && h.dirty && h.error.is_none())
            .map(|(&fh, _)| fh);
        if let Some(fh) = writer {
            self.save(fh)?;
        }
        Ok(())
    }
    fn detach(&mut self, ino: u64) {
        self.paths.remove(&ino);
        for handle in self.handles.values_mut().filter(|h| h.ino == ino) {
            handle.detached = true;
            handle.writer.take();
        }
    }
    fn resize(&mut self, ino: u64, fh: Option<u64>, size: u64) -> Result<(), i32> {
        let path = self.paths.get(&ino).cloned().ok_or(libc::ESTALE)?;
        let temporary = fh.is_none();
        let handle = match fh {
            Some(id) => id,
            None => self.open_file(ino, libc::O_WRONLY)?,
        };
        let result = (|| {
            let h = self.handles.get_mut(&handle).ok_or(EINVAL)?;
            if h.detached {
                return Err(libc::ESTALE);
            }
            if h.ino != ino || !h.writable {
                return Err(EACCES);
            }
            if let Some(error) = h.error {
                return Err(error);
            }
            if !h.dirty {
                self.engine.begin_write(&path).map_err(storage_errno)?;
            }
            if let Err(error) = h.data.set_len(size) {
                let code = errno(error);
                h.writer.take();
                h.error = Some(code);
                return Err(code);
            }
            h.dirty = true;
            if temporary || h.synchronous {
                self.save(handle)?;
            }
            Ok(())
        })();
        if temporary {
            self.handles.remove(&handle);
        }
        result
    }
    fn remove_entry(&mut self, p: u64, n: &OsStr, dir: bool) -> Result<(), i32> {
        let path = self.child(p, n)?;
        let a = self.attr(&path)?;
        if dir && a.kind != FileType::Directory {
            return Err(ENOTDIR);
        }
        if !dir && a.kind == FileType::Directory {
            return Err(EISDIR);
        }
        self.save_before_detach(a.ino)?;
        let engine = &self.engine;
        if dir && !engine.list(&path).map_err(|_| EIO)?.is_empty() {
            return Err(ENOTEMPTY);
        }
        engine.remove(&path).map_err(|_| EIO)?;
        self.detach(a.ino);
        Ok(())
    }
}
macro_rules! respond {
    ($reply:expr,$result:expr) => {
        match $result {
            Ok(()) => $reply.ok(),
            Err(e) => $reply.error(e),
        }
    };
}
fn xattr_reply(result: Result<Vec<u8>, i32>, size: u32, r: ReplyXattr) {
    match result {
        Err(e) => r.error(e),
        Ok(bytes) if size == 0 => r.size(bytes.len() as u32),
        Ok(bytes) if bytes.len() > size as usize => r.error(libc::ERANGE),
        Ok(bytes) => r.data(&bytes),
    }
}
impl Filesystem for Fs {
    fn getxattr(&mut self, _: &Request, ino: u64, name: &OsStr, size: u32, r: ReplyXattr) {
        let result = (|| {
            self.engine
                .stream_attr(
                    self.ids.get(&ino).ok_or(ENOENT)?,
                    name.to_str().ok_or(EINVAL)?,
                )
                .map_err(storage_errno)
        })();
        xattr_reply(result, size, r);
    }
    fn listxattr(&mut self, _: &Request, ino: u64, size: u32, r: ReplyXattr) {
        let result = (|| {
            self.engine
                .list_stream_attrs(self.ids.get(&ino).ok_or(ENOENT)?)
                .map_err(storage_errno)
        })();
        xattr_reply(result, size, r);
    }
    fn setxattr(
        &mut self,
        _: &Request,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        r: ReplyEmpty,
    ) {
        let result = (|| {
            if position != 0 {
                return Err(EINVAL);
            }
            self.engine
                .set_stream_attr(
                    self.ids.get(&ino).ok_or(ENOENT)?,
                    name.to_str().ok_or(EINVAL)?,
                    value,
                    flags,
                )
                .map_err(storage_errno)
        })();
        match result {
            Ok(()) => r.ok(),
            Err(e) => r.error(e),
        }
    }
    fn removexattr(&mut self, _: &Request, ino: u64, name: &OsStr, r: ReplyEmpty) {
        let result = (|| {
            self.engine
                .remove_stream_attr(
                    self.ids.get(&ino).ok_or(ENOENT)?,
                    name.to_str().ok_or(EINVAL)?,
                )
                .map_err(storage_errno)
        })();
        match result {
            Ok(()) => r.ok(),
            Err(e) => r.error(e),
        }
    }
    fn init(&mut self, _: &Request<'_>, config: &mut KernelConfig) -> Result<(), i32> {
        config
            .add_capabilities(consts::FUSE_ATOMIC_O_TRUNC)
            .map_err(|_| libc::EOPNOTSUPP)
    }

    fn statfs(&mut self, _: &Request, _: u64, reply: ReplyStatfs) {
        // Report one backing filesystem; multiple configured targets can share a disk.
        let filesystem = (|| -> Result<_> {
            let engine = &self.engine;
            let mut targets = engine
                .db
                .prepare("SELECT name FROM targets ORDER BY name")?;
            for name in targets.query_map([], |row| row.get::<_, String>(0))? {
                if let Ok(stat) = engine.target_stat(&name?) {
                    return Ok(stat);
                }
            }
            anyhow::bail!("no online storage target")
        })();
        let Ok(v) = filesystem else {
            return reply.error(EIO);
        };
        reply.statfs(
            v.f_blocks,
            v.f_bfree,
            v.f_bavail,
            v.f_files,
            v.f_ffree,
            v.f_frsize as u32,
            v.f_namemax as u32,
            v.f_frsize as u32,
        );
    }
    fn lookup(&mut self, _: &Request, p: u64, n: &OsStr, r: ReplyEntry) {
        match self.child(p, n).and_then(|p| self.attr(&p)) {
            Ok(a) => r.entry(&TTL, &a, 0),
            Err(e) => r.error(e),
        }
    }
    fn getattr(&mut self, _: &Request, i: u64, r: ReplyAttr) {
        match self
            .paths
            .get(&i)
            .cloned()
            .ok_or(ENOENT)
            .and_then(|p| self.attr(&p))
        {
            Ok(a) => r.attr(&TTL, &a),
            Err(e) => r.error(e),
        }
    }
    fn readdir(&mut self, _: &Request, i: u64, _: u64, offset: i64, mut r: ReplyDirectory) {
        let Some(path) = self.paths.get(&i).cloned() else {
            return r.error(ENOENT);
        };
        let names = match self.engine.list(&path) {
            Ok(n) => n,
            Err(_) => return r.error(EIO),
        };
        let parent = Path::new(&path)
            .parent()
            .and_then(Path::to_str)
            .unwrap_or("/");
        let mut rows = vec![
            (i, FileType::Directory, ".".into()),
            (
                self.attr(parent).map(|a| a.ino).unwrap_or(1),
                FileType::Directory,
                "..".into(),
            ),
        ];
        for name in names {
            if let Ok(a) = self.child(i, OsStr::new(&name)).and_then(|p| self.attr(&p)) {
                rows.push((a.ino, a.kind, name))
            }
        }
        for (idx, (ino, kind, name)) in rows.into_iter().enumerate().skip(offset as usize) {
            if r.add(ino, (idx + 1) as i64, kind, name) {
                break;
            }
        }
        r.ok()
    }
    fn mkdir(&mut self, _: &Request, p: u64, n: &OsStr, _: u32, _: u32, r: ReplyEntry) {
        let result = self.child(p, n).and_then(|p| {
            if self.attr(&p).is_ok() {
                return Err(EEXIST);
            }
            self.engine.mkdir(&p).map_err(|_| EIO)?;
            self.attr(&p)
        });
        match result {
            Ok(a) => r.entry(&TTL, &a, 0),
            Err(e) => r.error(e),
        }
    }
    fn create(
        &mut self,
        _: &Request,
        p: u64,
        n: &OsStr,
        _: u32,
        _: u32,
        flags: i32,
        r: ReplyCreate,
    ) {
        let result = self.child(p, n).and_then(|p| {
            if self.attr(&p).is_ok() {
                return Err(EEXIST);
            }
            self.engine.put(&p, &[]).map_err(storage_errno)?;
            let a = self.attr(&p)?;
            let fh = self.open_file(a.ino, flags)?;
            Ok((a, fh))
        });
        match result {
            Ok((a, fh)) => r.created(&TTL, &a, 0, fh, 1),
            Err(e) => r.error(e),
        }
    }
    fn open(&mut self, _: &Request, i: u64, flags: i32, r: ReplyOpen) {
        match self.open_file(i, flags) {
            Ok(f) => r.opened(f, 1),
            Err(e) => r.error(e),
        }
    }
    fn read(
        &mut self,
        _: &Request,
        i: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _: i32,
        _: Option<u64>,
        r: ReplyData,
    ) {
        let Some(handle) = self.handles.get(&fh).filter(|h| h.ino == i) else {
            return r.error(EINVAL);
        };
        let Ok(start) = u64::try_from(offset) else {
            return r.error(EINVAL);
        };
        let mut data = vec![0; size as usize];
        match handle.data.read_at(&mut data, start) {
            Ok(n) => r.data(&data[..n]),
            Err(e) => r.error(errno(e)),
        }
    }
    fn write(
        &mut self,
        _: &Request,
        _: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _: u32,
        _: i32,
        _: Option<u64>,
        r: ReplyWrite,
    ) {
        let Some(h) = self.handles.get_mut(&fh) else {
            return r.error(EINVAL);
        };
        if !h.writable {
            return r.error(EACCES);
        }
        if h.detached {
            return r.error(libc::ESTALE);
        }
        let Ok(mut start) = u64::try_from(offset) else {
            return r.error(EINVAL);
        };
        if data.is_empty() {
            return r.written(0);
        }
        if h.append {
            start = match h.data.metadata() {
                Ok(s) => s.len(),
                Err(e) => return r.error(errno(e)),
            };
        }
        if !h.dirty
            && let Some(path) = self.paths.get(&h.ino)
            && let Err(error) = self.engine.begin_write(path)
        {
            return r.error(storage_errno(
                error.context(format!("begin writing {path}")),
            ));
        }
        if let Some(error) = h.error {
            return r.error(error);
        }
        if let Err(e) = h.data.write_all_at(data, start) {
            let error = errno(e);
            h.writer.take();
            h.error = Some(error);
            return r.error(error);
        }
        h.dirty = true;
        if h.synchronous
            && let Err(e) = self.save(fh)
        {
            return r.error(e);
        }
        r.written(data.len() as u32)
    }
    fn flush(&mut self, _: &Request, _: u64, fh: u64, _: u64, r: ReplyEmpty) {
        respond!(r, self.save(fh))
    }
    fn fsync(&mut self, _: &Request, _: u64, fh: u64, _: bool, r: ReplyEmpty) {
        respond!(r, self.save(fh))
    }
    fn release(
        &mut self,
        _: &Request,
        _: u64,
        fh: u64,
        _: i32,
        _: Option<u64>,
        _: bool,
        r: ReplyEmpty,
    ) {
        let result = self.save(fh);
        self.handles.remove(&fh);
        respond!(r, result)
    }
    fn unlink(&mut self, _: &Request, p: u64, n: &OsStr, r: ReplyEmpty) {
        respond!(r, self.remove_entry(p, n, false))
    }
    fn rmdir(&mut self, _: &Request, p: u64, n: &OsStr, r: ReplyEmpty) {
        respond!(r, self.remove_entry(p, n, true))
    }
    fn rename(
        &mut self,
        _: &Request,
        p: u64,
        n: &OsStr,
        np: u64,
        nn: &OsStr,
        flags: u32,
        r: ReplyEmpty,
    ) {
        let result = (|| {
            let requested = self.child(p, n)?;
            let source = self.attr(&requested)?.ino;
            let a = self.paths.get(&source).cloned().ok_or(ENOENT)?;
            let b = self.child(np, nn)?;
            if a == b {
                return Ok(());
            }
            if flags & !1 != 0 {
                return Err(EINVAL);
            }
            let destination = self.attr(&b).ok().map(|node| node.ino);
            if flags == 1 && destination.is_some() {
                return Err(EEXIST);
            }
            if let Some(ino) = destination.filter(|ino| *ino != source) {
                self.save_before_detach(ino)?;
            }
            self.engine.rename(&a, &b).map_err(|_| EIO)?;
            if let Some(overwritten) = destination.filter(|ino| *ino != source) {
                self.detach(overwritten);
            }
            for p in self.paths.values_mut() {
                if p == &a || p.strip_prefix(&a).is_some_and(|s| s.starts_with('/')) {
                    *p = format!("{}{}", b, &p[a.len()..])
                }
            }
            Ok(())
        })();
        respond!(r, result)
    }
    fn setattr(
        &mut self,
        _: &Request,
        i: u64,
        _: Option<u32>,
        _: Option<u32>,
        _: Option<u32>,
        size: Option<u64>,
        _: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _: Option<SystemTime>,
        fh: Option<u64>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<u32>,
        r: ReplyAttr,
    ) {
        let result = (|| {
            let p = self.paths.get(&i).cloned().ok_or(ENOENT)?;
            if let Some(size) = size {
                self.resize(i, fh, size)?;
            }
            if let Some(value) = mtime {
                let time = match value {
                    TimeOrNow::Now => SystemTime::now(),
                    TimeOrNow::SpecificTime(t) => t,
                };
                let seconds = time
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(|_| EINVAL)?
                    .as_secs();
                self.engine.set_mtime(&p, seconds).map_err(|_| EIO)?;
            }
            self.attr(&p)
        })();
        match result {
            Ok(a) => r.attr(&TTL, &a),
            Err(e) => r.error(e),
        }
    }
}
pub(crate) struct MountedShare(Session<Fs>, String);

impl MountedShare {
    pub(crate) fn attach(engine: Engine, path: &Path) -> Result<Self> {
        let fs = Fs {
            engine,
            paths: HashMap::from([(1, "/".into())]),
            ids: HashMap::from([(1, "root".into())]),
            inodes: HashMap::from([("root".into(), 1)]),
            handles: HashMap::new(),
            next: 1,
        };
        let source = format!("tier-{}", uuid::Uuid::new_v4());
        Session::new(
            fs,
            path,
            &[
                MountOption::FSName(source.clone()),
                MountOption::AllowOther,
                MountOption::DefaultPermissions,
            ],
        )
        .map_err(|e| anyhow!("mount {}: {e}", path.display()))
        .map(|session| Self(session, source))
    }

    pub(crate) fn unmounter(&mut self) -> Result<shutdown::Shutdown> {
        shutdown::Shutdown::new(self.0.unmount_callable(), self.1.clone())
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        Ok(self.0.run()?)
    }
}
