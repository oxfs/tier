//! The control socket stays independently serviceable while data RPCs wait.
use crate::{pb, transport::TierTransport};
use std::os::{fd::AsRawFd, unix::net::UnixStream};
pub(crate) struct Control {
    socket: UnixStream,
    transport: Box<TierTransport>,
    attached: bool,
    next: u64,
    ack: Option<u64>,
}
impl TierTransport {
    pub(crate) fn connect_control(&mut self, path: &str) -> Result<(), i32> {
        if self.control_token.is_empty() {
            return Ok(());
        }
        let socket =
            UnixStream::connect(path).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        let mut transport = Box::new(TierTransport::new(socket.as_raw_fd()));
        transport.is_control = true;
        transport.submit(
            pb::Request {
                method: pb::Method::ControlAttach as i32,
                control_token: self.control_token.clone(),
                ..Default::default()
            },
            &[],
            1,
        )?;
        self.control = Some(Control {
            socket,
            transport,
            attached: false,
            next: 1,
            ack: None,
        });
        self.pump_control()
    }
    pub(crate) fn control_fd(&self) -> i32 {
        self.control.as_ref().map_or(-1, |c| c.socket.as_raw_fd())
    }
    pub(crate) fn control_writable(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|c| !c.transport.outgoing.is_empty())
    }
    pub(crate) fn pump_control(&mut self) -> Result<(), i32> {
        let Some(control) = self.control.as_mut() else {
            return Ok(());
        };
        control.transport.pump(true)?;
        while let Some((id, response)) = control.transport.ready.pop_front() {
            if response.view.error != 0 {
                return Err(response.view.error);
            }
            if id == 1 {
                control.attached = true;
            } else if control.ack == Some(id) {
                self.local.revoked_acknowledged();
                control.ack = None;
            } else {
                return Err(libc::EPROTO);
            }
        }
        while let Some(epoch) = control.transport.notifications.pop_front() {
            self.local.revoke(epoch);
        }
        if control.attached
            && control.ack.is_none()
            && let Some(epoch) = self.local.revoked
        {
            control.next = control.next.checked_add(1).ok_or(libc::EOVERFLOW)?;
            control.transport.submit(
                pb::Request {
                    method: pb::Method::RevokeAck as i32,
                    cache_epoch: epoch,
                    ..Default::default()
                },
                &[],
                control.next,
            )?;
            control.ack = Some(control.next);
            control.transport.pump(true)?;
        }
        Ok(())
    }
}
