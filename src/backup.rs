use crate::core::{Engine, now};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, LazyLock, Mutex},
    thread,
    time::UNIX_EPOCH,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub directories: Vec<PathBuf>,
    pub seconds: u64,
    pub changes: u64,
    pub keep: usize,
    #[serde(default)]
    pub retain_days: u64,
    #[serde(default)]
    pub hourly: u64,
    #[serde(default)]
    pub daily: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Status {
    pub path: PathBuf,
    pub seq: u64,
    pub time: u64,
    pub error: Option<String>,
}

impl Engine {
    pub fn schedule(&self, schedule: &Schedule) -> Result<()> {
        ensure!(
            schedule.seconds > 0 && schedule.changes > 0 && schedule.keep > 0,
            "snapshot thresholds must be positive"
        );
        let marker = self.backup_marker()?;
        let mut destinations = HashSet::new();
        for path in &schedule.directories {
            ensure!(
                path.is_absolute() && path.is_dir(),
                "snapshot destination must be an existing absolute directory"
            );
            ensure!(
                destinations.insert(fs::canonicalize(path)?),
                "duplicate snapshot destination"
            );
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path.join(&marker))?
                .sync_all()?;
            fs::File::open(path)?.sync_all()?;
        }
        self.put_record("backup/schedule", schedule, true)
    }

    fn backup_marker(&self) -> Result<String> {
        Ok(format!(
            ".tier-backup-{}",
            self.require::<String>("system/id")?
        ))
    }

    pub(crate) fn backup_status(&self) -> Result<Vec<serde_json::Value>> {
        let revision = self.business_revision()?;
        self.scan::<Status>("backup/status/")?
            .into_iter()
            .map(|(_, status)| {
                let mut value = serde_json::to_value(&status)?;
                value["lag"] = revision.saturating_sub(status.seq).into();
                Ok(value)
            })
            .collect()
    }

    pub fn backup_tick(&self) -> Result<()> {
        self.dispatch_backups(true)
    }
    pub fn schedule_backups(&self) -> Result<()> {
        self.dispatch_backups(false)
    }

    fn dispatch_backups(&self, force: bool) -> Result<()> {
        let Some(settings) = self.kv_get::<Schedule>("backup/schedule")? else {
            return Ok(());
        };
        let system = self.require::<String>("system/id")?;
        let marker = self.backup_marker()?;
        let lineage = if let Some(id) = self.kv_get::<String>("backup/lineage")? {
            id
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            self.put_record("backup/lineage", &id, true)?;
            id
        };
        let time = now()?;
        let revision = self.kv_get::<u64>("system/revision")?.unwrap_or(0);
        let mut jobs = Vec::new();
        for path in &settings.directories {
            let key = status_key(path);
            let last = self.kv_get::<Status>(&key)?;
            if !force
                && last.as_ref().is_some_and(|last| {
                    last.error.is_none()
                        && (revision == last.seq
                            || (revision.saturating_sub(last.seq) < settings.changes
                                && time.saturating_sub(last.time) < settings.seconds))
                })
            {
                continue;
            }
            if let Some(ticket) = BackupTicket::acquire((&system, path), force) {
                jobs.push((path.clone(), key, ticket));
            }
        }
        if jobs.is_empty() {
            return Ok(());
        }
        let image = Arc::new(self.snapshot()?);
        let seq = image["seq"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("invalid snapshot sequence"))?;
        thread::scope(|scope| -> Result<()> {
            let mut handles = Vec::new();
            for (path, key, ticket) in jobs {
                let image = image.clone();
                let marker = &marker;
                let settings = &settings;
                let lineage = &lineage;
                handles.push(scope.spawn(move || -> Result<()> {
                    let _ticket = ticket;
                    let result = publish_to(&path, &image, marker, settings, seq, time, lineage);
                    let error = result.as_ref().err().map(|e| format!("{e:#}"));
                    let previous = self.kv_get::<Status>(&key)?;
                    self.put_record(
                        &key,
                        &Status {
                            path: path.clone(),
                            seq: if error.is_none() {
                                seq
                            } else {
                                previous.as_ref().map_or(0, |s| s.seq)
                            },
                            time: if error.is_none() {
                                time
                            } else {
                                previous.as_ref().map_or(0, |s| s.time)
                            },
                            error,
                        },
                        false,
                    )?;
                    result
                }));
            }
            let mut errors = Vec::new();
            for handle in handles {
                match handle.join() {
                    Ok(Ok(())) => (),
                    Ok(Err(error)) => errors.push(format!("{error:#}")),
                    Err(_) => errors.push("snapshot worker panicked".into()),
                }
            }
            ensure!(errors.is_empty(), "{}", errors.join("; "));
            Ok(())
        })
    }
}

fn status_key(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    format!(
        "backup/status/{:x}",
        Sha256::digest(path.as_os_str().as_bytes())
    )
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
    for p in expired(
        &files,
        settings.keep,
        settings.retain_days,
        settings.hourly,
        settings.daily,
        now,
    ) {
        fs::remove_file(p)?;
    }
    pinned.sync_all()?;
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
    retain_days: u64,
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
        let recent = now.saturating_sub(*time) < retain_days.saturating_mul(86400);
        if index >= keep && !recent && !keep_hour && !keep_day {
            obsolete.push(path);
        }
    }
    obsolete
}
