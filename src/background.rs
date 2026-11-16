use crate::Store;
use anyhow::Result;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

pub(crate) struct Scan {
    _lock: File,
    cancelled: PathBuf,
}

impl Scan {
    pub(crate) fn interrupted(&self) -> Result<bool> {
        Ok(self.cancelled.try_exists()?)
    }
}

impl Drop for Scan {
    fn drop(&mut self) {
        // Disposable scheduling state; a crash may cancel one additional pass.
        let _ = fs::remove_file(&self.cancelled);
    }
}

impl Store {
    fn scan_lock(&self) -> Result<File> {
        self.verify_root(self.root())?;
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root().join(".tier/scan.lock"))?)
    }

    pub(crate) fn begin_scan(&self) -> Result<Option<Scan>> {
        let lock = self.scan_lock()?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(Some(Scan {
                _lock: lock,
                cancelled: self.root().join(".tier/scan.cancel"),
            })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn interrupt_scan(&self) -> Result<()> {
        let lock = self.scan_lock()?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                fs::write(self.root().join(".tier/scan.cancel"), [])?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn foreground_lock(&self, file: &File, exclusive: bool) -> Result<()> {
        let result = if exclusive {
            FileExt::try_lock_exclusive(file)
        } else {
            FileExt::try_lock_shared(file)
        };
        if let Err(error) = &result
            && error.kind() == io::ErrorKind::WouldBlock
        {
            self.interrupt_scan()?;
        }
        Ok(result?)
    }
}

// All workers in a scan share one budget; foreground reads never use it.
pub(crate) struct RateLimit {
    bytes_per_sec: u64,
    next: Mutex<Instant>,
}

impl RateLimit {
    pub(crate) fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            next: Mutex::new(Instant::now()),
        }
    }

    pub(crate) fn consume(&self, bytes: usize) {
        let deadline = {
            let mut next = self.next.lock().unwrap();
            *next = (*next).max(Instant::now())
                + Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec as f64);
            *next
        };
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}
