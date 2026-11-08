use anyhow::{Context, Result, ensure};
use fuser::SessionUnmounter;
use std::{
    fs::{self, File},
    io::Write,
};

pub(crate) struct Shutdown {
    mount: SessionUnmounter,
    source: String,
    abort: Option<File>,
}

// The per-session FUSE source identifies this mount even if its path is replaced.
fn connection(source: &str) -> Result<Option<u32>> {
    let table = fs::read("/proc/self/mountinfo")?;
    let table = String::from_utf8_lossy(&table);
    for line in table.lines() {
        let Some((mount, kind)) = line.split_once(" - ") else {
            continue;
        };
        let mut fields = kind.split_whitespace();
        if fields.next() != Some("fuse") || fields.next() != Some(source) {
            continue;
        }
        let device = mount
            .split_whitespace()
            .nth(2)
            .context("invalid mountinfo")?;
        return Ok(Some(
            device
                .strip_prefix("0:")
                .context("invalid FUSE device")?
                .parse()?,
        ));
    }
    Ok(None)
}

impl Shutdown {
    pub(super) fn new(mount: SessionUnmounter, source: String) -> Result<Self> {
        let id = connection(&source)?.context("Tier mount is missing")?;
        // Pin the kernel object now. A future mount may reuse this device number.
        let abort = match File::options()
            .write(true)
            .open(format!("/sys/fs/fuse/connections/{id}/abort"))
        {
            Ok(file) => Some(file),
            Err(error) => {
                tracing::warn!(%error,"FUSE connection control unavailable; namespace clones may delay shutdown");
                None
            }
        };
        Ok(Self {
            mount,
            source,
            abort,
        })
    }
    pub(crate) fn unmount(mut self) -> Result<()> {
        self.mount.unmount()?;
        // Usually detaching the mount wakes the device immediately. A clone in
        // another mount namespace can keep it live after the host path is gone.
        ensure!(
            connection(&self.source)?.is_none(),
            "Tier mount still attached after unmount"
        );
        if let Some(mut file) = self.abort {
            file.write_all(b"1")?;
        }
        Ok(())
    }
}
