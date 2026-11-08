use super::*;
impl Engine {
    pub fn reconcile(&self) -> Result<()> {
        self.run_reconcile(true)
    }
    pub fn automatic_reconcile(&self) -> Result<()> {
        self.run_reconcile(false)
    }
    fn drain_garbage(&self) -> Result<()> {
        let sql = "SELECT target, object
            FROM garbage
            WHERE NOT EXISTS(
            SELECT 1
            FROM copies
            WHERE copies.target=garbage.target AND copies.object=garbage.object)";
        let rows = self
            .db
            .prepare(sql)?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (target, key) in rows {
            if let Ok(root) = self.target(&target) {
                match fs::remove_file(root.join(&key)) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(_) => continue,
                }
                self.db.execute(
                    "DELETE FROM garbage WHERE target=? AND object=?",
                    params![target, key],
                )?;
            }
        }
        Ok(())
    }
    fn run_reconcile(&self, manual: bool) -> Result<()> {
        self.drain_garbage()?;
        let expired = self
            .db
            .prepare("SELECT id FROM trash WHERE expires<=?")?
            .query_map([now()?], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in expired {
            self.purge(id)?;
        }
        let sql = "WITH RECURSIVE live(id) AS (VALUES('root')
            UNION ALL
            SELECT n.id
            FROM nodes n
            JOIN live ON n.parent=live.id)
            SELECT id, mtime, hash
            FROM nodes
            JOIN live USING(id)
            WHERE dir=0
            AND NOT EXISTS(
            SELECT 1
            FROM pending_writes w
            WHERE w.node=nodes.id)";
        let mut nodes = self.db.prepare(sql)?;
        let mut errors = vec![];
        let pending = nodes
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(nodes);
        let mut cache_pools = std::collections::BTreeMap::<String, u8>::new();
        for (id, mtime, h) in pending {
            let p = self.policy(&id)?;
            let (targets, count, placement) = placement(&p, &id, &h)?;
            let tracking = self
                .db
                .query_row(
                    "SELECT incident FROM protection WHERE node=? AND hash=?",
                    params![id, placement],
                    |r| r.get::<_, Option<u64>>(0),
                )
                .optional()?;
            let protected = tracking.is_some();
            if !protected && now()?.saturating_sub(mtime) < p.delay_secs {
                continue;
            }
            let needed = crate::cloud::retained(&self.db, &id, count)?;
            let mut good = 0;
            let mut chosen = HashSet::new();
            for t in &targets {
                if good >= needed {
                    break;
                }
                let existing: Option<String> = self
                    .db
                    .query_row(
                        "SELECT object FROM copies WHERE node=? AND target=? AND hash=?",
                        params![id, t, h],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(key) = existing
                    && (if manual {
                        self.matches_copy(t, &key, &h)
                    } else {
                        self.verified_copy(t, &key, &h)
                    })
                {
                    chosen.insert(t.clone());
                    good += 1;
                    continue;
                }
                if protected && !manual {
                    let first = tracking.flatten().unwrap_or(now()?);
                    self.db.execute(
                        "UPDATE protection SET incident=? WHERE node=? AND incident IS NULL",
                        params![first, id],
                    )?;
                    if p.manual_repair || now()?.saturating_sub(first) < p.repair_grace_secs {
                        continue;
                    }
                }
                let result = (|| -> Result<String> {
                    let mut tmp = tempfile::tempfile()?;
                    self.copy_id(&id, &mut tmp)?;
                    ensure!(fingerprint(&mut tmp)?.1 == h, "source changed");
                    self.store(t, &id, &mut tmp)
                })();
                if let Ok(key) = result {
                    let tx = self.db.unchecked_transaction()?;
                    let current: String =
                        tx.query_row("SELECT hash FROM nodes WHERE id=?", [&id], |r| r.get(0))?;
                    let sql = "WITH RECURSIVE ancestors(id, parent) AS (
                        SELECT id, parent
                        FROM nodes
                        WHERE id=?
                        UNION ALL
                        SELECT n.id, n.parent
                        FROM nodes n
                        JOIN ancestors a ON n.id=a.parent)
                        SELECT EXISTS(
                        SELECT 1
                        FROM ancestors
                        WHERE id='root')
                        AND NOT EXISTS(
                        SELECT 1
                        FROM pending_writes
                        WHERE node=?)";
                    let eligible: bool = tx.query_row(sql, params![id, id], |r| r.get(0))?;
                    if eligible && current == h && self.policy(&id)?.desired().0.contains(t) {
                        let previous: Option<String> = tx
                            .query_row(
                                "SELECT object FROM copies WHERE node=? AND target=?",
                                params![id, t],
                                |r| r.get(0),
                            )
                            .optional()?;
                        tx.execute(
                            "INSERT OR REPLACE INTO copies VALUES(?,?,?,?)",
                            params![id, t, key, h],
                        )?;
                        tx.commit()?;
                        if let Some(previous) = previous
                            && let Ok(root) = self.target(t)
                        {
                            let _ = fs::remove_file(root.join(previous));
                        }
                        chosen.insert(t.clone());
                        good += 1;
                    } else if let Ok(root) = self.target(t) {
                        let _ = fs::remove_file(root.join(key));
                        super::report::cancelled(&id)?;
                    }
                }
            }
            if good < needed {
                errors.push(id)
            } else {
                self.db.execute(
                    "INSERT INTO protection VALUES(?,?,NULL) ON CONFLICT(node) DO UPDATE SET hash=excluded.hash,incident=NULL WHERE protection.hash!=excluded.hash OR protection.incident IS NOT NULL",
                    params![id, placement],
                )?;
                if needed > 0 && good >= needed {
                    self.trim_obsolete(&id, &h, &p, &chosen)?;
                }
                for t in &p.front {
                    cache_pools
                        .entry(t.clone())
                        .and_modify(|low| *low = (*low).min(p.low_water))
                        .or_insert(p.low_water);
                }
            }
        }
        for (target, low) in cache_pools {
            let _ = self.relieve_pressure(&target, low, None);
        }
        if !errors.is_empty() {
            return Err(anyhow!("under-replicated: {}", errors.join(", ")));
        }
        Ok(())
    }
    fn trim_obsolete(
        &self,
        id: &str,
        hash: &str,
        expected: &Policy,
        chosen: &HashSet<String>,
    ) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let policy = self.policy(id)?;
        if chosen.is_empty() || &policy != expected {
            return Ok(());
        }
        let sql = "SELECT hash=?
            AND NOT EXISTS(
            SELECT 1
            FROM pending_writes
            WHERE node=?)
            FROM nodes
            WHERE id=?";
        let confirmed: bool = tx.query_row(sql, params![hash, id, id], |r| r.get(0))?;
        if !confirmed {
            return Ok(());
        }
        let rows = tx
            .prepare("SELECT target,object FROM copies WHERE node=?")?
            .query_map([id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows
            .iter()
            .all(|(t, _)| chosen.contains(t) || policy.front.contains(t))
        {
            return Ok(());
        }
        for t in chosen {
            let key: Option<String> = tx
                .query_row(
                    "SELECT object FROM copies WHERE node=? AND target=? AND hash=?",
                    params![id, t, hash],
                    |r| r.get(0),
                )
                .optional()?;
            if !key.is_some_and(|k| self.matches_copy(t, &k, hash)) {
                return Ok(());
            }
        }
        let mut stale = vec![];
        for (t, key) in rows {
            if !chosen.contains(&t)
                && !policy.front.contains(&t)
                && let Ok(root) = self.target(&t)
            {
                tx.execute(
                    "DELETE FROM copies WHERE node=? AND target=? AND object=?",
                    params![id, t, key],
                )?;
                stale.push(root.join(key));
            }
        }
        tx.commit()?;
        for path in stale {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }
}
