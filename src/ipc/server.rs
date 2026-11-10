use super::pool::Pool;
use super::{Frame, MAX_DATA, MAX_INFLIGHT, MAX_MESSAGE, VERSION, protocol::*};
use crate::core::{Engine, FileHandle, Node};
use anyhow::{Result, anyhow};
use prost::Message;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, BufReader, BufWriter, Read, Seek, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_SESSIONS: usize = 64;
const MAX_HANDLES: usize = 4096;
const MAX_REQUESTS: usize = 128;
const MAX_CURSOR_BYTES: usize = 64 * 1024 * 1024;
const GLOBAL_CURSOR_BYTES: usize = 128 * 1024 * 1024;
const CURSOR_MEMORY_BYTES: usize = 256 * 1024;
const CURSOR_BUFFER_BYTES: usize = 64 * 1024;

fn errno(code: i32) -> anyhow::Error {
    io::Error::from_raw_os_error(code).into()
}

#[derive(Default)]
struct Counter {
    count: Mutex<usize>,
    changed: Condvar,
}
impl Counter {
    fn acquire(self: &Arc<Self>, maximum: usize) -> Permit {
        let mut count = self.count.lock().unwrap();
        while *count >= maximum {
            count = self.changed.wait(count).unwrap();
        }
        *count += 1;
        Permit(self.clone())
    }
    fn wait_empty(&self) {
        let mut count = self.count.lock().unwrap();
        while *count != 0 {
            count = self.changed.wait(count).unwrap();
        }
    }
}
struct Permit(Arc<Counter>);
impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.count.lock().unwrap() -= 1;
        self.0.changed.notify_all();
    }
}

struct Handle {
    node: String,
    file: Option<Mutex<FileHandle>>,
    readable: bool,
    writable: bool,
}
struct CursorBudget {
    used: Arc<Mutex<usize>>,
    bytes: usize,
}
impl CursorBudget {
    fn reserve(used: Arc<Mutex<usize>>, limit: usize) -> Result<Self> {
        let mut total = used.lock().unwrap();
        let bytes = limit.min(GLOBAL_CURSOR_BYTES.saturating_sub(*total));
        *total += bytes;
        drop(total);
        Ok(Self { used, bytes })
    }
    fn shrink(&mut self, bytes: usize) {
        assert!(bytes <= self.bytes);
        *self.used.lock().unwrap() -= self.bytes - bytes;
        self.bytes = bytes;
    }
}
impl Drop for CursorBudget {
    fn drop(&mut self) {
        *self.used.lock().unwrap() -= self.bytes;
    }
}
enum CursorSource {
    Memory(VecDeque<Entry>),
    Spooling(BufWriter<fs::File>),
    Disk(BufReader<fs::File>),
}
struct Cursor {
    source: CursorSource,
    pending: Option<Entry>,
    remaining: usize,
    memory_bytes: usize,
    budget: CursorBudget,
}
impl Cursor {
    fn new(budget: CursorBudget) -> Self {
        Self {
            source: CursorSource::Memory(VecDeque::new()),
            pending: None,
            remaining: 0,
            memory_bytes: 0,
            budget,
        }
    }
    fn append(&mut self, entry: Entry) -> Result<()> {
        let bytes = 512
            + 2 * entry.name.len()
            + 2 * entry.attr.as_ref().map_or(0, |attr| attr.node_id.len());
        if matches!(self.source, CursorSource::Memory(_))
            && self.memory_bytes + bytes > self.budget.bytes
        {
            let mut file = tempfile::tempfile()?;
            if let CursorSource::Memory(entries) = &mut self.source {
                for entry in entries.drain(..) {
                    Self::write_entry(&mut file, &entry)?;
                }
            }
            // Release the in-memory entries before allocating the spool buffer.
            // A fully exhausted global/session budget keeps unbuffered I/O.
            self.source = CursorSource::Memory(VecDeque::new());
            self.budget
                .shrink(self.budget.bytes.min(CURSOR_BUFFER_BYTES));
            self.source = CursorSource::Spooling(BufWriter::with_capacity(self.budget.bytes, file));
        }
        match &mut self.source {
            CursorSource::Memory(entries) => {
                entries.push_back(entry);
                self.memory_bytes += bytes;
            }
            CursorSource::Spooling(file) => Self::write_entry(file, &entry)?,
            CursorSource::Disk(_) => return Err(errno(libc::EINVAL)),
        }
        self.remaining += 1;
        Ok(())
    }
    fn write_entry(file: &mut impl Write, entry: &Entry) -> Result<()> {
        let bytes = entry.encode_to_vec();
        file.write_all(&(bytes.len() as u32).to_be_bytes())?;
        file.write_all(&bytes)?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        // BufWriter::drop ignores write failures: flush before publishing a cursor.
        if let CursorSource::Spooling(writer) = &mut self.source {
            writer.flush()?;
            let CursorSource::Spooling(writer) =
                std::mem::replace(&mut self.source, CursorSource::Memory(VecDeque::new()))
            else {
                unreachable!();
            };
            let mut file = writer.into_inner().map_err(|error| error.into_error())?;
            file.rewind()?;
            self.source = CursorSource::Disk(BufReader::with_capacity(self.budget.bytes, file));
        } else if matches!(self.source, CursorSource::Memory(_)) {
            self.budget.shrink(self.memory_bytes);
        }
        Ok(())
    }
    fn next(&mut self) -> Result<Option<Entry>> {
        if let Some(entry) = self.pending.take() {
            return Ok(Some(entry));
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        let entry = match &mut self.source {
            CursorSource::Memory(entries) => entries.pop_front().unwrap(),
            CursorSource::Spooling(_) => return Err(errno(libc::EINVAL)),
            CursorSource::Disk(file) => {
                let mut length = [0; 4];
                file.read_exact(&mut length)?;
                let length = u32::from_be_bytes(length) as usize;
                if length > MAX_MESSAGE {
                    return Err(errno(libc::EIO));
                }
                let mut bytes = vec![0; length];
                file.read_exact(&mut bytes)?;
                Entry::decode(bytes.as_slice())?
            }
        };
        self.remaining -= 1;
        Ok(Some(entry))
    }
    fn eof(&self) -> bool {
        self.remaining == 0 && self.pending.is_none()
    }
}

struct Session {
    engine: Engine,
    handles: HashMap<u64, Arc<Handle>>,
    cursors: HashMap<u64, Cursor>,
    next: u64,
    cursor_bytes: usize,
    cursor_budget: Arc<Mutex<usize>>,
    hello: bool,
    attached: bool,
}
impl Session {
    fn handle(&self, id: u64) -> Result<Arc<Handle>> {
        self.handles
            .get(&id)
            .cloned()
            .ok_or_else(|| errno(libc::EBADF))
    }
    fn insert(&mut self, handle: Handle) -> Result<u64> {
        if self.handles.len() + self.cursors.len() >= MAX_HANDLES {
            return Err(errno(libc::EMFILE));
        }
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| errno(libc::EMFILE))?;
        self.handles.insert(id, Arc::new(handle));
        Ok(id)
    }
    fn resolve(&self, parent: u64, path: &str) -> Result<Node> {
        if path.contains('\0') || path.contains('\\') {
            return Err(errno(libc::EINVAL));
        }
        let mut node = if path.starts_with('/') {
            self.engine.stat_id("root")?
        } else {
            self.engine.stat_id(&self.handle(parent)?.node)?
        };
        for part in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            if !node.is_dir {
                return Err(errno(libc::ENOTDIR));
            }
            node = if part == ".." {
                self.engine.stat_id(&self.engine.parent_id(&node.id)?)?
            } else {
                self.engine.stat_child(&node.id, part)?
            };
        }
        Ok(node)
    }
    fn entry(&self, parent: u64, path: &str) -> Result<(String, String)> {
        if path.is_empty() || path.ends_with('/') {
            return Err(errno(libc::EINVAL));
        }
        let (prefix, name) = path.rsplit_once('/').unwrap_or(("", path));
        if name.len() > 255 {
            return Err(errno(libc::ENAMETOOLONG));
        }
        if name == "." || name == ".." || name.contains(['\0', '\\']) {
            return Err(errno(libc::EINVAL));
        }
        let directory = self.resolve(
            parent,
            if path.starts_with('/') && prefix.is_empty() {
                "/"
            } else {
                prefix
            },
        )?;
        if !directory.is_dir {
            return Err(errno(libc::ENOTDIR));
        }
        Ok((directory.id, name.to_owned()))
    }
}

/// Owns the listener and drains admitted operations before releasing sessions.
pub struct IpcServer {
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    streams: Arc<Mutex<HashMap<u64, UnixStream>>>,
    sessions: Arc<Counter>,
    path: PathBuf,
    _lock: fs::File,
}
impl IpcServer {
    pub fn start(engine: Engine, path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("lock"))?;
        // The sidecar lock survives crashes and prevents racing stale-socket removal.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(path) {
                Ok(_) => return Err(anyhow!("IPC socket is active: {}", path.display())),
                Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {
                    fs::remove_file(path)?
                }
                Err(error) => return Err(error.into()),
            },
            Ok(_) => return Err(anyhow!("IPC path is not a socket: {}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let sessions = Arc::new(Counter::default());
        let workers = Arc::new(Counter::default());
        let pool = Pool::new();
        let cursor_budget = Arc::new(Mutex::new(0));
        let instance = uuid::Uuid::new_v4().to_string();
        let listener_thread = {
            let stop = stop.clone();
            let streams = streams.clone();
            let sessions = sessions.clone();
            thread::spawn(move || {
                let mut next = 0u64;
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if !trusted_peer(&stream)
                                || *sessions.count.lock().unwrap() >= MAX_SESSIONS
                            {
                                continue;
                            }
                            let Ok(control) = stream.try_clone() else {
                                continue;
                            };
                            next += 1;
                            let id = next;
                            streams.lock().unwrap().insert(id, control);
                            let permit = sessions.acquire(MAX_SESSIONS);
                            let streams = streams.clone();
                            let engine = engine.clone();
                            let workers = workers.clone();
                            let instance = instance.clone();
                            let pool = pool.clone();
                            let cursor_budget = cursor_budget.clone();
                            thread::spawn(move || {
                                let _permit = permit;
                                serve(stream, engine, workers, pool, cursor_budget, &instance);
                                streams.lock().unwrap().remove(&id);
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => {
                            tracing::error!(%error, "IPC accept failed");
                            break;
                        }
                    }
                }
            })
        };
        Ok(Self {
            stop,
            listener: Some(listener_thread),
            streams,
            sessions,
            path: path.to_owned(),
            _lock: lock,
        })
    }
    pub fn shutdown(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        let _ = listener.join();
        for stream in self.streams.lock().unwrap().values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.sessions.wait_empty();
        let _ = fs::remove_file(&self.path);
    }
}
impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn trusted_peer(stream: &UnixStream) -> bool {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED is kernel-provided, not a client assertion.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    result == 0 && (credentials.uid == 0 || credentials.uid == unsafe { libc::geteuid() })
}

fn serve(
    mut stream: UnixStream,
    engine: Engine,
    workers: Arc<Counter>,
    pool: Arc<Pool>,
    cursor_budget: Arc<Mutex<usize>>,
    instance: &str,
) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let session = Arc::new(Mutex::new(Session {
        engine,
        handles: HashMap::new(),
        cursors: HashMap::new(),
        next: u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) >> 1,
        cursor_bytes: 0,
        cursor_budget,
        hello: false,
        attached: false,
    }));
    let pending = Arc::new(Counter::default());
    let ids = Arc::new(Mutex::new(HashSet::new()));
    let Ok((replies, reply_writer)) = Replies::start(writer, ids.clone()) else {
        return;
    };
    let replies = Arc::new(replies);
    loop {
        // Backpressure before allocating the next frame, including its body.
        let local = pending.acquire(MAX_INFLIGHT);
        let frame = match Frame::read(&mut stream) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "invalid or interrupted IPC frame");
                break;
            }
        };
        let request = match Request::decode(frame.message.as_slice()) {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(request_id = frame.id, %error, "invalid IPC protobuf");
                break;
            }
        };
        if !ids.lock().unwrap().insert(frame.id) {
            tracing::warn!(request_id = frame.id, "duplicate in-flight IPC request");
            break;
        }
        let method = Method::try_from(request.method).unwrap_or(Method::Invalid);
        let data_valid = if method == Method::Write {
            frame.data.len() == request.length as usize
        } else {
            frame.data.is_empty()
        };
        if !data_valid || request.length as usize > MAX_DATA {
            let _ = send(
                &replies,
                frame.id,
                method,
                Err(errno(libc::EINVAL)),
                local,
                None,
            );
            continue;
        }
        let async_method = matches!(
            method,
            Method::Read | Method::Write | Method::Sync | Method::Getattr | Method::Setattr
        );
        if async_method {
            let handle = { session.lock().unwrap().handle(request.handle) };
            let handle = match handle {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = send(&replies, frame.id, method, Err(error), local, None);
                    continue;
                }
            };
            let engine = session.lock().unwrap().engine.clone();
            let global = workers.acquire(MAX_REQUESTS);
            let replies = replies.clone();
            pool.submit(handle.node.clone(), move || {
                let result = execute_file(&engine, &handle, request, frame.data);
                // CLOSE's barrier must not finish while a worker retains the handle.
                drop(handle);
                let _ = send(&replies, frame.id, method, result, local, Some(global));
            });
        } else {
            drop(local);
            pending.wait_empty();
            let result = execute(&mut session.lock().unwrap(), request, instance);
            let local = pending.acquire(MAX_INFLIGHT);
            if send(&replies, frame.id, method, result, local, None).is_err() {
                break;
            }
        }
    }
    pending.wait_empty();
    drop(replies);
    let _ = reply_writer.join();
    // Dropping FileHandles releases writers after every admitted operation.
}

type Reply = Result<(Response, Vec<u8>)>;
struct Outbound {
    frame: Frame,
    method: Method,
    _local: Permit,
    _global: Option<Permit>,
}

/// Slow readers consume their admission budget, never a storage worker. Every
/// queued reply retains a local permit, so the bounded queue cannot fill while
/// a sender still owns another permit. Enqueue must never wait for socket I/O.
struct Replies {
    queue: SyncSender<Outbound>,
    control: UnixStream,
}
impl Replies {
    fn start(
        mut writer: UnixStream,
        ids: Arc<Mutex<HashSet<u64>>>,
    ) -> io::Result<(Self, JoinHandle<()>)> {
        let control = writer.try_clone()?;
        let (queue, responses) = mpsc::sync_channel::<Outbound>(MAX_INFLIGHT);
        let thread = thread::spawn(move || {
            while let Ok(response) = responses.recv() {
                let result = response.frame.write(&mut writer);
                ids.lock().unwrap().remove(&response.frame.id);
                if let Err(error) = result {
                    // Wake the reader before dropping queued replies and their permits.
                    let _ = writer.shutdown(Shutdown::Both);
                    tracing::warn!(request_id = response.frame.id, method = ?response.method, %error, "IPC reply write failed");
                    break;
                }
            }
        });
        Ok((Self { queue, control }, thread))
    }
}

fn send(
    replies: &Replies,
    id: u64,
    method: Method,
    result: Reply,
    local: Permit,
    global: Option<Permit>,
) -> io::Result<()> {
    let (response, data) = match result {
        Ok(result) => result,
        Err(error) => {
            let code = error
                .chain()
                .find_map(|cause| {
                    cause
                        .downcast_ref::<io::Error>()
                        .and_then(io::Error::raw_os_error)
                })
                .unwrap_or(libc::EIO);
            if matches!(code, libc::EIO | libc::ENOSPC | libc::EPROTO) {
                tracing::error!(request_id = id, ?method, errno = code, error = %format!("{error:#}"), "IPC operation failed");
            } else {
                tracing::debug!(request_id = id, ?method, errno = code, %error, "IPC operation rejected");
            }
            (
                Response {
                    error: code,
                    message: error.to_string(),
                    ..Default::default()
                },
                vec![],
            )
        }
    };
    let response = Outbound {
        frame: Frame {
            id,
            message: response.encode_to_vec(),
            data,
        },
        method,
        _local: local,
        _global: global,
    };
    if let Err(error) = replies.queue.try_send(response) {
        let _ = replies.control.shutdown(Shutdown::Both);
        return Err(io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()));
    }
    Ok(())
}

fn ok(response: Response) -> Reply {
    Ok((response, vec![]))
}
fn handle(node: String, file: Option<FileHandle>, readable: bool, writable: bool) -> Handle {
    Handle {
        node,
        file: file.map(Mutex::new),
        readable,
        writable,
    }
}
fn execute(session: &mut Session, request: Request, instance: &str) -> Reply {
    let method = Method::try_from(request.method).map_err(|_| errno(libc::ENOSYS))?;
    if method != Method::Hello && !session.hello {
        return Err(errno(libc::EPROTO));
    }
    if !matches!(method, Method::Hello | Method::Attach) && !session.attached {
        return Err(errno(libc::ENOTCONN));
    }
    let mut response = Response::default();
    match method {
        Method::Hello => {
            if session.hello || request.protocol_version != u32::from(VERSION) {
                return Err(errno(libc::EPROTONOSUPPORT));
            }
            session.hello = true;
            response.instance = instance.to_owned();
            response.protocol_version = u32::from(VERSION);
            response.max_message = MAX_MESSAGE as u32;
            response.max_data = MAX_DATA as u32;
            response.max_inflight = MAX_INFLIGHT as u32;
        }
        Method::Attach => {
            if session.attached {
                return Err(errno(libc::EISCONN));
            }
            if !request.name.is_empty() && request.name != "/" {
                return Err(errno(libc::EINVAL));
            }
            let node = session.engine.stat_id("root")?;
            response.attr = Some(attr(&node));
            response.handle = session.insert(handle(node.id, None, false, false))?;
            session.attached = true;
        }
        Method::StatAt => {
            response.attr = Some(attr(&session.resolve(request.parent, &request.name)?))
        }
        Method::Open => {
            let access = Access::try_from(request.access).map_err(|_| errno(libc::EINVAL))?;
            let disposition =
                Disposition::try_from(request.disposition).map_err(|_| errno(libc::EINVAL))?;
            let writable = matches!(access, Access::WriteOnly | Access::ReadWrite);
            let readable = matches!(access, Access::ReadOnly | Access::ReadWrite);
            if request.truncate && !writable {
                return Err(errno(libc::EACCES));
            }
            let node = if request.handle != 0 {
                if disposition != Disposition::Existing {
                    return Err(errno(libc::EINVAL));
                }
                session
                    .engine
                    .stat_id(&session.handle(request.handle)?.node)?
            } else if disposition == Disposition::Existing {
                session.resolve(request.parent, &request.name)?
            } else {
                let (parent, name) = session.entry(request.parent, &request.name)?;
                if request.directory {
                    return Err(errno(libc::EISDIR));
                }
                let (node, created) = session.engine.create_file_at_result(
                    &parent,
                    &name,
                    disposition == Disposition::CreateNew,
                )?;
                response.created = created;
                node
            };
            if request.directory && !node.is_dir {
                return Err(errno(libc::ENOTDIR));
            }
            let file = if access == Access::Metadata || node.is_dir {
                if node.is_dir && writable {
                    return Err(errno(libc::EISDIR));
                }
                None
            } else {
                Some(
                    session
                        .engine
                        .open_id(&node.id, writable, request.truncate)?,
                )
            };
            response.attr = Some(attr(&session.engine.stat_id(&node.id)?));
            response.handle = session.insert(handle(node.id, file, readable, writable))?;
        }
        Method::Mkdir => {
            let (parent, name) = session.entry(request.parent, &request.name)?;
            response.attr = Some(attr(&session.engine.mkdir_at(&parent, &name)?));
        }
        Method::Rename => {
            let (parent, name) = session.entry(request.parent, &request.name)?;
            let (target, target_name) =
                session.entry(request.target_parent, &request.target_name)?;
            session
                .engine
                .rename_at(&parent, &name, &target, &target_name, request.replace)?;
        }
        Method::Unlink => {
            let (parent, name) = session.entry(request.parent, &request.name)?;
            session
                .engine
                .unlink_at(&parent, &name, request.directory)?;
        }
        Method::Fsinfo => {
            let (total, free) = session.engine.filesystem_stat()?;
            response.total_bytes = total;
            response.free_bytes = free;
        }
        Method::Close => {
            let entry = session
                .handles
                .remove(&request.handle)
                .ok_or_else(|| errno(libc::EBADF))?;
            let entry = Arc::try_unwrap(entry).map_err(|_| errno(libc::EBUSY))?;
            if let Some(file) = entry.file {
                file.into_inner().unwrap().close()?;
            }
        }
        Method::Opendir => {
            if session.handles.len() + session.cursors.len() >= MAX_HANDLES {
                return Err(errno(libc::EMFILE));
            }
            let node = session
                .engine
                .stat_id(&session.handle(request.handle)?.node)?;
            if !node.is_dir {
                return Err(errno(libc::ENOTDIR));
            }
            let budget = CursorBudget::reserve(
                session.cursor_budget.clone(),
                CURSOR_MEMORY_BYTES.min(MAX_CURSOR_BYTES.saturating_sub(session.cursor_bytes)),
            )?;
            let mut cursor = Cursor::new(budget);
            session.engine.visit_children(&node.id, |name, node| {
                cursor.append(Entry {
                    name,
                    attr: Some(attr(&node)),
                })
            })?;
            cursor.finish()?;
            let id = session.next;
            session.next += 1;
            session.cursor_bytes += cursor.budget.bytes;
            session.cursors.insert(id, cursor);
            response.handle = id;
        }
        Method::Readdir => {
            let cursor = session
                .cursors
                .get_mut(&request.handle)
                .ok_or_else(|| errno(libc::EBADF))?;
            let count = request.max_entries.clamp(1, 4096) as usize;
            let budget = (request.max_bytes as usize).clamp(4096, MAX_MESSAGE - 128);
            let mut bytes = 0;
            while response.entries.len() < count {
                let Some(entry) = cursor.next()? else {
                    break;
                };
                let size = entry.encoded_len() + 8;
                if size + bytes > budget {
                    cursor.pending = Some(entry);
                    break;
                }
                response.entries.push(entry);
                bytes += size;
            }
            response.eof = cursor.eof();
            response.count = response.entries.len() as u32;
        }

        Method::Closedir => {
            let cursor = session
                .cursors
                .remove(&request.handle)
                .ok_or_else(|| errno(libc::EBADF))?;
            session.cursor_bytes -= cursor.budget.bytes;
        }
        Method::Detach => {
            session.handles.clear();
            session.cursors.clear();
            session.cursor_bytes = 0;
            session.attached = false;
        }
        _ => return Err(errno(libc::ENOSYS)),
    }
    ok(response)
}

fn execute_file(engine: &Engine, handle: &Handle, request: Request, data: Vec<u8>) -> Reply {
    let method = Method::try_from(request.method).map_err(|_| errno(libc::ENOSYS))?;
    let mut response = Response::default();
    match method {
        Method::Getattr => response.attr = Some(attr(&engine.stat_id(&handle.node)?)),
        Method::Setattr => {
            let changes = request.changes.ok_or_else(|| errno(libc::EINVAL))?;
            let btime = changes.btime.map(system_time).transpose()?;
            if changes.has_mode {
                let mode = changes.mode;
                let node = engine.stat_id(&handle.node)?;
                let expected = if node.is_dir { 0o770 } else { 0o660 };
                if mode & 0o7777 != expected {
                    return Err(errno(libc::EOPNOTSUPP));
                }
            }
            let atime = changes.atime.map(system_time).transpose()?;
            let mtime = changes.mtime.map(system_time).transpose()?;
            if changes.has_size {
                let size = changes.size;
                if !handle.writable {
                    return Err(errno(libc::EBADF));
                }
                handle
                    .file
                    .as_ref()
                    .ok_or_else(|| errno(libc::EBADF))?
                    .lock()
                    .unwrap()
                    .set_len(size)?;
            }
            engine.set_times(&handle.node, atime, mtime)?;
            if let Some(btime) = btime {
                engine.set_birth_time(&handle.node, btime)?;
            }
            response.attr = Some(attr(&engine.stat_id(&handle.node)?));
        }
        Method::Read => {
            if !handle.readable {
                return Err(errno(libc::EBADF));
            }
            let mut buffer = vec![0; request.length as usize];
            let read = handle
                .file
                .as_ref()
                .ok_or_else(|| errno(libc::EISDIR))?
                .lock()
                .unwrap()
                .read_at(request.offset, &mut buffer)?;
            buffer.truncate(read);
            response.count = read as u32;
            response.eof = read < request.length as usize;
            return Ok((response, buffer));
        }
        Method::Write => {
            if !handle.writable {
                return Err(errno(libc::EBADF));
            }
            let file = handle
                .file
                .as_ref()
                .ok_or_else(|| errno(libc::EBADF))?
                .lock()
                .unwrap();
            response.count = file.write_at(request.offset, &data)? as u32;
            if request.stable {
                file.sync()?;
            }
        }
        Method::Sync => {
            if let Some(file) = &handle.file {
                file.lock().unwrap().sync()?;
            } else {
                return Err(errno(libc::EBADF));
            }
        }
        _ => return Err(errno(libc::ENOSYS)),
    }
    ok(response)
}

fn timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(value) => Timestamp {
            seconds: value.as_secs() as i64,
            nanos: value.subsec_nanos(),
        },
        Err(error) => {
            let value = error.duration();
            if value.subsec_nanos() == 0 {
                Timestamp {
                    seconds: -(value.as_secs() as i64),
                    nanos: 0,
                }
            } else {
                Timestamp {
                    seconds: -(value.as_secs() as i64) - 1,
                    nanos: 1_000_000_000 - value.subsec_nanos(),
                }
            }
        }
    }
}
fn system_time(value: Timestamp) -> Result<SystemTime> {
    if value.nanos >= 1_000_000_000 {
        return Err(errno(libc::EINVAL));
    }
    let seconds = if value.seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(value.seconds as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(value.seconds.unsigned_abs()))
    };
    seconds
        .and_then(|time| time.checked_add(Duration::from_nanos(value.nanos as u64)))
        .ok_or_else(|| errno(libc::EOVERFLOW))
}
fn attr(node: &Node) -> Attr {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(node.id.as_bytes());
    let inode = if node.id == "root" {
        1
    } else {
        u64::from_le_bytes(digest[..8].try_into().unwrap()) | 2
    };
    Attr {
        node_id: node.id.clone(),
        inode,
        directory: node.is_dir,
        size: node.size,
        allocated: node.size.div_ceil(4096).saturating_mul(4096),
        mode: if node.is_dir {
            libc::S_IFDIR | 0o770
        } else {
            libc::S_IFREG | 0o660
        },
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        nlink: if node.is_dir { 2 } else { 1 },
        atime: Some(timestamp(node.atime)),
        mtime: Some(timestamp(node.mtime)),
        ctime: Some(timestamp(node.ctime)),
        btime: Some(timestamp(node.btime)),
        version: node.version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spooled_cursor_preserves_order_pending_and_memory_budget() -> Result<()> {
        for available in [0, 32, 4096, CURSOR_MEMORY_BYTES] {
            let used = Arc::new(Mutex::new(0));
            let mut cursor = Cursor::new(CursorBudget::reserve(used.clone(), available)?);
            for index in 0..1000 {
                cursor.append(Entry {
                    name: format!("entry-{index:04}"),
                    attr: None,
                })?;
            }
            cursor.finish()?;
            assert!(matches!(cursor.source, CursorSource::Disk(_)));
            assert_eq!(cursor.budget.bytes, available.min(CURSOR_BUFFER_BYTES));
            assert_eq!(*used.lock().unwrap(), cursor.budget.bytes);
            let first = cursor.next()?.unwrap();
            cursor.pending = Some(first);
            for index in 0..1000 {
                assert_eq!(cursor.next()?.unwrap().name, format!("entry-{index:04}"));
            }
            assert!(cursor.eof());
            assert!(cursor.next()?.is_none());
            drop(cursor);
            assert_eq!(*used.lock().unwrap(), 0);
        }
        Ok(())
    }

    #[test]
    fn cursor_finish_propagates_buffered_write_failure() -> Result<()> {
        let temporary = tempfile::NamedTempFile::new()?;
        let readonly = fs::File::open(temporary.path())?;
        let used = Arc::new(Mutex::new(0));
        let mut cursor = Cursor::new(CursorBudget::reserve(used, 4096)?);
        cursor.source = CursorSource::Spooling(BufWriter::with_capacity(4096, readonly));
        cursor.append(Entry {
            name: "buffered".into(),
            attr: None,
        })?;
        assert!(cursor.finish().is_err());
        Ok(())
    }

    #[test]
    fn spooled_cursor_rejects_truncated_record() -> Result<()> {
        let used = Arc::new(Mutex::new(0));
        let mut cursor = Cursor::new(CursorBudget::reserve(used, 32)?);
        cursor.append(Entry {
            name: "truncated".into(),
            attr: None,
        })?;
        cursor.finish()?;
        let CursorSource::Disk(reader) = &mut cursor.source else {
            panic!("expected spool")
        };
        reader.get_mut().set_len(3)?;
        assert!(cursor.next().is_err());
        Ok(())
    }
}
