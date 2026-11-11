//! Incremental framing and nonblocking socket I/O, driven by the caller.
use crate::{HEADER, MAX_DATA, MAX_MESSAGE, ffi::ResponseOwner, pb};
use prost::Message;
use std::collections::{HashMap, VecDeque};
pub(crate) struct Outgoing {
    pub(crate) id: u64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) sent: usize,
}
pub(crate) struct Pending {
    pub(crate) method: i32,
    pub(crate) request: pb::Request,
    pub(crate) sent: bool,
}
pub struct TierTransport {
    pub(crate) fd: i32,
    pub(crate) local: crate::local::Local,
    pub(crate) control: Option<crate::control::Control>,
    pub(crate) control_token: String,
    pub(crate) is_control: bool,
    pub(crate) notifications: VecDeque<u64>,
    pub(crate) max_data: usize,
    pub(crate) error: i32,
    pub(crate) outgoing: VecDeque<Outgoing>,
    pub(crate) pending: HashMap<u64, Pending>,
    pub(crate) ready: VecDeque<(u64, Box<ResponseOwner>)>,
    pub(crate) header: [u8; HEADER],
    pub(crate) header_used: usize,
    pub(crate) body: Vec<u8>,
    pub(crate) body_used: usize,
}
impl TierTransport {
    pub(crate) fn abort(&mut self, error: i32) {
        self.detach_control();
        self.error = error;
        self.outgoing.clear();
        self.pending.clear();
        self.ready.clear();
        self.body = Vec::new();
        self.control_token.clear();
    }
    pub(crate) fn new(fd: i32) -> Self {
        Self {
            fd,
            local: crate::local::Local::new(1024, fd),
            control: None,
            control_token: String::new(),
            is_control: false,
            notifications: VecDeque::new(),
            max_data: MAX_DATA,
            error: 0,
            outgoing: VecDeque::new(),
            pending: HashMap::new(),
            ready: VecDeque::new(),
            header: [0; HEADER],
            header_used: 0,
            body: Vec::new(),
            body_used: 0,
        }
    }
    pub(crate) fn submit(&mut self, request: pb::Request, data: &[u8], id: u64) -> Result<(), i32> {
        if self.local.contains(request.handle) {
            let (response, data) = if request.method == pb::Method::Read as i32 {
                let mut job = self
                    .local
                    .read(request.handle, request.offset, request.length as usize)?
                    .ok_or(libc::EBADF)?;
                job.run();
                (
                    pb::Response {
                        error: job.error,
                        count: if job.error == 0 {
                            job.data.len() as u32
                        } else {
                            0
                        },
                        ..Default::default()
                    },
                    job.data,
                )
            } else {
                (
                    self.local.request(&request)?.ok_or(libc::EBADF)?,
                    Vec::new(),
                )
            };
            self.ready
                .push_back((id, ResponseOwner::new(response, data, 0)?));
            return Ok(());
        }
        if self.error != 0 {
            return Err(self.error);
        }
        if id == 0 || self.pending.contains_key(&id) {
            return Err(libc::EINVAL);
        }
        if self.control.is_some() {
            self.pump_control()?;
            if let Some(response) = self.local.request(&request)? {
                self.ready
                    .push_back((id, ResponseOwner::new(response, Vec::new(), 0)?));
                return Ok(());
            }
        }
        let mlen = request.encoded_len();
        if mlen > MAX_MESSAGE || data.len() > self.max_data {
            return Err(libc::EMSGSIZE);
        }
        let mut bytes = Vec::with_capacity(HEADER + mlen + data.len());
        bytes.extend_from_slice(b"TIER\0\x01\0\0");
        bytes.extend_from_slice(&id.to_be_bytes());
        bytes.extend_from_slice(&(mlen as u32).to_be_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        request.encode(&mut bytes).map_err(|_| libc::EIO)?;
        bytes.extend_from_slice(data);
        self.pending.insert(
            id,
            Pending {
                method: request.method,
                request,
                sent: false,
            },
        );
        self.outgoing.push_back(Outgoing { id, bytes, sent: 0 });
        Ok(())
    }
    pub(crate) fn pump(&mut self, writable: bool) -> Result<(), i32> {
        if self.error != 0 {
            return Err(self.error);
        }
        self.pump_control()?;
        if writable {
            while let Some(frame) = self.outgoing.front_mut() {
                let bytes = &frame.bytes[frame.sent..];
                // SAFETY: fd is borrowed from C, bytes is valid for this syscall.
                let result = unsafe {
                    libc::send(
                        self.fd,
                        bytes.as_ptr().cast(),
                        bytes.len(),
                        libc::MSG_NOSIGNAL,
                    )
                };
                match io_result(result, libc::EPIPE)? {
                    Some(n) => frame.sent += n,
                    None => break,
                }
                if frame.sent == frame.bytes.len() {
                    self.pending.get_mut(&frame.id).ok_or(libc::EPROTO)?.sent = true;
                    self.outgoing.pop_front();
                }
            }
        }
        loop {
            if self.header_used < HEADER {
                let Some(n) = receive(self.fd, &mut self.header[self.header_used..])? else {
                    break;
                };
                self.header_used += n;
                if self.header_used != HEADER {
                    continue;
                }
                let mlen = u32::from_be_bytes(self.header[16..20].try_into().unwrap()) as usize;
                let dlen = u32::from_be_bytes(self.header[20..24].try_into().unwrap()) as usize;
                if &self.header[..8] != b"TIER\0\x01\0\0"
                    || mlen > MAX_MESSAGE
                    || dlen > self.max_data
                {
                    return Err(libc::EPROTO);
                }
                self.body.resize(mlen + dlen, 0);
            }
            if self.body_used < self.body.len() {
                let Some(n) = receive(self.fd, &mut self.body[self.body_used..])? else {
                    break;
                };
                self.body_used += n;
                if self.body_used != self.body.len() {
                    continue;
                }
            }
            let id = u64::from_be_bytes(self.header[8..16].try_into().unwrap());
            let mlen = u32::from_be_bytes(self.header[16..20].try_into().unwrap()) as usize;
            if self.is_control && id == 0 {
                let response =
                    pb::Response::decode(&self.body[..mlen]).map_err(|_| libc::EPROTO)?;
                if response.error != 0 || self.body.len() != mlen || response.cache_epoch == 0 {
                    return Err(libc::EPROTO);
                }
                self.notifications.push_back(response.cache_epoch);
                self.header_used = 0;
                self.body_used = 0;
                continue;
            }
            let pending = self.pending.get(&id).ok_or(libc::EPROTO)?;
            if !pending.sent {
                return Err(libc::EPROTO);
            }
            let response = pb::Response::decode(&self.body[..mlen]).map_err(|_| libc::EPROTO)?;
            if !(0..=4095).contains(&response.error)
                || (self.body.len() > mlen
                    && (pending.method != pb::Method::Read as i32 || response.error != 0))
            {
                return Err(libc::EPROTO);
            }
            if pending.method == pb::Method::Hello as i32 {
                self.control_token = response.control_token.clone();
            }
            self.local.observe(&pending.request, &response);
            // Raw data stays in its receive allocation, following the protobuf bytes.
            let response = ResponseOwner::new(response, std::mem::take(&mut self.body), mlen)?;
            self.pending.remove(&id);
            self.ready.push_back((id, response));
            self.header_used = 0;
            self.body_used = 0;
        }
        self.pump_control()?;
        Ok(())
    }
}
fn io_result(result: isize, closed: i32) -> Result<Option<usize>, i32> {
    if result > 0 {
        return Ok(Some(result as usize));
    }
    if result == 0 {
        return Err(closed);
    }
    let error = std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO);
    match error {
        libc::EINTR => Ok(Some(0)),
        libc::EAGAIN => Ok(None),
        _ => Err(error),
    }
}
fn receive(fd: i32, bytes: &mut [u8]) -> Result<Option<usize>, i32> {
    // SAFETY: bytes is a valid writable slice; C keeps fd open.
    io_result(
        unsafe { libc::recv(fd, bytes.as_mut_ptr().cast(), bytes.len(), 0) },
        libc::ECONNRESET,
    )
}
