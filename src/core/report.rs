use super::*;
impl Engine {
    fn health(&self) -> Result<Vec<serde_json::Value>> {
        let mut result = vec![];
        let files=self.db.prepare("WITH RECURSIVE tree(id,path) AS (VALUES('root','') UNION ALL SELECT n.id,path||'/'||name FROM nodes n JOIN tree ON parent=tree.id) SELECT n.id,path,mtime,hash,EXISTS(SELECT 1 FROM pending_writes WHERE node=n.id) FROM nodes n JOIN tree USING(id) WHERE dir=0")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u64>(2)?,r.get::<_,String>(3)?,r.get::<_,bool>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, path, mtime, hash, pending) in files {
            let policy = self.policy(&id)?;
            let (targets, count) = policy.desired();
            let required = crate::cloud::retained(&self.db, &id, count)?;
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
                if self.target(&target).is_ok_and(|r| r.join(key).is_file()) {
                    if targets.contains(&target) {
                        long += 1
                    }
                    domains.insert(domain);
                    locations.push(target);
                }
            }
            let status = if pending {
                "unconfirmed"
            } else if long >= required {
                "healthy"
            } else if now()?.saturating_sub(mtime) < policy.delay_secs {
                "waiting"
            } else if policy.manual_repair {
                "needs_attention"
            } else {
                "under_replicated"
            };
            result.push(serde_json::json!({"path":path,"status":status,"current":long,"required":required,"readable_local":locations,"readable_domains":domains.len(),"repair":if policy.manual_repair{"manual"}else{"auto"}}));
        }
        Ok(result)
    }
    pub fn state(&self) -> Result<serde_json::Value> {
        let mut state = self.snapshot()?;
        state["archive_errors"] = crate::cloud::errors(&self.db)?;
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
                    .and_then(|r| Ok(fs::metadata(r.join(key))?.len()))
                    .ok();
                let value = sizes.entry(target).or_insert(Some(0u64));
                *value = value.zip(bytes).map(|(sum, size)| sum + size);
            }
            row["logical_bytes"] = logical.into();
            row["target_bytes"] = serde_json::to_value(sizes)?;
        }
        state["backup_status"]=self.db.prepare("SELECT path,seq,time,error FROM backup_status")?.query_map([],|r|Ok(serde_json::json!({"path":r.get::<_,String>(0)?,"seq":r.get::<_,u64>(1)?,"time":r.get::<_,u64>(2)?,"error":r.get::<_,Option<String>>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?.into();
        for row in state["targets"].as_array_mut().context("invalid targets")? {
            use std::os::unix::fs::MetadataExt;
            row["device"] = row["path"]
                .as_str()
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.dev())
                .into();
            row["online"] = serde_json::Value::Bool(
                row["name"].as_str().is_some_and(|n| self.target(n).is_ok()),
            );
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
