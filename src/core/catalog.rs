use super::*;
impl Engine {
    // Never wait for body I/O while holding the namespace lock. Resolve again
    // after waiting because a competing rename may have changed the operands.
    fn namespace_files<T>(
        &self,
        resolve: impl Fn() -> Result<Vec<String>>,
        mut operation: impl FnMut(&[parking_lot::MutexGuard<'_, FileState>]) -> Result<T>,
    ) -> Result<T> {
        loop {
            let namespace = self.inner.namespace.write();
            let mut ids = resolve()?;
            ids.sort();
            ids.dedup();
            let locks = ids.iter().map(|id| self.file_lock(id)).collect::<Vec<_>>();
            let mut guards = Vec::new();
            let mut busy = None;
            for lock in &locks {
                if let Some(guard) = lock.try_lock() {
                    guards.push(guard);
                } else {
                    busy = Some(lock.clone());
                    break;
                }
            }
            if let Some(busy) = busy {
                drop(guards);
                drop(namespace);
                drop(busy.lock());
                continue;
            }
            return operation(&guards);
        }
    }

    pub(crate) fn stat_id(&self, id: &str) -> Result<Node> {
        self.physical_attrs(self.record(id)?)
    }
    /// The record is a closed-file/archive snapshot, not a write-through stat cache.
    /// Never acquire a file mutex here: namespace callers may already hold it.
    fn physical_attrs(&self, r: Record) -> Result<Node> {
        let mut node = r.node;
        if !node.is_dir {
            let mut copies = self.copies(&node.id)?;
            copies.sort_by_key(|copy| Some(&copy.target) != r.primary.as_ref());
            for copy in copies
                .into_iter()
                .filter(|copy| copy.version == node.version)
            {
                if let Ok(metadata) = self
                    .target(&copy.target)
                    .and_then(|root| Ok(root.open_object(&copy.object)?.metadata()?))
                {
                    Self::apply_physical_attrs(&mut node, &metadata)?;
                    break;
                }
            }
        }
        Ok(node)
    }
    pub(crate) fn apply_physical_attrs(node: &mut Node, metadata: &fs::Metadata) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        node.size = metadata.len();
        node.mtime = metadata.modified()?;
        node.atime = metadata.accessed()?;
        let nanos =
            i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
        let magnitude = nanos.unsigned_abs();
        let elapsed = std::time::Duration::new(
            (magnitude / 1_000_000_000).try_into()?,
            (magnitude % 1_000_000_000) as u32,
        );
        node.ctime = if nanos < 0 {
            UNIX_EPOCH - elapsed
        } else {
            UNIX_EPOCH + elapsed
        };
        Ok(())
    }
    pub(crate) fn restore_physical_times(file: &fs::File, node: &Node) -> Result<()> {
        file.set_times(
            fs::FileTimes::new()
                .set_accessed(node.atime)
                .set_modified(node.mtime),
        )?;
        Ok(())
    }
    pub(crate) fn copy_physical_times(source: &fs::File, destination: &fs::File) -> Result<()> {
        let metadata = source.metadata()?;
        destination.set_times(
            fs::FileTimes::new()
                .set_accessed(metadata.accessed()?)
                .set_modified(metadata.modified()?),
        )?;
        Ok(())
    }
    pub(crate) fn parent_id(&self, id: &str) -> Result<String> {
        Ok(self.record(id)?.parent.unwrap_or_else(|| "root".into()))
    }
    pub(crate) fn stat_child(&self, parent: &str, name: &str) -> Result<Node> {
        let _guard = self.inner.namespace.read();
        let node = self.stat_child_unlocked(parent, name)?;
        self.stat_id(&node.id)
    }
    fn stat_child_unlocked(&self, parent: &str, name: &str) -> Result<Node> {
        if !self.record(parent)?.node.is_dir {
            return Err(failure(libc::ENOTDIR));
        }
        let id: String = self.require(&entry(parent, name))?;
        Ok(self.record(&id)?.node)
    }
    pub(crate) fn children(&self, parent: &str) -> Result<Vec<(String, Node)>> {
        let mut out = vec![];
        self.visit_children(parent, |name, node| {
            out.push((name, node));
            Ok(())
        })?;
        Ok(out)
    }
    pub(crate) fn visit_children(
        &self,
        parent: &str,
        mut visitor: impl FnMut(String, Node) -> Result<()>,
    ) -> Result<()> {
        let snapshot = {
            let _guard = self.inner.namespace.read();
            if !self.record(parent)?.node.is_dir {
                return Err(failure(libc::ENOTDIR));
            }
            self.inner.db.snapshot()
        };
        let prefix = schema::entries(parent);
        for item in snapshot.iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward)) {
            let (k, v) = item?;
            if !k.starts_with(prefix.as_bytes()) {
                break;
            }
            let id: String = Self::decode(&v)?;
            let bytes = snapshot
                .get_pinned(schema::node(&id))?
                .ok_or_else(|| failure(libc::ENOENT))?;
            let r: Record = Self::decode(&bytes)?;
            let name = r.name.clone();
            visitor(name, self.physical_attrs(r)?)?;
        }
        Ok(())
    }
    pub fn stat(&self, path: &str) -> Result<Node> {
        let _guard = self.inner.namespace.read();
        let node = self.lookup_unlocked(path)?;
        self.stat_id(&node.id)
    }
    /// Caller holds the namespace lock while resolving and using this identity.
    pub(crate) fn lookup_unlocked(&self, path: &str) -> Result<Node> {
        if path == "/" {
            return Ok(self.record("root")?.node);
        }
        ensure!(path.starts_with('/'), "invalid path");
        let mut node = self.record("root")?.node;
        for name in path[1..].split('/') {
            ensure!(safe_name(name), "invalid path");
            node = self.stat_child_unlocked(&node.id, name)?;
        }
        Ok(node)
    }
    pub fn list(&self, path: &str) -> Result<Vec<String>> {
        Ok(self
            .children(&self.stat(path)?.id)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }
    pub(crate) fn set_times(
        &self,
        id: &str,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> Result<()> {
        let lock = self.file_lock(id);
        let _guard = lock.lock();
        let mut r = self.record(id)?;
        if !r.node.is_dir {
            let mut times = fs::FileTimes::new();
            if let Some(t) = atime {
                times = times.set_accessed(t);
            }
            if let Some(t) = mtime {
                times = times.set_modified(t);
            }
            for copy in self
                .copies(id)?
                .into_iter()
                .filter(|c| c.version == r.node.version)
            {
                if let Ok(root) = self.target(&copy.target) {
                    root.open_object(&copy.object)?.set_times(times)?;
                }
            }
        }
        if let Some(t) = atime {
            r.node.atime = t;
        }
        if let Some(t) = mtime {
            r.node.mtime = t;
        }
        r.node.ctime = SystemTime::now();
        self.put_record(&schema::node(id), &r, false)
    }
    pub(crate) fn set_birth_time(&self, id: &str, time: SystemTime) -> Result<()> {
        let lock = self.file_lock(id);
        let _guard = lock.lock();
        let mut r = self.record(id)?;
        r.node.btime = time;
        self.put_record(&schema::node(id), &r, false)
    }
    pub fn inherit_policy(&self, path: &str, archive: bool) -> Result<()> {
        let _guard = self.inner.namespace.write();
        let n = self.lookup_unlocked(path)?;
        ensure!(n.is_dir, "policy requires directory");
        ensure!(archive || n.id != "root", "root requires storage policy");
        self.atomic(
            vec![Self::rescan_record(&n.id)?],
            vec![if archive {
                schema::archive_policy(&n.id)
            } else {
                schema::policy(&n.id)
            }],
            false,
        )?;
        Ok(())
    }
    pub fn mkdir(&self, path: &str) -> Result<()> {
        let (p, n) = split(path)?;
        self.mkdir_at(&self.stat(p)?.id, n).map(|_| ())
    }
    pub(crate) fn mkdir_at(&self, parent: &str, name: &str) -> Result<Node> {
        self.create_node(parent, name, true, true).map(|(n, _)| n)
    }
    pub(crate) fn create_file_at(&self, parent: &str, name: &str, exclusive: bool) -> Result<Node> {
        self.create_node(parent, name, false, exclusive)
            .map(|(n, _)| n)
    }
    pub(crate) fn create_file_at_result(
        &self,
        parent: &str,
        name: &str,
        exclusive: bool,
    ) -> Result<(Node, bool)> {
        self.create_node(parent, name, false, exclusive)
    }
    fn create_node(
        &self,
        parent: &str,
        name: &str,
        dir: bool,
        exclusive: bool,
    ) -> Result<(Node, bool)> {
        ensure!(safe_name(name), "invalid name");
        let _guard = self.inner.namespace.write();
        if !self.record(parent)?.node.is_dir {
            return Err(failure(libc::ENOTDIR));
        }
        let k = entry(parent, name);
        if let Some(id) = self.kv_get::<String>(&k)? {
            if exclusive {
                return Err(failure(libc::EEXIST));
            }
            let n = self.stat_id(&id)?;
            if n.is_dir {
                return Err(failure(libc::EISDIR));
            }
            return Ok((n, false));
        }
        let r = Record::new(
            Uuid::new_v4().to_string(),
            Some(parent.into()),
            name.into(),
            dir,
        );
        self.atomic(
            vec![
                Self::encoded(schema::node(&r.node.id), &r)?,
                Self::encoded(k, &r.node.id)?,
            ],
            vec![],
            false,
        )?;
        Ok((r.node, true))
    }
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (fp, fnm) = split(from)?;
        let (tp, tnm) = split(to)?;
        self.rename_at(&self.stat(fp)?.id, fnm, &self.stat(tp)?.id, tnm, true)
    }
    pub(crate) fn rename_at(
        &self,
        fp: &str,
        from: &str,
        tp: &str,
        to: &str,
        replace: bool,
    ) -> Result<()> {
        ensure!(safe_name(from) && safe_name(to), "invalid name");
        self.namespace_files(
            || {
                let mut ids = vec![self.stat_child_unlocked(fp, from)?.id];
                if let Some(id) = self.kv_get::<String>(&entry(tp, to))? {
                    ids.push(id);
                }
                Ok(ids)
            },
            |_| {
                let src = self.stat_child_unlocked(fp, from)?;
                if !self.record(tp)?.node.is_dir {
                    return Err(failure(libc::ENOTDIR));
                }
                let mut parent = Some(tp.to_owned());
                while let Some(id) = parent {
                    if id == src.id {
                        return Err(failure(libc::EINVAL));
                    }
                    parent = self.record(&id)?.parent;
                }
                let mut puts = vec![];
                let mut deletes = vec![entry(fp, from)];
                let mut replaced_pin = None;
                let destination = self.kv_get::<String>(&entry(tp, to))?;
                if let Some(dst) = destination
                    && dst != src.id
                {
                    if !replace {
                        return Err(failure(libc::EEXIST));
                    }
                    let n = self.record(&dst)?;
                    if n.node.is_dir != src.is_dir {
                        return Err(failure(if n.node.is_dir {
                            libc::EISDIR
                        } else {
                            libc::ENOTDIR
                        }));
                    }
                    if n.node.is_dir && !self.scan::<String>(&schema::entries(&dst))?.is_empty() {
                        return Err(failure(libc::ENOTEMPTY));
                    }
                    let original = self.path_for(&dst)?.unwrap_or_default();
                    replaced_pin =
                        Some(self.detach_mutations(n, &original, &mut puts, &mut deletes)?);
                }
                let mut record = self.record(&src.id)?;
                record.parent = Some(tp.into());
                record.name = to.into();
                record.node.ctime = SystemTime::now();
                puts.push(Self::encoded(schema::node(&src.id), &record)?);
                puts.push(Self::encoded(entry(tp, to), &src.id)?);
                puts.push(Self::rescan_record(&src.id)?);
                self.atomic(puts, deletes, false)?;
                drop(replaced_pin);
                Ok(())
            },
        )
    }

    fn detach_mutations(
        &self,
        mut r: Record,
        path: &str,
        puts: &mut Vec<(String, Vec<u8>)>,
        deletes: &mut Vec<String>,
    ) -> Result<pin::FilePin> {
        let pin = self
            .try_idle_pin(&r.node.id)?
            .ok_or_else(|| failure(libc::EBUSY))?;
        let policy = self.policy(&r.node.id)?;
        let archive_policy = self.archive_policy(&r.node.id)?;
        let ttl = policy.trash_ttl_secs;
        let time = now()?;
        if let Some(parent) = &r.parent {
            deletes.push(entry(parent, &r.name));
        }
        let next = self.kv_get::<i64>(schema::TRASH_SEQ)?.unwrap_or(0) + 1;
        let trash = Trash {
            id: next,
            node: r.node.id.clone(),
            original: path.into(),
            deleted: time,
            expires: time.saturating_add(ttl),
            policy,
            archive_policy,
        };
        r.parent = None;
        puts.extend([
            Self::encoded(schema::node(&r.node.id), &r)?,
            Self::encoded(schema::trash(next), &trash)?,
            Self::encoded(schema::TRASH_SEQ.into(), &next)?,
        ]);
        Ok(pin)
    }
    pub fn remove(&self, path: &str) -> Result<()> {
        self.namespace_files(
            || Ok(vec![self.lookup_unlocked(path)?.id]),
            |_| {
                let n = self.lookup_unlocked(path)?;
                ensure!(n.id != "root", "cannot remove root");
                let mut puts = vec![];
                let mut deletes = vec![];
                let _pin =
                    self.detach_mutations(self.record(&n.id)?, path, &mut puts, &mut deletes)?;
                self.atomic(puts, deletes, false)
            },
        )
    }
    pub(crate) fn unlink_at(&self, parent: &str, name: &str, is_dir: bool) -> Result<()> {
        self.namespace_files(
            || Ok(vec![self.stat_child_unlocked(parent, name)?.id]),
            |_| {
                let n = self.stat_child_unlocked(parent, name)?;
                if n.is_dir != is_dir {
                    return Err(failure(if n.is_dir {
                        libc::EISDIR
                    } else {
                        libc::ENOTDIR
                    }));
                }
                if is_dir && !self.scan::<String>(&schema::entries(&n.id))?.is_empty() {
                    return Err(failure(libc::ENOTEMPTY));
                }
                let path = self.path_for(&n.id)?.unwrap_or_default();
                let mut puts = vec![];
                let mut deletes = vec![];
                let _pin =
                    self.detach_mutations(self.record(&n.id)?, &path, &mut puts, &mut deletes)?;
                self.atomic(puts, deletes, false)
            },
        )
    }
    pub fn restore(&self, id: i64, path: &str) -> Result<()> {
        let (p, name) = split(path)?;
        self.namespace_files(
            || Ok(vec![self.require::<Trash>(&schema::trash(id))?.node]),
            |_| {
                let parent = self.lookup_unlocked(p)?;
                if !parent.is_dir {
                    return Err(failure(libc::ENOTDIR));
                }
                if self.kv_get::<String>(&entry(&parent.id, name))?.is_some() {
                    return Err(failure(libc::EEXIST));
                }
                let t: Trash = self.require(&schema::trash(id))?;
                let mut r = self.record(&t.node)?;
                r.parent = Some(parent.id.clone());
                r.name = name.into();
                self.atomic(
                    vec![
                        Self::encoded(schema::node(&t.node), &r)?,
                        Self::encoded(entry(&parent.id, name), &t.node)?,
                        Self::rescan_record(&t.node)?,
                    ],
                    vec![schema::trash(id)],
                    false,
                )?;
                Ok(())
            },
        )
    }
    pub(crate) fn subtree(&self, id: &str) -> Result<Vec<Record>> {
        let mut out = vec![];
        let mut todo = vec![id.to_owned()];
        while let Some(id) = todo.pop() {
            let r = self.record(&id)?;
            if r.node.is_dir {
                todo.extend(
                    self.scan::<String>(&schema::entries(&id))?
                        .into_iter()
                        .map(|(_, id)| id),
                );
            }
            out.push(r);
        }
        Ok(out)
    }
    pub fn purge(&self, id: i64) -> Result<()> {
        self.namespace_files(
            || {
                let t: Trash = self.require(&schema::trash(id))?;
                Ok(self
                    .subtree(&t.node)?
                    .into_iter()
                    .map(|r| r.node.id)
                    .collect())
            },
            |guards| {
                let t: Trash = self.require(&schema::trash(id))?;
                let records = self.subtree(&t.node)?;
                ensure!(guards.iter().all(|g| g.handles == 0), "file is open");
                let _pins = records
                    .iter()
                    .map(|r| {
                        self.try_idle_pin(&r.node.id)?
                            .ok_or_else(|| failure(libc::EBUSY))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut puts = vec![];
                let mut deletes = vec![schema::trash(id)];
                for r in records {
                    let nid = &r.node.id;
                    for c in self.copies(nid)? {
                        puts.push(Self::encoded(schema::garbage(&c.target), &c)?);
                        deletes.push(schema::copy(nid, &c.target));
                    }
                    for (k, c) in
                        self.scan::<crate::archive::ArchiveCopy>(&schema::archive_copies(nid))?
                    {
                        puts.push(Self::encoded(schema::archive_gc(), &c)?);
                        deletes.push(k);
                    }
                    deletes.extend([
                        schema::node(nid),
                        schema::policy(nid),
                        schema::archive_policy(nid),
                        schema::rescan(nid),
                    ]);
                    for (k, _) in self.scan::<String>(&schema::entries(nid))? {
                        deletes.push(k);
                    }
                }
                self.atomic(puts, deletes, true)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_attributes_do_not_require_per_write_metadata_updates() -> Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Engine::open(&root.path().join("metadata"))?;
        let disk = root.path().join("disk");
        fs::create_dir(&disk)?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                write_cache: vec!["disk".into()],
                low_water: 99,
                high_water: 100,
                ..Default::default()
            },
        )?;
        let handle = engine.open_file("/file", true, true, false)?;
        let id = engine.stat("/file")?.id;
        assert!(engine.local_file_info(&id)?.is_none());
        let key = schema::node(&id);
        let before: Option<serde_json::Value> = engine.inner.db.get(&key)?;
        handle.write_at(0, b"contents")?;
        assert_eq!(engine.inner.db.get::<serde_json::Value>(&key)?, before);
        assert_eq!(engine.stat("/file")?.size, 8);
        assert_eq!(engine.children("root")?[0].1.size, 8);
        handle.set_len(3)?;
        assert_eq!(engine.inner.db.get::<serde_json::Value>(&key)?, before);
        assert_eq!(engine.stat_id(&id)?.size, 3);
        let time = UNIX_EPOCH + std::time::Duration::from_secs(123456789);
        engine.set_times(&id, Some(time), Some(time))?;
        assert_eq!(engine.stat_id(&id)?.mtime, time);
        handle.close()?;
        assert_eq!(engine.record(&id)?.node.size, 3);
        assert_eq!(engine.record(&id)?.node.mtime, time);
        engine.reconcile()?;
        assert_eq!(engine.stat_id(&id)?.mtime, time);
        let local = engine
            .local_file_info(&id)?
            .context("missing local mapping")?;
        assert_eq!(local.node.size, 3);
        assert_eq!(fs::read(&local.path)?, b"con");
        use std::os::unix::fs::MetadataExt;
        let physical = fs::metadata(&local.path)?;
        assert_eq!(local.device, physical.dev());
        assert_eq!(local.inode, physical.ino());
        let writer = engine.open_file("/file", true, false, false)?;
        assert!(engine.local_file_info(&id)?.is_none());
        writer.close()?;
        Ok(())
    }

    #[test]
    fn missing_dirty_body_is_not_recreated_from_stale_empty_snapshot() -> Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Engine::open(&root.path().join("metadata"))?;
        let disk = root.path().join("disk");
        fs::create_dir(&disk)?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                write_cache: vec!["disk".into()],
                low_water: 99,
                high_water: 100,
                ..Default::default()
            },
        )?;
        let handle = engine.open_file("/file", true, true, false)?;
        let id = engine.stat("/file")?.id;
        let dirty = engine.record(&id)?;
        handle.write_at(0, b"only copy")?;
        handle.close()?;
        // A crash before close leaves the durable invalidation and old stat snapshot.
        engine.put_record(&schema::node(&id), &dirty, true)?;
        drop(engine);
        let engine = Engine::open(&root.path().join("metadata"))?;
        assert_eq!(engine.stat("/file")?.size, 9);
        fs::rename(&disk, root.path().join("offline"))?;
        fs::create_dir(&disk)?;
        // A different available target makes accidental empty-file creation observable.
        let spare = root.path().join("spare");
        fs::create_dir(&spare)?;
        engine.register_target("spare", &spare, Some("spare"), true)?;
        engine.set_policy(
            "/",
            &Policy {
                write_cache: vec!["spare".into()],
                low_water: 99,
                high_water: 100,
                ..Default::default()
            },
        )?;
        assert!(engine.get("/file").is_err());
        assert_eq!(engine.record(&id)?.primary, Some("disk".into()));
        Ok(())
    }

    #[test]
    fn waiting_for_file_io_does_not_block_other_namespace_operations() -> Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Engine::open(&root.path().join("metadata"))?;
        engine.mkdir("/busy")?;
        let node = engine.stat("/busy")?;
        let file_lock = engine.file_lock(&node.id);
        let held_io = file_lock.lock();
        std::thread::scope(|scope| -> Result<()> {
            let rename = scope.spawn(|| engine.rename("/busy", "/moved"));
            std::thread::sleep(std::time::Duration::from_millis(50));
            let (send, receive) = std::sync::mpsc::channel();
            let shared = &engine;
            scope.spawn(move || {
                send.send(shared.mkdir("/unrelated")).unwrap();
            });
            let result = receive.recv_timeout(std::time::Duration::from_secs(2));
            drop(held_io);
            rename.join().unwrap()?;
            result.context("unrelated metadata blocked behind file I/O")??;
            Ok(())
        })?;
        assert!(engine.stat("/unrelated")?.is_dir);
        assert!(engine.stat("/moved")?.is_dir);
        Ok(())
    }
}
