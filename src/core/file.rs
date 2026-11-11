use super::*;
use std::os::unix::fs::FileExt;
pub(crate) struct Body {
    pub file: fs::File,
    pub root: target::TargetDirectory,
    pub copy: Copy,
}
/// A verified local placement exported under the IPC cache authorization gate.
/// Device and inode let the client reject a replaced mount or object before use.
pub(crate) struct LocalFile {
    pub node: Node,
    pub path: std::path::PathBuf,
    pub device: u64,
    pub inode: u64,
    pub pin_path: std::path::PathBuf,
    pub pin_offset: u64,
}
pub struct FileHandle {
    engine: Engine,
    id: String,
    write: bool,
    state: Arc<Mutex<FileState>>,
    closed: bool,
    _pin: pin::FilePin,
}
impl FileHandle {
    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self.state.lock();
        self.engine.ensure_body(&self.id, &mut state)?;
        Ok(state.body.as_ref().unwrap().file.read_at(out, offset)?)
    }
    pub fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize> {
        if !self.write {
            return Err(failure(libc::EBADF));
        }
        let mut state = self.state.lock();
        self.engine.ensure_body(&self.id, &mut state)?;
        let body = state.body.as_ref().unwrap();
        Ok(body.file.write_at(data, offset)?)
    }
    pub fn set_len(&self, len: u64) -> Result<()> {
        if !self.write {
            return Err(failure(libc::EBADF));
        }
        let mut state = self.state.lock();
        self.engine.ensure_body(&self.id, &mut state)?;
        state.body.as_ref().unwrap().file.set_len(len)?;
        Ok(())
    }
    pub fn sync(&self) -> Result<()> {
        let mut state = self.state.lock();
        if !self.write && state.body.is_none() {
            return self.engine.sync_metadata();
        }
        self.engine.ensure_body(&self.id, &mut state)?;
        let b = state.body.as_ref().unwrap();
        Engine::sync_object(&b.root, &b.copy.object, &b.file)?;
        self.engine.sync_metadata()
    }
    pub fn close(mut self) -> Result<()> {
        self.finish()
    }
    fn finish(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut state = self.state.lock();
        state.handles = state.handles.saturating_sub(1);
        if self.write {
            state.writer = false;
            let mut r = self.engine.record(&self.id)?;
            if let Some(b) = &state.body {
                Engine::apply_physical_attrs(&mut r.node, &b.file.metadata()?)?;
            }
            r.closed_at = now()?;
            self.engine.put_record(&schema::node(&self.id), &r, false)?;
            self.engine.schedule_file(&self.id)?;
        }
        Ok(())
    }
}
impl Drop for FileHandle {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            eprintln!("closing file {}: {e:#}", self.id);
        }
    }
}
impl Engine {
    /// Does not recall archived files. The caller must retain its cache grant
    /// while publishing/using the result and revoke it before storage mutation.
    pub(crate) fn local_file_info(&self, id: &str) -> Result<Option<LocalFile>> {
        use std::os::unix::fs::MetadataExt;
        let lock = self.file_lock(id);
        let Some(state) = lock.try_lock() else {
            return Ok(None);
        };
        let r = self.record(id)?;
        if r.node.is_dir || state.writer {
            return Ok(None);
        }
        let mut copies = self.copies(id)?;
        copies.sort_by_key(|copy| Some(&copy.target) != r.primary.as_ref());
        for copy in copies.into_iter().filter(|copy| {
            copy.version == r.node.version && Some(&copy.target) == r.primary.as_ref()
        }) {
            let Ok(root) = self.target(&copy.target) else {
                continue;
            };
            let Ok(file) = root.open_object(&copy.object) else {
                continue;
            };
            let metadata = file.metadata()?;
            let target: Target = self.require(&schema::target(&copy.target))?;
            let mut node = r.node.clone();
            Self::apply_physical_attrs(&mut node, &metadata)?;
            let (pin_path, pin_offset) = self.pin_location(id);
            return Ok(Some(LocalFile {
                node,
                path: target.path.join(&copy.object),
                device: metadata.dev(),
                inode: metadata.ino(),
                pin_path,
                pin_offset,
            }));
        }
        Ok(None)
    }

    pub fn open_file(
        &self,
        path: &str,
        write: bool,
        create: bool,
        truncate: bool,
    ) -> Result<FileHandle> {
        let node = match self.stat(path) {
            Ok(n) => n,
            Err(e)
                if create
                    && e.downcast_ref::<io::Error>()
                        .is_some_and(|e| e.raw_os_error() == Some(libc::ENOENT)) =>
            {
                let (p, n) = split(path)?;
                self.create_file_at(&self.stat(p)?.id, n, false)?
            }
            Err(e) => return Err(e),
        };
        self.open_id(&node.id, write, truncate)
    }
    pub(crate) fn open_id(&self, id: &str, write: bool, truncate: bool) -> Result<FileHandle> {
        let state = self.file_lock(id);
        let mut guard = state.lock();
        let mut r = self.record(id)?;
        if r.node.is_dir {
            return Err(failure(libc::EISDIR));
        }
        if write && guard.writer {
            return Err(failure(libc::EBUSY));
        }
        if truncate && !write {
            return Err(failure(libc::EACCES));
        }
        let idle = if write { self.try_idle_pin(id)? } else { None };
        let exclusive = idle.is_some();
        let pin = match idle {
            Some(pin) => pin,
            None => self.shared_pin(id)?,
        };
        if write {
            let policy = self.policy(id)?;
            let invalidates_protected = !r.dirty && r.hash.is_some();
            if r.primary.is_none() && (truncate || r.hash.is_none()) {
                let target = self.choose_target(&policy, id)?;
                let (root, object, file) = self.create_object(&target, id)?;
                let copy = Copy {
                    node: id.into(),
                    target: target.clone(),
                    object,
                    version: r.node.version,
                    hash: None,
                };
                guard.body = Some(Body { file, root, copy });
                r.primary = Some(target);
            } else {
                // RPC readers share FileState and follow this descriptor. Direct
                // readers are pinned to the recorded primary, so writable OPEN
                // must never silently reuse an RPC reader's fallback replica.
                if let Some(primary) = &r.primary {
                    let current = self
                        .copies(id)?
                        .into_iter()
                        .find(|copy| &copy.target == primary && copy.version == r.node.version);
                    let opened = current.and_then(|copy| {
                        self.target(primary)
                            .and_then(|root| {
                                let file = root.open_object(&copy.object)?;
                                Ok(Body { file, root, copy })
                            })
                            .ok()
                    });
                    if let Some(body) = opened {
                        guard.body = Some(body);
                    } else if !exclusive {
                        return Err(failure(libc::EIO));
                    } else {
                        guard.body = None;
                    }
                }
                if exclusive {
                    self.ensure_body_inner(id, &mut guard, true)?;
                } else {
                    self.ensure_body(id, &mut guard)?;
                }
            }
            // Durable invalidation precedes in-place mutation so an old replica cannot
            // become current after a crash. Body writes retain ordinary filesystem semantics.
            r.node.version = r
                .node
                .version
                .checked_add(1)
                .context("file version exhausted")?;
            r.dirty = true;
            r.placement = None;
            r.hash = None;
            r.error = None;
            r.incident = None;
            let body = guard.body.as_mut().unwrap();
            body.file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(body.root.root.join(&body.copy.object))?;
            r.primary = Some(body.copy.target.clone());
            body.copy.version = r.node.version;
            body.copy.hash = None;
            let copy_key = schema::copy(id, &body.copy.target);
            let mut changes = vec![
                Self::encoded(schema::node(id), &r)?,
                Self::encoded(copy_key.clone(), &body.copy)?,
            ];
            if let Some(old) = self.kv_get::<Copy>(&copy_key)?
                && old.object != body.copy.object
            {
                changes.push(Self::encoded(schema::garbage(&old.target), &old)?);
            }
            self.atomic(changes, vec![], invalidates_protected)?;
            if truncate {
                body.file.set_len(0)?;
            }
            self.schedule_file(id)?;
            guard.writer = true;
        }
        pin.shared(self.pin_location(id).1)?;
        guard.handles += 1;
        drop(guard);
        Ok(FileHandle {
            engine: self.clone(),
            id: id.into(),
            write,
            state,
            closed: false,
            _pin: pin,
        })
    }
    pub(crate) fn ensure_body(&self, id: &str, state: &mut FileState) -> Result<()> {
        self.ensure_body_inner(id, state, false)
    }
    fn ensure_body_inner(
        &self,
        id: &str,
        state: &mut FileState,
        replace_primary: bool,
    ) -> Result<()> {
        if state.body.is_some() {
            return Ok(());
        }
        let r = self.record(id)?;
        let mut copies = self.copies(id)?;
        copies.sort_by_key(|c| Some(&c.target) != r.primary.as_ref());
        for c in copies.into_iter().filter(|c| c.version == r.node.version) {
            if let Ok((root, file)) = self.target(&c.target).and_then(|root| {
                let file = root.open_object(&c.object)?;
                Ok((root, file))
            }) {
                state.body = Some(Body {
                    file,
                    root,
                    copy: c,
                });
                return Ok(());
            }
        }
        if r.primary.is_some() && !replace_primary {
            return Err(failure(libc::EIO));
        }
        self.recall_body(id, state)
    }
    pub(crate) fn recall_body(&self, id: &str, state: &mut FileState) -> Result<()> {
        let r = self.record(id)?;
        let (body, garbage) = self.download_body(id, &r)?;
        self.publish_body(id, r, body, garbage, state)
    }
    fn download_body(&self, id: &str, r: &Record) -> Result<(Body, String)> {
        // Archive-only files are recalled directly into a target, verified before
        // publishing; normal local reads never scan or copy the whole file.
        let policy = self.policy(id)?;
        let target = self.choose_target(&policy, id)?;
        let (root, object, mut file) = self.create_object(&target, id)?;
        let copy = Copy {
            node: id.into(),
            target: target.clone(),
            object,
            version: r.node.version,
            hash: r.hash.clone(),
        };
        let garbage = schema::garbage(&target);
        self.put_record(&garbage, &copy, true)?;
        // A dirty file's persisted size may predate its writes. Only a never-
        // materialized file can be created empty; a missing disk is not emptiness.
        let transfer = (|| -> Result<()> {
            if r.primary.is_some()
                || r.node.size != 0
                || r.hash.is_some()
                || !self.copies(id)?.is_empty()
            {
                self.recall_archive(id, &mut file)?;
            }
            Self::restore_physical_times(&file, &r.node)?;
            Self::sync_object(&root, &copy.object, &file)
        })();
        if let Err(error) = transfer {
            if root.remove_object(&copy.object).is_ok() {
                self.delete_record(&garbage)?;
            }
            return Err(error);
        }
        Ok((Body { file, root, copy }, garbage))
    }
    fn publish_body(
        &self,
        id: &str,
        mut r: Record,
        body: Body,
        garbage: String,
        state: &mut FileState,
    ) -> Result<()> {
        let copy = &body.copy;
        r.primary = Some(copy.target.clone());
        let mut puts = vec![
            Self::encoded(schema::node(id), &r)?,
            Self::encoded(schema::copy(id, &copy.target), copy)?,
        ];
        if let Some(old) = self.kv_get::<Copy>(&schema::copy(id, &copy.target))? {
            puts.push(Self::encoded(schema::garbage(&old.target), &old)?);
        }
        self.atomic(puts, vec![garbage], true)?;
        state.body = Some(body);
        Ok(())
    }
    /// Background transfers never retain an exclusive pin across remote I/O.
    pub(crate) fn recall_idle(&self, id: &str) -> Result<bool> {
        let lock = self.file_lock(id);
        let r = {
            let Some(_pin) = self.try_idle_pin(id)? else {
                return Ok(false);
            };
            let _state = lock.lock();
            self.record(id)?
        };
        let (body, garbage) = self.download_body(id, &r)?;
        let pin = self.try_idle_pin(id)?;
        if pin.is_none() {
            if body.root.remove_object(&body.copy.object).is_ok() {
                self.delete_record(&garbage)?;
            }
            return Ok(false);
        }
        let mut state = lock.lock();
        let latest = self.record(id)?;
        if latest.node.version != r.node.version {
            if body.root.remove_object(&body.copy.object).is_ok() {
                self.delete_record(&garbage)?;
            }
            return Ok(false);
        }
        self.publish_body(id, latest, body, garbage, &mut state)?;
        Ok(true)
    }
    pub fn put(&self, path: &str, data: &[u8]) -> Result<()> {
        let h = self.open_file(path, true, true, true)?;
        let mut offset = 0;
        while offset < data.len() {
            let n = h.write_at(offset as u64, &data[offset..])?;
            ensure!(n > 0, "short write");
            offset += n;
        }
        h.close()
    }
    pub fn get(&self, path: &str) -> Result<Vec<u8>> {
        let h = self.open_file(path, false, false, false)?;
        let mut out = vec![];
        let mut buf = vec![0; 65536];
        loop {
            let n = h.read_at(out.len() as u64, &mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }
}
