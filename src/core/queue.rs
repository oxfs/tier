//! Durable due-time queues for local protection and archive maintenance.
use super::*;

impl Engine {
    pub(crate) fn schedule_work(&self, kind: schema::WorkKind, id: &str, due: u64) -> Result<()> {
        self.inner.db.schedule(kind, id, Some(due))
    }

    pub(crate) fn schedule_file(&self, id: &str) -> Result<()> {
        let r = self.record(id)?;
        let p = self.policy(id)?;
        self.schedule_work(
            schema::WorkKind::Local,
            id,
            r.closed_at.saturating_add(p.persist_delay_secs),
        )?;
        let delay = self.archive_policy(id)?.map(|p| p.delay_secs).unwrap_or(0);
        self.schedule_work(
            schema::WorkKind::Archive,
            id,
            r.closed_at.saturating_add(delay.max(p.persist_delay_secs)),
        )
    }
    pub(crate) fn due_work(&self, kind: schema::WorkKind, manual: bool) -> Result<Vec<String>> {
        if manual {
            return Ok(self
                .scan::<Record>(schema::NODES)?
                .into_iter()
                .filter(|(_, r)| !r.node.is_dir)
                .map(|(_, r)| r.node.id)
                .collect());
        }
        let prefix = schema::queue_prefix(kind);
        let limit = schema::queue(kind, now()?, "~");
        let mut ids = vec![];
        let snapshot = self.inner.db.snapshot();
        for item in snapshot.iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward)) {
            let (k, v) = item?;
            if !k.starts_with(prefix.as_bytes()) || k.as_ref() > limit.as_bytes() {
                break;
            }
            ids.push(Self::decode(&v)?);
            if ids.len() >= 128 {
                break;
            }
        }
        Ok(ids)
    }
    pub(crate) fn reschedule_work(
        &self,
        kind: schema::WorkKind,
        id: &str,
        failed: bool,
    ) -> Result<()> {
        let lock = self.file_lock(id);
        // Leave the due entry intact when foreground I/O owns this state.
        let Some(state) = lock.try_lock() else {
            return Ok(());
        };
        let _namespace = self.inner.namespace.read();
        if let Some(r) = self.kv_get::<Record>(&schema::node(id))?
            && self.live(id)?
        {
            let delay = if state.writer || self.try_idle_pin(id)?.is_none() {
                5
            } else if failed {
                60
            } else {
                3600
            };
            let due = if r.dirty && !state.writer {
                let p = self.policy(id)?;
                r.closed_at
                    .saturating_add(p.persist_delay_secs)
                    .max(now()?.saturating_add(5))
            } else {
                now()?.saturating_add(delay)
            };
            self.schedule_work(kind, id, due)?;
        } else {
            self.inner.db.schedule(kind, id, None)?;
        }
        Ok(())
    }
}
/// A generation prevents an old page from consuming a newly reset task.
/// The cursor is a complete entry key so Unicode case folding uses DB order.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Rescan {
    generation: String,
    cursor: String,
}
impl Engine {
    pub(crate) fn rescan_record(id: &str) -> Result<(String, Vec<u8>)> {
        Self::encoded(
            schema::rescan(id),
            &Rescan {
                generation: Uuid::new_v4().to_string(),
                cursor: String::new(),
            },
        )
    }
    pub(crate) fn rebuild_work_queue(&self) -> Result<()> {
        let _namespace = self.inner.namespace.write();
        self.atomic(vec![Self::rescan_record("root")?], vec![], true)
    }
    fn schedule_live_file(&self, id: &str) -> Result<()> {
        let lock = self.file_lock(id);
        let _state = lock.lock();
        let _namespace = self.inner.namespace.read();
        if let Some(record) = self.kv_get::<Record>(&schema::node(id))?
            && !record.node.is_dir
            && self.live(id)?
        {
            self.schedule_file(id)?;
        }
        Ok(())
    }
    /// Requires maintenance serialization. Every tick visits at most 128 entries
    /// (or empty tasks), without holding the namespace lock while walking a tree.
    pub(crate) fn resume_rescans(&self) -> Result<()> {
        let snapshot = self.inner.db.snapshot();
        let mut tasks = Vec::new();
        for item in snapshot.iterator(IteratorMode::From(
            schema::RESCANS.as_bytes(),
            Direction::Forward,
        )) {
            let (key, value) = item?;
            let Some(id) = key.strip_prefix(schema::RESCANS.as_bytes()) else {
                break;
            };
            tasks.push((
                String::from_utf8(id.to_vec())?,
                Self::decode::<Rescan>(&value)?,
            ));
            if tasks.len() == 128 {
                break;
            }
        }
        let mut budget = 128;
        for (id, task) in tasks {
            if budget == 0 {
                break;
            }
            let view = self.inner.db.snapshot();
            let record: Option<Record> = Self::view_get(&view, &schema::node(&id))?;
            let mut children = Vec::new();
            let mut cursor = None;
            let mut visited = 0;
            if record.as_ref().is_some_and(|r| r.node.is_dir) {
                let prefix = schema::entries(&id);
                let start = if task.cursor.is_empty() {
                    &prefix
                } else {
                    &task.cursor
                };
                for item in view.iterator(IteratorMode::From(start.as_bytes(), Direction::Forward))
                {
                    let (key, value) = item?;
                    if !key.starts_with(prefix.as_bytes()) {
                        break;
                    }
                    if !task.cursor.is_empty() && key.as_ref() <= task.cursor.as_bytes() {
                        continue;
                    }
                    if visited == budget {
                        break;
                    }
                    let child: String = Self::decode(&value)?;
                    let node: Record = Self::view_get(&view, &schema::node(&child))?
                        .context("directory index points to missing node")?;
                    if node.node.is_dir {
                        children.push(child);
                    } else {
                        self.schedule_live_file(&child)?;
                    }
                    cursor = Some(String::from_utf8(key.to_vec())?);
                    visited += 1;
                }
            } else if record.is_some() {
                self.schedule_live_file(&id)?;
            }
            // A full page can conservatively leave an empty final page. This
            // keeps the budget strict and avoids reading the next entry twice.
            let next = if visited == budget { cursor } else { None };
            self.finish_rescan(&id, &task, children, next)?;
            budget -= visited.max(1);
        }
        Ok(())
    }
    fn finish_rescan(
        &self,
        id: &str,
        task: &Rescan,
        children: Vec<String>,
        cursor: Option<String>,
    ) -> Result<()> {
        let _namespace = self.inner.namespace.read();
        let key = schema::rescan(id);
        if self.kv_get::<Rescan>(&key)?.as_ref() != Some(task) {
            return Ok(());
        }
        // A detach/purge can race a page. Do not fan out new tasks for dead
        // subtrees; a later restore enqueues its root in the restore batch.
        let live = self.kv_get::<Record>(&schema::node(id))?.is_some() && self.live(id)?;
        let mut puts = Vec::new();
        let mut deletes = vec![key.clone()];
        if live {
            for child in children {
                if self.kv_get::<Record>(&schema::node(&child))?.is_some() && self.live(&child)? {
                    // A parent policy may have changed after an existing child
                    // task passed its cursor. Restart it to cover those entries.
                    puts.push(Self::rescan_record(&child)?);
                }
            }
            if let Some(cursor) = cursor {
                puts.push(Self::encoded(
                    key,
                    &Rescan {
                        generation: task.generation.clone(),
                        cursor,
                    },
                )?);
                deletes.clear();
            }
        }
        // File queue updates preceded this batch in the same WAL. A durable
        // cursor therefore cannot skip a lost queue update, even without fsync
        // on each background page. WAL must never be disabled.
        self.atomic(puts, deletes, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, Engine)> {
        let directory = tempfile::tempdir()?;
        let disk = directory.path().join("disk");
        fs::create_dir(&disk)?;
        let engine = Engine::open(&directory.path().join("metadata"))?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                persistent_storage: vec!["disk".into()],
                persistent_copies: 1,
                ..Default::default()
            },
        )?;
        engine.resume_rescans()?;
        Ok((directory, engine))
    }
    fn queued(engine: &Engine, id: &str) -> Result<bool> {
        Ok(engine
            .kv_get::<u64>(&schema::queue_at(schema::WorkKind::Local, id))?
            .is_some())
    }
    fn clear_files(engine: &Engine, ids: &[String]) -> Result<()> {
        for id in ids {
            engine
                .inner
                .db
                .schedule(schema::WorkKind::Local, id, None)?;
        }
        Ok(())
    }
    fn drain(engine: &Engine) -> Result<()> {
        for _ in 0..32 {
            if engine.scan::<Rescan>(schema::RESCANS)?.is_empty() {
                return Ok(());
            }
            engine.resume_rescans()?;
        }
        bail!("rescan did not finish")
    }

    #[test]
    fn concurrent_schedule_and_cancel_leave_exactly_one_index_or_none() -> Result<()> {
        let (_directory, engine) = fixture()?;
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| -> Result<()> {
            let threads = (0..8)
                .map(|worker| {
                    let engine = &engine;
                    let barrier = &barrier;
                    scope.spawn(move || -> Result<()> {
                        barrier.wait();
                        for tick in 0..200 {
                            let due = if tick % 3 == 0 {
                                None
                            } else {
                                Some(worker * 200 + tick)
                            };
                            engine
                                .inner
                                .db
                                .schedule(schema::WorkKind::Local, "file", due)?;
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            for thread in threads {
                thread.join().unwrap()?;
            }
            Ok(())
        })?;
        let pointer: Option<u64> =
            engine.kv_get(&schema::queue_at(schema::WorkKind::Local, "file"))?;
        let entries = engine.scan::<String>(&schema::queue_prefix(schema::WorkKind::Local))?;
        match pointer {
            Some(due) => assert_eq!(
                entries,
                vec![(
                    schema::queue(schema::WorkKind::Local, due, "file"),
                    "file".into()
                )]
            ),
            None => assert!(entries.is_empty()),
        }
        engine
            .inner
            .db
            .schedule(schema::WorkKind::Local, "file", None)?;
        assert!(engine.scan::<String>(schema::QUEUE)?.is_empty());
        Ok(())
    }

    #[test]
    fn directory_rescan_is_bounded_and_resumes_after_reopening() -> Result<()> {
        let (directory, engine) = fixture()?;
        engine.mkdir("/old")?;
        let parent = engine.stat("/old")?.id;
        let ids = (0..257)
            .map(|index| {
                engine
                    .create_file_at(&parent, &format!("file-{index:03}"), true)
                    .map(|n| n.id)
            })
            .collect::<Result<Vec<_>>>()?;
        engine.rename("/old", "/new")?;
        assert_eq!(engine.scan::<Rescan>(schema::RESCANS)?.len(), 1);
        assert!(engine.scan::<String>(schema::QUEUE)?.is_empty());
        engine.resume_rescans()?;
        assert_eq!(engine.scan::<u64>(schema::QUEUE_AT)?.len(), 256); // two kinds per file
        engine.sync_metadata()?;
        drop(engine);
        let engine = Engine::open(&directory.path().join("metadata"))?;
        drain(&engine)?;
        for id in ids {
            assert!(queued(&engine, &id)?);
        }
        assert!(engine.scan::<Rescan>(schema::RESCANS)?.is_empty());
        Ok(())
    }

    #[test]
    fn stale_page_cannot_consume_policy_reset_or_entry_moved_behind_cursor() -> Result<()> {
        let (_directory, engine) = fixture()?;
        engine.mkdir("/directory")?;
        let parent = engine.stat("/directory")?.id;
        let ids = (0..130)
            .map(|index| {
                engine
                    .create_file_at(&parent, &format!("file-{index:03}"), true)
                    .map(|n| n.id)
            })
            .collect::<Result<Vec<_>>>()?;
        let policy = engine.policy(&parent)?;
        engine.set_policy("/directory", &policy)?;
        engine.resume_rescans()?;
        let old: Rescan = engine.require(&schema::rescan(&parent))?;
        assert!(!old.cursor.is_empty());
        clear_files(&engine, &ids)?;
        engine.set_policy("/directory", &policy)?;
        let reset: Rescan = engine.require(&schema::rescan(&parent))?;
        assert_ne!(old.generation, reset.generation);
        engine.finish_rescan(&parent, &old, vec![], None)?;
        assert_eq!(engine.require::<Rescan>(&schema::rescan(&parent))?, reset);
        engine.resume_rescans()?;
        // The remaining entry moves to a name before the committed cursor.
        engine.rename("/directory/file-129", "/directory/aaa")?;
        let inserted = engine.open_file("/directory/new", true, true, false)?;
        inserted.close()?;
        let created = engine.stat("/directory/new")?.id;
        drain(&engine)?;
        for id in ids {
            assert!(queued(&engine, &id)?);
        }
        assert!(queued(&engine, &created)?);
        Ok(())
    }

    #[test]
    fn parent_policy_restart_revisits_child_cursor_and_trash_does_not_fan_out() -> Result<()> {
        let (_directory, engine) = fixture()?;
        engine.mkdir("/parent")?;
        engine.mkdir("/parent/child")?;
        let parent = engine.stat("/parent")?.id;
        let child = engine.stat("/parent/child")?.id;
        let ids = (0..130)
            .map(|index| {
                engine
                    .create_file_at(&child, &format!("file-{index:03}"), true)
                    .map(|n| n.id)
            })
            .collect::<Result<Vec<_>>>()?;
        engine.set_policy("/parent", &engine.policy(&parent)?)?;
        engine.resume_rescans()?; // discover child
        engine.resume_rescans()?; // child reaches its first cursor
        assert!(
            !engine
                .require::<Rescan>(&schema::rescan(&child))?
                .cursor
                .is_empty()
        );
        clear_files(&engine, &ids)?;
        // Completing an older child cursor cannot substitute for rescanning
        // entries already passed before an inherited parent policy changed.
        let mut changed = engine.policy(&parent)?;
        changed.persist_delay_secs += 1;
        engine.set_policy("/parent", &changed)?;
        drain(&engine)?;
        for id in &ids {
            assert!(queued(&engine, id)?);
        }
        engine.set_policy("/parent", &engine.policy(&parent)?)?;
        engine.remove("/parent")?;
        drain(&engine)?;
        assert!(engine.scan::<Rescan>(schema::RESCANS)?.is_empty());
        engine.reschedule_work(schema::WorkKind::Local, &ids[0], false)?;
        assert!(!queued(&engine, &ids[0])?);
        engine.restore(1, "/restored")?;
        drain(&engine)?;
        assert!(queued(&engine, &ids[0])?);
        Ok(())
    }
}
