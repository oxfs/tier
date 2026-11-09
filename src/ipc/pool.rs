use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

type Task = Box<dyn FnOnce() + Send + 'static>;
#[derive(Default)]
struct Queue {
    files: HashMap<String, VecDeque<Task>>,
    ready: VecDeque<String>,
    stopped: bool,
}
#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
}

/// Only the head of each file queue is runnable. Workers never wait behind
/// another worker for the same file, while unrelated files remain concurrent.
pub(super) struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}
impl Pool {
    pub fn new() -> Arc<Self> {
        let shared = Arc::new(Shared::default());
        let count = thread::available_parallelism()
            .map_or(4, usize::from)
            .clamp(4, 32);
        let mut threads = Vec::with_capacity(count);
        for _ in 0..count {
            let shared = shared.clone();
            threads.push(thread::spawn(move || {
                loop {
                    let (key, task) = {
                        let mut queue = shared.queue.lock().unwrap();
                        while queue.ready.is_empty() && !queue.stopped {
                            queue = shared.changed.wait(queue).unwrap();
                        }
                        let Some(key) = queue.ready.pop_front() else {
                            return;
                        };
                        let task = queue.files.get_mut(&key).unwrap().pop_front().unwrap();
                        (key, task)
                    };
                    task();
                    let mut queue = shared.queue.lock().unwrap();
                    if queue.files[&key].is_empty() {
                        queue.files.remove(&key);
                    } else {
                        queue.ready.push_back(key);
                        shared.changed.notify_one();
                    }
                }
            }));
        }
        Arc::new(Self { shared, threads })
    }
    pub fn submit(&self, key: String, task: impl FnOnce() + Send + 'static) {
        let mut queue = self.shared.queue.lock().unwrap();
        let first = !queue.files.contains_key(&key);
        queue
            .files
            .entry(key.clone())
            .or_default()
            .push_back(Box::new(task));
        if first {
            queue.ready.push_back(key);
            self.shared.changed.notify_one();
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.queue.lock().unwrap().stopped = true;
        self.shared.changed.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn blocked_file_does_not_block_other_files_and_keeps_its_order() {
        let pool = Pool::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        pool.submit("a".into(), move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let second_tx = finished_tx.clone();
        pool.submit("a".into(), move || second_tx.send("a").unwrap());
        pool.submit("b".into(), move || finished_tx.send("b").unwrap());
        assert_eq!(
            finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "b"
        );
        assert!(finished_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        assert_eq!(
            finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "a"
        );
    }
}
