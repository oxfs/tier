mod catalog;
mod file;
mod reconcile;
mod report;
pub(crate) use report::Writer;
mod target;
use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub front: Vec<String>,
    pub front_count: usize,
    pub long: Vec<String>,
    pub long_count: usize,
    pub delay_secs: u64,
    #[serde(default)]
    pub direct_fallback: bool,
    #[serde(default)]
    pub fallback_count: Option<usize>,
    #[serde(default)]
    pub manual_repair: bool,
    #[serde(default)]
    pub repair_grace_secs: u64,
    #[serde(default = "default_low")]
    pub low_water: u8,
    #[serde(default = "default_high")]
    pub high_water: u8,
    #[serde(default = "default_ttl")]
    pub trash_ttl_secs: u64,
}
impl Policy {
    fn desired(&self) -> (&[String], usize) {
        if self.long_count == 0 {
            (&self.front, self.front_count)
        } else {
            (&self.long, self.long_count)
        }
    }
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            front: vec![],
            front_count: 1,
            long: vec![],
            long_count: 0,
            delay_secs: 0,
            direct_fallback: false,
            fallback_count: None,
            manual_repair: false,
            repair_grace_secs: 0,
            low_water: default_low(),
            high_water: default_high(),
            trash_ttl_secs: default_ttl(),
        }
    }
}
fn default_low() -> u8 {
    70
}
fn default_high() -> u8 {
    90
}
fn default_ttl() -> u64 {
    604800
}
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
}
pub struct Engine {
    pub(crate) db: Connection,
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', '\0']) && name != "." && name != ".."
}
fn split(path: &str) -> Result<(&str, &str)> {
    ensure!(path.starts_with('/') && path != "/", "invalid path");
    ensure!(path[1..].split('/').all(safe_name), "invalid path");
    let (parent, name) = path.rsplit_once('/').unwrap();
    Ok((if parent.is_empty() { "/" } else { parent }, name))
}
fn fingerprint<R: Read + Seek>(reader: &mut R) -> Result<(u64, String)> {
    reader.rewind()?;
    let mut hash = Sha256::new();
    let size = io::copy(reader, &mut hash)?;
    Ok((size, format!("{:x}", hash.finalize())))
}
fn rank(names: &mut [String], id: &str) {
    names.sort_by_cached_key(|name| Sha256::digest(format!("{id}/{name}")).to_vec());
}
impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.create_collation("tier_name", |a, b| {
            unicase::UniCase::new(a).cmp(&unicase::UniCase::new(b))
        })?;
        crate::backup::configure(&db)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA recursive_triggers=ON; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS nodes(id TEXT PRIMARY KEY,parent TEXT REFERENCES nodes(id),name TEXT COLLATE NOCASE NOT NULL,dir INTEGER NOT NULL,size INTEGER NOT NULL DEFAULT 0,hash TEXT,mtime INTEGER NOT NULL DEFAULT 0,display_mtime INTEGER,UNIQUE(parent,name));
      INSERT OR IGNORE INTO nodes(id,name,dir) VALUES('root','',1);
      CREATE TABLE IF NOT EXISTS targets(name TEXT PRIMARY KEY,path TEXT UNIQUE NOT NULL,identity TEXT UNIQUE NOT NULL,domain TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS copies(node TEXT REFERENCES nodes(id),target TEXT REFERENCES targets(name),object TEXT NOT NULL,hash TEXT NOT NULL,PRIMARY KEY(node,target));
      CREATE TABLE IF NOT EXISTS garbage(target TEXT NOT NULL,object TEXT NOT NULL,PRIMARY KEY(target,object));
      CREATE TRIGGER IF NOT EXISTS retire_deleted AFTER DELETE ON copies BEGIN INSERT OR IGNORE INTO garbage VALUES(old.target,old.object); END;
      CREATE TRIGGER IF NOT EXISTS retire_updated AFTER UPDATE OF object ON copies WHEN old.object!=new.object BEGIN INSERT OR IGNORE INTO garbage VALUES(old.target,old.object); END;
      CREATE INDEX IF NOT EXISTS copies_by_target ON copies(target,node);
      CREATE TABLE IF NOT EXISTS policies(node TEXT PRIMARY KEY REFERENCES nodes(id),policy TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS system_identity(id INTEGER PRIMARY KEY CHECK(id=1),uuid TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS pending_writes(node TEXT PRIMARY KEY REFERENCES nodes(id));
      CREATE TABLE IF NOT EXISTS protection(node TEXT PRIMARY KEY REFERENCES nodes(id),hash TEXT NOT NULL,incident INTEGER);
      CREATE TABLE IF NOT EXISTS trash(id INTEGER PRIMARY KEY AUTOINCREMENT,node TEXT UNIQUE,original TEXT,deleted INTEGER,expires INTEGER NOT NULL);")?;
        db.execute(
            "INSERT OR IGNORE INTO system_identity VALUES(1,?)",
            [Uuid::new_v4().to_string()],
        )?;
        db.execute("CREATE UNIQUE INDEX IF NOT EXISTS namespace_names ON nodes(parent,name COLLATE tier_name)",[])
            .context("creating Unicode name index; sibling names must be distinct after case folding")?;
        crate::archive::init(&db)?;
        let engine = Self { db };
        crate::backup::init(&engine)?;
        Ok(engine)
    }
}

fn inherited_policy(db: &Connection, id: &str) -> Result<Policy> {
    let sql = "WITH RECURSIVE ancestors(id, parent, d) AS (
        SELECT id, parent, 0
        FROM nodes
        WHERE id=?
        UNION ALL
        SELECT n.id, n.parent, d+1
        FROM nodes n
        JOIN ancestors a ON n.id=a.parent)
        SELECT p.policy
        FROM ancestors a
        JOIN policies p ON p.node=a.id
        ORDER BY d LIMIT 1";
    let raw: String = db
        .query_row(sql, [id], |r| r.get(0))
        .context("no applicable storage policy")?;
    Ok(serde_json::from_str(&raw)?)
}
fn placement(policy: &Policy, id: &str, hash: &str) -> Result<(Vec<String>, usize, String)> {
    let (targets, count) = policy.desired();
    let mut targets = targets.to_vec();
    rank(&mut targets, id);
    let signature = serde_json::to_string(&(hash, count, &targets))?;
    Ok((targets, count, signature))
}
pub(crate) fn current_file(db: &Connection, id: &str, hash: &str) -> Result<bool> {
    let sql = "WITH RECURSIVE chain(id, parent) AS (
        SELECT id, parent
        FROM nodes
        WHERE id=?1 AND hash=?2
        UNION ALL
        SELECT n.id, n.parent
        FROM nodes n
        JOIN chain c ON n.id=c.parent)
        SELECT EXISTS(
        SELECT 1
        FROM chain
        WHERE id='root')
        AND NOT EXISTS(
        SELECT 1
        FROM pending_writes
        WHERE node=?1)";
    Ok(db.query_row(sql, params![id, hash], |r| r.get(0))?)
}
pub(crate) fn placement_confirmed(db: &Connection, id: &str, hash: &str) -> Result<bool> {
    if !current_file(db, id, hash)? {
        return Ok(false);
    }
    let policy = inherited_policy(db, id)?;
    let (_, _, signature) = placement(&policy, id, hash)?;
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM protection WHERE node=? AND hash=? AND incident IS NULL)",
        params![id, signature],
        |r| r.get(0),
    )?)
}
