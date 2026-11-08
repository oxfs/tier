use super::*;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::sync::{LazyLock, Mutex};

static WRITERS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
pub(crate) struct Writer(String);
impl Writer {
    pub(crate) fn new(id: String) -> Self {
        WRITERS.lock().unwrap().insert(id.clone());
        Self(id)
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        WRITERS.lock().unwrap().remove(&self.0);
    }
}

static CANCELLED: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Mutex::default);
pub(super) fn cancelled(id: &str) -> Result<()> {
    let time = now()?;
    let mut rows = CANCELLED.lock().unwrap();
    rows.retain(|_, at| time.saturating_sub(*at) < 300);
    rows.insert(id.to_owned(), time);
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Signature(u64, u64, i64, i64, u64);
type VerificationCache = HashMap<(PathBuf, String), (Signature, bool, std::time::Instant)>;
static VERIFIED: LazyLock<Mutex<VerificationCache>> = LazyLock::new(Mutex::default);
impl Engine {
    pub(crate) fn verified_copy(&self, target: &str, key: &str, expected: &str) -> bool {
        self.verify(target, key, expected, true)
    }
    pub(super) fn cached_verified(&self, target: &str, key: &str, expected: &str) -> bool {
        self.verify(target, key, expected, false)
    }
    fn verify(&self, target: &str, key: &str, expected: &str, allow_hash: bool) -> bool {
        let Ok(root) = self.target(target) else {
            return false;
        };
        let path = root.cache_key(key);
        let Ok(mut file) = root.open_object(key) else {
            return false;
        };
        let stamp =
            |m: &fs::Metadata| Signature(m.dev(), m.ino(), m.ctime(), m.ctime_nsec(), m.len());
        let Ok(meta) = file.metadata() else {
            return false;
        };
        let signature = stamp(&meta);
        let entry = (path, expected.to_owned());
        if let Some((old, valid, checked)) = VERIFIED.lock().unwrap().get(&entry)
            && *old == signature
            && checked.elapsed().as_secs() < 3600
        {
            return *valid;
        }
        if !allow_hash {
            return false;
        }
        let valid = fingerprint(&mut file).is_ok_and(|(_, h)| h == expected);
        if !file.metadata().is_ok_and(|m| stamp(&m) == signature) {
            return false;
        }
        let mut cache = VERIFIED.lock().unwrap();
        if cache.len() >= 65536 {
            cache.clear()
        }
        cache.insert(entry, (signature, valid, std::time::Instant::now()));
        valid
    }
}
impl Engine {
    fn health(&self) -> Result<Vec<serde_json::Value>> {
        let mut result = vec![];
        let sql = "WITH RECURSIVE tree(id, path) AS (VALUES('root', '')
            UNION ALL
            SELECT n.id, path||'/'||name
            FROM nodes n
            JOIN tree ON parent=tree.id)
            SELECT n.id, path, mtime, hash, EXISTS(
            SELECT 1
            FROM pending_writes
            WHERE node=n.id)
            FROM nodes n
            JOIN tree USING(id)
            WHERE dir=0";
        let files = self
            .db
            .prepare(sql)?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, path, mtime, hash, pending) in files {
            let policy = self.policy(&id)?;
            let (targets, count) = policy.desired();
            let required = crate::archive::retained(&self.db, &id, count)?;
            let mut locations = vec![];
            let mut domains = HashSet::new();
            let mut long = 0;
            let rows = self
                .db
                .prepare("SELECT target,object,domain FROM copies JOIN targets ON targets.name=copies.target WHERE node=? AND hash=?")?
                .query_map(params![id, hash], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,r.get::<_,String>(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (target, key, domain) in rows {
                if self.verified_copy(&target, &key, &hash) {
                    if targets.contains(&target) {
                        long += 1
                    }
                    domains.insert(domain);
                    locations.push(target);
                }
            }
            let status = if pending {
                if WRITERS.lock().unwrap().contains(&id) {
                    "writing"
                } else {
                    "unconfirmed"
                }
            } else if required == 0 && locations.is_empty() {
                "remote_only"
            } else if long >= required {
                "healthy"
            } else if now()?.saturating_sub(mtime) < policy.delay_secs {
                "waiting"
            } else if policy.manual_repair {
                "needs_attention"
            } else {
                "under_replicated"
            };
            let cancelled = CANCELLED
                .lock()
                .unwrap()
                .get(&id)
                .copied()
                .filter(|at| now().is_ok_and(|t| t.saturating_sub(*at) < 300));
            result.push(serde_json::json!({
                "path": path,
                "status": status,
                "current": long,
                "required": required,
                "readable_local": locations,
                "readable_domains": domains.len(),
                "last_copy_cancelled": cancelled,
                "repair": if policy.manual_repair { "manual" } else { "auto" },
            }));
        }
        Ok(result)
    }
    fn status_catalog(&self) -> Result<serde_json::Value> {
        // Read the UI's catalog in one short transaction. Device probes and
        // content verification happen after this transaction has closed.
        let tx = self.db.unchecked_transaction()?;
        let seq: u64 = tx.query_row("SELECT seq FROM revision WHERE id=1", [], |r| r.get(0))?;
        let rows = |sql: &str| -> Result<Vec<serde_json::Value>> {
            let mut statement = tx.prepare(sql)?;
            let mut cursor = statement.query([])?;
            let mut values = Vec::new();
            while let Some(row) = cursor.next()? {
                values.push(serde_json::from_str(row.get_ref(0)?.as_str()?)?);
            }
            Ok(values)
        };
        let targets = rows(
            "SELECT json_object('name',name,'path',path,'identity',identity,'domain',domain)
            FROM targets ORDER BY name",
        )?;
        let trash=rows("SELECT json_object('id',id,'node',node,'original',original,'deleted',deleted,'expires',expires)
            FROM trash ORDER BY id")?;
        let cloud_targets = rows(
            "SELECT json_object('name',t.name,'url',t.url,'object_count',count(c.key))
            FROM cloud_targets t LEFT JOIN cloud_copies c ON c.target=t.name
            GROUP BY t.name,t.url ORDER BY t.name",
        )?;
        let backup_schedule =
            rows("SELECT json_object('config',json(config)) FROM backup_schedule")?;
        let mut state = serde_json::json!({
            "seq":seq,"targets":targets,"trash":trash,"cloud_targets":cloud_targets,
            "backup_schedule":backup_schedule,"policies":[],"cloud_policy":[],"archive_errors":[]
        });
        {
            let sql = "WITH RECURSIVE tree(id,path) AS (
                VALUES('root','/')
                UNION ALL SELECT n.id,rtrim(t.path,'/')||'/'||n.name
                FROM nodes n JOIN tree t ON n.parent=t.id
            ), entries(kind,node,settings) AS (
                SELECT 'policies',node,policy FROM policies
                UNION ALL SELECT 'cloud_policy',node,settings FROM cloud_policy
                UNION ALL SELECT 'archive_errors',node,error FROM archive_errors
            )
            SELECT kind,node,path,settings FROM entries JOIN tree ON tree.id=entries.node
            ORDER BY kind,path";
            let mut query = tx.prepare(sql)?;
            let mut cursor = query.query([])?;
            while let Some(row) = cursor.next()? {
                let kind: &str = row.get_ref(0)?.as_str()?;
                let raw: &str = row.get_ref(3)?.as_str()?;
                let mut record = serde_json::json!({"node":row.get::<_,String>(1)?,"path":row.get::<_,String>(2)?});
                match kind {
                    "policies" => record["policy"] = serde_json::from_str(raw)?,
                    "cloud_policy" => record["settings"] = serde_json::from_str(raw)?,
                    "archive_errors" => record["error"] = raw.into(),
                    _ => unreachable!(),
                }
                state[kind].as_array_mut().unwrap().push(record);
            }
        }
        tx.commit()?;
        Ok(state)
    }

    pub fn state(&self) -> Result<serde_json::Value> {
        let mut state = self.status_catalog()?;
        state["health"] = self.health()?.into();
        for row in state["trash"].as_array_mut().context("invalid trash")? {
            let node = row["node"].as_str().context("invalid trash object")?;
            let subtree = "WITH RECURSIVE tree(id) AS (VALUES(?) UNION ALL SELECT n.id FROM nodes n JOIN tree ON n.parent=tree.id)";
            let logical: u64 = self.db.query_row(
                &format!("{subtree} SELECT coalesce(sum(size),0) FROM nodes JOIN tree USING(id)"),
                [node],
                |r| r.get(0),
            )?;
            let copies = self
                .db
                .prepare(&format!(
                    "{subtree} SELECT target,object FROM copies WHERE node IN tree"
                ))?
                .query_map([node], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut sizes = std::collections::BTreeMap::new();
            for (target, key) in copies {
                let bytes = self
                    .target(&target)
                    .and_then(|r| Ok(r.open_object(&key)?.metadata()?.len()))
                    .ok();
                let value = sizes.entry(target).or_insert(Some(0u64));
                *value = value.zip(bytes).map(|(sum, size)| sum + size);
            }
            row["logical_bytes"] = logical.into();
            row["target_bytes"] = serde_json::to_value(sizes)?;
        }
        let sql = "SELECT path, seq, time, error
            FROM backup_status";
        let statuses = self
            .db
            .prepare(sql)?
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "path": row.get::<_, String>(0)?,
                    "seq": row.get::<_, u64>(1)?,
                    "time": row.get::<_, u64>(2)?,
                    "error": row.get::<_, Option<String>>(3)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        state["backup_status"] = statuses.into();
        let seq = state["seq"].as_u64().context("invalid snapshot sequence")?;
        if let Some(rows) = state["backup_status"].as_array_mut() {
            for row in rows {
                row["lag"] = seq.saturating_sub(row["seq"].as_u64().unwrap_or(0)).into();
            }
        }
        for row in state["targets"].as_array_mut().context("invalid targets")? {
            use std::os::unix::fs::MetadataExt;
            let stats = row["name"].as_str().and_then(|n| self.target_stat(n).ok());
            row["device"] = row["path"]
                .as_str()
                .filter(|_| stats.is_some())
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.dev())
                .into();
            row["online"] = stats.is_some().into();
            row["total_bytes"] = stats
                .as_ref()
                .map(|v| v.f_blocks.saturating_mul(v.f_frsize))
                .into();
            row["available_bytes"] = stats
                .as_ref()
                .map(|v| v.f_bavail.saturating_mul(v.f_frsize))
                .into();
        }
        Ok(state)
    }
    pub fn snapshot(&self) -> Result<serde_json::Value> {
        let tx = self.db.unchecked_transaction()?;
        let seq: u64 = tx.query_row("SELECT seq FROM revision WHERE id=1", [], |r| r.get(0))?;
        let system_id: String =
            tx.query_row("SELECT uuid FROM system_identity WHERE id=1", [], |r| {
                r.get(0)
            })?;
        let mut out = serde_json::json!({"format":1,"seq":seq,"system_id":system_id});
        for table in [
            "nodes",
            "copies",
            "targets",
            "policies",
            "trash",
            "protection",
            "pending_writes",
            "backup_schedule",
            "cloud_targets",
            "cloud_policy",
            "cloud_copies",
        ] {
            let mut st = tx.prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
            let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
            let mut rows = st.query([])?;
            let mut data = vec![];
            while let Some(r) = rows.next()? {
                let mut obj = serde_json::Map::new();
                for (i, n) in names.iter().enumerate() {
                    use rusqlite::types::ValueRef;
                    obj.insert(
                        n.clone(),
                        match r.get_ref(i)? {
                            ValueRef::Null => serde_json::Value::Null,
                            ValueRef::Integer(v) => v.into(),
                            ValueRef::Text(v) => std::str::from_utf8(v)?.into(),
                            _ => bail!("unsupported snapshot field"),
                        },
                    );
                }
                data.push(obj)
            }
            out[table] = serde_json::to_value(data)?;
        }
        tx.commit()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_uses_resolved_paths_without_recovery_internals() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let disk = dir.path().join("disk");
        fs::create_dir(&disk)?;
        let engine = Engine::open(&dir.path().join("db"))?;
        engine.add_target("disk", &disk)?;
        engine.mkdir("/家庭")?;
        engine.set_policy(
            "/家庭",
            &Policy {
                front: vec!["disk".into()],
                ..Policy::default()
            },
        )?;
        engine.add_archive(
            "remote",
            "http://localhost/webdav",
            Some("DAV_USER"),
            Some("DAV_SECRET"),
        )?;
        let state = engine.state()?;
        assert_eq!(state["policies"][0]["path"], "/家庭");
        assert_eq!(state["policies"][0]["policy"]["front"][0], "disk");
        assert_eq!(state["cloud_targets"][0]["object_count"], 0);
        assert!(state["cloud_targets"][0].get("pass_env").is_none());
        for table in [
            "nodes",
            "copies",
            "cloud_copies",
            "pending_writes",
            "protection",
        ] {
            assert!(state.get(table).is_none());
            assert!(engine.snapshot()?.get(table).is_some());
        }
        Ok(())
    }
    #[test]
    fn active_writer_and_checksum_failures_are_visible() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("disk");
        fs::create_dir(&root)?;
        let engine = Engine::open(&tmp.path().join("db"))?;
        engine.add_target("disk", &root)?;
        engine.set_policy(
            "/",
            &Policy {
                front: vec!["disk".into()],
                long: vec!["disk".into()],
                long_count: 1,
                ..Default::default()
            },
        )?;
        engine.put("/doc", b"good")?;
        let id = engine.stat("/doc")?.id;
        let state =
            || -> Result<String> { Ok(engine.health()?[0]["status"].as_str().unwrap().into()) };
        assert_eq!(state()?, "healthy");
        let key: String =
            engine
                .db
                .query_row("SELECT object FROM copies WHERE node=?", [&id], |r| {
                    r.get(0)
                })?;
        fs::write(root.join(key), b"evil")?;
        assert_eq!(state()?, "under_replicated");
        assert!(engine.automatic_reconcile().is_err());
        engine.begin_write("/doc")?;
        let handle = Writer::new(id);
        assert_eq!(state()?, "writing");
        drop(handle);
        assert_eq!(state()?, "unconfirmed");
        Ok(())
    }
}
