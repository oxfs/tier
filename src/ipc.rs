//! Local, session-scoped storage protocol used by the Samba VFS client.
mod pool;
mod server;

pub use server::IpcServer;
pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/tier.ipc.rs"));
}

use prost::Message;
use std::io::{self, IoSlice, Read, Write};

pub const VERSION: u16 = 1;
pub const MAGIC: u32 = 0x5449_4552;
pub const MAX_MESSAGE: usize = 1024 * 1024;
pub const MAX_DATA: usize = 1024 * 1024;
pub const MAX_INFLIGHT: usize = 32;

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

    pub fn write(&self, writer: &mut impl Write) -> io::Result<()> {
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
