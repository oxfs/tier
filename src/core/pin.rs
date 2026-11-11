//! Kernel-owned logical pins survive daemon restarts while a client lives.
//! Each pin opens a separate description: dup() would share OFD lock ownership.
use super::*;
use std::os::fd::AsRawFd;

pub(crate) struct FilePin(fs::File);

impl FilePin {
    pub(crate) fn shared(&self, offset: u64) -> Result<()> {
        if !self.lock(offset, libc::F_RDLCK)? {
            return Err(failure(libc::EBUSY));
        }
        Ok(())
    }

    fn lock(&self, offset: u64, kind: libc::c_int) -> Result<bool> {
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = kind as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        lock.l_start = offset.try_into()?;
        lock.l_len = 1;
        if unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
            return Ok(false);
        }
        Err(error.into())
    }
}

impl Engine {
    pub(crate) fn pin_location(&self, id: &str) -> (std::path::PathBuf, u64) {
        let digest = Sha256::digest(id.as_bytes());
        let offset = u64::from_be_bytes(digest[..8].try_into().unwrap()) & (i64::MAX as u64 - 1);
        (self.inner.db.path().join("access.lock"), offset)
    }

    fn open_pin(&self, id: &str) -> Result<(FilePin, u64)> {
        let (path, offset) = self.pin_location(id);
        let file = fs::OpenOptions::new().read(true).write(true).open(path)?;
        Ok((FilePin(file), offset))
    }

    pub(crate) fn try_idle_pin(&self, id: &str) -> Result<Option<FilePin>> {
        let (pin, offset) = self.open_pin(id)?;
        Ok(pin.lock(offset, libc::F_WRLCK)?.then_some(pin))
    }

    pub(crate) fn shared_pin(&self, id: &str) -> Result<FilePin> {
        let (pin, offset) = self.open_pin(id)?;
        if !pin.lock(offset, libc::F_RDLCK)? {
            return Err(failure(libc::EBUSY));
        }
        Ok(pin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn client_pin_survives_daemon_restart_and_delays_background_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("db");
        let disk = root.path().join("disk");
        fs::create_dir(&disk)?;
        let engine = Engine::open(&database)?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                persistent_storage: vec!["disk".into()],
                persistent_copies: 1,
                ..Default::default()
            },
        )?;
        engine.put("/old", b"data")?;
        let id = engine.stat("/old")?.id;
        let old = engine.copies(&id)?[0].object.clone();
        let pin = engine.shared_pin(&id)?;
        let pin_inode = fs::metadata(engine.pin_location(&id).0)?.ino();
        drop(engine);
        let engine = Engine::open(&database)?;
        assert_eq!(fs::metadata(engine.pin_location(&id).0)?.ino(), pin_inode);
        assert!(engine.try_idle_pin(&id)?.is_none());
        engine.rename("/old", "/new")?;
        engine.reconcile()?;
        assert_eq!(engine.copies(&id)?[0].object, old);
        assert_eq!(
            engine
                .remove("/new")
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error),
            Some(libc::EBUSY)
        );
        let writer = engine.open_file("/new", true, false, false)?;
        writer.write_at(0, b"next")?;
        writer.close()?;
        assert_eq!(fs::read(disk.join(&old))?, b"next");
        drop(pin);
        engine.reconcile()?;
        assert_ne!(engine.copies(&id)?[0].object, old);
        engine.remove("/new")?;
        Ok(())
    }

    #[test]
    fn missing_primary_cannot_switch_a_writer_while_a_client_is_pinned() -> Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Engine::open(&root.path().join("db"))?;
        for name in ["a", "b"] {
            let path = root.path().join(name);
            fs::create_dir(&path)?;
            engine.register_target(name, &path, Some(name), true)?;
        }
        engine.set_policy(
            "/",
            &Policy {
                persistent_storage: vec!["a".into(), "b".into()],
                persistent_copies: 2,
                ..Default::default()
            },
        )?;
        engine.put("/file", b"old")?;
        engine.reconcile()?;
        let id = engine.stat("/file")?.id;
        let primary = engine.record(&id)?.primary.unwrap();
        let pin = engine.shared_pin(&id)?;
        fs::rename(root.path().join(&primary), root.path().join("offline"))?;
        let reader = engine.open_file("/file", false, false, false)?;
        let mut bytes = [0; 3];
        reader.read_at(0, &mut bytes)?;
        assert_eq!(&bytes, b"old");
        assert!(engine.open_file("/file", true, false, false).is_err());
        fs::rename(root.path().join("offline"), root.path().join(&primary))?;
        let writer = engine.open_file("/file", true, false, false)?;
        writer.write_at(0, b"new")?;
        reader.read_at(0, &mut bytes)?;
        assert_eq!(&bytes, b"new");
        writer.close()?;
        reader.close()?;
        drop(pin);
        Ok(())
    }
}
