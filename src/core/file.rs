use super::*;
impl Engine {
    pub fn begin_write(&self, path: &str) -> Result<()> {
        let n = self.stat(path)?;
        ensure!(!n.is_dir, "is directory");
        self.db
            .execute("INSERT OR IGNORE INTO pending_writes VALUES(?)", [n.id])?;
        Ok(())
    }
    pub fn put(&self, path: &str, data: &[u8]) -> Result<()> {
        self.put_reader(path, &mut io::Cursor::new(data))
    }
    pub fn put_from(&self, path: &str, input: &mut fs::File) -> Result<()> {
        self.put_reader(path, input)
    }
    fn put_reader<R: Read + Seek>(&self, path: &str, data: &mut R) -> Result<()> {
        let (parent, name) = split(path)?;
        let parent = self.stat(parent)?;
        ensure!(parent.is_dir, "not a directory");
        let existing = self
            .db
            .query_row(
                "SELECT id,dir FROM nodes WHERE parent=? AND name=? COLLATE tier_name",
                params![parent.id, name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .optional()?;
        let was_present = existing.is_some();
        let id = match existing {
            Some((_, true)) => bail!("is directory"),
            Some((id, false)) => id,
            None => Uuid::new_v4().to_string(),
        };
        let mut p = self.policy(&parent.id)?;
        rank(&mut p.write_cache, &id);
        rank(&mut p.persistent_storage, &id);
        if was_present {
            self.db
                .execute("INSERT OR IGNORE INTO pending_writes VALUES(?)", [&id])?;
        }
        let (size, hash) = fingerprint(data)?;
        let confirmations = |targets: &[String],
                             required: usize,
                             watermark: Option<u8>|
         -> Result<Vec<(String, String)>> {
            let mut copies = Vec::new();
            let mut failures = Vec::new();
            let mut full = false;
            let mut candidates = self.candidates(targets)?;
            while let Some((t, domain)) = candidates.next() {
                let result = (|| {
                    if let Some(threshold) = watermark {
                        self.relieve_pressure(t, threshold, Some(size))?;
                    }
                    self.store(t, &id, data)
                })();
                match result {
                    Ok(key) => {
                        candidates.confirmed(domain);
                        copies.push((t.clone(), key));
                        if copies.len() == required {
                            return Ok(copies);
                        }
                    }
                    Err(error) => {
                        full |= error
                            .downcast_ref::<io::Error>()
                            .and_then(io::Error::raw_os_error)
                            == Some(libc::ENOSPC);
                        failures.push(format!("{t}: {error:#}"));
                    }
                }
            }
            self.discard_objects(&copies);
            let message = format!(
                "only {} of {} confirmation targets available: {}",
                copies.len(),
                required,
                failures.join("; ")
            );
            if full {
                return Err(io::Error::from_raw_os_error(libc::ENOSPC)).context(message);
            }
            bail!(message)
        };
        let mut confirmations = confirmations;
        let copies = if p.write_cache.is_empty() {
            ensure!(p.persistent_copies > 0, "no persistent storage configured");
            confirmations(&p.persistent_storage, p.persistent_copies, None)?
        } else {
            match confirmations(&p.write_cache, p.cache_copies, Some(p.high_water)) {
                Err(_) if p.direct_fallback => confirmations(
                    &p.persistent_storage,
                    p.fallback_count.unwrap_or(p.persistent_copies),
                    None,
                )?,
                result => result?,
            }
        };
        let publish = (|| -> Result<()> {
            let tx = self.write_transaction()?;
            let mut latest = self.policy(&parent.id)?;
            rank(&mut latest.write_cache, &id);
            rank(&mut latest.persistent_storage, &id);
            ensure!(latest == p, "policy changed during write");
            let current = self.stat(path).ok();
            ensure!(
                current.as_ref().map(|n| n.id.as_str()) == was_present.then_some(id.as_str()),
                "path changed during write"
            );
            let sql = "INSERT INTO nodes(id, parent, name, dir, size, hash, mtime)
                VALUES(?, ?, ?, 0, ?, ?, ?)
                ON CONFLICT(id) DO UPDATE SET size=excluded.size, hash=excluded.hash, mtime=excluded.mtime, display_mtime=NULL";
            tx.execute(sql, params![id, parent.id, name, size, hash, now()?])?;
            for (t, key) in &copies {
                tx.execute(
                    "INSERT OR REPLACE INTO copies VALUES(?,?,?,?)",
                    params![id, t, key, hash],
                )?;
            }
            tx.execute("DELETE FROM pending_writes WHERE node=?", [&id])?;
            tx.commit()?;
            Ok(())
        })();
        if publish.is_err() {
            self.discard_objects(&copies);
        }
        publish
    }
    fn discard_objects(&self, objects: &[(String, String)]) {
        for (target, key) in objects {
            if let Ok(root) = self.target(target) {
                let _ = root.remove_object(key);
            }
        }
    }
    pub fn copy_to(&self, path: &str, out: &mut fs::File) -> Result<()> {
        let n = self.stat(path)?;
        ensure!(!n.is_dir, "is directory");
        self.copy_id(&n.id, out).map(|_| ())
    }
    pub(crate) fn stage_copy(&self, id: &str, out: &mut fs::File) -> Result<String> {
        // Local staging already read back and verified the destination. WebDAV
        // verified the incoming stream, so read back the staged file before reuse.
        match self.copy_id(id, out)? {
            Some(hash) => Ok(hash),
            None => Ok(fingerprint(out)?.1),
        }
    }
    fn copy_id(&self, id: &str, out: &mut fs::File) -> Result<Option<String>> {
        let mut st=self.db.prepare("SELECT target,object,c.hash FROM copies c JOIN nodes n ON n.id=c.node AND n.hash=c.hash WHERE node=?")?;
        let rows = st.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (t, key, h) = row?;
            let Ok(mut source) = self.target(&t).and_then(|p| Ok(p.open_object(&key)?)) else {
                continue;
            };
            out.set_len(0)?;
            out.rewind()?;
            if io::copy(&mut source, out).is_ok() && fingerprint(out)?.1 == h {
                out.rewind()?;
                return Ok(Some(h));
            }
        }
        crate::archive::recall_to(&self.db, id, out)?;
        Ok(None)
    }
    pub fn get(&self, path: &str) -> Result<Vec<u8>> {
        let n = self.stat(path)?;
        ensure!(!n.is_dir, "is directory");
        self.read_id(&n.id)
    }
    pub(crate) fn read_id(&self, id: &str) -> Result<Vec<u8>> {
        let mut file = tempfile::tempfile()?;
        self.copy_id(id, &mut file)?;
        file.rewind()?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(data)
    }
}
