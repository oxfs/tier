//! C views and ownership boundary; no Samba objects cross into Rust.
use crate::MAX_DATA;
use crate::{pb, transport::TierTransport};
use std::{
    ffi::{CStr, CString, c_char},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
};
#[repr(C)]
#[derive(Default)]
pub struct TierTimestamp {
    pub(crate) seconds: i64,
    pub(crate) nanos: u32,
}
#[repr(C)]
pub struct TierAttr {
    pub(crate) node_id: *mut c_char,
    pub(crate) inode: u64,
    pub(crate) directory: bool,
    pub(crate) size: u64,
    pub(crate) allocated: u64,
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) nlink: u32,
    pub(crate) atime: *mut TierTimestamp,
    pub(crate) mtime: *mut TierTimestamp,
    pub(crate) ctime: *mut TierTimestamp,
    pub(crate) btime: *mut TierTimestamp,
    pub(crate) version: u64,
}
#[repr(C)]
pub struct TierChanges {
    pub(crate) size: u64,
    pub(crate) atime: *const TierTimestamp,
    pub(crate) mtime: *const TierTimestamp,
    pub(crate) mode: u32,
    pub(crate) has_size: bool,
    pub(crate) has_mode: bool,
    pub(crate) btime: *const TierTimestamp,
}
#[repr(C)]
pub struct TierRequest {
    pub(crate) method: i32,
    pub(crate) handle: u64,
    pub(crate) parent: u64,
    pub(crate) name: *const c_char,
    pub(crate) target_parent: u64,
    pub(crate) target_name: *const c_char,
    pub(crate) access: i32,
    pub(crate) disposition: i32,
    pub(crate) truncate: bool,
    pub(crate) directory: bool,
    pub(crate) replace: bool,
    pub(crate) offset: u64,
    pub(crate) length: u32,
    pub(crate) stable: bool,
    pub(crate) changes: *const TierChanges,
    pub(crate) max_entries: u32,
    pub(crate) max_bytes: u32,
    pub(crate) protocol_version: u32,
    pub(crate) local_access: bool,
}
#[repr(C)]
pub struct TierEntry {
    pub(crate) name: *mut c_char,
    pub(crate) attr: *mut TierAttr,
}
#[repr(C)]
pub struct TierResponse {
    pub(crate) error: i32,
    pub(crate) message: *mut c_char,
    pub(crate) handle: u64,
    pub(crate) attr: *mut TierAttr,
    pub(crate) n_entries: usize,
    pub(crate) entries: *mut *mut TierEntry,
    pub(crate) count: u32,
    pub(crate) eof: bool,
    pub(crate) created: bool,
    pub(crate) total_bytes: u64,
    pub(crate) free_bytes: u64,
    pub(crate) instance: *mut c_char,
    pub(crate) protocol_version: u32,
    pub(crate) max_message: u32,
    pub(crate) max_data: u32,
    pub(crate) max_inflight: u32,
    pub(crate) data: *mut u8,
    pub(crate) data_length: usize,
}
// Each attribute owns its timestamps in one stable allocation.
struct OwnedAttr {
    view: TierAttr,
    timestamps: [TierTimestamp; 4],
}
// Boxed attributes keep addresses stable while the owning vector grows.
#[derive(Default)]
#[allow(clippy::vec_box)]
struct Arena {
    pub(crate) strings: Vec<CString>,
    attrs: Vec<Box<OwnedAttr>>,
    entries: Vec<TierEntry>,
}
impl Arena {
    fn string(&mut self, value: String) -> Result<*mut c_char, i32> {
        let value = CString::new(value).map_err(|_| libc::EPROTO)?;
        let pointer = value.as_ptr().cast_mut();
        self.strings.push(value);
        Ok(pointer)
    }
    fn attr(&mut self, value: Option<pb::Attr>) -> Result<*mut TierAttr, i32> {
        let Some(value) = value else {
            return Ok(ptr::null_mut());
        };
        let times = [value.atime, value.mtime, value.ctime, value.btime];
        let mut owned = Box::new(OwnedAttr {
            view: TierAttr {
                node_id: self.string(value.node_id)?,
                inode: value.inode,
                directory: value.directory,
                size: value.size,
                allocated: value.allocated,
                mode: value.mode,
                uid: value.uid,
                gid: value.gid,
                nlink: value.nlink,
                atime: ptr::null_mut(),
                mtime: ptr::null_mut(),
                ctime: ptr::null_mut(),
                btime: ptr::null_mut(),
                version: value.version,
            },
            timestamps: std::array::from_fn(|_| TierTimestamp::default()),
        });
        let mut addresses = [ptr::null_mut(); 4];
        for (index, time) in times.into_iter().enumerate() {
            if let Some(time) = time {
                owned.timestamps[index] = TierTimestamp {
                    seconds: time.seconds,
                    nanos: time.nanos,
                };
                addresses[index] = &mut owned.timestamps[index];
            }
        }
        [
            owned.view.atime,
            owned.view.mtime,
            owned.view.ctime,
            owned.view.btime,
        ] = addresses;
        let pointer = &mut owned.view as *mut _;
        self.attrs.push(owned);
        Ok(pointer)
    }
}
// view is the first member so a C response pointer also identifies its owner.
#[repr(C)]
pub(crate) struct ResponseOwner {
    pub(crate) view: TierResponse,
    arena: Arena,
    pub(crate) entries: Vec<*mut TierEntry>,
    pub(crate) data: Vec<u8>,
}
impl ResponseOwner {
    pub(crate) fn new(
        response: pb::Response,
        mut data: Vec<u8>,
        data_offset: usize,
    ) -> Result<Box<Self>, i32> {
        let mut arena = Arena {
            entries: Vec::with_capacity(response.entries.len()),
            attrs: Vec::with_capacity(response.entries.len() + 1),
            strings: Vec::with_capacity(response.entries.len() * 2 + 3),
        };
        let mut entries = Vec::with_capacity(response.entries.len());
        for entry in response.entries {
            let entry = TierEntry {
                name: arena.string(entry.name)?,
                attr: arena.attr(entry.attr)?,
            };
            arena.entries.push(entry);
        }
        // Publish entry pointers only after the contiguous storage is complete.
        entries.extend(arena.entries.iter_mut().map(|entry| entry as *mut _));
        let view = TierResponse {
            error: response.error,
            message: arena.string(response.message)?,
            handle: response.handle,
            attr: arena.attr(response.attr)?,
            n_entries: entries.len(),
            entries: entries.as_mut_ptr(),
            count: response.count,
            eof: response.eof,
            created: response.created,
            total_bytes: response.total_bytes,
            free_bytes: response.free_bytes,
            instance: arena.string(response.instance)?,
            protocol_version: response.protocol_version,
            max_message: response.max_message,
            max_data: response.max_data,
            max_inflight: response.max_inflight,
            data: data.as_mut_ptr().wrapping_add(data_offset),
            data_length: data.len() - data_offset,
        };
        Ok(Box::new(Self {
            view,
            arena,
            entries,
            data,
        }))
    }
}
unsafe fn string(value: *const c_char) -> Result<String, i32> {
    if value.is_null() {
        return Ok(String::new());
    }
    // SAFETY: request strings are NUL-terminated and borrowed for submit.
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| libc::EILSEQ)
}
unsafe fn timestamp(value: *const TierTimestamp) -> Option<pb::Timestamp> {
    // SAFETY: optional request members live for the duration of submit.
    unsafe { value.as_ref() }.map(|v| pb::Timestamp {
        seconds: v.seconds,
        nanos: v.nanos,
    })
}
impl TierRequest {
    unsafe fn to_proto(&self) -> Result<pb::Request, i32> {
        // SAFETY: the adapter supplies live request members for this call.
        let changes = unsafe { self.changes.as_ref() }.map(|v| unsafe {
            pb::Changes {
                size: v.size,
                atime: timestamp(v.atime),
                mtime: timestamp(v.mtime),
                mode: v.mode,
                has_size: v.has_size,
                has_mode: v.has_mode,
                btime: timestamp(v.btime),
            }
        });
        Ok(pb::Request {
            method: self.method,
            handle: self.handle,
            parent: self.parent,
            name: unsafe { string(self.name)? },
            target_parent: self.target_parent,
            target_name: unsafe { string(self.target_name)? },
            access: self.access,
            disposition: self.disposition,
            truncate: self.truncate,
            directory: self.directory,
            replace: self.replace,
            offset: self.offset,
            length: self.length,
            stable: self.stable,
            changes,
            max_entries: self.max_entries,
            max_bytes: self.max_bytes,
            protocol_version: self.protocol_version,
            local_access: self.local_access,
            ..Default::default()
        })
    }
}
fn guard(f: impl FnOnce() -> Result<(), i32>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e,
        Err(_) => libc::EIO,
    }
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn tier_transport_new(fd: i32) -> *mut TierTransport {
    Box::into_raw(Box::new(TierTransport::new(fd)))
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_free(transport: *mut TierTransport) {
    if !transport.is_null() {
        let mut transport = unsafe { Box::from_raw(transport) };
        transport.abort(libc::ECONNRESET);
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_submit(
    transport: *mut TierTransport,
    request: *const TierRequest,
    data: *const u8,
    length: usize,
    id: u64,
) -> i32 {
    guard(|| {
        let transport = unsafe { transport.as_mut() }.ok_or(libc::EINVAL)?;
        let request = unsafe { request.as_ref() }.ok_or(libc::EINVAL)?;
        if length > transport.max_data {
            return Err(libc::EMSGSIZE);
        }
        let data = if length == 0 {
            &[]
        } else {
            if data.is_null() {
                return Err(libc::EINVAL);
            }
            unsafe { std::slice::from_raw_parts(data, length) }
        };
        let mut request = unsafe { request.to_proto()? };
        if request.method == pb::Method::Hello as i32 {
            request.local_access = transport.local.peer.is_some();
        }
        transport.submit(request, data, id)
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_pump(
    transport: *mut TierTransport,
    writable: bool,
) -> i32 {
    let Some(transport) = (unsafe { transport.as_mut() }) else {
        return libc::EINVAL;
    };
    let error = guard(|| transport.pump(writable));
    if error != 0 {
        transport.error = error;
    }
    error
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_wants_write(
    transport: *const TierTransport,
) -> bool {
    unsafe { transport.as_ref() }.is_some_and(|t| t.error == 0 && !t.outgoing.is_empty())
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_next(
    transport: *mut TierTransport,
    id: *mut u64,
) -> *mut TierResponse {
    let Some(transport) = (unsafe { transport.as_mut() }) else {
        return ptr::null_mut();
    };
    if id.is_null() {
        return ptr::null_mut();
    }
    let Some((response_id, response)) = transport.ready.pop_front() else {
        return ptr::null_mut();
    };
    unsafe {
        *id = response_id;
    }
    Box::into_raw(response).cast()
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_set_max_data(
    transport: *mut TierTransport,
    max_data: u32,
) {
    if let Some(transport) = unsafe { transport.as_mut() } {
        transport.max_data = (max_data as usize).min(MAX_DATA);
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_response_free(response: *mut TierResponse) {
    if !response.is_null() {
        drop(unsafe { Box::from_raw(response.cast::<ResponseOwner>()) });
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_control_connect(
    t: *mut TierTransport,
    path: *const c_char,
) -> i32 {
    guard(|| {
        unsafe { t.as_mut() }
            .ok_or(libc::EINVAL)?
            .connect_control(&unsafe { string(path)? })
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_control_fd(t: *const TierTransport) -> i32 {
    unsafe { t.as_ref() }.map_or(-1, TierTransport::control_fd)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_control_wants_write(t: *const TierTransport) -> bool {
    unsafe { t.as_ref() }.is_some_and(TierTransport::control_writable)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_local_read(
    t: *mut TierTransport,
    handle: u64,
    offset: u64,
    length: usize,
    job: *mut *mut crate::local::ReadJob,
) -> i32 {
    guard(|| {
        let t = unsafe { t.as_mut() }.ok_or(libc::EINVAL)?;
        let job = unsafe { job.as_mut() }.ok_or(libc::EINVAL)?;
        *job = ptr::null_mut();
        if let Some(read) = t.local.read(handle, offset, length)? {
            *job = Box::into_raw(Box::new(read));
        }
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_local_read_run(job: *mut crate::local::ReadJob) {
    if let Some(job) = unsafe { job.as_mut() }
        && guard(|| {
            job.run();
            Ok(())
        }) != 0
    {
        job.error = libc::EIO;
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_local_read_finish(
    job: *mut crate::local::ReadJob,
) -> *mut TierResponse {
    if job.is_null() {
        return ptr::null_mut();
    }
    let mut job = unsafe { Box::from_raw(job) };
    let data = std::mem::take(&mut job.data);
    let response = pb::Response {
        error: job.error,
        count: if job.error == 0 { data.len() as u32 } else { 0 },
        ..Default::default()
    };
    ResponseOwner::new(response, data, 0).map_or(ptr::null_mut(), |r| Box::into_raw(r).cast())
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_is_local_handle(t: *const TierTransport, handle: u64) -> bool {
    unsafe { t.as_ref() }.is_some_and(|t| t.local.contains(handle))
}

/// Counters are updated only on the owning tevent thread, before read dispatch.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_local_hits(t: *const TierTransport, hits: *mut [u64; 3]) {
    if let Some(hits) = unsafe { hits.as_mut() } {
        *hits = unsafe { t.as_ref() }.map_or([0; 3], |t| t.local.hits);
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_transport_abort(t: *mut TierTransport, error: i32) {
    if let Some(t) = unsafe { t.as_mut() } {
        t.abort(error);
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn tier_control_pump(t: *mut TierTransport) -> i32 {
    guard(|| unsafe { t.as_mut() }.ok_or(libc::EINVAL)?.pump_control())
}
