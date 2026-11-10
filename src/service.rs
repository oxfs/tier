use crate::{
    admin,
    core::Engine,
    ipc::{IpcOptions, IpcServer},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::sync::{Mutex, watch};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeOptions {
    pub max_blocking_threads: usize,
    pub worker_threads: usize,
    pub admin_request_limit: u32,
    pub background_task_limit: usize,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            max_blocking_threads: 8,
            worker_threads: 1,
            admin_request_limit: 2,
            background_task_limit: 1,
        }
    }
}

impl RuntimeOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.max_blocking_threads > 0,
            "runtime.max_blocking_threads must be positive"
        );
        ensure!(
            self.worker_threads > 0,
            "runtime.worker_threads must be positive"
        );
        ensure!(
            self.max_blocking_threads
                .checked_add(self.worker_threads)
                .is_some_and(|total| total <= isize::MAX as usize),
            "runtime thread limits exceed the supported range"
        );
        for (name, limit) in [
            ("admin_request_limit", self.admin_request_limit as usize),
            ("background_task_limit", self.background_task_limit),
        ] {
            ensure!(
                limit > 0 && limit <= tokio::sync::Semaphore::MAX_PERMITS,
                "runtime.{name} must be within 1..={}",
                tokio::sync::Semaphore::MAX_PERMITS
            );
        }
        Ok(())
    }
}

pub(crate) struct Maintenance {
    engine: Engine,
    // Acquired before spawn_blocking: waiters must not occupy storage threads.
    pub(crate) gate: Arc<Mutex<()>>,
    export_gate: Arc<Mutex<()>>,
    metadata_gate: Arc<Mutex<()>>,
}
impl Maintenance {
    pub(crate) fn new(engine: Engine) -> Self {
        Self {
            engine,
            gate: Arc::new(Mutex::new(())),
            export_gate: Arc::new(Mutex::new(())),
            metadata_gate: Arc::new(Mutex::new(())),
        }
    }
    pub(crate) fn backup_gate(&self, kind: crate::backup::Kind) -> Arc<Mutex<()>> {
        match kind {
            crate::backup::Kind::Export => self.export_gate.clone(),
            crate::backup::Kind::Metadata => self.metadata_gate.clone(),
        }
    }
    fn tick(&self) {
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

pub fn serve(
    engine: Engine,
    socket: &Path,
    addr: SocketAddr,
    options: &RuntimeOptions,
    ipc_options: &IpcOptions,
) -> Result<()> {
    options.validate()?;
    ipc_options.validate()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(options.worker_threads)
        .enable_all()
        .max_blocking_threads(options.max_blocking_threads)
        .thread_name("tier-runtime")
        .build()?;
    let result = runtime.block_on(run(engine.clone(), socket, addr, options, ipc_options));
    // Retain the final Engine owner until all runtime tasks are gone. RocksDB
    // close runs on this synchronous service thread, never a Tokio worker.
    drop(runtime);
    let sync = engine.sync_metadata();
    drop(engine);
    result.and(sync)
}

async fn run(
    engine: Engine,
    socket: &Path,
    addr: SocketAddr,
    options: &RuntimeOptions,
    ipc_options: &IpcOptions,
) -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let ipc = IpcServer::start(engine.clone(), socket, ipc_options).await?;
    let maintenance = Arc::new(Maintenance::new(engine.clone()));
    let admin = match admin::start(
        addr,
        engine.clone(),
        maintenance.clone(),
        options.admin_request_limit,
    )
    .await
    {
        Ok(admin) => admin,
        Err(error) => {
            ipc.shutdown().await;
            return Err(error);
        }
    };
    tracing::info!(address = %admin.addr, socket = %socket.display(), "Tier ready");
    let (stop, stopped) = watch::channel(false);
    // Background schedules wait for their own limit before using the blocking pool.
    let limiter = Arc::new(tokio::sync::Semaphore::new(options.background_task_limit));
    let workers = [
        None,
        Some(crate::backup::Kind::Export),
        Some(crate::backup::Kind::Metadata),
    ]
    .map(|kind| {
        tokio::spawn(background(
            engine.clone(),
            maintenance.clone(),
            limiter.clone(),
            stopped.clone(),
            kind,
        ))
    });
    tokio::select! { _ = terminate.recv() => (), _ = interrupt.recv() => () }
    let _ = stop.send(true);
    let ((), admin_result) = tokio::join!(ipc.shutdown(), admin.shutdown());
    let mut result = admin_result;
    for worker in workers {
        let completed = worker
            .await
            .context("background scheduler failed")
            .and_then(|result| result);
        result = result.and(completed);
    }
    result
}

async fn background(
    engine: Engine,
    maintenance: Arc<Maintenance>,
    limiter: Arc<tokio::sync::Semaphore>,
    mut stopped: watch::Receiver<bool>,
    kind: Option<crate::backup::Kind>,
) -> Result<()> {
    let period = Duration::from_secs(if kind.is_some() { 1 } else { 2 });
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed() => return Ok(()),
            _ = interval.tick() => {}
        }
        let gate = match kind {
            Some(kind) => maintenance.backup_gate(kind),
            None => maintenance.gate.clone(),
        }
        .lock_owned()
        .await;
        let permit = limiter.clone().acquire_owned().await?;
        if *stopped.borrow() {
            return Ok(());
        }
        let engine = engine.clone();
        let maintenance = maintenance.clone();
        tokio::task::spawn_blocking(move || {
            let (_permit, _gate) = (permit, gate);
            match kind {
                None => maintenance.tick(),
                Some(kind) => {
                    if let Err(error) = engine.schedule_backup(kind) {
                        tracing::error!(kind = kind.as_str(), error = %format_args!("{error:#}"), "scheduled backup or export failed");
                    }
                }
            }
        }).await.context("background worker failed")?;
    }
}
