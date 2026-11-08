use super::*;
impl Engine {
    pub fn stat(&self, path: &str) -> Result<Node> {
        if path == "/" {
            return Ok(Node {
                id: "root".into(),
                is_dir: true,
                size: 0,
                mtime: 0,
            });
        }
        let (parent, name) = split(path)?;
        let parent = self.stat(parent)?;
        self.db
            .query_row(
                "SELECT id,dir,size,coalesce(display_mtime,mtime) FROM nodes WHERE parent=? AND name=?",
                params![parent.id, name],
                |r| {
                    Ok(Node {
                        id: r.get(0)?,
                        is_dir: r.get(1)?,
                        size: r.get(2)?,mtime:r.get(3)?,
                    })
                },
            )
            .with_context(|| format!("lookup {path}"))
    }
    pub fn list(&self, path: &str) -> Result<Vec<String>> {
        let node = self.stat(path)?;
        ensure!(node.is_dir, "not a directory");
        let mut st = self
            .db
            .prepare("SELECT name FROM nodes WHERE parent=? ORDER BY name")?;
        let items = st
            .query_map([node.id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(items)
    }
    pub fn set_mtime(&self, path: &str, seconds: u64) -> Result<()> {
        let node = self.stat(path)?;
        self.db.execute(
            "UPDATE nodes SET display_mtime=? WHERE id=?",
            params![seconds, node.id],
        )?;
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
        let tx = self.db.unchecked_transaction()?;
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
        let tx = self.db.unchecked_transaction()?;
        self.detach(&n.id, path)?;
        tx.commit()?;
        Ok(())
    }
    pub fn restore(&self, id: i64, path: &str) -> Result<()> {
        let (p, name) = split(path)?;
        let p = self.stat(p)?;
        ensure!(p.is_dir, "not directory");
        let tx = self.db.unchecked_transaction()?;
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
        let tx = self.db.unchecked_transaction()?;
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
                let _ = fs::remove_file(root.join(key));
            }
        }
        Ok(())
    }
}
