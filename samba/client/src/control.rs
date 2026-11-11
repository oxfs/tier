//! The control socket stays independently serviceable while data RPCs wait.
use crate::{pb, transport::TierTransport};
use std::os::{fd::AsRawFd, unix::net::UnixStream};
pub(crate) struct Control {
    socket: UnixStream,
    transport: Box<TierTransport>,
    attached: bool,
    next: u64,
    ack: Option<(u64, u64)>,
}

impl TierTransport {
    pub(crate) fn detach_control(&mut self) {
        // Clear mappings before telling the daemon it may stop protecting them.
        // Existing data descriptors have independent OFD pins.
        self.local.disconnected();
        if let Some(mut control) = self.control.take()
            && let Some(id) = control.next.checked_add(1)
        {
            let request = pb::Request {
                method: pb::Method::Detach as i32,
                ..Default::default()
            };
            if control.transport.submit(request, &[], id).is_ok() {
                let _ = control.transport.pump(true);
            }
        }
    }
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
        loop {
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
                } else if let Some((ack_id, epoch)) = control.ack
                    && ack_id == id
                {
                    self.local.revoked_acknowledged(epoch);
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
                control.ack = Some((control.next, epoch));
                control.transport.pump(true)?;
            }
            // Sending the ACK can also receive its response and the next
            // revocation. Drain decoded events before waiting for fd readiness.
            if control.transport.ready.is_empty() && control.transport.notifications.is_empty() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use std::io::Read;

    #[test]
    fn disconnect_sends_detach_after_already_queued_control_frames() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut control = Box::new(TierTransport::new(socket.as_raw_fd()));
        control.is_control = true;
        control
            .submit(
                pb::Request {
                    method: pb::Method::RevokeAck as i32,
                    cache_epoch: 1,
                    ..Default::default()
                },
                &[],
                2,
            )
            .unwrap();
        let mut transport = TierTransport::new(-1);
        transport.control = Some(Control {
            socket,
            transport: control,
            attached: true,
            next: 2,
            ack: Some((2, 1)),
        });
        transport.abort(libc::ECONNRESET);
        let mut wire = Vec::new();
        peer.read_to_end(&mut wire).unwrap();
        let mut methods = Vec::new();
        while !wire.is_empty() {
            let size = u32::from_be_bytes(wire[16..20].try_into().unwrap()) as usize;
            let request = pb::Request::decode(&wire[crate::HEADER..crate::HEADER + size]).unwrap();
            methods.push(request.method);
            wire.drain(..crate::HEADER + size);
        }
        assert_eq!(
            methods,
            [pb::Method::RevokeAck as i32, pb::Method::Detach as i32]
        );
        assert!(transport.control.is_none());
    }
}
