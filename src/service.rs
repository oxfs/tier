use crate::{admin, core::Engine, mount};
use anyhow::{Result, ensure};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

/// Serializes manual and background repairs for one running NAS.
pub(crate) struct Maintenance {
    database: PathBuf,
    gate: Mutex<()>,
}

impl Maintenance {
    pub(crate) fn new(database: &Path) -> Self {
        Self {
            database: database.into(),
            gate: Mutex::new(()),
        }
    }

    pub(crate) fn repair(&self) -> Result<()> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("maintenance unavailable"))?;
        repair(&Engine::open(&self.database)?)
    }

    fn tick(&self, engine: &Engine) {
        let _guard = self.gate.lock().expect("maintenance worker panicked");
        if let Err(error) = engine.automatic_reconcile() {
            tracing::error!(error = %format_args!("{error:#}"), "storage reconciliation failed");
        }
        if let Err(error) = engine.automatic_archive() {
            tracing::error!(error = %format_args!("{error:#}"), "archive reconciliation failed");
        }
    }
}

pub fn repair(engine: &Engine) -> Result<()> {
    let local = engine.reconcile();
    let remote = engine.archive_tick();
    match (local, remote) {
        (Err(local), Err(remote)) => anyhow::bail!("storage: {local:#}; archive: {remote:#}"),
        (local, remote) => local.and(remote),
    }
}

pub fn serve(engine: Engine, database: &Path, mountpoint: &Path, addr: SocketAddr) -> Result<()> {
    ensure!(
        mountpoint.is_dir(),
        "mountpoint must be an existing directory"
    );
    let background = Engine::open(database)?;
    let snapshots = Engine::open(database)?;
    let maintenance = Arc::new(Maintenance::new(database));
    let mut share = mount::MountedShare::attach(engine, mountpoint)?;
    let admin = admin::start(addr, Engine::open(database)?, maintenance.clone())?;
    let unmounter = share.unmounter()?;
    let mut terminate = {
        let _guard = admin.runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?
    };
    admin.runtime.spawn(async move {
        if terminate.recv().await.is_some() {
            let result = tokio::task::spawn_blocking(move || unmounter.unmount()).await;
            if let Err(error) = result.map_err(anyhow::Error::from).and_then(|r| r) {
                tracing::error!(%error, "failed to unmount on shutdown");
            }
        }
    });
    tracing::info!(address = %admin.addr, mount = %mountpoint.display(), "Tier administration ready");
    let running = AtomicBool::new(true);
    thread::scope(|scope| {
        let active = &running;
        let copies = scope.spawn(move || {
            periodically(active, Duration::from_secs(2), || {
                maintenance.tick(&background)
            })
        });
        let backups = scope.spawn(move || {
            periodically(active, Duration::from_secs(1), || {
                if let Err(error) = snapshots.schedule_backups() {
                    tracing::error!(error = %format_args!("{error:#}"), "scheduled snapshot failed");
                }
            })
        });
        let result = share.run();
        running.store(false, Ordering::Release);
        copies.thread().unpark();
        backups.thread().unpark();
        result
    })
}

fn periodically(active: &AtomicBool, interval: Duration, mut work: impl FnMut()) {
    while active.load(Ordering::Acquire) {
        work();
        thread::park_timeout(interval);
    }
}
