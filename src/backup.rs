use crate::{core::Engine, recovery};
use anyhow::{Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::fd::AsRawFd,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize, Deserialize)]
pub struct Schedule {
    pub directories: Vec<PathBuf>,
    pub seconds: u64,
    pub changes: u64,
    pub keep: usize,
}

pub fn init(engine: &Engine) -> Result<()> {
    engine.db.execute_batch("CREATE TABLE IF NOT EXISTS revision(id INTEGER PRIMARY KEY CHECK(id=1),seq INTEGER NOT NULL);
    INSERT OR IGNORE INTO revision VALUES(1,0);
    CREATE TABLE IF NOT EXISTS backup_schedule(id INTEGER PRIMARY KEY CHECK(id=1),config TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS backup_status(path TEXT PRIMARY KEY,seq INTEGER NOT NULL,time INTEGER NOT NULL,error TEXT);")?;
    for table in [
        "nodes",
        "targets",
        "copies",
        "policies",
        "trash",
        "pending_writes",
        "backup_schedule",
        "cloud_targets",
        "cloud_policy",
        "cloud_copies",
    ] {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            engine.db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS revision_{table}_{event} AFTER {event} ON {table} BEGIN UPDATE revision SET seq=seq+1 WHERE id=1; END;"))?;
        }
    }
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
        use rusqlite::OptionalExtension;
        let Some(raw) = self
            .db
            .query_row("SELECT config FROM backup_schedule WHERE id=1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        else {
            return Ok(());
        };
        let s: Schedule = serde_json::from_str(&raw)?;
        let seq: u64 = self
            .db
            .query_row("SELECT seq FROM revision WHERE id=1", [], |r| r.get(0))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let marker = self.backup_marker()?;
        let mut image = None;
        for dir in s.directories {
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
            if old == seq && time != 0
                || now.saturating_sub(time) < s.seconds && seq.saturating_sub(old) < s.changes
            {
                continue;
            }
            let data = match &image {
                Some(data) => data,
                None => image.insert(self.snapshot()?),
            };
            let captured = data["seq"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("invalid snapshot sequence"))?;
            let prefix = format!(
                "tier-{}-",
                data["system_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing system identity"))?
            );
            let result = (|| -> Result<()> {
                let pinned = fs::File::open(&dir)?;
                let dir = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
                ensure!(
                    dir.join(&marker).is_file(),
                    "snapshot destination identity missing"
                );
                let file = dir.join(format!("{prefix}{captured:020}.json"));
                if file.exists() {
                    ensure!(
                        recovery::load(&file)? == *data,
                        "snapshot sequence collision"
                    );
                } else {
                    recovery::publish(data, &file)?;
                }
                let mut files = fs::read_dir(&dir)?
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|s| s.to_str())
                            .and_then(|n| n.strip_prefix(&prefix))
                            .and_then(|n| n.strip_suffix(".json"))
                            .is_some_and(|n| n.len() == 20 && n.bytes().all(|b| b.is_ascii_digit()))
                    })
                    .collect::<Vec<_>>();
                files.sort();
                for p in files.iter().rev().skip(s.keep) {
                    fs::remove_file(p)?;
                }
                Ok(())
            })();
            let (saved, at, error) = match result {
                Ok(()) => (captured, now, None),
                Err(e) => (old, time, Some(e.to_string())),
            };
            self.db.execute(
                "INSERT OR REPLACE INTO backup_status VALUES(?,?,?,?)",
                params![path, saved, at, error],
            )?;
        }
        Ok(())
    }
}
