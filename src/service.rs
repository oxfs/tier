use crate::{admin, core::Engine, ipc::IpcServer};
use anyhow::Result;
use std::{
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub(crate) struct Maintenance {
    engine: Engine,
    gate: Mutex<()>,
}
impl Maintenance {
    pub(crate) fn new(engine: Engine) -> Self {
        Self {
            engine,
            gate: Mutex::new(()),
        }
    }
    pub(crate) fn repair(&self) -> Result<()> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("maintenance unavailable"))?;
        repair(&self.engine)
    }
    fn tick(&self) {
        let _guard = self.gate.lock().expect("maintenance worker panicked");
        if let Err(error) = self.engine.automatic_reconcile() {
            tracing::error!(error = %format_args!("{error:#}"), "storage reconciliation failed");
        }
        if let Err(error) = self.engine.automatic_archive() {
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

pub fn serve(engine: Engine, socket: &Path, addr: SocketAddr) -> Result<()> {
    let ipc = IpcServer::start(engine.clone(), socket)?;
    let maintenance = Arc::new(Maintenance::new(engine.clone()));
    let admin = admin::start(addr, engine.clone(), maintenance.clone())?;
    let (mut terminate, mut interrupt) = {
        let _guard = admin.runtime.enter();
        (
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
        )
    };
    tracing::info!(address = %admin.addr, socket = %socket.display(), "Tier ready");
    let running = AtomicBool::new(true);
    thread::scope(|scope| {
        let active = &running;
        let copies = scope
            .spawn(move || periodically(active, Duration::from_secs(2), || maintenance.tick()));
        let snapshots = engine.clone();
        let backups = scope.spawn(move || periodically(active, Duration::from_secs(1), || {
            if let Err(error) = snapshots.schedule_backups() {
                tracing::error!(error = %format_args!("{error:#}"), "scheduled snapshot failed");
            }
        }));
        admin.runtime.block_on(async {
            tokio::select! { _ = terminate.recv() => (), _ = interrupt.recv() => () }
        });
        running.store(false, Ordering::Release);
        copies.thread().unpark();
        backups.thread().unpark();
        ipc.shutdown();
    });
    engine.sync_metadata()?;
    drop(admin);
    Ok(())
}

fn periodically(active: &AtomicBool, interval: Duration, mut work: impl FnMut()) {
    while active.load(Ordering::Acquire) {
        work();
        thread::park_timeout(interval);
    }
}
