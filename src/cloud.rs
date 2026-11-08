use crate::{archive::Archive, core::Engine};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, Seek},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn init(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS cloud_targets(name TEXT PRIMARY KEY,url TEXT NOT NULL,user_env TEXT,pass_env TEXT);
        CREATE TABLE IF NOT EXISTS cloud_policy(node TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,settings TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS cloud_copies(node TEXT REFERENCES nodes(id) ON DELETE CASCADE,target TEXT REFERENCES cloud_targets(name),key TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(node,target));
        CREATE TABLE IF NOT EXISTS cloud_gc(target TEXT NOT NULL,key TEXT NOT NULL,PRIMARY KEY(target,key));
        CREATE TABLE IF NOT EXISTS archive_errors(node TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,error TEXT NOT NULL);
        CREATE TRIGGER IF NOT EXISTS gc_cloud_copy AFTER DELETE ON cloud_copies BEGIN INSERT OR IGNORE INTO cloud_gc VALUES(OLD.target,OLD.key); END;")?;
    Ok(())
}

pub fn errors(db: &Connection) -> Result<serde_json::Value> {
    let rows = db
        .prepare("SELECT node,error FROM archive_errors ORDER BY node")?
        .query_map([], |r| {
            Ok(serde_json::json!({"node":r.get::<_,String>(0)?,"error":r.get::<_,String>(1)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CloudPolicy {
    pub targets: Vec<String>,
    pub copies: usize,
    pub delay_secs: u64,
    pub retain_local: usize,
}

fn policy(db: &Connection, id: &str) -> Result<Option<CloudPolicy>> {
    let sql = "WITH RECURSIVE a(id, parent, depth) AS (
        SELECT id, parent, 0
        FROM nodes
        WHERE id=?
        UNION ALL
        SELECT n.id, n.parent, a.depth+1
        FROM nodes n
        JOIN a ON n.id=a.parent)
        SELECT settings
        FROM cloud_policy
        JOIN a ON a.id=node
        ORDER BY depth LIMIT 1";
    let settings: Option<String> = db.query_row(sql, [id], |r| r.get(0)).optional()?;
    Ok(settings.map(|s| serde_json::from_str(&s)).transpose()?)
}

fn client(db: &Connection, target: &str) -> Result<Archive> {
    let (url, user, pass): (String, Option<String>, Option<String>) = db.query_row(
        "SELECT url,user_env,pass_env FROM cloud_targets WHERE name=?",
        [target],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    configured_client(&url, user.as_deref(), pass.as_deref())
}
pub(crate) fn configured_client(
    url: &str,
    user: Option<&str>,
    pass: Option<&str>,
) -> Result<Archive> {
    let credentials = user
        .zip(pass)
        .map(|(u, p)| {
            Ok::<_, anyhow::Error>((
                std::env::var(u).context("missing WebDAV username environment variable")?,
                std::env::var(p).context("missing WebDAV password environment variable")?,
            ))
        })
        .transpose()?;
    Archive::new(
        url,
        credentials.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
    )
}

fn copies(db: &Connection, id: &str) -> Result<Vec<(String, String, String)>> {
    let sql = "SELECT target, key, c.hash
        FROM cloud_copies c
        JOIN nodes n ON n.id=c.node AND n.hash=c.hash
        WHERE node=?";
    Ok(db
        .prepare(sql)?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn recall_to(db: &Connection, id: &str, out: &mut fs::File) -> Result<()> {
    let mut errors = Vec::new();
    for (target, key, hash) in copies(db, id)? {
        out.set_len(0)?;
        let size: i64 = db.query_row("SELECT size FROM nodes WHERE id=?", [id], |r| r.get(0))?;
        ensure!(size >= 0, "invalid file size");
        if size > 0 {
            use std::os::fd::AsRawFd;
            let error = unsafe { libc::posix_fallocate(out.as_raw_fd(), 0, size) };
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error))
                    .context("reserving space before archive recall");
            }
        }
        out.rewind()?;
        match client(db, &target).and_then(|a| a.read_to(&key, &hash, size as u64, out)) {
            Ok(()) => {
                out.rewind()?;
                return Ok(());
            }
            Err(error) => errors.push(format!("{target}: {error}")),
        }
    }
    bail!("no readable current copy for {id}: {}", errors.join(", "))
}

pub fn retained(db: &Connection, id: &str, default: usize) -> Result<usize> {
    let Some(p) = policy(db, id)? else {
        return Ok(default);
    };
    let count = copies(db, id)?
        .iter()
        .filter(|(t, _, _)| p.targets.contains(t))
        .count();
    Ok(if p.copies > 0 && count >= p.copies {
        p.retain_local
    } else {
        default
    })
}

impl Engine {
    pub fn add_archive(
        &self,
        name: &str,
        url: &str,
        user_env: Option<&str>,
        pass_env: Option<&str>,
    ) -> Result<()> {
        ensure!(
            user_env.is_some() == pass_env.is_some(),
            "supply both credential variable names"
        );
        Archive::new(url, None)?;
        self.db.execute(
            "INSERT INTO cloud_targets VALUES(?,?,?,?)",
            params![name, url, user_env, pass_env],
        )?;
        Ok(())
    }
    pub fn set_cloud_policy(&self, path: &str, p: &CloudPolicy) -> Result<()> {
        let n = self.stat(path)?;
        ensure!(
            n.is_dir
                && p.copies <= p.targets.len()
                && p.targets.iter().collect::<HashSet<_>>().len() == p.targets.len(),
            "invalid archive policy"
        );
        for t in &p.targets {
            self.db
                .query_row("SELECT 1 FROM cloud_targets WHERE name=?", [t], |_| Ok(()))?;
        }
        self.db.execute(
            "INSERT OR REPLACE INTO cloud_policy VALUES(?,?)",
            params![n.id, serde_json::to_string(p)?],
        )?;
        Ok(())
    }
    pub fn archive_tick(&self) -> Result<()> {
        let sql = "WITH RECURSIVE live(id) AS (VALUES('root')
            UNION ALL
            SELECT n.id
            FROM nodes n
            JOIN live ON n.parent=live.id)
            SELECT id, hash, mtime
            FROM nodes
            JOIN live USING(id)
            WHERE dir=0";
        let rows = self
            .db
            .prepare(sql)?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut errors = Vec::new();
        for (id, h, mtime) in rows {
            match self.archive_one(&id, &h, now.saturating_sub(mtime)) {
                Ok(()) => {
                    self.db
                        .execute("DELETE FROM archive_errors WHERE node=?", [&id])?;
                }
                Err(error) => {
                    self.db.execute(
                        "INSERT OR REPLACE INTO archive_errors VALUES(?,?)",
                        params![id, error.to_string()],
                    )?;
                    errors.push(format!("{id}: {error}"));
                }
            }
        }
        let garbage = self
            .db
            .prepare("SELECT target,key FROM cloud_gc")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (t, k) in garbage {
            match client(&self.db, &t).and_then(|c| c.remove(&k)) {
                Ok(()) => {
                    self.db.execute(
                        "DELETE FROM cloud_gc WHERE target=? AND key=?",
                        params![t, k],
                    )?;
                }
                Err(e) => errors.push(format!("{t}: {e}")),
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join(", "));
        Ok(())
    }

    fn archive_one(&self, id: &str, h: &str, age: u64) -> Result<()> {
        let configured = policy(&self.db, id)?;
        if configured.as_ref().is_none_or(|p| p.copies == 0) {
            return self.release_archives(id, h, configured.as_ref());
        }
        let Some(p) = configured else {
            return Ok(());
        };
        if age < p.delay_secs
            || p.copies == 0
            || !crate::core::placement_confirmed(&self.db, id, h)?
        {
            return Ok(());
        }
        let mut current = copies(&self.db, id)?;
        let mut count = current
            .iter()
            .filter(|(t, _, _)| p.targets.contains(t))
            .count();
        let mut failures = Vec::new();
        for t in &p.targets {
            if count >= p.copies {
                break;
            }
            if current.iter().any(|(existing, _, _)| existing == t) {
                continue;
            }
            let mut contents = tempfile::tempfile()?;
            self.copy_id(id, &mut contents)?;
            use sha2::{Digest, Sha256};
            let mut fingerprint = Sha256::new();
            contents.rewind()?;
            io::copy(&mut contents, &mut fingerprint)?;
            if format!("{:x}", fingerprint.finalize()) != h {
                return Ok(());
            }
            let uploaded = client(&self.db, t)
                .and_then(|client| Ok((client.upload(&mut contents, h)?, client)));
            let (key, archive) = match uploaded {
                Ok(pair) => pair,
                Err(error) => {
                    failures.push(format!("{t}: {error}"));
                    continue;
                }
            };
            let tx = self.db.unchecked_transaction()?;
            let valid: String =
                tx.query_row("SELECT hash FROM nodes WHERE id=?", [id], |r| r.get(0))?;
            if valid != h
                || !crate::core::placement_confirmed(&tx, id, h)?
                || policy(&tx, id)?.as_ref() != Some(&p)
            {
                drop(tx);
                archive.remove(&key)?;
                return Ok(());
            }
            let old: Option<String> = tx
                .query_row(
                    "SELECT key FROM cloud_copies WHERE node=? AND target=?",
                    params![id, t],
                    |r| r.get(0),
                )
                .optional()?;
            tx.execute(
                "INSERT OR REPLACE INTO cloud_copies VALUES(?,?,?,?)",
                params![id, t, key, h],
            )?;
            if let Some(old) = old {
                tx.execute(
                    "INSERT OR IGNORE INTO cloud_gc VALUES(?,?)",
                    params![t, old],
                )?;
            }
            tx.commit()?;
            current.push((t.clone(), key, h.into()));
            count += 1;
        }
        ensure!(
            count >= p.copies,
            "archive placement incomplete: {}",
            failures.join(", ")
        );
        let locals = self
            .db
            .prepare("SELECT target,object FROM copies WHERE node=? AND hash=? ORDER BY target")?
            .query_map(params![id, h], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let local_policy = self.policy(id)?;
        let mut readable: Vec<_> = locals
            .into_iter()
            .filter(|(t, k)| self.verified_copy(t, k, h))
            .collect();
        readable.sort_by_key(|(target, _)| !local_policy.long.contains(target));
        let selected: Vec<_> = current
            .iter()
            .filter(|(t, _, _)| p.targets.contains(t))
            .take(p.copies)
            .collect();
        let obsolete: Vec<_> = current.iter().filter(|c| !selected.contains(c)).collect();
        if readable.len() <= p.retain_local && obsolete.is_empty() {
            return Ok(());
        }
        // Verify the currently selected remote objects immediately before eviction.
        let size: u64 = self
            .db
            .query_row("SELECT size FROM nodes WHERE id=?", [id], |r| r.get(0))?;
        for (t, key, hash) in &selected {
            client(&self.db, t)?.read_to(key, hash, size, &mut io::sink())?;
        }
        let tx = self.db.unchecked_transaction()?;
        let latest: String =
            tx.query_row("SELECT hash FROM nodes WHERE id=?", [id], |r| r.get(0))?;
        if latest != h
            || !crate::core::placement_confirmed(&tx, id, h)?
            || policy(&tx, id)?.as_ref() != Some(&p)
        {
            return Ok(());
        }
        for (t, key, _) in obsolete {
            tx.execute(
                "DELETE FROM cloud_copies WHERE node=? AND target=? AND key=?",
                params![id, t, key],
            )?;
        }
        let evict: Vec<_> = readable.into_iter().skip(p.retain_local).collect();
        for (t, key) in &evict {
            if tx.execute(
                "DELETE FROM copies WHERE node=? AND target=? AND hash=? AND object=?",
                params![id, t, h, key],
            )? == 1
            {
                tx.execute(
                    "INSERT OR IGNORE INTO garbage(target,object) VALUES(?,?)",
                    params![t, key],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    fn release_archives(&self, id: &str, h: &str, expected: Option<&CloudPolicy>) -> Result<()> {
        if !self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM cloud_copies WHERE node=?)",
            [id],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        if !crate::core::placement_confirmed(&tx, id, h)? || policy(&tx, id)?.as_ref() != expected {
            return Ok(());
        }
        let locals = tx
            .prepare("SELECT target,object FROM copies WHERE node=? AND hash=?")?
            .query_map(params![id, h], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if locals.iter().any(|(t, k)| self.matches_copy(t, k, h)) {
            tx.execute("DELETE FROM cloud_copies WHERE node=?", [id])?;
            tx.commit()?;
        }
        Ok(())
    }
}
