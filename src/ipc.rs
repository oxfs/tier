//! Local, session-scoped storage protocol used by the Samba VFS client.
pub(crate) mod direct;
mod scheduler;
mod server;

pub use server::IpcServer;
pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/tier.ipc.rs"));
}

use prost::Message;
use std::io::{self, IoSlice, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u16 = 1;
pub const MAGIC: u32 = 0x5449_4552;
pub const MAX_MESSAGE: usize = 1024 * 1024;
pub const MAX_DATA: usize = 1024 * 1024;
pub const MAX_INFLIGHT: usize = 32;

// One slot reserves the maximum encoded request and reply buffers. Decoded
// metadata is separate; this is not a total process-memory limit. One additional
// frame per connection is bounded by the protocol limits while awaiting a slot.
const BUFFER_MIB_PER_SLOT: usize = 4;

/// IPC resource limits. Data and metadata replies have separate budgets so a
/// slow bulk reader cannot consume the memory reserved for metadata requests.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IpcOptions {
    pub max_connections: usize,
    pub max_inflight_per_connection: usize,
    pub max_running_requests: usize,
    pub max_handles_per_connection: usize,
    /// Buffer budgets reserve worst-case frames in 4 MiB increments.
    pub data_buffer_mib: usize,
    pub control_buffer_mib: usize,
    pub connection_data_buffer_mib: usize,
    pub connection_control_buffer_mib: usize,
    pub reply_timeout_secs: u64,
    pub global_cursor_memory_mib: usize,
    pub connection_cursor_memory_mib: usize,
    pub per_cursor_memory_kib: usize,
    pub cursor_buffer_kib: usize,
    pub direct_cache_entries: usize,
    pub direct_lease_secs: u64,
}
impl Default for IpcOptions {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_inflight_per_connection: MAX_INFLIGHT,
            max_running_requests: 128,
            max_handles_per_connection: 4096,
            data_buffer_mib: 128,
            control_buffer_mib: 128,
            connection_data_buffer_mib: 16,
            connection_control_buffer_mib: 16,
            reply_timeout_secs: 30,
            global_cursor_memory_mib: 128,
            connection_cursor_memory_mib: 64,
            per_cursor_memory_kib: 256,
            cursor_buffer_kib: 64,
            direct_cache_entries: 4096,
            direct_lease_secs: 30,
        }
    }
}
impl IpcOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        ensure!(
            self.direct_cache_entries <= u32::MAX as usize,
            "ipc.direct_cache_entries exceeds protocol limit"
        );
        ensure!(
            self.direct_lease_secs > 0 && self.direct_lease_secs <= 3600,
            "ipc.direct_lease_secs must be between 1 and 3600"
        );
        for (name, value) in [
            ("max_connections", self.max_connections),
            (
                "max_inflight_per_connection",
                self.max_inflight_per_connection,
            ),
            ("max_running_requests", self.max_running_requests),
            (
                "max_handles_per_connection",
                self.max_handles_per_connection,
            ),
        ] {
            ensure!(
                value > 0
                    && value <= tokio::sync::Semaphore::MAX_PERMITS
                    && value <= u32::MAX as usize,
                "ipc.{name} must be between 1 and {}",
                tokio::sync::Semaphore::MAX_PERMITS.min(u32::MAX as usize)
            );
        }
        for (name, value) in [
            ("data_buffer_mib", self.data_buffer_mib),
            ("control_buffer_mib", self.control_buffer_mib),
            (
                "connection_data_buffer_mib",
                self.connection_data_buffer_mib,
            ),
            (
                "connection_control_buffer_mib",
                self.connection_control_buffer_mib,
            ),
        ] {
            ensure!(
                value >= BUFFER_MIB_PER_SLOT && value.is_multiple_of(BUFFER_MIB_PER_SLOT),
                "ipc.{name} must be a positive multiple of {BUFFER_MIB_PER_SLOT} MiB"
            );
            ensure!(
                value.checked_mul(1024 * 1024).is_some()
                    && value / BUFFER_MIB_PER_SLOT <= tokio::sync::Semaphore::MAX_PERMITS,
                "ipc.{name} is too large"
            );
        }
        ensure!(
            self.connection_data_buffer_mib <= self.data_buffer_mib,
            "ipc.connection_data_buffer_mib must not exceed data_buffer_mib"
        );
        ensure!(
            self.connection_control_buffer_mib <= self.control_buffer_mib,
            "ipc.connection_control_buffer_mib must not exceed control_buffer_mib"
        );
        ensure!(
            self.reply_timeout_secs > 0
                && std::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(self.reply_timeout_secs))
                    .is_some(),
            "ipc.reply_timeout_secs must be positive and representable"
        );
        for (name, value, unit) in [
            (
                "global_cursor_memory_mib",
                self.global_cursor_memory_mib,
                1024 * 1024,
            ),
            (
                "connection_cursor_memory_mib",
                self.connection_cursor_memory_mib,
                1024 * 1024,
            ),
            ("per_cursor_memory_kib", self.per_cursor_memory_kib, 1024),
            ("cursor_buffer_kib", self.cursor_buffer_kib, 1024),
        ] {
            ensure!(
                value
                    .checked_mul(unit)
                    .is_some_and(|bytes| bytes <= isize::MAX as usize),
                "ipc.{name} is too large"
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Frame {
    pub id: u64,
    pub message: Vec<u8>,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn read(reader: &mut impl Read) -> io::Result<Option<Self>> {
        let mut header = [0u8; 24];
        let received = loop {
            match reader.read(&mut header) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if received == 0 {
            return Ok(None);
        }
        reader.read_exact(&mut header[received..])?;
        let (message_len, data_len) = Self::lengths(&header)?;
        let mut message = vec![0; message_len];
        let mut data = vec![0; data_len];
        reader.read_exact(&mut message)?;
        reader.read_exact(&mut data)?;
        Ok(Some(Self {
            id: u64::from_be_bytes(header[8..16].try_into().unwrap()),
            message,
            data,
        }))
    }

    fn lengths(header: &[u8; 24]) -> io::Result<(usize, usize)> {
        let magic = u32::from_be_bytes(header[..4].try_into().unwrap());
        let version = u16::from_be_bytes(header[4..6].try_into().unwrap());
        let flags = u16::from_be_bytes(header[6..8].try_into().unwrap());
        let message_len = u32::from_be_bytes(header[16..20].try_into().unwrap()) as usize;
        let data_len = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
        if magic != MAGIC
            || version != VERSION
            || flags != 0
            || message_len > MAX_MESSAGE
            || data_len > MAX_DATA
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid IPC frame",
            ));
        }
        Ok((message_len, data_len))
    }

    pub(super) async fn read_async(
        reader: &mut (impl AsyncRead + Unpin),
    ) -> io::Result<Option<Self>> {
        let mut header = [0; 24];
        let received = reader.read(&mut header).await?;
        if received == 0 {
            return Ok(None);
        }
        reader.read_exact(&mut header[received..]).await?;
        let (message_len, data_len) = Self::lengths(&header)?;
        let mut message = vec![0; message_len];
        let mut data = vec![0; data_len];
        reader.read_exact(&mut message).await?;
        reader.read_exact(&mut data).await?;
        Ok(Some(Self {
            id: u64::from_be_bytes(header[8..16].try_into().unwrap()),
            message,
            data,
        }))
    }

    pub(super) async fn write_async(
        &self,
        writer: &mut (impl AsyncWrite + Unpin),
    ) -> io::Result<()> {
        let header = self.header()?;
        let mut slices = [
            IoSlice::new(&header),
            IoSlice::new(&self.message),
            IoSlice::new(&self.data),
        ];
        let mut remaining = &mut slices[..];
        while !remaining.is_empty() {
            match writer.write_vectored(remaining).await? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                written => IoSlice::advance_slices(&mut remaining, written),
            }
        }
        Ok(())
    }

    fn header(&self) -> io::Result<[u8; 24]> {
        if self.message.len() > MAX_MESSAGE || self.data.len() > MAX_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC frame too large",
            ));
        }
        let mut header = [0u8; 24];
        header[..4].copy_from_slice(&MAGIC.to_be_bytes());
        header[4..6].copy_from_slice(&VERSION.to_be_bytes());
        header[8..16].copy_from_slice(&self.id.to_be_bytes());
        header[16..20].copy_from_slice(&(self.message.len() as u32).to_be_bytes());
        header[20..24].copy_from_slice(&(self.data.len() as u32).to_be_bytes());
        Ok(header)
    }

    pub fn write(&self, writer: &mut impl Write) -> io::Result<()> {
        let header = self.header()?;
        let mut slices = [
            IoSlice::new(&header),
            IoSlice::new(&self.message),
            IoSlice::new(&self.data),
        ];
        let mut remaining = &mut slices[..];
        while !remaining.is_empty() {
            match writer.write_vectored(remaining) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => IoSlice::advance_slices(&mut remaining, written),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn request(id: u64, request: &protocol::Request, data: Vec<u8>) -> Self {
        Self {
            id,
            message: request.encode_to_vec(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_limits_reject_zero_overflow_and_unfulfillable_budgets() {
        let defaults = serde_json::to_value(IpcOptions::default()).unwrap();
        for (field, value) in [
            ("max_connections", 0),
            ("max_inflight_per_connection", 0),
            ("max_running_requests", 0),
            ("max_handles_per_connection", 0),
            ("data_buffer_mib", 0),
            ("control_buffer_mib", 6),
            ("connection_data_buffer_mib", 132),
            ("connection_control_buffer_mib", 0),
            ("connection_control_buffer_mib", 6),
            ("connection_control_buffer_mib", 132),
            ("reply_timeout_secs", 0),
            ("data_buffer_mib", usize::MAX as u64),
            ("global_cursor_memory_mib", usize::MAX as u64),
            ("per_cursor_memory_kib", usize::MAX as u64),
            ("reply_timeout_secs", u64::MAX),
        ] {
            let mut value_map = defaults.clone();
            value_map[field] = value.into();
            let options: IpcOptions = serde_json::from_value(value_map).unwrap();
            assert!(options.validate().is_err(), "accepted {field}={value}");
        }
        let options: IpcOptions = serde_json::from_str("{}").unwrap();
        assert!(options.validate().is_ok());
        assert!(serde_json::from_str::<IpcOptions>(r#"{"data_buffer_mb":128}"#).is_err());
    }
}
