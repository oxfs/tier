//! Revocable local-file access. The coordinator blocks grant publication during
//! revocation, not storage work: concurrent mutations join the same cohort.
use super::protocol::*;
use crate::core::Engine;
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    os::unix::ffi::OsStrExt,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{RwLock, mpsc, watch};

pub(crate) fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC is shared by local processes, unlike process-relative clocks.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    assert_eq!(result, 0, "CLOCK_MONOTONIC unavailable");
    (time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64)
}

pub(crate) struct DirectAccess {
    transition: tokio::sync::Mutex<()>,
    publication: Arc<RwLock<()>>,
    mutations: AtomicUsize,
    generation: AtomicU64,
    sessions: Mutex<HashMap<String, Arc<DirectSession>>>,
}
impl DirectAccess {
    pub(crate) fn new() -> Self {
        Self {
            transition: tokio::sync::Mutex::new(()),
            publication: Arc::new(RwLock::new(())),
            mutations: AtomicUsize::new(0),
            generation: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }
    pub(super) fn session(
        self: &Arc<Self>,
        max_sessions: usize,
        lease: Duration,
    ) -> Option<Arc<DirectSession>> {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() >= max_sessions {
            return None;
        }
        let session = Arc::new(DirectSession {
            token: uuid::Uuid::new_v4().to_string(),
            coordinator: Arc::downgrade(self),
            state: Mutex::new(SessionState {
                epoch: 1,
                deadline: 0,
                control: None,
                revocation: None,
            }),
            fenced: AtomicBool::new(false),
            disconnected: AtomicBool::new(false),
            lease,
        });
        sessions.insert(session.token.clone(), session.clone());
        Some(session)
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    pub(super) fn grant_generation(&self) -> u64 {
        let Ok(_publication) = self.publication.try_read() else {
            return 0;
        };
        if self.mutations.load(Ordering::Acquire) == 0 {
            self.generation()
        } else {
            0
        }
    }
    pub(super) fn find(&self, token: &str) -> Option<Arc<DirectSession>> {
        self.sessions.lock().unwrap().get(token).cloned()
    }
    pub(crate) async fn mutation(self: &Arc<Self>) -> Result<MutationPermit> {
        let _transition = self.transition.lock().await;
        let _publication = self.publication.write().await;
        if self.mutations.load(Ordering::Acquire) == 0 {
            self.generation.fetch_add(1, Ordering::AcqRel);
            let sessions: Vec<_> = { self.sessions.lock().unwrap().values().cloned().collect() };
            // Notify every session before awaiting any one of them.
            let pending = sessions
                .iter()
                .filter_map(|session| session.revoke())
                .collect::<Vec<_>>();
            for (session, deadline, mut response) in pending {
                let remaining = Duration::from_nanos(deadline.saturating_sub(monotonic_ns()));
                if !matches!(
                    tokio::time::timeout(remaining, async {
                        response.wait_for(|done| *done).await.map(|_| ())
                    })
                    .await,
                    Ok(Ok(()))
                ) {
                    {
                        let _state = session.state.lock().unwrap();
                        session.fenced.store(true, Ordering::Release);
                    }
                    // On disconnect, wait out the original grant deadline. A client
                    // must reject a result completed after this deadline too.
                    let remaining = Duration::from_nanos(deadline.saturating_sub(monotonic_ns()));
                    tokio::time::sleep(remaining).await;
                }
            }
            self.mutations.fetch_add(1, Ordering::AcqRel);
        } else {
            self.mutations.fetch_add(1, Ordering::AcqRel);
        }
        Ok(MutationPermit(self.clone()))
    }
}
pub(crate) struct MutationPermit(Arc<DirectAccess>);
impl Drop for MutationPermit {
    fn drop(&mut self) {
        self.0.mutations.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct Revocation {
    pub epoch: u64,
    pub done: watch::Sender<bool>,
}
struct SessionState {
    epoch: u64,
    deadline: u64,
    control: Option<mpsc::UnboundedSender<Revocation>>,
    revocation: Option<watch::Receiver<bool>>,
}
pub(super) struct DirectSession {
    pub token: String,
    coordinator: Weak<DirectAccess>,
    state: Mutex<SessionState>,
    fenced: AtomicBool,
    disconnected: AtomicBool,
    lease: Duration,
}
impl DirectSession {
    pub fn healthy(&self) -> bool {
        !self.fenced.load(Ordering::Acquire)
    }
    pub fn attach(&self) -> Result<mpsc::UnboundedReceiver<Revocation>> {
        let mut state = self.state.lock().unwrap();
        ensure!(
            self.healthy() && state.control.is_none(),
            "control session unavailable"
        );
        let (send, receive) = mpsc::unbounded_channel();
        state.control = Some(send);
        Ok(receive)
    }
    pub fn disconnect(self: &Arc<Self>) {
        self.fenced.store(true, Ordering::Release);
        if self.disconnected.swap(true, Ordering::AcqRel) {
            return;
        }
        let session = self.clone();
        tokio::spawn(async move {
            let deadline = session.state.lock().unwrap().deadline;
            tokio::time::sleep(Duration::from_nanos(
                deadline.saturating_sub(monotonic_ns()),
            ))
            .await;
            if let Some(coordinator) = session.coordinator.upgrade() {
                coordinator.sessions.lock().unwrap().remove(&session.token);
            }
        });
    }
    pub fn epoch(&self) -> u64 {
        self.state.lock().unwrap().epoch
    }
    pub fn grant(
        &self,
        engine: &Engine,
        id: &str,
        generation: u64,
        version: u64,
    ) -> Result<Option<LocalMapping>> {
        let Some(coordinator) = self.coordinator.upgrade() else {
            return Ok(None);
        };
        if !self.healthy() || coordinator.mutations.load(Ordering::Acquire) != 0 {
            return Ok(None);
        }
        let Ok(_publication) = coordinator.publication.clone().try_read_owned() else {
            return Ok(None);
        };
        if coordinator.mutations.load(Ordering::Acquire) != 0
            || coordinator.generation() != generation
        {
            return Ok(None);
        }
        let Some(local) = engine.local_file_info(id)? else {
            return Ok(None);
        };
        if local.node.version != version {
            return Ok(None);
        }
        let mut state = self.state.lock().unwrap();
        if !self.healthy()
            || state
                .control
                .as_ref()
                .is_none_or(|control| control.is_closed())
        {
            return Ok(None);
        }
        // The client retires an expired mapping epoch before obtaining another.
        // Open descriptors are independently pinned and do not need renewal.
        if state.deadline != 0 && monotonic_ns() >= state.deadline {
            return Ok(None);
        }
        let deadline =
            monotonic_ns().saturating_add(self.lease.as_nanos().min(u64::MAX as u128) as u64);
        state.deadline = state.deadline.max(deadline);
        Ok(Some(LocalMapping {
            path: local.path.as_os_str().as_bytes().to_vec(),
            epoch: state.epoch,
            deadline_ns: deadline,
            device: local.device,
            inode: local.inode,
            pin_path: local.pin_path.as_os_str().as_bytes().to_vec(),
            pin_offset: local.pin_offset,
        }))
    }
    fn revoke(self: &Arc<Self>) -> Option<(Arc<Self>, u64, watch::Receiver<bool>)> {
        let mut state = self.state.lock().unwrap();
        if state.deadline == 0 {
            return None;
        }
        if let Some(receive) = &state.revocation {
            return Some((self.clone(), state.deadline, receive.clone()));
        }
        let (done, receive) = watch::channel(false);
        state.revocation = Some(receive.clone());
        if let Some(control) = &state.control {
            let _ = control.send(Revocation {
                epoch: state.epoch,
                done,
            });
        }
        Some((self.clone(), state.deadline, receive))
    }
    pub fn acknowledge(&self, epoch: u64) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        ensure!(
            self.healthy() && epoch == state.epoch,
            "invalid direct-access epoch"
        );
        state.deadline = 0;
        state.revocation = None;
        state.epoch = state
            .epoch
            .checked_add(1)
            .ok_or_else(|| super::server::errno(libc::EOVERFLOW))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn granted(
        coordinator: &Arc<DirectAccess>,
        lease: Duration,
    ) -> (Arc<DirectSession>, mpsc::UnboundedReceiver<Revocation>) {
        let session = coordinator.session(8, lease).unwrap();
        let receiver = session.attach().unwrap();
        let mut state = session.state.lock().unwrap();
        state.deadline = monotonic_ns() + lease.as_nanos() as u64;
        drop(state);
        (session, receiver)
    }

    #[tokio::test]
    async fn mutations_wait_for_revocation_then_run_concurrently() -> Result<()> {
        let coordinator = Arc::new(DirectAccess::new());
        let (_session, mut notifications) = granted(&coordinator, Duration::from_secs(5));
        let shared = coordinator.clone();
        let first = tokio::spawn(async move { shared.mutation().await });
        let notice = notifications.recv().await.unwrap();
        assert!(!first.is_finished());
        notice.done.send(true)?;
        let first = first.await??;
        let second = tokio::time::timeout(Duration::from_secs(1), coordinator.mutation()).await??;
        assert_eq!(coordinator.mutations.load(Ordering::Acquire), 2);
        drop(first);
        assert_eq!(coordinator.mutations.load(Ordering::Acquire), 1);
        drop(second);
        assert_eq!(coordinator.mutations.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_mutation_reuses_outstanding_revocation() -> Result<()> {
        let coordinator = Arc::new(DirectAccess::new());
        let (_session, mut notifications) = granted(&coordinator, Duration::from_secs(5));
        let shared = coordinator.clone();
        let first = tokio::spawn(async move { shared.mutation().await });
        let notice = notifications.recv().await.unwrap();
        first.abort();
        let _ = first.await;
        let shared = coordinator.clone();
        let second = tokio::spawn(async move { shared.mutation().await });
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        assert!(notifications.try_recv().is_err());
        notice.done.send(true)?;
        drop(second.await??);
        Ok(())
    }

    #[tokio::test]
    async fn disconnected_sessions_remain_fenced_until_their_deadline() -> Result<()> {
        let coordinator = Arc::new(DirectAccess::new());
        let (session, notifications) = granted(&coordinator, Duration::from_millis(80));
        let token = session.token.clone();
        let deadline = session.state.lock().unwrap().deadline;
        session.disconnect();
        drop(notifications);
        drop(session);
        assert!(coordinator.find(&token).is_some());
        let permit = coordinator.mutation().await?;
        assert!(monotonic_ns() >= deadline);
        drop(permit);
        Ok(())
    }

    #[tokio::test]
    async fn idle_expired_epoch_can_retire_before_any_mutation_fences_it() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let engine = Engine::open(directory.path())?;
        let coordinator = engine.direct_access();
        let (session, _notifications) = granted(&coordinator, Duration::from_millis(1));
        session.state.lock().unwrap().deadline = monotonic_ns().saturating_sub(1);
        session.acknowledge(1)?;
        assert_eq!(session.epoch(), 2);
        assert_eq!(session.state.lock().unwrap().deadline, 0);
        session.disconnect();
        Ok(())
    }
    #[tokio::test]
    async fn stale_acknowledgement_cannot_retire_new_mapping_epoch() -> Result<()> {
        let coordinator = Arc::new(DirectAccess::new());
        let (session, _notifications) = granted(&coordinator, Duration::from_secs(5));
        session.acknowledge(1)?;
        let deadline = monotonic_ns() + 5_000_000_000;
        session.state.lock().unwrap().deadline = deadline;
        assert!(session.acknowledge(1).is_err());
        assert_eq!(session.epoch(), 2);
        assert_eq!(session.state.lock().unwrap().deadline, deadline);
        session.acknowledge(2)?;
        session.disconnect();
        Ok(())
    }
}
