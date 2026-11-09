use anyhow::Result;
use prost::Message;
use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    time::Duration,
};
use tier::{
    core::{Engine, Policy},
    ipc::{Frame, IpcServer, MAGIC, MAX_DATA, MAX_MESSAGE, VERSION, protocol::*},
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
        let frame = Frame::read(&mut self.stream)?.unwrap();
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
fn fixture() -> Result<(tempfile::TempDir, Engine, IpcServer)> {
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
    let server = IpcServer::start(engine.clone(), &temp.path().join("tier.sock"))?;
    Ok((temp, engine, server))
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
fn ordered_write_sync_and_read_across_two_handles() -> Result<()> {
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
    let mut responses = HashMap::new();
    for _ in 0..3 {
        let (id, response, data) = client.receive()?;
        assert_eq!(response.error, 0, "{}", response.message);
        responses.insert(id, (response, data));
    }
    assert_eq!(responses[&write].0.count, MAX_DATA as u32);
    assert_eq!(responses[&sync].0.error, 0);
    assert_eq!(responses[&read].1, data);
    client.call(
        Request {
            method: Method::Close as i32,
            handle: writer,
            ..Default::default()
        },
        vec![],
    )?;
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
    let (temp, engine, server) = fixture()?;
    let path = temp.path().join("tier.sock");
    let mut client = Client::connect(&path)?;
    let writer = client.open("file", Access::ReadWrite, Disposition::CreateNew)?;
    client.call(
        Request {
            method: Method::Write as i32,
            handle: writer,
            length: 4,
            stable: true,
            ..Default::default()
        },
        b"safe".to_vec(),
    )?;
    drop(client);
    server.shutdown();
    let _server = IpcServer::start(engine.clone(), &path)?;
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
    assert_eq!(engine.get("/file")?, b"safe");
    Ok(())
}

#[test]
fn malformed_session_does_not_kill_listener_and_stale_socket_is_replaced() -> Result<()> {
    let (temp, engine, server) = fixture()?;
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
    assert!(IpcServer::start(engine.clone(), &path).is_err());
    server.shutdown();
    let stale = UnixListener::bind(&path)?;
    drop(stale);
    let _server = IpcServer::start(engine, &path)?;
    let _client = Client::connect(&path)?;
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
