//! C views and ownership boundary; no Samba objects cross into Rust.
use crate::MAX_DATA;
use crate::{pb, transport::TierTransport};
use std::{
    ffi::{CStr, CString, c_char},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
};
#[repr(C)]
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
// Boxed elements keep addresses stable while the owning vectors grow.
#[derive(Default)]
#[allow(clippy::vec_box)]
struct Arena {
    pub(crate) strings: Vec<CString>,
    pub(crate) timestamps: Vec<Box<TierTimestamp>>,
    pub(crate) attrs: Vec<Box<TierAttr>>,
    pub(crate) entries: Vec<Box<TierEntry>>,
}
impl Arena {
    fn string(&mut self, value: String) -> Result<*mut c_char, i32> {
        let value = CString::new(value).map_err(|_| libc::EPROTO)?;
        let pointer = value.as_ptr().cast_mut();
        self.strings.push(value);
        Ok(pointer)
    }
    fn timestamp(&mut self, value: Option<pb::Timestamp>) -> *mut TierTimestamp {
        let Some(value) = value else {
            return ptr::null_mut();
        };
        let mut value = Box::new(TierTimestamp {
            seconds: value.seconds,
            nanos: value.nanos,
        });
        let pointer = &mut *value as *mut _;
        self.timestamps.push(value);
        pointer
    }
    fn attr(&mut self, value: Option<pb::Attr>) -> Result<*mut TierAttr, i32> {
        let Some(value) = value else {
            return Ok(ptr::null_mut());
        };
        let mut value = Box::new(TierAttr {
            node_id: self.string(value.node_id)?,
            inode: value.inode,
            directory: value.directory,
            size: value.size,
            allocated: value.allocated,
            mode: value.mode,
            uid: value.uid,
            gid: value.gid,
            nlink: value.nlink,
            atime: self.timestamp(value.atime),
            mtime: self.timestamp(value.mtime),
            ctime: self.timestamp(value.ctime),
            btime: self.timestamp(value.btime),
            version: value.version,
        });
        let pointer = &mut *value as *mut _;
        self.attrs.push(value);
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
        let mut arena = Arena::default();
        let mut entries = Vec::with_capacity(response.entries.len());
        for entry in response.entries {
            let mut entry = Box::new(TierEntry {
                name: arena.string(entry.name)?,
                attr: arena.attr(entry.attr)?,
            });
            entries.push(&mut *entry as *mut _);
            arena.entries.push(entry);
        }
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
        drop(unsafe { Box::from_raw(transport) });
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
        transport.submit(unsafe { request.to_proto()? }, data, id)
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
