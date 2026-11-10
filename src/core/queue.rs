//! Durable due-time queues for local protection and archive maintenance.
use super::*;

impl Engine {
    pub(crate) fn schedule_work(&self, kind: &str, id: &str, due: u64) -> Result<()> {
        let pointer = schema::queue_at(kind, id);
        let tx = self.inner.db.transaction();
        let old = tx
            .get_for_update(&pointer, true)?
            .map(|v| Self::decode::<u64>(&v))
            .transpose()?;
        if old == Some(due) {
            return Ok(());
        }
        if let Some(old) = old {
            tx.delete(schema::queue(kind, old, id))?;
        }
        tx.put(&pointer, schema::encode(&due)?)?;
        tx.put(schema::queue(kind, due, id), schema::encode(&id)?)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn schedule_file(&self, id: &str) -> Result<()> {
        let r = self.record(id)?;
        let p = self.policy(id)?;
        self.schedule_work(
            "local",
            id,
            r.closed_at.saturating_add(p.persist_delay_secs),
        )?;
        let delay = self.archive_policy(id)?.map(|p| p.delay_secs).unwrap_or(0);
        self.schedule_work(
            "archive",
            id,
            r.closed_at.saturating_add(delay.max(p.persist_delay_secs)),
        )
    }
    pub(crate) fn due_work(&self, kind: &str, manual: bool) -> Result<Vec<String>> {
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
        for item in self
            .inner
            .db
            .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
        {
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
    pub(crate) fn reschedule_work(&self, kind: &str, id: &str, failed: bool) -> Result<()> {
        if let Ok(r) = self.record(id)
            && self.live(id)?
        {
            let lock = self.file_lock(id);
            let state = lock.lock();
            let delay = if state.writer {
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
            let pointer = schema::queue_at(kind, id);
            if let Some(old) = self.kv_get::<u64>(&pointer)? {
                self.atomic(vec![], vec![pointer, schema::queue(kind, old, id)], false)?;
            }
        }
        Ok(())
    }
}
impl Engine {
    pub(crate) fn rebuild_work_queue(&self) -> Result<()> {
        for (_, r) in self.scan::<Record>(schema::NODES)? {
            if !r.node.is_dir && self.live(&r.node.id)? {
                self.schedule_file(&r.node.id)?;
            }
        }
        Ok(())
    }
}
