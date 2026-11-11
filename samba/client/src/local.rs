//! Connection-scoped physical mappings. A revoked grant is never reused.
use crate::{MAX_DATA, pb};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, MetadataExt},
    },
    path::Path,
    sync::Arc,
};

pub(crate) const LOCAL_ID: u64 = 1 << 63;
#[derive(Clone)]
struct Mapping {
    local: pb::LocalMapping,
    attr: pb::Attr,
    used: u64,
}
struct Handle {
    mapping: Mapping,
    file: Arc<File>,
    _pin: Arc<File>,
}
pub(crate) struct Local {
    pub(crate) peer: Option<Arc<File>>,
    pub(crate) hits: [u64; 3],
    cache: HashMap<(String, String), Mapping>,
    order: BTreeMap<u64, (String, String)>,
    normal: HashMap<u64, String>,
    handle_limit: usize,
    handles: HashMap<u64, Handle>,
    cursors: HashMap<u64, String>,
    capacity: usize,
    tick: u64,
    next_id: u64,
    pub(crate) revoked: Option<u64>,
    revoked_through: u64,
}
fn peer(fd: i32) -> Option<Arc<File>> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&credentials) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return None;
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, credentials.pid, 0) };
    (fd >= 0).then(|| Arc::new(unsafe { File::from_raw_fd(fd as i32) }))
}
fn alive(peer: &File) -> Result<(), i32> {
    let mut p = libc::pollfd {
        fd: peer.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        match unsafe { libc::poll(&mut p, 1, 0) } {
            0 => return Ok(()),
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            _ => return Err(libc::EIO),
        }
    }
}
pub(crate) fn now() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC is shared with the same-host daemon.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0 {
        return u64::MAX;
    }
    (t.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(t.tv_nsec as u64)
}
fn error(e: std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}
fn attributes(file: &File, original: &pb::Attr) -> Result<pb::Attr, i32> {
    let m = file.metadata().map_err(error)?;
    let mut attr = original.clone();
    attr.size = m.size();
    attr.allocated = m.blocks() * 512;
    attr.atime = Some(pb::Timestamp {
        seconds: m.atime(),
        nanos: m.atime_nsec() as u32,
    });
    attr.mtime = Some(pb::Timestamp {
        seconds: m.mtime(),
        nanos: m.mtime_nsec() as u32,
    });
    attr.ctime = Some(pb::Timestamp {
        seconds: m.ctime(),
        nanos: m.ctime_nsec() as u32,
    });
    Ok(attr)
}
impl Local {
    pub(crate) fn new(capacity: usize, fd: i32) -> Self {
        Self {
            peer: peer(fd),
            hits: [0; 3],
            cache: HashMap::new(),
            order: BTreeMap::new(),
            normal: HashMap::new(),
            handle_limit: 4096,
            handles: HashMap::new(),
            cursors: HashMap::new(),
            capacity,
            tick: 0,
            next_id: LOCAL_ID,
            revoked: None,
            revoked_through: 0,
        }
    }
    pub(crate) fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        self.cache.clear();
        self.order.clear();
    }
    pub(crate) fn observe(&mut self, request: &pb::Request, response: &pb::Response) {
        if response.error != 0 {
            return;
        }
        if request.method == pb::Method::Hello as i32 {
            self.set_capacity(response.max_cache_entries as usize);
            if response.max_handles != 0 {
                self.handle_limit = response.max_handles as usize;
            }
        }
        if (request.method == pb::Method::Open as i32
            || request.method == pb::Method::Attach as i32)
            && let Some(attr) = &response.attr
        {
            self.normal.insert(response.handle, attr.node_id.clone());
        }
        if request.method == pb::Method::Close as i32 || request.method == pb::Method::Detach as i32
        {
            self.normal.remove(&request.handle);
        }
        if request.method == pb::Method::Opendir as i32
            && let Some(node) = self.normal.get(&request.handle)
        {
            self.cursors.insert(response.handle, node.clone());
        }
        if request.method == pb::Method::Closedir as i32 {
            self.cursors.remove(&request.handle);
        }
        if self.revoked.is_some() {
            return;
        }
        if let (Some(local), Some(attr)) = (&response.local, &response.attr) {
            self.insert(request.parent, &request.name, local, attr);
        }
        if request.method == pb::Method::Readdir as i32
            && let Some(parent) = self.cursors.get(&request.handle).cloned()
        {
            for entry in &response.entries {
                if let (Some(local), Some(attr)) = (&entry.local, &entry.attr) {
                    self.insert_node(&parent, &entry.name, local, attr);
                }
            }
        }
    }
    fn insert(&mut self, parent: u64, name: &str, local: &pb::LocalMapping, attr: &pb::Attr) {
        if let Some(parent) = self.normal.get(&parent).cloned() {
            self.insert_node(&parent, name, local, attr);
        }
    }
    fn insert_node(&mut self, parent: &str, name: &str, local: &pb::LocalMapping, attr: &pb::Attr) {
        if self.peer.is_none()
            || self.capacity == 0
            || attr.directory
            || local.epoch <= self.revoked_through
            || now() >= local.deadline_ns
        {
            return;
        }
        let key = (parent.to_owned(), name.to_owned());
        if let Some(old) = self.cache.remove(&key) {
            self.order.remove(&old.used);
        }
        if self.cache.len() >= self.capacity
            && let Some((_, key)) = self.order.pop_first()
        {
            self.cache.remove(&key);
        }
        self.tick = self.tick.wrapping_add(1);
        self.order.insert(self.tick, key.clone());
        self.cache.insert(
            key,
            Mapping {
                local: local.clone(),
                attr: attr.clone(),
                used: self.tick,
            },
        );
    }
    pub(crate) fn request(&mut self, r: &pb::Request) -> Result<Option<pb::Response>, i32> {
        if [
            pb::Method::Open as i32,
            pb::Method::Attach as i32,
            pb::Method::Opendir as i32,
        ]
        .contains(&r.method)
            && self.handles.len() + self.normal.len() + self.cursors.len() >= self.handle_limit
        {
            return Err(libc::EMFILE);
        }
        if r.method == pb::Method::Close as i32 && self.handles.remove(&r.handle).is_some() {
            return Ok(Some(pb::Response::default()));
        }
        if let Some(h) = self.handles.get(&r.handle) {
            return match pb::Method::try_from(r.method) {
                Ok(pb::Method::Getattr) => Ok(Some(pb::Response {
                    attr: Some(attributes(&h.file, &h.mapping.attr)?),
                    ..Default::default()
                })),
                Ok(pb::Method::Sync) => Ok(Some(pb::Response::default())),
                _ => Err(libc::EBADF),
            };
        }
        if self.revoked.is_some() {
            return Ok(None);
        }
        let stat = r.method == pb::Method::StatAt as i32;
        let open = r.local_access
            && r.method == pb::Method::Open as i32
            && r.access == pb::Access::ReadOnly as i32
            && r.disposition == pb::Disposition::Existing as i32
            && !r.truncate
            && !r.directory;
        if !stat && !open {
            return Ok(None);
        }
        let Some(parent) = self.normal.get(&r.parent) else {
            return Ok(None);
        };
        let key = (parent.clone(), r.name.clone());
        let Some(mapping) = self.cache.get_mut(&key) else {
            return Ok(None);
        };
        self.order.remove(&mapping.used);
        self.tick = self.tick.wrapping_add(1);
        mapping.used = self.tick;
        self.order.insert(self.tick, key.clone());
        let mapping = mapping.clone();
        let result = (|| {
            alive(self.peer.as_ref().ok_or(libc::EIO)?)?;
            if now() >= mapping.local.deadline_ns {
                return Err(libc::ESTALE);
            }
            let pin = pin(&mapping.local)?;
            let file = File::open(Path::new(std::ffi::OsStr::from_bytes(&mapping.local.path)))
                .map_err(error)?;
            let metadata = file.metadata().map_err(error)?;
            if metadata.dev() != mapping.local.device || metadata.ino() != mapping.local.inode {
                return Err(libc::ESTALE);
            }
            let attr = attributes(&file, &mapping.attr)?;
            if now() >= mapping.local.deadline_ns {
                return Err(libc::ESTALE);
            }
            alive(self.peer.as_ref().ok_or(libc::EIO)?)?;
            let handle = if open {
                self.next_id = self.next_id.checked_add(1).ok_or(libc::EMFILE)?;
                self.handles.insert(
                    self.next_id,
                    Handle {
                        mapping,
                        file: Arc::new(file),
                        _pin: Arc::new(pin),
                    },
                );
                self.next_id
            } else {
                0
            };
            Ok(pb::Response {
                attr: Some(attr),
                handle,
                ..Default::default()
            })
        })();
        match result {
            Ok(r) => {
                let counter = usize::from(open);
                self.hits[counter] = self.hits[counter].saturating_add(1);
                Ok(Some(r))
            }
            Err(_) => {
                if let Some(old) = self.cache.remove(&key) {
                    self.order.remove(&old.used);
                }
                Ok(None)
            }
        }
    }
    pub(crate) fn contains(&self, handle: u64) -> bool {
        self.handles.contains_key(&handle)
    }
    pub(crate) fn read(
        &mut self,
        handle: u64,
        offset: u64,
        length: usize,
    ) -> Result<Option<ReadJob>, i32> {
        let Some(h) = self.handles.get(&handle) else {
            return Ok(None);
        };
        if length > MAX_DATA || offset > i64::MAX as u64 {
            return Err(libc::EINVAL);
        }
        self.hits[2] = self.hits[2].saturating_add(1);
        Ok(Some(ReadJob {
            file: h.file.clone(),
            _pin: h._pin.clone(),
            offset,
            data: vec![0; length],
            error: 0,
        }))
    }
    pub(crate) fn revoke(&mut self, epoch: u64) {
        if epoch <= self.revoked_through {
            return;
        }
        self.cache.clear();
        self.order.clear();
        self.revoked_through = self.revoked_through.max(epoch);
        self.revoked = Some(epoch);
    }
    pub(crate) fn revoked_acknowledged(&mut self, epoch: u64) {
        if self.revoked.is_some_and(|revoked| revoked <= epoch) {
            self.revoked = None;
        }
    }
    pub(crate) fn disconnected(&mut self) {
        self.cache.clear();
        self.order.clear();
        self.normal.clear();
        self.cursors.clear();
    }
}
fn pin(mapping: &pb::LocalMapping) -> Result<File, i32> {
    let file =
        File::open(Path::new(std::ffi::OsStr::from_bytes(&mapping.pin_path))).map_err(error)?;
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_RDLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = i64::try_from(mapping.pin_offset).map_err(|_| libc::EINVAL)?;
    lock.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == -1 {
        return Err(error(std::io::Error::last_os_error()));
    }
    Ok(file)
}

pub struct ReadJob {
    file: Arc<File>,
    _pin: Arc<File>,
    offset: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) error: i32,
}
impl ReadJob {
    pub(crate) fn run(&mut self) {
        match self
            .file
            .read_at(&mut self.data, self.offset)
            .map_err(error)
        {
            Ok(n) => self.data.truncate(n),
            Err(e) => self.error = e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::net::UnixStream,
        sync::atomic::{AtomicUsize, Ordering},
    };
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Fixture {
        local: Local,
        path: std::path::PathBuf,
        pin_path: std::path::PathBuf,
        mapping: pb::LocalMapping,
        attr: pb::Attr,
    }
    impl Fixture {
        fn new(capacity: usize) -> Self {
            let (socket, _peer) = UnixStream::pair().unwrap();
            let path = std::env::temp_dir().join(format!(
                "tier-direct-client-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let pin_path = path.with_extension("pins");
            File::create(&pin_path).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(b"current contents").unwrap();
            let metadata = file.metadata().unwrap();
            let mut local = Local::new(capacity, socket.as_raw_fd());
            local.normal.insert(1, "parent".into());
            Self {
                local,
                path: path.clone(),
                pin_path: pin_path.clone(),
                mapping: pb::LocalMapping {
                    epoch: 1,
                    path: path.as_os_str().as_bytes().to_vec(),
                    deadline_ns: now() + 30_000_000_000,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    pin_path: pin_path.as_os_str().as_bytes().to_vec(),
                    pin_offset: 100,
                },
                attr: pb::Attr {
                    node_id: "node".into(),
                    inode: 42,
                    mode: 0o100644,
                    ..Default::default()
                },
            }
        }
        fn insert(&mut self, name: &str) {
            self.local.insert(1, name, &self.mapping, &self.attr);
        }
        fn open(&mut self, name: &str) -> u64 {
            self.local
                .request(&pb::Request {
                    method: pb::Method::Open as i32,
                    parent: 1,
                    name: name.into(),
                    access: pb::Access::ReadOnly as i32,
                    local_access: true,
                    ..Default::default()
                })
                .unwrap()
                .unwrap()
                .handle
        }
        fn exclusive(&self) -> std::io::Result<File> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.pin_path)?;
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = 100;
            lock.l_len = 1;
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(file)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(&self.pin_path);
        }
    }
    fn stat(name: &str) -> pb::Request {
        pb::Request {
            method: pb::Method::StatAt as i32,
            parent: 1,
            name: name.into(),
            ..Default::default()
        }
    }
    #[test]
    fn delayed_ack_does_not_clear_a_newer_revocation() {
        let mut local = Local::new(0, -1);
        local.revoke(1);
        local.revoke(2);
        local.revoked_acknowledged(1);
        assert_eq!(local.revoked, Some(2));
        local.revoked_acknowledged(2);
        assert_eq!(local.revoked, None);
        local.revoke(1);
        assert_eq!(local.revoked, None);
    }
    #[test]
    fn readdir_mapping_is_shared_by_distinct_handles_to_the_same_directory() {
        let mut f = Fixture::new(2);
        f.local.observe(
            &pb::Request {
                method: pb::Method::Opendir as i32,
                handle: 1,
                ..Default::default()
            },
            &pb::Response {
                handle: 17,
                ..Default::default()
            },
        );
        f.local.observe(
            &pb::Request {
                method: pb::Method::Readdir as i32,
                handle: 17,
                ..Default::default()
            },
            &pb::Response {
                entries: vec![pb::Entry {
                    name: "a".into(),
                    attr: Some(f.attr.clone()),
                    local: Some(f.mapping.clone()),
                }],
                ..Default::default()
            },
        );
        f.local.observe(
            &pb::Request {
                method: pb::Method::Open as i32,
                ..Default::default()
            },
            &pb::Response {
                handle: 2,
                attr: Some(pb::Attr {
                    node_id: "parent".into(),
                    directory: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        f.local.observe(
            &pb::Request {
                method: pb::Method::Close as i32,
                handle: 1,
                ..Default::default()
            },
            &pb::Response::default(),
        );
        let opened = f
            .local
            .request(&pb::Request {
                method: pb::Method::Open as i32,
                parent: 2,
                name: "a".into(),
                access: pb::Access::ReadOnly as i32,
                local_access: true,
                ..Default::default()
            })
            .unwrap()
            .unwrap();
        assert!(opened.handle > LOCAL_ID);
        let mut job = f.local.read(opened.handle, 0, 16).unwrap().unwrap();
        job.run();
        assert_eq!(job.data, b"current contents");
    }
    #[test]
    fn open_handle_survives_cache_revocation_and_pins_until_last_read_finishes() {
        let mut f = Fixture::new(2);
        f.insert("a");
        let attr = f.local.request(&stat("a")).unwrap().unwrap().attr.unwrap();
        assert_eq!(attr.inode, 42);
        assert_eq!(attr.size, 16);
        assert_eq!(attr.mode, 0o100644);
        let handle = f.open("a");
        assert!(handle > LOCAL_ID);
        let mut job = f.local.read(handle, 0, 16).unwrap().unwrap();
        assert_eq!(
            f.exclusive().unwrap_err().raw_os_error(),
            Some(libc::EAGAIN)
        );
        f.local.revoke(1);
        f.local.revoked_acknowledged(1);
        f.insert("a"); // An old READDIR response cannot repopulate the cache.
        assert!(f.local.request(&stat("a")).unwrap().is_none());
        f.local
            .request(&pb::Request {
                method: pb::Method::Close as i32,
                handle,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            f.exclusive().unwrap_err().raw_os_error(),
            Some(libc::EAGAIN)
        );
        job.run();
        assert_eq!(job.error, 0);
        assert_eq!(job.data, b"current contents");
        drop(job);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match f.exclusive() {
                Ok(_) => break,
                Err(e)
                    if e.raw_os_error() == Some(libc::EAGAIN)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::yield_now()
                }
                Err(e) => panic!("read pin remained locked: {e}"),
            }
        }
        f.mapping.epoch = 2;
        f.insert("a");
        assert!(f.local.request(&stat("a")).unwrap().is_some());
    }
    #[test]
    fn lru_eviction_does_not_close_live_handles() {
        let mut f = Fixture::new(2);
        f.insert("a");
        let handle = f.open("a");
        f.insert("b");
        f.local.request(&stat("a")).unwrap();
        f.insert("c");
        assert!(f.local.request(&stat("b")).unwrap().is_none());
        assert!(f.local.request(&stat("a")).unwrap().is_some());
        f.insert("d");
        f.insert("e");
        let mut job = f.local.read(handle, 0, 7).unwrap().unwrap();
        job.run();
        assert_eq!(job.data, b"current");
    }
    #[test]
    fn locked_background_mapping_and_replaced_object_fall_back_to_tier() {
        let mut f = Fixture::new(2);
        f.insert("a");
        let background = f.exclusive().unwrap();
        assert!(f.local.request(&stat("a")).unwrap().is_none());
        drop(background);
        // Other tests spawn processes. A fork can briefly inherit the OFD lock
        // until exec closes CLOEXEC descriptors, so wait for actual release.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let _released = loop {
            match pin(&f.mapping) {
                Ok(pin) => break pin,
                Err(libc::EAGAIN) if std::time::Instant::now() < deadline => {
                    std::thread::yield_now()
                }
                Err(error) => panic!("background pin remained locked: {error}"),
            }
        };
        f.insert("a");
        let handle = f.open("a");
        std::fs::remove_file(&f.path).unwrap();
        std::fs::write(&f.path, b"wrong object").unwrap();
        assert!(f.local.request(&stat("a")).unwrap().is_none());
        let mut job = f.local.read(handle, 0, 16).unwrap().unwrap();
        job.run();
        assert_eq!(job.data, b"current contents");
    }
    #[test]
    fn metadata_opens_remain_remote_and_readonly_handles_reject_mutations() {
        let mut f = Fixture::new(2);
        f.insert("a");
        assert!(
            f.local
                .request(&pb::Request {
                    method: pb::Method::Open as i32,
                    parent: 1,
                    name: "a".into(),
                    access: pb::Access::Metadata as i32,
                    ..Default::default()
                })
                .unwrap()
                .is_none()
        );
        let handle = f.open("a");
        assert!(matches!(
            f.local.request(&pb::Request {
                method: pb::Method::Setattr as i32,
                handle,
                ..Default::default()
            }),
            Err(libc::EBADF)
        ));
        assert!(
            f.local
                .request(&pb::Request {
                    method: pb::Method::Sync as i32,
                    handle,
                    ..Default::default()
                })
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn wire_failure_releases_buffers_but_preserves_open_local_files() {
        let mut f = Fixture::new(2);
        f.insert("a");
        let handle = f.open("a");
        let mut transport = crate::transport::TierTransport::new(-1);
        std::mem::swap(&mut transport.local, &mut f.local);
        transport
            .submit(
                pb::Request {
                    method: pb::Method::Write as i32,
                    handle: 1,
                    ..Default::default()
                },
                &[0; 8192],
                1,
            )
            .unwrap();
        assert!(!transport.outgoing.is_empty());
        transport.abort(libc::ECONNRESET);
        assert!(transport.outgoing.is_empty());
        assert!(transport.pending.is_empty());
        transport
            .submit(
                pb::Request {
                    method: pb::Method::Read as i32,
                    handle,
                    length: 16,
                    ..Default::default()
                },
                &[],
                2,
            )
            .unwrap();
        let (_, response) = transport.ready.pop_front().unwrap();
        assert_eq!(response.data, b"current contents");
        assert_eq!(transport.local.hits[1], 1);
        assert_eq!(transport.local.hits[2], 1);
    }
    #[test]
    fn daemon_death_or_expiry_fences_new_opens_but_not_open_fds() {
        let mut f = Fixture::new(2);
        f.insert("a");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        assert!(fd >= 0);
        f.local.peer = Some(Arc::new(unsafe { File::from_raw_fd(fd as i32) }));
        let handle = f.open("a");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(f.local.request(&stat("a")).unwrap().is_none());
        f.local
            .handles
            .get_mut(&handle)
            .unwrap()
            .mapping
            .local
            .deadline_ns = 0;
        let mut job = f.local.read(handle, 0, 16).unwrap().unwrap();
        job.run();
        assert_eq!(job.data, b"current contents");
        assert!(
            f.local
                .request(&pb::Request {
                    method: pb::Method::Getattr as i32,
                    handle,
                    ..Default::default()
                })
                .unwrap()
                .is_some()
        );
        f.mapping.deadline_ns = 0;
        f.insert("b");
        assert!(f.local.request(&stat("b")).unwrap().is_none());
    }
}
