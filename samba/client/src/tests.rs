use super::*;
use crate::{ffi::*, transport::*};
use prost::Message;
use std::ffi::CStr;
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

fn connection(method: pb::Method) -> (UnixStream, UnixStream, TierTransport) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    peer.set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    client.set_nonblocking(true).unwrap();
    let mut transport = TierTransport::new(client.as_raw_fd());
    transport
        .submit(
            pb::Request {
                method: method as i32,
                ..Default::default()
            },
            &[],
            1,
        )
        .unwrap();
    transport.pump(true).unwrap();
    let mut header = [0; HEADER];
    peer.read_exact(&mut header).unwrap();
    let length = u32::from_be_bytes(header[16..20].try_into().unwrap()) as usize;
    peer.read_exact(&mut vec![0; length]).unwrap();
    (client, peer, transport)
}
fn frame(response: pb::Response, data: &[u8]) -> Vec<u8> {
    let message = response.encode_to_vec();
    let mut bytes = b"TIER\0\x01\0\0".to_vec();
    bytes.extend_from_slice(&1u64.to_be_bytes());
    bytes.extend_from_slice(&(message.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&message);
    bytes.extend_from_slice(data);
    bytes
}
#[test]
fn fragmented_reply_at_every_boundary_retains_data_after_transport_drop() {
    let response = pb::Response {
        attr: Some(pb::Attr {
            node_id: "node".into(),
            mtime: Some(pb::Timestamp {
                seconds: 42,
                nanos: 7,
            }),
            ..Default::default()
        }),
        entries: vec![pb::Entry {
            name: "entry".into(),
            attr: None,
        }],
        ..Default::default()
    };
    let bytes = frame(response, b"file contents");
    for split in 0..bytes.len() {
        let (_client, mut peer, mut transport) = connection(pb::Method::Read);
        peer.write_all(&bytes[..split]).unwrap();
        transport.pump(false).unwrap();
        assert!(transport.ready.is_empty());
        peer.write_all(&bytes[split..]).unwrap();
        transport.pump(false).unwrap();
        let (id, response) = transport.ready.pop_front().unwrap();
        assert_eq!(id, 1);
        drop(transport);
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(response.view.data, response.view.data_length),
                b"file contents"
            );
            assert_eq!(
                CStr::from_ptr((*response.view.attr).node_id).to_bytes(),
                b"node"
            );
            assert_eq!((*(*response.view.attr).mtime).seconds, 42);
            assert_eq!(
                CStr::from_ptr((**response.view.entries).name).to_bytes(),
                b"entry"
            );
        }
    }
}
#[test]
fn malformed_replies_fail_closed() {
    let base = frame(pb::Response::default(), &[]);
    let mut cases = Vec::new();
    for offset in [0, 5, 7, 15] {
        let mut bytes = base.clone();
        bytes[offset] ^= 0xff;
        cases.push(bytes);
    }
    for offset in [16, 20] {
        let mut bytes = base.clone();
        bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        cases.push(bytes);
    }
    for error in [-1, 4096] {
        cases.push(frame(
            pb::Response {
                error,
                ..Default::default()
            },
            &[],
        ));
    }
    cases.push(frame(
        pb::Response {
            error: libc::EIO,
            ..Default::default()
        },
        b"data",
    ));
    cases.push(frame(
        pb::Response {
            message: "embedded\0nul".into(),
            ..Default::default()
        },
        &[],
    ));
    let mut invalid_proto = base;
    invalid_proto[16..20].copy_from_slice(&1u32.to_be_bytes());
    invalid_proto.push(0xff);
    cases.push(invalid_proto);
    for bytes in cases {
        let (_client, mut peer, mut transport) = connection(pb::Method::Read);
        peer.write_all(&bytes).unwrap();
        assert_eq!(
            unsafe { tier_transport_pump(&mut transport, false) },
            libc::EPROTO
        );
        assert_eq!(transport.pump(false), Err(libc::EPROTO));
    }
    let (_client, mut peer, mut transport) = connection(pb::Method::Getattr);
    peer.write_all(&frame(pb::Response::default(), b"illegal data"))
        .unwrap();
    assert_eq!(transport.pump(false), Err(libc::EPROTO));
}
#[test]
fn reply_before_disconnect_remains_available() {
    let (_client, mut peer, mut transport) = connection(pb::Method::Getattr);
    peer.write_all(&frame(pb::Response::default(), &[]))
        .unwrap();
    drop(peer);
    assert_eq!(transport.pump(false), Err(libc::ECONNRESET));
    assert_eq!(transport.ready.pop_front().unwrap().0, 1);
}
#[test]
fn partial_writes_preserve_frame_order_and_content() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    peer.set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    client.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    let capacity: libc::c_int = 4096;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&capacity as *const libc::c_int).cast(),
                size_of_val(&capacity) as _,
            )
        },
        0
    );
    let mut transport = TierTransport::new(client.as_raw_fd());
    let data = vec![0x5a; 1024 * 1024];
    let request = pb::Request {
        method: pb::Method::Write as i32,
        ..Default::default()
    };
    transport.submit(request.clone(), &data, 1).unwrap();
    transport.submit(request.clone(), b"second", 2).unwrap();
    transport.pump(true).unwrap();
    assert!(!transport.outgoing.is_empty());
    let mut received = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "partial write stalled"
        );
        let mut bytes = [0; 8192];
        loop {
            match peer.read(&mut bytes) {
                Ok(n) => received.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected read: {other:?}"),
            }
        }
        if transport.outgoing.is_empty() {
            break;
        }
        transport.pump(true).unwrap();
    }
    let first_length = HEADER + request.encoded_len() + data.len();
    assert_eq!(
        &received[HEADER + request.encoded_len()..first_length],
        data
    );
    assert_eq!(
        &received[first_length + HEADER + request.encoded_len()..],
        b"second"
    );
    assert!(transport.pending.values().all(|p| p.sent));
}
#[test]
fn duplicate_unsent_and_oversized_requests_are_rejected() {
    let (_client, mut peer, mut transport) = connection(pb::Method::Read);
    assert_eq!(
        transport.submit(pb::Request::default(), &[], 1),
        Err(libc::EINVAL)
    );
    assert_eq!(
        transport.submit(pb::Request::default(), &vec![0; MAX_DATA + 1], 2),
        Err(libc::EMSGSIZE)
    );
    transport.submit(pb::Request::default(), &[], 2).unwrap();
    let mut bytes = frame(pb::Response::default(), &[]);
    bytes[8..16].copy_from_slice(&2u64.to_be_bytes());
    peer.write_all(&bytes).unwrap();
    assert_eq!(transport.pump(false), Err(libc::EPROTO));
}

#[test]
fn c_header_matches_rust_layout_and_protocol_enums() {
    use std::{
        mem::{align_of, offset_of, size_of},
        process::{Command, Stdio},
    };
    let mut source = String::from("#include \"tier_rust.h\"\n");
    macro_rules! layout {
        ($ty:ty, $($field:ident),+ $(,)?) => {{
            let name = stringify!($ty);
            source.push_str(&format!("_Static_assert(sizeof({name}) == {}, \"{name} size\");\n", size_of::<$ty>()));
            source.push_str(&format!("_Static_assert(_Alignof({name}) == {}, \"{name} align\");\n", align_of::<$ty>()));
            $(source.push_str(&format!("_Static_assert(offsetof({name}, {}) == {}, \"{name}.{}\");\n", stringify!($field), offset_of!($ty, $field), stringify!($field)));)+
        }};
    }
    layout!(TierTimestamp, seconds, nanos);
    layout!(
        TierAttr, node_id, inode, directory, size, allocated, mode, uid, gid, nlink, atime, mtime,
        ctime, btime, version
    );
    layout!(
        TierChanges,
        size,
        atime,
        mtime,
        mode,
        has_size,
        has_mode,
        btime
    );
    layout!(
        TierRequest,
        method,
        handle,
        parent,
        name,
        target_parent,
        target_name,
        access,
        disposition,
        truncate,
        directory,
        replace,
        offset,
        length,
        stable,
        changes,
        max_entries,
        max_bytes,
        protocol_version
    );
    layout!(TierEntry, name, attr);
    layout!(
        TierResponse,
        error,
        message,
        handle,
        attr,
        n_entries,
        entries,
        count,
        eof,
        created,
        total_bytes,
        free_bytes,
        instance,
        protocol_version,
        max_message,
        max_data,
        max_inflight,
        data,
        data_length
    );
    for method in 0..=18 {
        let method = pb::Method::try_from(method).unwrap();
        source.push_str(&format!(
            "_Static_assert(TIER_METHOD_{} == {}, \"method\");\n",
            method.as_str_name(),
            method as i32
        ));
    }
    for value in 0..=3 {
        let access = pb::Access::try_from(value).unwrap();
        source.push_str(&format!(
            "_Static_assert(TIER_ACCESS_{} == {}, \"access\");\n",
            access.as_str_name(),
            access as i32
        ));
    }
    for value in 0..=2 {
        let disposition = pb::Disposition::try_from(value).unwrap();
        source.push_str(&format!(
            "_Static_assert(TIER_DISPOSITION_{} == {}, \"disposition\");\n",
            disposition.as_str_name(),
            disposition as i32
        ));
    }
    let mut compiler = Command::new("cc")
        .args([
            "-std=c11",
            "-Werror",
            "-fsyntax-only",
            "-x",
            "c",
            "-I",
            env!("CARGO_MANIFEST_DIR"),
            "-",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("C compiler for plugin ABI check");
    compiler
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    assert!(compiler.wait().unwrap().success());
}

#[test]
fn out_of_order_replies_keep_ids_and_ffi_ownership() {
    let (_client, mut peer, mut transport) = connection(pb::Method::Read);
    transport
        .submit(
            pb::Request {
                method: pb::Method::Read as i32,
                ..Default::default()
            },
            &[],
            2,
        )
        .unwrap();
    transport.pump(true).unwrap();
    let mut second = frame(pb::Response::default(), b"second");
    second[8..16].copy_from_slice(&2u64.to_be_bytes());
    peer.write_all(&second).unwrap();
    peer.write_all(&frame(pb::Response::default(), b"first"))
        .unwrap();
    transport.pump(false).unwrap();
    let mut id = 0;
    let second = unsafe { tier_transport_next(&mut transport, &mut id) };
    assert_eq!(id, 2);
    let first = unsafe { tier_transport_next(&mut transport, &mut id) };
    assert_eq!(id, 1);
    assert!(unsafe { tier_transport_next(&mut transport, &mut id) }.is_null());
    drop(transport);
    unsafe {
        assert_eq!(
            std::slice::from_raw_parts((*second).data, (*second).data_length),
            b"second"
        );
        assert_eq!(
            std::slice::from_raw_parts((*first).data, (*first).data_length),
            b"first"
        );
        tier_response_free(second);
        tier_response_free(first);
        tier_response_free(std::ptr::null_mut());
        tier_transport_free(std::ptr::null_mut());
    }
}
