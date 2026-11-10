use anyhow::Result;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinHandle,
};

type Tail = (Arc<()>, oneshot::Receiver<()>);

/// Register file order before spawning tasks. Waiting for the previous operation
/// is asynchronous, so a hot file cannot occupy the blocking pool with waiters.
#[derive(Default)]
pub(super) struct FileOrder {
    tails: Mutex<HashMap<String, Tail>>,
}

struct Turn {
    order: Arc<FileOrder>,
    key: String,
    token: Arc<()>,
    _finished: oneshot::Sender<()>,
}
impl Drop for Turn {
    fn drop(&mut self) {
        let mut tails = self.order.tails.lock().unwrap();
        if tails
            .get(&self.key)
            .is_some_and(|(token, _)| Arc::ptr_eq(token, &self.token))
        {
            tails.remove(&self.key);
        }
        // Dropping the sender also releases the next operation after a panic.
    }
}

impl FileOrder {
    pub fn spawn<T: Send + 'static>(
        self: &Arc<Self>,
        key: String,
        workers: Arc<Semaphore>,
        task: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> JoinHandle<Result<T>> {
        let (finished, receiver) = oneshot::channel();
        let token = Arc::new(());
        let previous = self
            .tails
            .lock()
            .unwrap()
            .insert(key.clone(), (token.clone(), receiver));
        let turn = Turn {
            order: self.clone(),
            key,
            token,
            _finished: finished,
        };
        let ready = if previous.is_none() {
            workers.clone().try_acquire_owned().ok()
        } else {
            None
        };
        let run = move |permit: OwnedSemaphorePermit| {
            let _turn = turn;
            let _permit = permit;
            task()
        };
        // Ready work goes straight to the blocking pool. Only an operation that
        // must wait owns an async task; its join handle also tracks completion.
        // Dropping that handle detaches execution rather than cancelling a turn
        // and prematurely releasing its successor.
        if let Some(permit) = ready {
            tokio::task::spawn_blocking(move || run(permit))
        } else {
            tokio::spawn(async move {
                if let Some((_, previous)) = previous {
                    let _ = previous.await;
                }
                let permit = workers.acquire_owned().await.unwrap();
                tokio::task::spawn_blocking(move || run(permit)).await?
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn hot_file_waits_outside_blocking_pool_and_preserves_order() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let order = Arc::new(FileOrder::default());
            let workers = Arc::new(Semaphore::new(2));
            let (started_tx, started_rx) = oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let first = order.spawn("a".into(), workers.clone(), move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            });
            started_rx.await.unwrap();
            let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
            let second_tx = finished_tx.clone();
            let second = order.spawn("a".into(), workers.clone(), move || {
                second_tx.send("a").unwrap();
                Ok(())
            });
            let other = order.spawn("b".into(), workers, move || {
                finished_tx.send("b").unwrap();
                Ok(())
            });
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), finished_rx.recv())
                    .await
                    .unwrap(),
                Some("b")
            );
            assert!(finished_rx.try_recv().is_err());
            release_tx.send(()).unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), finished_rx.recv())
                    .await
                    .unwrap(),
                Some("a")
            );
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();
            other.await.unwrap().unwrap();
            assert!(order.tails.lock().unwrap().is_empty());
        });
    }
    #[test]
    fn cancelled_waiters_cannot_release_a_running_turn_or_skip_its_predecessor() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let order = Arc::new(FileOrder::default());
            let workers = Arc::new(Semaphore::new(2));
            let (started, running) = oneshot::channel();
            let (release, blocked) = mpsc::channel();
            let first = order.spawn("file".into(), workers.clone(), move || {
                started.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            });
            running.await.unwrap();
            drop(first);
            assert_eq!(workers.available_permits(), 1);
            let (completed, mut completion) = tokio::sync::mpsc::unbounded_channel();
            let second_completed = completed.clone();
            let second = order.spawn("file".into(), workers.clone(), move || {
                second_completed.send(2).unwrap();
                Ok(())
            });
            drop(second);
            let third = order.spawn("file".into(), workers.clone(), move || {
                completed.send(3).unwrap();
                Ok(())
            });
            // Use the remaining worker as a scheduling barrier; neither queued
            // file operation may start while the first still owns its turn.
            order
                .spawn("other".into(), workers.clone(), || Ok(()))
                .await
                .unwrap()
                .unwrap();
            assert!(completion.try_recv().is_err());
            assert_eq!(workers.available_permits(), 1);
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(2), third)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(completion.recv().await, Some(2));
            assert_eq!(completion.recv().await, Some(3));
            assert_eq!(workers.available_permits(), 2);
            assert!(order.tails.lock().unwrap().is_empty());
        });
    }
    #[test]
    fn panicking_operation_releases_its_file_turn() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let order = Arc::new(FileOrder::default());
            let workers = Arc::new(Semaphore::new(1));
            let (started, running) = oneshot::channel();
            let (release, blocked) = mpsc::channel();
            let first = order.spawn("file".into(), workers.clone(), move || -> Result<()> {
                started.send(()).unwrap();
                blocked.recv().unwrap();
                panic!("storage operation failed");
            });
            running.await.unwrap();
            let next = order.spawn("file".into(), workers.clone(), || Ok(()));
            release.send(()).unwrap();
            assert!(first.await.unwrap_err().is_panic());
            tokio::time::timeout(Duration::from_secs(2), next)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(workers.available_permits(), 1);
            assert!(order.tails.lock().unwrap().is_empty());
        });
    }
    #[test]
    fn unavailable_execution_permit_waits_without_occupying_a_blocking_thread() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let order = Arc::new(FileOrder::default());
            let workers = Arc::new(Semaphore::new(1));
            let held = workers.clone().acquire_owned().await.unwrap();
            let (completed, mut completion) = oneshot::channel();
            let operation = order.spawn("file".into(), workers.clone(), move || {
                completed.send(()).unwrap();
                Ok(())
            });
            tokio::task::yield_now().await;
            tokio::time::timeout(Duration::from_secs(2), tokio::task::spawn_blocking(|| ()))
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                completion.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            drop(held);
            tokio::time::timeout(Duration::from_secs(2), operation)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            completion.await.unwrap();
            assert_eq!(workers.available_permits(), 1);
            assert!(order.tails.lock().unwrap().is_empty());
        });
    }
}
