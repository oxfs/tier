use super::*;
use std::time::Duration;
fn unix_timestamp(seconds: i64, nanos: u32) -> SystemTime {
    if seconds < 0 {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
            + Duration::from_nanos(nanos.into())
    } else {
        UNIX_EPOCH + Duration::new(seconds as u64, nanos)
    }
}
fn timestamp_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(error) => {
            let d = error.duration();
            let nanos = d.subsec_nanos();
            if nanos == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - nanos)
            }
        }
    }
}
impl Engine {
    pub fn stat(&self, path: &str) -> Result<Node> {
        const SELECT: &str = "SELECT id,dir,size,coalesce(display_mtime,mtime),coalesce(display_mtime_nsec,0),display_atime,coalesce(display_atime_nsec,0) FROM nodes";
        fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
            let mtime = unix_timestamp(row.get(3)?, row.get(4)?);
            let nanos = row.get(6)?;
            let atime = row
                .get::<_, Option<i64>>(5)?
                .map(|s| unix_timestamp(s, nanos))
                .unwrap_or(mtime);
            Ok(Node {
                id: row.get(0)?,
                is_dir: row.get(1)?,
                size: row.get(2)?,
                mtime,
                atime,
            })
        }
        if path == "/" {
            return Ok(self
                .db
                .query_row(&format!("{SELECT} WHERE id='root'"), [], decode)?);
        }
        let (parent, name) = split(path)?;
        let parent = self.stat(parent)?;
        self.db
            .query_row(
                &format!("{SELECT} WHERE parent=? AND name=? COLLATE tier_name"),
                params![parent.id, name],
                decode,
            )
            .with_context(|| format!("lookup {path}"))
    }
    pub fn list(&self, path: &str) -> Result<Vec<String>> {
        let node = self.stat(path)?;
        ensure!(node.is_dir, "not a directory");
        let mut st = self
            .db
            .prepare("SELECT name FROM nodes WHERE parent=? ORDER BY name COLLATE tier_name")?;
        let items = st
            .query_map([node.id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(items)
    }
    pub(crate) fn set_times(
        &self,
        id: &str,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> Result<()> {
        let a = atime.map(timestamp_parts);
        let m = mtime.map(timestamp_parts);
        self.db.execute("UPDATE nodes SET display_atime=coalesce(?1,display_atime),display_atime_nsec=coalesce(?2,display_atime_nsec),display_mtime=coalesce(?3,display_mtime),display_mtime_nsec=coalesce(?4,display_mtime_nsec) WHERE id=?5",
            params![a.map(|t|t.0),a.map(|t|t.1),m.map(|t|t.0),m.map(|t|t.1),id])?;
        Ok(())
    }
    pub fn inherit_policy(&self, path: &str, archive: bool) -> Result<()> {
        let directory = self.stat(path)?;
        ensure!(directory.is_dir, "policy requires a directory");
        let sql = if archive {
            "DELETE FROM cloud_policy WHERE node=?"
        } else {
            ensure!(
                directory.id != "root",
                "the root directory requires a local policy"
            );
            "DELETE FROM policies WHERE node=?"
        };
        self.db.execute(sql, [directory.id])?;
        Ok(())
    }
    pub fn mkdir(&self, path: &str) -> Result<()> {
        let (parent, name) = split(path)?;
        let p = self.stat(parent)?;
        ensure!(p.is_dir, "not a directory");
        self.db.execute(
            "INSERT INTO nodes(id,parent,name,dir,mtime) VALUES(?,?,?,1,?)",
            params![Uuid::new_v4().to_string(), p.id, name, now()?],
        )?;
        Ok(())
    }
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let n = self.stat(from)?;
        ensure!(n.id != "root", "root cannot move");
        let (p, name) = split(to)?;
        let p = self.stat(p)?;
        ensure!(p.is_dir, "not a directory");
        let sql = "WITH RECURSIVE tree(id) AS (VALUES(?)
            UNION ALL
            SELECT n.id
            FROM nodes n
            JOIN tree ON n.parent=tree.id)
            SELECT EXISTS(
            SELECT 1
            FROM tree
            WHERE id=?)";
        let cycle: bool = self.db.query_row(sql, params![n.id, p.id], |r| r.get(0))?;
        ensure!(!cycle, "directory cycle");
        let tx = self.write_transaction()?;
        if let Ok(dst) = self.stat(to)
            && dst.id != n.id
        {
            ensure!(dst.is_dir == n.is_dir, "type mismatch");
            if dst.is_dir {
                ensure!(self.list(to)?.is_empty(), "directory not empty")
            }
            self.detach(&dst.id, to)?;
        }
        tx.execute(
            "UPDATE nodes SET parent=?,name=? WHERE id=?",
            params![p.id, name, n.id],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn detach(&self, id: &str, path: &str) -> Result<()> {
        let ttl = self.policy(id)?.trash_ttl_secs;
        let time = now()?;
        self.db
            .execute("UPDATE nodes SET parent=NULL WHERE id=?", [id])?;
        self.db.execute(
            "INSERT INTO trash(node,original,deleted,expires) VALUES(?,?,?,?)",
            params![id, path, time, time.saturating_add(ttl)],
        )?;
        Ok(())
    }
    pub fn remove(&self, path: &str) -> Result<()> {
        let n = self.stat(path)?;
        ensure!(n.id != "root", "cannot remove root");
        let tx = self.write_transaction()?;
        self.detach(&n.id, path)?;
        tx.commit()?;
        Ok(())
    }
    pub fn restore(&self, id: i64, path: &str) -> Result<()> {
        let (p, name) = split(path)?;
        let p = self.stat(p)?;
        ensure!(p.is_dir, "not directory");
        let tx = self.write_transaction()?;
        ensure!(
            tx.execute(
                "UPDATE nodes SET parent=?,name=? WHERE id=(SELECT node FROM trash WHERE id=?)",
                params![p.id, name, id]
            )? == 1,
            "unknown trash entry"
        );
        tx.execute("DELETE FROM trash WHERE id=?", [id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn purge(&self, id: i64) -> Result<()> {
        let tx = self.write_transaction()?;
        let node: String = tx.query_row("SELECT node FROM trash WHERE id=?", [id], |r| r.get(0))?;
        let tree = "WITH RECURSIVE subtree(id) AS (VALUES(?) UNION ALL SELECT nodes.id FROM nodes JOIN subtree ON parent=subtree.id)";
        let mut objects = vec![];
        {
            let mut st = tx.prepare(&format!(
                "{tree} SELECT target,object FROM copies WHERE node IN subtree"
            ))?;
            for row in st.query_map([&node], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })? {
                objects.push(row?)
            }
        }
        for table in [
            "copies",
            "policies",
            "pending_writes",
            "protection",
            "cloud_policy",
            "cloud_copies",
        ] {
            tx.execute(
                &format!("{tree} DELETE FROM {table} WHERE node IN subtree"),
                [&node],
            )?;
        }
        tx.execute("DELETE FROM trash WHERE id=?", [id])?;
        tx.execute(
            &format!("{tree} DELETE FROM nodes WHERE id IN subtree"),
            [&node],
        )?;
        tx.commit()?;
        for (name, key) in objects {
            if let Ok(root) = self.target(&name) {
                let _ = root.remove_object(&key);
            }
        }
        Ok(())
    }
}
