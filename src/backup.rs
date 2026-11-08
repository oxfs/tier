use crate::core::Engine;
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, functions::FunctionFlags, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, LazyLock, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub directories: Vec<PathBuf>,
    pub seconds: u64,
    pub changes: u64,
    pub keep: usize,
    #[serde(default)]
    pub hourly: u64,
    #[serde(default)]
    pub daily: u64,
}

pub(crate) fn configure(db: &rusqlite::Connection) -> Result<()> {
    // A token is stable within a SQLite transaction and unique across connections.
    // Changes inside a rolled-back transaction (including savepoints) roll back in SQL.
    let connection_id = uuid::Uuid::new_v4();
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    use std::sync::atomic::Ordering::Relaxed;
    let commit = generation.clone();
    db.commit_hook(Some(move || {
        commit.fetch_add(1, Relaxed);
        false
    }));
    let rollback = generation.clone();
    db.rollback_hook(Some(move || {
        rollback.fetch_add(1, Relaxed);
    }));
    db.create_scalar_function(
        "tier_transaction",
        0,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS,
        move |_| Ok(format!("{connection_id}-{}", generation.load(Relaxed))),
    )?;
    Ok(())
}

pub fn init(engine: &Engine) -> Result<()> {
    let tx = engine.db.unchecked_transaction()?;
    let installed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='backup_transaction')",
        [],
        |r| r.get(0),
    )?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS revision(id INTEGER PRIMARY KEY CHECK(id=1),seq INTEGER NOT NULL);
    INSERT OR IGNORE INTO revision VALUES(1,0);
    CREATE TABLE IF NOT EXISTS backup_schedule(id INTEGER PRIMARY KEY CHECK(id=1),config TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS backup_status(path TEXT PRIMARY KEY,seq INTEGER NOT NULL,time INTEGER NOT NULL,error TEXT);
    CREATE TABLE IF NOT EXISTS backup_transaction(id INTEGER PRIMARY KEY CHECK(id=1),token TEXT NOT NULL);
    INSERT OR IGNORE INTO backup_transaction VALUES(1,'');
    CREATE TABLE IF NOT EXISTS backup_changes(second INTEGER PRIMARY KEY,seq INTEGER NOT NULL);
    CREATE TRIGGER IF NOT EXISTS track_backup_revision AFTER UPDATE OF seq ON revision
    WHEN EXISTS(SELECT 1 FROM backup_schedule)
    BEGIN INSERT INTO backup_changes VALUES(unixepoch(),NEW.seq)
          ON CONFLICT(second) DO UPDATE SET seq=NEW.seq; END;")?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS backup_lineage(id INTEGER PRIMARY KEY CHECK(id=1),uuid TEXT NOT NULL)")?;
    tx.execute(
        "INSERT OR IGNORE INTO backup_lineage VALUES(1,?)",
        [uuid::Uuid::new_v4().to_string()],
    )?;
    if installed {
        tx.commit()?;
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO backup_changes SELECT unixepoch(),seq FROM revision",
        [],
    )?;
    for table in [
        "nodes",
        "targets",
        "copies",
        "policies",
        "trash",
        "pending_writes",
        "protection",
        "backup_schedule",
        "cloud_targets",
        "cloud_policy",
        "cloud_copies",
    ] {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            tx.execute_batch(&format!(
                "DROP TRIGGER IF EXISTS revision_{table}_{event};
                CREATE TRIGGER revision_{table}_{event} AFTER {event} ON {table}
                WHEN (SELECT token FROM backup_transaction WHERE id=1) != tier_transaction()
                BEGIN
                    UPDATE revision SET seq=seq+1 WHERE id=1;
                    UPDATE backup_transaction SET token=tier_transaction() WHERE id=1;
                END;"
            ))?;
        }
    }
    tx.commit()?;
    Ok(())
}

impl Engine {
    pub fn schedule(&self, s: &Schedule) -> Result<()> {
        ensure!(
            s.seconds > 0 && s.changes > 0 && s.keep > 0,
            "all snapshot thresholds must be positive"
        );
        let marker = self.backup_marker()?;
        for path in &s.directories {
            ensure!(
                path.is_absolute() && path.is_dir(),
                "backup destination must be an existing absolute directory"
            );
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.join(&marker))?
                .sync_all()?;
            fs::File::open(path)?.sync_all()?;
        }
        self.db.execute(
            "INSERT OR REPLACE INTO backup_schedule VALUES(1,?)",
            [serde_json::to_string(s)?],
        )?;
        Ok(())
    }
    fn backup_marker(&self) -> Result<String> {
        let system: String =
            self.db
                .query_row("SELECT uuid FROM system_identity WHERE id=1", [], |r| {
                    r.get(0)
                })?;
        Ok(format!(".tier-backup-{system}"))
    }
    pub fn backup_tick(&self) -> Result<()> {
        self.dispatch_backups(true)
    }
    pub fn schedule_backups(&self) -> Result<()> {
        self.dispatch_backups(false)
    }
    fn dispatch_backups(&self, wait: bool) -> Result<()> {
        let Some(raw) = self
            .db
            .query_row("SELECT config FROM backup_schedule WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        else {
            return Ok(());
        };
        let s = Arc::new(serde_json::from_str::<Schedule>(&raw)?);
        let seq: u64 = self
            .db
            .query_row("SELECT seq FROM revision WHERE id=1", [], |r| r.get(0))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let marker = self.backup_marker()?;
        let lineage: String =
            self.db
                .query_row("SELECT uuid FROM backup_lineage WHERE id=1", [], |r| {
                    r.get(0)
                })?;
        let mut image = None;
        let database = PathBuf::from(
            self.db
                .path()
                .ok_or_else(|| anyhow::anyhow!("missing database path"))?,
        );
        let mut jobs = Vec::new();
        for dir in &s.directories {
            let Some(ticket) = BackupTicket::acquire((&marker, dir), wait) else {
                continue;
            };
            let path = dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid destination"))?;
            let (old, time): (u64, u64) = self
                .db
                .query_row(
                    "SELECT seq,time FROM backup_status WHERE path=?",
                    [path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((0, 0));
            if time != 0 {
                if old >= seq {
                    continue;
                }
                let first: u64 = self
                    .db
                    .query_row(
                        "SELECT min(second) FROM backup_changes WHERE seq>?",
                        [old],
                        |r| r.get::<_, Option<u64>>(0),
                    )?
                    .unwrap_or(now);
                if now.saturating_sub(first) < s.seconds && seq - old < s.changes {
                    continue;
                }
            }
            let data = match &image {
                Some(data) => data,
                None => image.insert(Arc::new(self.snapshot()?)),
            };
            let captured = data["seq"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("invalid snapshot sequence"))?;
            let data = Arc::clone(data);
            let dir = dir.clone();
            let marker = marker.clone();
            let database = database.clone();
            let settings = Arc::clone(&s);
            let lineage = lineage.clone();
            let job = thread::Builder::new()
                .name("tier-backup".into())
                .spawn(move || {
                    let _ticket = ticket;
                    let result = (|| -> Result<()> {
                        let db = rusqlite::Connection::open(database)?;
                        let result =
                            publish_to(&dir, &data, &marker, &settings, captured, now, &lineage);
                        let (saved, at, error) = match result {
                            Ok(()) => (
                                captured,
                                SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                                None,
                            ),
                            Err(e) => (old, time, Some(e.to_string())),
                        };
                        db.execute(
                            "INSERT OR REPLACE INTO backup_status VALUES(?,?,?,?)",
                            params![dir.to_str(), saved, at, error],
                        )?;
                        Ok(())
                    })();
                    if let Err(error) = &result {
                        tracing::warn!(path = %dir.display(), %error, "snapshot destination failed");
                    }
                    result
                })?;
            if wait {
                jobs.push(job);
            }
        }
        for job in jobs {
            job.join()
                .map_err(|_| anyhow::anyhow!("snapshot worker panicked"))??;
        }
        // Keep one watermark before the scheduling window even if a device stays offline.
        self.db.execute("DELETE FROM backup_changes WHERE second < (SELECT max(second) FROM backup_changes WHERE second<=?)",[now.saturating_sub(s.seconds)])?;
        Ok(())
    }
}

fn publish_to(
    dir: &Path,
    data: &serde_json::Value,
    marker: &str,
    settings: &Schedule,
    captured: u64,
    now: u64,
    lineage: &str,
) -> Result<()> {
    let prefix = format!(
        "tier-{}-{lineage}-",
        data["system_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing system identity"))?
    );
    let pinned = fs::File::open(dir)?;
    let dir = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
    ensure!(
        dir.join(marker).is_file(),
        "snapshot destination identity missing"
    );
    let file = dir.join(format!("{prefix}{captured:020}.json"));
    if file.exists() {
        ensure!(
            crate::snapshot::load(&file)? == *data,
            "snapshot sequence collision"
        );
    } else {
        crate::snapshot::publish(data, &file)?;
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(seq) = name
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .and_then(|n| n.strip_suffix(".json"))
            .filter(|n| n.len() == 20)
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if !entry.file_type()?.is_file() {
            continue;
        }
        let published = entry
            .metadata()?
            .modified()?
            .duration_since(UNIX_EPOCH)?
            .as_secs();
        files.push((seq, published, entry.path()));
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for p in expired(&files, settings.keep, settings.hourly, settings.daily, now) {
        fs::remove_file(p)?;
    }
    Ok(())
}

type TargetKey = (String, PathBuf);
static ACTIVE: LazyLock<(Mutex<HashSet<TargetKey>>, Condvar)> = LazyLock::new(Default::default);
struct BackupTicket(TargetKey);
impl BackupTicket {
    fn acquire((system, dir): (&str, &Path), wait: bool) -> Option<Self> {
        let key = (system.to_owned(), dir.to_owned());
        let (mutex, ready) = &*ACTIVE;
        let mut active = mutex.lock().unwrap();
        while active.contains(&key) {
            if !wait {
                return None;
            }
            active = ready.wait(active).unwrap();
        }
        active.insert(key.clone());
        Some(Self(key))
    }
}
impl Drop for BackupTicket {
    fn drop(&mut self) {
        ACTIVE.0.lock().unwrap().remove(&self.0);
        ACTIVE.1.notify_all();
    }
}

fn expired(
    files: &[(u64, u64, PathBuf)],
    keep: usize,
    hourly: u64,
    daily: u64,
    now: u64,
) -> Vec<&PathBuf> {
    let mut hours = HashSet::new();
    let mut days = HashSet::new();
    let mut obsolete = Vec::new();
    for (index, (_, time, path)) in files.iter().enumerate() {
        let hour = *time / 3600;
        let day = *time / 86400;
        let keep_hour = (now / 3600).saturating_sub(hour) < hourly && hours.insert(hour);
        let keep_day = (now / 86400).saturating_sub(day) < daily && days.insert(day);
        if index >= keep && !keep_hour && !keep_day {
            obsolete.push(path);
        }
    }
    obsolete
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn stalled_device_does_not_delay_new_revisions_on_another_device() -> Result<()> {
        use std::time::{Duration, Instant};
        let temp = tempdir()?;
        let engine = Engine::open(&temp.path().join("db"))?;
        let slow = temp.path().join("slow");
        let fast = temp.path().join("fast");
        fs::create_dir(&slow)?;
        fs::create_dir(&fast)?;
        engine.schedule(&Schedule {
            directories: vec![slow.clone(), fast.clone()],
            seconds: 30,
            changes: 1,
            keep: 1,
            hourly: 0,
            daily: 0,
        })?;
        let image = engine.snapshot()?;
        let first = image["seq"].as_u64().unwrap();
        let lineage: String =
            engine
                .db
                .query_row("SELECT uuid FROM backup_lineage WHERE id=1", [], |r| {
                    r.get(0)
                })?;
        let fifo = slow.join(format!(
            "tier-{}-{lineage}-{first:020}.json",
            image["system_id"].as_str().unwrap()
        ));
        ensure!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success(),
            "mkfifo failed"
        );
        let held = fs::OpenOptions::new().read(true).write(true).open(&fifo)?;
        let await_fast = |revision: u64| -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                engine.schedule_backups()?;
                let saved: Option<u64> = engine
                    .db
                    .query_row(
                        "SELECT seq FROM backup_status WHERE path=?",
                        [fast.to_str().unwrap()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if saved == Some(revision) {
                    return Ok(());
                }
                ensure!(Instant::now() < deadline, "healthy destination stalled");
                thread::sleep(Duration::from_millis(10));
            }
        };
        await_fast(first)?;
        engine.mkdir("/change")?;
        await_fast(first + 1)?;
        assert!(BackupTicket::acquire((&engine.backup_marker()?, &slow), false).is_none());
        fs::remove_file(fifo)?;
        drop(held);
        engine.backup_tick()?;
        let saved: u64 = engine.db.query_row(
            "SELECT seq FROM backup_status WHERE path=?",
            [slow.to_str().unwrap()],
            |r| r.get(0),
        )?;
        assert_eq!(saved, first + 1);
        Ok(())
    }

    #[test]
    fn rows_in_one_commit_advance_once_and_rollback_does_not_advance() -> Result<()> {
        let tmp = tempdir()?;
        let engine = Engine::open(&tmp.path().join("db"))?;
        let revision = || -> Result<i64> {
            Ok(engine
                .db
                .query_row("SELECT seq FROM revision", [], |r| r.get(0))?)
        };
        let before = revision()?;
        {
            let tx = engine.db.unchecked_transaction()?;
            engine.mkdir("/a")?;
            engine.mkdir("/b")?;
            tx.commit()?;
        }
        assert_eq!(revision()?, before + 1);
        {
            let _rollback = engine.db.unchecked_transaction()?;
            engine.mkdir("/lost")?;
        }
        assert_eq!(revision()?, before + 1);
        engine.mkdir("/c")?;
        assert_eq!(revision()?, before + 2);
        Ok(())
    }

    #[test]
    fn long_idle_time_does_not_trigger_an_early_backup() -> Result<()> {
        let tmp = tempdir()?;
        let engine = Engine::open(&tmp.path().join("db"))?;
        let directory = tmp.path().join("backups");
        fs::create_dir(&directory)?;
        engine.schedule(&Schedule {
            directories: vec![directory],
            seconds: 3600,
            changes: 10,
            keep: 1,
            hourly: 0,
            daily: 0,
        })?;
        engine.backup_tick()?;
        let saved = || -> Result<u64> {
            Ok(engine
                .db
                .query_row("SELECT seq FROM backup_status", [], |r| r.get(0))?)
        };
        let baseline = saved()?;
        engine.db.execute("UPDATE backup_status SET time=1", [])?;
        engine.mkdir("/new")?;
        engine.backup_tick()?;
        assert_eq!(saved()?, baseline);
        engine
            .db
            .execute("UPDATE backup_changes SET second=1 WHERE seq>?", [baseline])?;
        engine.backup_tick()?;
        assert_eq!(saved()?, baseline + 1);
        Ok(())
    }

    #[test]
    fn retention_keeps_the_latest_snapshot_in_each_selected_bucket() {
        let times = [240, 239, 238, 225, 224, 200, 170, 160, 100];
        let files: Vec<_> = times
            .iter()
            .map(|h| (*h, *h * 3600, PathBuf::from(h.to_string())))
            .collect();
        let removed = expired(&files, 1, 3, 4, 240 * 3600);
        let removed: Vec<_> = removed.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(removed, ["225", "224", "160", "100"]);
    }
}
