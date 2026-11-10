use anyhow::{Context, Result};
use prost::Message;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Cursor, Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::Duration,
};
use tier::{
    core::{Engine, Policy},
    ipc::{
        Frame, IpcOptions, IpcServer, MAGIC, MAX_DATA, MAX_INFLIGHT, MAX_MESSAGE, VERSION,
        protocol::*,
    },
};

struct Client {
    stream: UnixStream,
    next: u64,
    root: u64,
}
impl Client {
    fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut client = Self {
            stream,
            next: 1,
            root: 0,
        };
        client.call(
            Request {
                method: Method::Hello as i32,
                protocol_version: VERSION.into(),
                ..Default::default()
            },
            vec![],
        )?;
        client.root = client
            .call(
                Request {
                    method: Method::Attach as i32,
                    ..Default::default()
                },
                vec![],
            )?
            .0
            .handle;
        Ok(client)
    }
    fn submit(&mut self, request: Request, data: Vec<u8>) -> Result<u64> {
        let id = self.next;
        self.next += 1;
        Frame::request(id, &request, data).write(&mut self.stream)?;
        Ok(id)
    }
    fn receive(&mut self) -> Result<(u64, Response, Vec<u8>)> {
        let frame = Frame::read(&mut self.stream)?.context("IPC connection closed before reply")?;
        Ok((
            frame.id,
            Response::decode(frame.message.as_slice())?,
            frame.data,
        ))
    }
    fn call(&mut self, request: Request, data: Vec<u8>) -> Result<(Response, Vec<u8>)> {
        let id = self.submit(request, data)?;
        let (received, response, data) = self.receive()?;
        assert_eq!(received, id);
        assert_eq!(response.error, 0, "{}", response.message);
        Ok((response, data))
    }
    fn open(&mut self, name: &str, access: Access, disposition: Disposition) -> Result<u64> {
        Ok(self
            .call(
                Request {
                    method: Method::Open as i32,
                    parent: self.root,
                    name: name.into(),
                    access: access as i32,
                    disposition: disposition as i32,
                    ..Default::default()
                },
                vec![],
            )?
            .0
            .handle)
    }
}
struct TestServer {
    runtime: tokio::runtime::Runtime,
    server: Option<IpcServer>,
}
impl TestServer {
    fn start(engine: Engine, path: &Path, blocking_threads: usize) -> Result<Self> {
        Self::with_options(engine, path, blocking_threads, &IpcOptions::default())
    }
    fn with_options(
        engine: Engine,
        path: &Path,
        blocking_threads: usize,
        options: &IpcOptions,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(blocking_threads)
            .enable_all()
            .build()?;
        let server = runtime.block_on(IpcServer::start(engine, path, options))?;
        Ok(Self {
            runtime,
            server: Some(server),
        })
    }
    fn shutdown(&mut self) {
        if let Some(server) = self.server.take() {
            self.runtime.block_on(server.shutdown());
        }
    }
}
impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn fixture() -> Result<(tempfile::TempDir, Engine, TestServer)> {
    fixture_with_threads(2)
}
fn fixture_with_threads(
    blocking_threads: usize,
) -> Result<(tempfile::TempDir, Engine, TestServer)> {
    fixture_with_options(blocking_threads, &IpcOptions::default())
}
fn fixture_with_options(
    blocking_threads: usize,
    options: &IpcOptions,
) -> Result<(tempfile::TempDir, Engine, TestServer)> {
    let temp = tempfile::tempdir()?;
    let engine = Engine::open(&temp.path().join("metadata"))?;
    let target = temp.path().join("storage");
    fs::create_dir(&target)?;
    engine.register_target("storage", &target, None, true)?;
    engine.set_policy(
        "/",
        &Policy {
            persistent_storage: vec!["storage".into()],
            persistent_copies: 1,
            ..Default::default()
        },
    )?;
    let server = TestServer::with_options(
        engine.clone(),
        &temp.path().join("tier.sock"),
        blocking_threads,
        options,
    )?;
    Ok((temp, engine, server))
}

#[test]
fn additive_protobuf_fields_do_not_change_existing_request_semantics() -> Result<()> {
    let (temp, _engine, _server) = fixture()?;
    let mut stream = UnixStream::connect(temp.path().join("tier.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    // Stable HELLO wire vector plus an unknown varint field 100. This exercises
    // the actual server parser rather than assuming every protobuf runtime
    // preserves unknown fields when re-encoding.
    Frame {
        id: 1,
        message: vec![0x08, 0x01, 0x90, 0x01, 0x01, 0xa0, 0x06, 0x07],
        data: vec![],
    }
    .write(&mut stream)?;
    let frame = Frame::read(&mut stream)?.unwrap();
    let response = Response::decode(frame.message.as_slice())?;
    assert_eq!(response.error, 0);
    assert_eq!(response.protocol_version, VERSION as u32);

    let omitted = Changes::decode([].as_slice())?;
    let zero_size = Changes::decode([0x28, 0x01].as_slice())?;
    assert!(!omitted.has_size);
    assert!(zero_size.has_size);
    assert_eq!(zero_size.size, 0);
    Ok(())
}

#[test]
fn framing_rejects_bad_headers_and_truncated_payloads() -> Result<()> {
    let frame = Frame::request(
        42,
        &Request {
            method: Method::Write as i32,
            length: 3,
            ..Default::default()
        },
        b"abc".to_vec(),
    );
    let mut bytes = vec![];
    frame.write(&mut bytes)?;
    let decoded = Frame::read(&mut Cursor::new(&bytes))?.unwrap();
    assert_eq!(decoded.id, 42);
    assert_eq!(decoded.data, b"abc");
    assert!(Frame::read(&mut Cursor::new(&bytes[..bytes.len() - 1])).is_err());
    for (offset, value) in [(0, 0), (5, 2), (7, 1)] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        assert!(Frame::read(&mut Cursor::new(bad)).is_err());
    }
    for (offset, length) in [(16, MAX_MESSAGE + 1), (20, MAX_DATA + 1)] {
        let mut bad = bytes.clone();
        bad[offset..offset + 4].copy_from_slice(&(length as u32).to_be_bytes());
        assert!(Frame::read(&mut Cursor::new(bad)).is_err());
    }
    assert!(Frame::read(&mut Cursor::new([]))?.is_none());
    Ok(())
}

#[test]
fn pipelined_write_sync_read_and_close_across_two_handles() -> Result<()> {
    let (temp, engine, _server) = fixture()?;
    let mut client = Client::connect(&temp.path().join("tier.sock"))?;
    let writer = client.open("file", Access::ReadWrite, Disposition::CreateNew)?;
    let reader = client.open("file", Access::ReadOnly, Disposition::Existing)?;
    let data = vec![0x53; MAX_DATA];
    let write = client.submit(
        Request {
            method: Method::Write as i32,
            handle: writer,
            length: data.len() as u32,
            ..Default::default()
        },
        data.clone(),
    )?;
    let sync = client.submit(
        Request {
            method: Method::Sync as i32,
            handle: reader,
            ..Default::default()
        },
        vec![],
    )?;
    let read = client.submit(
        Request {
            method: Method::Read as i32,
            handle: reader,
            length: data.len() as u32,
            ..Default::default()
        },
        vec![],
    )?;
    let close = client.submit(
        Request {
            method: Method::Close as i32,
            handle: writer,
            ..Default::default()
        },
        vec![],
    )?;
    let mut responses = HashMap::new();
    for _ in 0..4 {
        let (id, response, data) = client.receive()?;
        assert_eq!(response.error, 0, "{}", response.message);
        responses.insert(id, (response, data));
    }
    assert_eq!(responses[&write].0.count, MAX_DATA as u32);
    assert_eq!(responses[&sync].0.error, 0);
    assert_eq!(responses[&read].1, data);
    assert_eq!(responses[&close].0.error, 0);
    client.submit(
        Request {
            method: Method::Getattr as i32,
            handle: writer,
            ..Default::default()
        },
        vec![],
    )?;
    assert_eq!(client.receive()?.1.error, libc::EBADF);
    assert_eq!(engine.get("/file")?, data);
    Ok(())
}

#[test]
fn stable_handles_survive_rename_and_directory_cursor_is_a_view() -> Result<()> {
    let (temp, _engine, _server) = fixture()?;
    let mut client = Client::connect(&temp.path().join("tier.sock"))?;
    let metadata = client.open("file", Access::Metadata, Disposition::CreateNew)?;
    let cursor = client
        .call(
            Request {
                method: Method::Opendir as i32,
                handle: client.root,
                ..Default::default()
            },
            vec![],
        )?
        .0
        .handle;
    client.call(
        Request {
            method: Method::Rename as i32,
            parent: client.root,
            name: "file".into(),
            target_parent: client.root,
            target_name: "renamed".into(),
            replace: false,
            ..Default::default()
        },
        vec![],
    )?;
    let opened = client
        .call(
            Request {
                method: Method::Open as i32,
                handle: metadata,
                access: Access::ReadWrite as i32,
                ..Default::default()
            },
            vec![],
        )?
        .0;
    client.call(
        Request {
            method: Method::Write as i32,
            handle: opened.handle,
            length: 3,
            ..Default::default()
        },
        b"new".to_vec(),
    )?;
    let page = client
        .call(
            Request {
                method: Method::Readdir as i32,
                handle: cursor,
                max_entries: 1,
                max_bytes: 4096,
                ..Default::default()
            },
            vec![],
        )?
        .0;
    assert_eq!(page.entries[0].name, "file");
    assert!(page.eof);
    let current = client
        .call(
            Request {
                method: Method::StatAt as i32,
                parent: client.root,
                name: "renamed".into(),
                ..Default::default()
            },
            vec![],
        )?
        .0
        .attr
        .unwrap();
    assert_eq!(current.node_id, opened.attr.unwrap().node_id);
    assert_eq!(current.size, 3);
    Ok(())
}

#[test]
fn disconnect_drains_writes_and_new_session_rejects_old_handles() -> Result<()> {
    let (temp, engine, mut server) = fixture()?;
    let path = temp.path().join("tier.sock");
    let mut client = Client::connect(&path)?;
    let writer = client.open("file", Access::ReadWrite, Disposition::CreateNew)?;
    let mut writes = HashSet::new();
    for index in 0..8 {
        writes.insert(client.submit(
            Request {
                method: Method::Write as i32,
                handle: writer,
                offset: index * 4,
                length: 4,
                stable: index == 7,
                ..Default::default()
            },
            b"safe".to_vec(),
        )?);
    }
    // EOF follows the complete requests. The server must finish accepted work
    // and drain its replies even when the client has stopped sending.
    client.stream.shutdown(Shutdown::Write)?;
    for _ in 0..writes.len() {
        let (id, response, _) = client.receive()?;
        assert!(writes.remove(&id), "unexpected or duplicate write reply");
        assert_eq!(response.error, 0, "{}", response.message);
        assert_eq!(response.count, 4);
    }
    assert!(Frame::read(&mut client.stream)?.is_none());
    server.shutdown();
    drop(client);
    let _server = TestServer::start(engine.clone(), &path, 2)?;
    let mut next = Client::connect(&path)?;
    next.submit(
        Request {
            method: Method::Read as i32,
            handle: writer,
            length: 4,
            ..Default::default()
        },
        vec![],
    )?;
    assert_eq!(next.receive()?.1.error, libc::EBADF);
    assert_eq!(engine.get("/file")?, b"safe".repeat(8));
    // Disconnect cleanup releases the writer reservation as well as IPC handles.
    engine.open_file("/file", true, false, false)?.close()?;
    Ok(())
}

#[test]
fn malformed_session_does_not_kill_listener_and_stale_socket_is_replaced() -> Result<()> {
    let (temp, engine, mut server) = fixture()?;
    let path = temp.path().join("tier.sock");
    let mut bad = UnixStream::connect(&path)?;
    let mut header = [0u8; 24];
    header[..4].copy_from_slice(&MAGIC.to_be_bytes());
    header[4..6].copy_from_slice(&VERSION.to_be_bytes());
    header[16..20].copy_from_slice(&((MAX_MESSAGE + 1) as u32).to_be_bytes());
    bad.write_all(&header)?;
    bad.set_read_timeout(Some(Duration::from_secs(2)))?;
    assert_eq!(bad.read(&mut [0u8; 1])?, 0);
    let _client = Client::connect(&path)?;
    assert!(TestServer::start(engine.clone(), &path, 2).is_err());
    server.shutdown();
    let stale = UnixListener::bind(&path)?;
    drop(stale);
    let _server = TestServer::start(engine, &path, 2)?;
    let _client = Client::connect(&path)?;
    Ok(())
}

#[test]
fn partial_frame_does_not_block_other_sessions_or_shutdown() -> Result<()> {
    let (temp, _engine, mut server) = fixture_with_threads(1)?;
    let path = temp.path().join("tier.sock");
    let mut stalled = UnixStream::connect(&path)?;
    let mut bytes = Vec::new();
    Frame::request(
        1,
        &Request {
            method: Method::Hello as i32,
            protocol_version: VERSION.into(),
            ..Default::default()
        },
        vec![],
    )
    .write(&mut bytes)?;
    stalled.write_all(&bytes[..12])?;
    let mut active = Client::connect(&path)?;
    active.open("progress", Access::Metadata, Disposition::CreateNew)?;
    let started = std::time::Instant::now();
    server.shutdown();
    anyhow::ensure!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown waited for an incomplete IPC frame"
    );
    Ok(())
}

#[test]
fn completing_requests_preserve_the_next_partially_received_frame() -> Result<()> {
    let (temp, engine, _server) = fixture_with_threads(1)?;
    let mut client = Client::connect(&temp.path().join("tier.sock"))?;
    let handle = client.open("file", Access::ReadWrite, Disposition::CreateNew)?;
    let payload = vec![0x61; 64];
    for split in [1, 12, 23, 24, 32, 63] {
        let first = client.submit(
            Request {
                method: Method::Getattr as i32,
                handle,
                ..Default::default()
            },
            vec![],
        )?;
        let second = client.next;
        client.next += 1;
        let mut bytes = Vec::new();
        Frame::request(
            second,
            &Request {
                method: Method::Write as i32,
                handle,
                length: payload.len() as u32,
                ..Default::default()
            },
            payload.clone(),
        )
        .write(&mut bytes)?;
        client.stream.write_all(&bytes[..split])?;
        // Complete the earlier operation while the next frame is incomplete.
        // Its completion must not cancel/restart the in-progress socket read.
        let (id, response, _) = client.receive()?;
        assert_eq!(id, first);
        assert_eq!(response.error, 0, "{}", response.message);
        client.stream.write_all(&bytes[split..])?;
        let (id, response, _) = client.receive()?;
        assert_eq!(id, second);
        assert_eq!(response.error, 0, "{}", response.message);
        assert_eq!(response.count, payload.len() as u32);
    }
    assert_eq!(engine.get("/file")?, payload);
    Ok(())
}

#[test]
fn directory_spill_keeps_snapshot_and_pages_bounded() -> Result<()> {
    let (temp, engine, _server) = fixture()?;
    for n in 0..1000 {
        engine.mkdir(&format!("/directory-{n:04}"))?;
    }
    let mut client = Client::connect(&temp.path().join("tier.sock"))?;
    let cursor = client
        .call(
            Request {
                method: Method::Opendir as i32,
                handle: client.root,
                ..Default::default()
            },
            vec![],
        )?
        .0
        .handle;
    engine.rename("/directory-0000", "/changed")?;
    let mut names = vec![];
    loop {
        let page = client
            .call(
                Request {
                    method: Method::Readdir as i32,
                    handle: cursor,
                    max_entries: 31,
                    max_bytes: 4096,
                    ..Default::default()
                },
                vec![],
            )?
            .0;
        assert!(page.entries.len() <= 31);
        assert!(page.encoded_len() <= 4096 + 128);
        names.extend(page.entries.into_iter().map(|entry| entry.name));
        if page.eof {
            break;
        }
    }
    assert_eq!(names.len(), 1000);
    assert_eq!(names[0], "directory-0000");
    assert_eq!(names[999], "directory-0999");
    client.call(
        Request {
            method: Method::Closedir as i32,
            handle: cursor,
            ..Default::default()
        },
        vec![],
    )?;
    Ok(())
}

#[test]
fn invalid_write_body_and_timestamp_are_rejected_without_modifying_file() -> Result<()> {
    let (temp, _engine, _server) = fixture()?;
    let mut client = Client::connect(&temp.path().join("tier.sock"))?;
    let file = client.open("file", Access::ReadWrite, Disposition::CreateNew)?;
    client.submit(
        Request {
            method: Method::Write as i32,
            handle: file,
            length: 2,
            ..Default::default()
        },
        b"one".to_vec(),
    )?;
    assert_eq!(client.receive()?.1.error, libc::EINVAL);
    client.submit(
        Request {
            method: Method::Setattr as i32,
            handle: file,
            changes: Some(Changes {
                has_size: true,
                size: 1024,
                mtime: Some(Timestamp {
                    seconds: 0,
                    nanos: 1_000_000_000,
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        vec![],
    )?;
    assert_eq!(client.receive()?.1.error, libc::EINVAL);
    let attr = client
        .call(
            Request {
                method: Method::Getattr as i32,
                handle: file,
                ..Default::default()
            },
            vec![],
        )?
        .0
        .attr
        .unwrap();
    assert_eq!(attr.size, 0);
    Ok(())
}

#[test]
fn attach_rejects_an_unsupported_subdirectory_instead_of_exporting_root() -> Result<()> {
    let (temp, engine, _server) = fixture()?;
    engine.mkdir("/subset")?;
    let stream = UnixStream::connect(temp.path().join("tier.sock"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut client = Client {
        stream,
        next: 1,
        root: 0,
    };
    client.call(
        Request {
            method: Method::Hello as i32,
            protocol_version: VERSION.into(),
            ..Default::default()
        },
        vec![],
    )?;
    client.submit(
        Request {
            method: Method::Attach as i32,
            name: "/subset".into(),
            ..Default::default()
        },
        vec![],
    )?;
    assert_eq!(client.receive()?.1.error, libc::EINVAL);
    assert_ne!(
        client
            .call(
                Request {
                    method: Method::Attach as i32,
                    name: "/".into(),
                    ..Default::default()
                },
                vec![]
            )?
            .0
            .handle,
        0
    );
    Ok(())
}

#[test]
fn stalled_read_replies_do_not_starve_other_sessions() -> Result<()> {
    let (temp, engine, mut server) = fixture_with_threads(1)?;
    let path = temp.path().join("tier.sock");
    let stalled_sessions = 8;
    let mut stalled = Vec::new();
    let mut handles = Vec::new();
    for index in 0..stalled_sessions {
        let name = format!("slow-reader-{index}");
        engine.put(&format!("/{name}"), &vec![0x5a; MAX_DATA])?;
        let mut client = Client::connect(&path)?;
        handles.push(client.open(&name, Access::ReadOnly, Disposition::Existing)?);
        stalled.push(client);
    }
    let mut active = Client::connect(&path)?;
    let handle = active.open("progress", Access::Metadata, Disposition::CreateNew)?;
    active
        .stream
        .set_read_timeout(Some(Duration::from_secs(2)))?;
    let progress = (|| -> Result<()> {
        for (client, handle) in stalled.iter_mut().zip(handles) {
            for _ in 0..MAX_INFLIGHT {
                client.submit(
                    Request {
                        method: Method::Read as i32,
                        handle,
                        length: MAX_DATA as u32,
                        ..Default::default()
                    },
                    vec![],
                )?;
            }
            // Wait for the response to start, then deliberately stop consuming
            // its large body. Each request uses a distinct file and session.
            let mut header = [0; 24];
            client.stream.read_exact(&mut header)?;
            anyhow::ensure!(
                u32::from_be_bytes(header[20..24].try_into()?) == MAX_DATA as u32,
                "expected a full read response"
            );
        }
        let (response, _) = active
            .call(
                Request {
                    method: Method::Setattr as i32,
                    handle,
                    changes: Some(Changes {
                        mtime: Some(Timestamp {
                            seconds: 42,
                            nanos: 0,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                vec![],
            )
            .context("SETATTR blocked behind other sessions' unread replies")?;
        anyhow::ensure!(
            response.attr.unwrap().mtime.unwrap().seconds == 42,
            "unrelated metadata request did not complete"
        );
        Ok(())
    })();
    // Shutdown must interrupt pending response writes even while peers remain
    // connected. In the failing implementation this also prevents a slow test
    // teardown from waiting for every socket's write timeout.
    let started = std::time::Instant::now();
    server.shutdown();
    anyhow::ensure!(
        started.elapsed() < Duration::from_secs(2),
        "blocked IPC shutdown"
    );
    progress
}

#[test]
fn configured_single_slot_limits_keep_protocol_and_storage_usable() -> Result<()> {
    let options = IpcOptions {
        max_connections: 2,
        max_inflight_per_connection: 1,
        max_running_requests: 1,
        max_handles_per_connection: 2,
        data_buffer_mib: 4,
        control_buffer_mib: 4,
        connection_data_buffer_mib: 4,
        connection_control_buffer_mib: 4,
        global_cursor_memory_mib: 0,
        connection_cursor_memory_mib: 0,
        per_cursor_memory_kib: 0,
        cursor_buffer_kib: 0,
        ..Default::default()
    };
    let (temp, engine, mut server) = fixture_with_options(1, &options)?;
    let path = temp.path().join("tier.sock");
    let mut client = Client {
        stream: UnixStream::connect(&path)?,
        next: 1,
        root: 0,
    };
    client
        .stream
        .set_read_timeout(Some(Duration::from_secs(2)))?;
    assert_eq!(
        client
            .call(
                Request {
                    method: Method::Hello as i32,
                    protocol_version: VERSION.into(),
                    ..Default::default()
                },
                vec![]
            )?
            .0
            .max_inflight,
        1
    );
    client.root = client
        .call(
            Request {
                method: Method::Attach as i32,
                ..Default::default()
            },
            vec![],
        )?
        .0
        .handle;
    let handle = client.open("limited", Access::ReadWrite, Disposition::CreateNew)?;
    client.submit(
        Request {
            method: Method::Open as i32,
            parent: client.root,
            name: "limited".into(),
            access: Access::Metadata as i32,
            disposition: Disposition::Existing as i32,
            ..Default::default()
        },
        vec![],
    )?;
    assert_eq!(client.receive()?.1.error, libc::EMFILE);
    let body = vec![0x5a; MAX_DATA];
    client.call(
        Request {
            method: Method::Write as i32,
            handle,
            length: body.len() as u32,
            ..Default::default()
        },
        body.clone(),
    )?;
    assert_eq!(
        client
            .call(
                Request {
                    method: Method::Read as i32,
                    handle,
                    length: body.len() as u32,
                    ..Default::default()
                },
                vec![]
            )?
            .1,
        body
    );
    client.call(
        Request {
            method: Method::Close as i32,
            handle,
            ..Default::default()
        },
        vec![],
    )?;
    let cursor = client
        .call(
            Request {
                method: Method::Opendir as i32,
                handle: client.root,
                ..Default::default()
            },
            vec![],
        )?
        .0
        .handle;
    let page = client
        .call(
            Request {
                method: Method::Readdir as i32,
                handle: cursor,
                max_entries: 10,
                max_bytes: 4096,
                ..Default::default()
            },
            vec![],
        )?
        .0;
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "limited");
    assert!(page.eof);
    server.shutdown();
    assert_eq!(engine.get("/limited")?, body);
    Ok(())
}
