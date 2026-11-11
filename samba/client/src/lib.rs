//! Nonblocking Tier IPC transport driven by Samba's tevent adapter.
//! No runtime or threads are created here; C retains socket ownership.
mod control;
mod ffi;
mod local;
mod transport;
mod pb {
    include!(concat!(env!("OUT_DIR"), "/tier.ipc.rs"));
}
const HEADER: usize = 24;
const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_DATA: usize = 4 * 1024 * 1024;
#[cfg(test)]
mod tests;
