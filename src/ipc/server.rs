use super::scheduler::FileOrder;
use super::{BUFFER_MIB_PER_SLOT, Frame, IpcOptions, MAX_DATA, MAX_MESSAGE, VERSION, protocol::*};
use crate::core::{Engine, FileHandle, Node};
use anyhow::{Result, anyhow};
use futures_util::{StreamExt, stream::FuturesUnordered};
use prost::Message;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, BufReader, BufWriter, Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinSet,
};

struct Shared {
    options: Arc<IpcOptions>,
    engine: Engine,
    workers: Arc<Semaphore>,
    buffers: Buffers,
    file_order: Arc<FileOrder>,
    cursor_budget: Arc<Mutex<usize>>,
    instance: String,
}
struct Buffers {
    data: Arc<Semaphore>,
    control: Arc<Semaphore>,
}
impl Buffers {
    fn new(options: &IpcOptions) -> Self {
        Self {
            data: Arc::new(Semaphore::new(
                options.data_buffer_mib / BUFFER_MIB_PER_SLOT,
            )),
            control: Arc::new(Semaphore::new(
                options.control_buffer_mib / BUFFER_MIB_PER_SLOT,
            )),
        }
    }
}
struct BufferPermit {
    _global: OwnedSemaphorePermit,
    _connection: OwnedSemaphorePermit,
}
impl Buffers {
    async fn reserve(
        &self,
        method: Method,
        data: Arc<Semaphore>,
        control: Arc<Semaphore>,
    ) -> BufferPermit {
        let (global, connection) =
            if matches!(method, Method::Read | Method::Write | Method::Readdir) {
                (&self.data, data)
            } else {
                (&self.control, control)
            };
        // A saturated connection waits locally, without reserving global space
        // needed by another connection in the same traffic class.
        let connection = connection.acquire_owned().await.unwrap();
        let global = global.clone().acquire_owned().await.unwrap();
        BufferPermit {
            _global: global,
            _connection: connection,
        }
    }
}
fn errno(code: i32) -> anyhow::Error {
    io::Error::from_raw_os_error(code).into()
}

struct Handle {
    node: String,
    // Only blocking operations access this lock; it never crosses an await.
    file: Option<Mutex<FileHandle>>,
    readable: bool,
    writable: bool,
}
struct CursorBudget {
    used: Arc<Mutex<usize>>,
    bytes: usize,
}
impl CursorBudget {
    fn reserve(used: Arc<Mutex<usize>>, limit: usize, global_limit: usize) -> Result<Self> {
        let mut total = used.lock().unwrap();
        let bytes = limit.min(global_limit.saturating_sub(*total));
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
    buffer_bytes: usize,
}
impl Cursor {
    fn new(budget: CursorBudget, buffer_bytes: usize) -> Self {
        Self {
            source: CursorSource::Memory(VecDeque::new()),
            pending: None,
            remaining: 0,
            memory_bytes: 0,
            budget,
            buffer_bytes,
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
            self.budget.shrink(self.budget.bytes.min(self.buffer_bytes));
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
    options: Arc<IpcOptions>,
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
        if self.handles.len() + self.cursors.len() >= self.options.max_handles_per_connection {
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
/// Drop signals cancellation; callers use shutdown to await storage cleanup.
pub struct IpcServer {
    stop: watch::Sender<bool>,
    listener: Option<tokio::task::JoinHandle<()>>,
}
impl IpcServer {
    pub async fn start(engine: Engine, path: &Path, options: &IpcOptions) -> Result<Self> {
        options.validate()?;
        let options = Arc::new(options.clone());
        let path = path.to_owned();
        let (listener, lock, path) = tokio::task::spawn_blocking(move || -> Result<_> {
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
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    match std::os::unix::net::UnixStream::connect(&path) {
                        Ok(_) => return Err(anyhow!("IPC socket is active: {}", path.display())),
                        Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {
                            fs::remove_file(&path)?
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(_) => return Err(anyhow!("IPC path is not a socket: {}", path.display())),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok((listener, lock, path))
        })
        .await??;
        let listener = UnixListener::from_std(listener)?;
        let (stop, mut stopped) = watch::channel(false);
        let shutdown = stop.clone();
        let listener = tokio::spawn(async move {
            let shared = Arc::new(Shared {
                engine,
                workers: Arc::new(Semaphore::new(options.max_running_requests)),
                buffers: Buffers::new(&options),
                options,
                file_order: Arc::new(FileOrder::default()),
                cursor_budget: Arc::new(Mutex::new(0)),
                instance: uuid::Uuid::new_v4().to_string(),
            });
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = async { let _ = stopped.wait_for(|value| *value).await; } => break,
                    result = sessions.join_next(), if !sessions.is_empty() => {
                        if let Some(Err(error)) = result { tracing::error!(%error, "IPC session failed"); }
                    }
                    incoming = listener.accept() => {
                        match incoming {
                            Ok((stream, _)) if trusted_peer(&stream) && sessions.len() < shared.options.max_connections => {
                                sessions.spawn(serve(stream, shared.clone(), stopped.clone()));
                            }
                            Ok(_) => {},
                            Err(error) => { tracing::error!(%error, "IPC accept failed"); break; }
                        }
                    }
                }
            }
            shutdown.send_replace(true);
            drop(listener);
            // Keep the lock until all FileHandles have been released, even when
            // the public server was dropped without explicitly awaiting shutdown.
            while let Some(result) = sessions.join_next().await {
                if let Err(error) = result {
                    tracing::error!(%error, "IPC session failed while draining");
                }
            }
            let _ = tokio::task::spawn_blocking(move || {
                let _ = fs::remove_file(path);
                drop(shared);
                drop(lock);
            })
            .await;
        });
        Ok(Self {
            stop,
            listener: Some(listener),
        })
    }
    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        if let Some(listener) = self.listener.take()
            && let Err(error) = listener.await
        {
            tracing::error!(%error, "IPC listener failed");
        }
    }
}
impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

fn trusted_peer(stream: &UnixStream) -> bool {
    stream.peer_cred().is_ok_and(|credentials| {
        credentials.uid() == 0 || credentials.uid() == unsafe { libc::geteuid() }
    })
}

async fn serve(stream: UnixStream, shared: Arc<Shared>, mut stopped: watch::Receiver<bool>) {
    let (mut reader, mut writer) = stream.into_split();
    let mut session = Some(Session {
        options: shared.options.clone(),
        engine: shared.engine.clone(),
        handles: HashMap::new(),
        cursors: HashMap::new(),
        next: u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) >> 1,
        cursor_bytes: 0,
        cursor_budget: shared.cursor_budget.clone(),
        hello: false,
        attached: false,
    });
    let limiter = Arc::new(Semaphore::new(shared.options.max_inflight_per_connection));
    let data_buffers = Arc::new(Semaphore::new(
        shared.options.connection_data_buffer_mib / BUFFER_MIB_PER_SLOT,
    ));
    let control_buffers = Arc::new(Semaphore::new(
        shared.options.connection_control_buffer_mib / BUFFER_MIB_PER_SLOT,
    ));
    // This lock protects only an in-memory set; it never crosses await or I/O.
    let ids = Arc::new(Mutex::new(HashSet::new()));
    let (replies, mut responses) =
        mpsc::channel::<Outbound>(shared.options.max_inflight_per_connection);
    let (failed, mut failure) = watch::channel(false);
    let writer_ids = ids.clone();
    let mut writer_failure = failure.clone();
    let writer_failed = failed.clone();
    let mut writer_stop = stopped.clone();
    let reply_timeout = Duration::from_secs(shared.options.reply_timeout_secs);
    let reply_writer = tokio::spawn(async move {
        loop {
            let response = tokio::select! {
                biased;
                _ = async { let _ = writer_stop.wait_for(|value| *value).await; } => break,
                _ = async { let _ = writer_failure.wait_for(|value| *value).await; } => break,
                response = responses.recv() => match response { Some(response) => response, None => break },
            };
            let result = tokio::select! {
                biased;
                _ = async { let _ = writer_stop.wait_for(|value| *value).await; } => break,
                _ = async { let _ = writer_failure.wait_for(|value| *value).await; } => break,
                result = tokio::time::timeout(reply_timeout, response.frame.write_async(&mut writer)) => result,
            };
            writer_ids.lock().unwrap().remove(&response.frame.id);
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!(request_id = response.frame.id, method = ?response.method, error = ?result, "IPC reply write failed");
                break;
            }
        }
        writer_failed.send_replace(true);
    });
    let mut tasks = FuturesUnordered::new();
    'requests: loop {
        let limited_frame = async {
            // Limit in-flight requests and queued replies before allocating a frame.
            let local = limiter.clone().acquire_owned().await.unwrap();
            let frame = Frame::read_async(&mut reader).await;
            (local, frame)
        };
        // Keep the same read future across completions: cancelling a partial
        // frame read would lose its header or body and corrupt the stream.
        let (local, frame) = {
            tokio::pin!(limited_frame);
            loop {
                tokio::select! {
                    biased;
                    _ = async { let _ = stopped.wait_for(|value| *value).await; } => break 'requests,
                    _ = async { let _ = failure.wait_for(|value| *value).await; } => break 'requests,
                    _ = tasks.next(), if !tasks.is_empty() => {},
                    frame = &mut limited_frame => break frame,
                }
            }
        };
        let frame = match frame {
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
        let global = {
            let reservation =
                shared
                    .buffers
                    .reserve(method, data_buffers.clone(), control_buffers.clone());
            // Poll completions while waiting for buffers, retaining this request's
            // position in the semaphore waiters across each completion.
            tokio::pin!(reservation);
            loop {
                tokio::select! {
                    biased;
                    _ = async { let _ = stopped.wait_for(|value| *value).await; } => break 'requests,
                    _ = async { let _ = failure.wait_for(|value| *value).await; } => break 'requests,
                    _ = tasks.next(), if !tasks.is_empty() => {},
                    permit = &mut reservation => break permit,
                }
            }
        };
        let data_valid = if method == Method::Write {
            frame.data.len() == request.length as usize
        } else {
            frame.data.is_empty()
        };
        if !data_valid || request.length as usize > MAX_DATA {
            if send(
                &replies,
                frame.id,
                method,
                Err(errno(libc::EINVAL)),
                local,
                global,
            )
            .is_err()
            {
                break;
            }
            continue;
        }
        if matches!(
            method,
            Method::Read | Method::Write | Method::Sync | Method::Getattr | Method::Setattr
        ) {
            let current = session.as_ref().unwrap();
            let handle = match current.handle(request.handle) {
                Ok(handle) => handle,
                Err(error) => {
                    if send(&replies, frame.id, method, Err(error), local, global).is_err() {
                        break;
                    }
                    continue;
                }
            };
            let engine = current.engine.clone();
            let replies = replies.clone();
            let operation =
                shared
                    .file_order
                    .spawn(handle.node.clone(), shared.workers.clone(), move || {
                        let result = execute_file(&engine, &handle, request, frame.data);
                        // FileHandle destruction and its core locks stay on this thread.
                        drop(handle);
                        send(&replies, frame.id, method, result, local, global)
                    });
            let failed = failed.clone();
            tasks.push(async move {
                let result = operation
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                if let Err(error) = result {
                    failed.send_replace(true);
                    tracing::error!(%error, "IPC storage operation failed");
                }
            });
        } else {
            // Namespace changes and CLOSE are session barriers, independent of
            // whether the client has consumed replies from earlier operations.
            while tasks.next().await.is_some() {}
            let mut current = session.take().unwrap();
            let instance = shared.instance.clone();
            let permit = shared.workers.clone().acquire_owned().await.unwrap();
            let replies = replies.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let result = execute(&mut current, request, &instance);
                (
                    current,
                    send(&replies, frame.id, method, result, local, global),
                )
            })
            .await;
            let result = match result {
                Ok((current, result)) => {
                    session = Some(current);
                    result
                }
                Err(error) => {
                    tracing::error!(%error, "IPC session operation failed");
                    break;
                }
            };
            if result.is_err() {
                break;
            }
        }
    }
    // Never abort admitted work: a timed-out/disconnected client does not cancel
    // a disk write, release its execution permit, or race handle destruction.
    while tasks.next().await.is_some() {}
    drop(replies);
    let _ = reply_writer.await;
    if let Err(error) = tokio::task::spawn_blocking(move || drop(session)).await {
        tracing::error!(%error, "IPC session cleanup failed");
    }
}

type Reply = Result<(Response, Vec<u8>)>;
struct Outbound {
    frame: Frame,
    method: Method,
    _local: OwnedSemaphorePermit,
    _global: BufferPermit,
}

fn send(
    replies: &mpsc::Sender<Outbound>,
    id: u64,
    method: Method,
    result: Reply,
    local: OwnedSemaphorePermit,
    global: BufferPermit,
) -> Result<()> {
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
    // Each sender owns one of the same in-flight permits as queued replies,
    // so enqueue never waits for network I/O or holds an execution permit.
    replies
        .try_send(Outbound {
            frame: Frame {
                id,
                message: response.encode_to_vec(),
                data,
            },
            method,
            _local: local,
            _global: global,
        })
        .map_err(|error| anyhow!("IPC reply queue closed: {error}"))
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
            response.max_inflight = session.options.max_inflight_per_connection as u32;
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
            if session.handles.len() + session.cursors.len()
                >= session.options.max_handles_per_connection
            {
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
                (session.options.per_cursor_memory_kib * 1024).min(
                    (session.options.connection_cursor_memory_mib * 1024 * 1024)
                        .saturating_sub(session.cursor_bytes),
                ),
                session.options.global_cursor_memory_mib * 1024 * 1024,
            )?;
            let mut cursor = Cursor::new(budget, session.options.cursor_buffer_kib * 1024);
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

    #[tokio::test]
    async fn full_data_budget_does_not_block_metadata() {
        let options = IpcOptions::default();
        let buffers = Buffers::new(&options);
        let global_slots = options.data_buffer_mib / BUFFER_MIB_PER_SLOT;
        let local_slots = options.connection_data_buffer_mib / BUFFER_MIB_PER_SLOT;
        let data = buffers
            .data
            .clone()
            .acquire_many_owned(global_slots as u32)
            .await
            .unwrap();
        let session = Arc::new(Semaphore::new(local_slots));
        let control_slots = options.connection_control_buffer_mib / BUFFER_MIB_PER_SLOT;
        let control = Arc::new(Semaphore::new(control_slots));
        let metadata = tokio::time::timeout(
            Duration::from_secs(1),
            buffers.reserve(Method::Getattr, session.clone(), control.clone()),
        )
        .await
        .unwrap();
        assert_eq!(buffers.control.available_permits(), global_slots - 1);
        assert_eq!(control.available_permits(), control_slots - 1);
        assert_eq!(session.available_permits(), local_slots);
        for method in [Method::Read, Method::Write, Method::Readdir] {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(10),
                    buffers.reserve(method, session.clone(), control.clone())
                )
                .await
                .is_err()
            );
        }
        assert_eq!(session.available_permits(), local_slots);
        drop(data);
        drop(metadata);
        assert_eq!(control.available_permits(), control_slots);
        let read = buffers
            .reserve(Method::Read, session.clone(), control.clone())
            .await;
        assert_eq!(session.available_permits(), local_slots - 1);
        assert_eq!(buffers.control.available_permits(), global_slots);
        drop(read);
        assert_eq!(session.available_permits(), local_slots);
    }

    #[tokio::test]
    async fn saturated_connection_preserves_control_budget_for_other_connections() {
        let options = IpcOptions::default();
        let buffers = Buffers::new(&options);
        let global_slots = options.control_buffer_mib / BUFFER_MIB_PER_SLOT;
        let local_slots = options.connection_control_buffer_mib / BUFFER_MIB_PER_SLOT;
        let data = Arc::new(Semaphore::new(
            options.connection_data_buffer_mib / BUFFER_MIB_PER_SLOT,
        ));
        let busy = Arc::new(Semaphore::new(local_slots));
        let other = Arc::new(Semaphore::new(local_slots));
        let mut retained = Vec::new();
        for _ in 0..local_slots {
            retained.push(
                buffers
                    .reserve(Method::Getattr, data.clone(), busy.clone())
                    .await,
            );
        }
        assert_eq!(busy.available_permits(), 0);
        assert_eq!(
            buffers.control.available_permits(),
            global_slots - local_slots
        );
        let blocked = buffers.reserve(Method::Getattr, data.clone(), busy.clone());
        tokio::pin!(blocked);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut blocked)
                .await
                .is_err()
        );
        let progress = tokio::time::timeout(
            Duration::from_secs(1),
            buffers.reserve(Method::Getattr, data, other.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            buffers.control.available_permits(),
            global_slots - local_slots - 1
        );
        drop(progress);
        retained.pop();
        let resumed = tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .unwrap();
        drop(resumed);
        drop(retained);
        assert_eq!(buffers.control.available_permits(), global_slots);
        assert_eq!(busy.available_permits(), local_slots);
        assert_eq!(other.available_permits(), local_slots);
    }

    #[test]
    fn spooled_cursor_preserves_order_pending_and_memory_budget() -> Result<()> {
        for available in [0, 32, 4096, 256 * 1024] {
            let used = Arc::new(Mutex::new(0));
            let mut cursor = Cursor::new(
                CursorBudget::reserve(used.clone(), available, 128 * 1024 * 1024)?,
                64 * 1024,
            );
            for index in 0..1000 {
                cursor.append(Entry {
                    name: format!("entry-{index:04}"),
                    attr: None,
                })?;
            }
            cursor.finish()?;
            assert!(matches!(cursor.source, CursorSource::Disk(_)));
            assert_eq!(cursor.budget.bytes, available.min(64 * 1024));
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
        let mut cursor = Cursor::new(
            CursorBudget::reserve(used, 4096, 128 * 1024 * 1024)?,
            64 * 1024,
        );
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
        let mut cursor = Cursor::new(
            CursorBudget::reserve(used, 32, 128 * 1024 * 1024)?,
            64 * 1024,
        );
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
