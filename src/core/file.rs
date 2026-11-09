use super::*;
use std::os::unix::fs::FileExt;
pub(crate) struct Body {
    pub file: fs::File,
    pub root: target::TargetDirectory,
    pub copy: Copy,
}
pub struct FileHandle {
    engine: Engine,
    id: String,
    write: bool,
    state: Arc<Mutex<FileState>>,
    closed: bool,
}
impl FileHandle {
    pub fn node_id(&self) -> &str {
        &self.id
    }
    pub fn read_at(&self, offset: u64, out: &mut [u8]) -> Result<usize> {
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
        self.engine
            .ensure_write_capacity(&self.id, &mut state, offset, data.len() as u64)?;
        let body = state.body.as_ref().unwrap();
        let n = body.file.write_at(data, offset)?;
        let mut r = self.engine.record(&self.id)?;
        r.node.size = body.file.metadata()?.len();
        r.node.mtime = SystemTime::now();
        r.node.ctime = r.node.mtime;
        self.engine.put_record(&key("node", &self.id), &r, false)?;
        Ok(n)
    }
    pub fn set_len(&self, len: u64) -> Result<()> {
        if !self.write {
            return Err(failure(libc::EBADF));
        }
        let mut state = self.state.lock();
        self.engine.ensure_body(&self.id, &mut state)?;
        state.body.as_ref().unwrap().file.set_len(len)?;
        let mut r = self.engine.record(&self.id)?;
        r.node.size = len;
        r.node.mtime = SystemTime::now();
        r.node.ctime = r.node.mtime;
        self.engine.put_record(&key("node", &self.id), &r, false)
    }
    pub fn sync(&self) -> Result<()> {
        let mut state = self.state.lock();
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
                r.node.size = b.file.metadata()?.len();
            }
            r.closed_at = now()?;
            self.engine.put_record(&key("node", &self.id), &r, false)?;
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
                self.ensure_body(id, &mut guard)?;
            }
            let current_target = &guard.body.as_ref().unwrap().copy.target;
            if !policy.write_cache.is_empty()
                && !policy.write_cache.contains(current_target)
                && let Ok(target) = self.choose_target(&policy, id)
                && &target != current_target
            {
                let (root, object, mut file) = self.create_object(&target, id)?;
                let transfer = (|| -> Result<()> {
                    if !truncate {
                        let old = guard.body.as_ref().unwrap();
                        let mut source = old.root.open_object(&old.copy.object)?;
                        io::copy(&mut source, &mut file)?;
                        if let Some(expected) = &r.hash {
                            ensure!(
                                fingerprint(&mut file)?.1 == *expected,
                                "source checksum mismatch"
                            );
                        }
                    }
                    Self::sync_object(&root, &object, &file)
                })();
                if let Err(e) = transfer {
                    let _ = root.remove_object(&object);
                    return Err(e);
                }
                let copy = Copy {
                    node: id.into(),
                    target,
                    object,
                    version: r.node.version,
                    hash: None,
                };
                guard.body = Some(Body { file, root, copy });
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
            let copy_key = format!("copy/{id}/{}", body.copy.target);
            let mut changes = vec![
                Self::encoded(key("node", id), &r)?,
                Self::encoded(copy_key.clone(), &body.copy)?,
            ];
            if let Some(old) = self.kv_get::<Copy>(&copy_key)?
                && old.object != body.copy.object
            {
                changes.push(Self::encoded(
                    format!("garbage/{}/{}", old.target, Uuid::new_v4()),
                    &old,
                )?);
            }
            self.atomic(changes, vec![], invalidates_protected)?;
            if truncate {
                body.file.set_len(0)?;
                r.node.size = 0;
                r.node.mtime = SystemTime::now();
                self.put_record(&key("node", id), &r, false)?;
            }
            self.schedule_file(id)?;
            guard.writer = true;
        } else {
            self.ensure_body(id, &mut guard)?;
        }
        guard.handles += 1;
        drop(guard);
        Ok(FileHandle {
            engine: self.clone(),
            id: id.into(),
            write,
            state,
            closed: false,
        })
    }
    pub(crate) fn ensure_body(&self, id: &str, state: &mut FileState) -> Result<()> {
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
        self.recall_body(id, state)
    }
    pub(crate) fn recall_body(&self, id: &str, state: &mut FileState) -> Result<()> {
        let mut r = self.record(id)?;
        // Archive-only files are recalled directly into a target, verified before
        // publishing; normal local reads never scan or copy the whole file.
        let policy = self.policy(id)?;
        let target = self.choose_target(&policy, id)?;
        let (root, object, mut file) = self.create_object(&target, id)?;
        if (r.node.size != 0 || r.hash.is_some())
            && let Err(e) = self.recall_archive(id, &mut file)
        {
            let _ = root.remove_object(&object);
            return Err(e);
        }
        Self::sync_object(&root, &object, &file)?;
        let copy = Copy {
            node: id.into(),
            target: target.clone(),
            object,
            version: r.node.version,
            hash: r.hash.clone(),
        };
        r.primary = Some(target);
        let mut puts = vec![
            Self::encoded(key("node", id), &r)?,
            Self::encoded(format!("copy/{id}/{}", copy.target), &copy)?,
        ];
        if let Some(old) = self.kv_get::<Copy>(&format!("copy/{id}/{}", copy.target))? {
            puts.push(Self::encoded(
                format!("garbage/{}/{}", old.target, Uuid::new_v4()),
                &old,
            )?);
        }
        self.atomic(puts, vec![], true)?;
        state.body = Some(Body { file, root, copy });
        Ok(())
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
    pub fn put_from(&self, path: &str, input: &mut fs::File) -> Result<()> {
        input.rewind()?;
        let h = self.open_file(path, true, true, true)?;
        let mut buf = vec![0; 1024 * 1024];
        let mut offset = 0;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let mut done = 0;
            while done < n {
                let written = h.write_at(offset + done as u64, &buf[done..n])?;
                ensure!(written > 0, "short write");
                done += written;
            }
            offset += n as u64;
        }
        h.close()
    }
    pub fn copy_to(&self, path: &str, out: &mut fs::File) -> Result<()> {
        self.copy_id(&self.stat(path)?.id, out)
    }
    pub(crate) fn copy_id(&self, id: &str, out: &mut fs::File) -> Result<()> {
        let h = self.open_id(id, false, false)?;
        let mut offset = 0;
        let mut buf = vec![0; 1024 * 1024];
        out.set_len(0)?;
        out.rewind()?;
        loop {
            let n = h.read_at(offset, &mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            offset += n as u64;
        }
        out.rewind()?;
        Ok(())
    }
    pub fn get(&self, path: &str) -> Result<Vec<u8>> {
        self.read_id(&self.stat(path)?.id)
    }
    pub(crate) fn read_id(&self, id: &str) -> Result<Vec<u8>> {
        let h = self.open_id(id, false, false)?;
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

impl Engine {
    fn ensure_write_capacity(
        &self,
        id: &str,
        state: &mut FileState,
        offset: u64,
        length: u64,
    ) -> Result<()> {
        let body = state.body.as_ref().context("missing file body")?;
        let p = self.policy(id)?;
        let additional = offset
            .checked_add(length)
            .ok_or_else(|| failure(libc::EFBIG))?
            .saturating_sub(body.file.metadata()?.len());
        let stat = body.root.stat()?;
        let available = stat.f_bavail.saturating_mul(stat.f_frsize);
        let used = 100
            - stat
                .f_bavail
                .saturating_sub(additional.div_ceil(stat.f_frsize.max(1)))
                .saturating_mul(100)
                / stat.f_blocks.max(1);
        let cache = p.write_cache.contains(&body.copy.target);
        if available >= additional && (!cache || used < u64::from(p.high_water)) {
            return Ok(());
        }
        if cache && self.relieve_cache(&body.copy.target, id, additional, p.high_water)? {
            return Ok(());
        }
        if !cache || !p.direct_fallback {
            return Err(failure(libc::ENOSPC));
        }
        let needed = body.file.metadata()?.len().saturating_add(additional);
        let mut targets = p.persistent_storage.clone();
        rank(&mut targets, id);
        for name in targets {
            if name == body.copy.target {
                continue;
            }
            let Ok(stat) = self.target_stat(&name) else {
                continue;
            };
            if stat.f_bavail.saturating_mul(stat.f_frsize) < needed {
                continue;
            }
            let (root, object, mut file) = match self.create_object(&name, id) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut input = body.root.open_object(&body.copy.object)?;
            if let Err(e) = io::copy(&mut input, &mut file).and_then(|_| file.sync_all()) {
                let _ = root.remove_object(&object);
                return Err(e.into());
            }
            Self::sync_object(&root, &object, &file)?;
            let mut r = self.record(id)?;
            let old = body.copy.clone();
            let copy = Copy {
                node: id.into(),
                target: name.clone(),
                object,
                version: r.node.version,
                hash: None,
            };
            r.primary = Some(name.clone());
            self.atomic(
                vec![
                    Self::encoded(key("node", id), &r)?,
                    Self::encoded(format!("copy/{id}/{name}"), &copy)?,
                    Self::encoded(format!("garbage/{}/{}", old.target, Uuid::new_v4()), &old)?,
                ],
                vec![format!("copy/{id}/{}", old.target)],
                true,
            )?;
            state.body = Some(Body { file, root, copy });
            return Ok(());
        }
        Err(failure(libc::ENOSPC))
    }
}
