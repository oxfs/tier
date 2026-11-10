//! Independent CSV exports and durable incremental RocksDB backup repositories.
use crate::{
    core::{Engine, schema},
    export,
};
use anyhow::{Context, Result, ensure};
use rocksdb::{
    Env,
    backup::{BackupEngine, BackupEngineOptions, RestoreOptions},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{Condvar, LazyLock, Mutex},
    thread,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Export,
    Metadata,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Metadata => "metadata",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Self::Export => schema::EXPORT_SCHEDULE,
            Self::Metadata => schema::METADATA_SCHEDULE,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub enabled: bool,
    pub directories: Vec<PathBuf>,
    pub seconds: u64,
    pub keep: usize,
}
impl Schedule {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.seconds > 0 && self.keep > 0 && u32::try_from(self.keep).is_ok(),
            "seconds and keep must be positive; keep must fit in u32"
        );
        ensure!(
            !self.enabled || !self.directories.is_empty(),
            "enabled schedule needs a destination"
        );
        ensure!(
            self.directories.iter().all(|p| p.is_absolute()),
            "backup destinations must be absolute paths"
        );
        ensure!(
            self.directories.iter().collect::<BTreeSet<_>>().len() == self.directories.len(),
            "duplicate backup destination"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Status {
    kind: Kind,
    path: PathBuf,
    time: u64,
    attempted_at: u64,
    latest: Option<String>,
    error: Option<String>,
}

fn metadata_marker(path: &Path) -> PathBuf {
    path.join(".tier-metadata-id")
}
fn export_marker(path: &Path, system: &str) -> PathBuf {
    path.join(format!(".tier-export-{system}"))
}

impl Engine {
    pub fn set_backup_schedule(&self, kind: Kind, schedule: &Schedule) -> Result<()> {
        schedule.validate()?;
        let mut schedule = schedule.clone();
        let previous = self.kv_get::<Schedule>(kind.key())?;
        let disabling_existing = !schedule.enabled
            && previous
                .as_ref()
                .is_some_and(|s| s.directories == schedule.directories);
        if !disabling_existing {
            let system = self.require::<String>(schema::SYSTEM_ID)?;
            let mut unique = BTreeSet::new();
            for path in &mut schedule.directories {
                *path = self.prepare_backup_directory(kind, path, &system)?;
                ensure!(unique.insert(path.clone()), "duplicate backup destination");
            }
        }

        self.put_record(kind.key(), &schedule, true)
    }

    fn prepare_backup_directory(&self, kind: Kind, path: &Path, system: &str) -> Result<PathBuf> {
        let path = fs::canonicalize(path)?;
        let database = fs::canonicalize(self.inner.db.path())?;
        ensure!(path.is_dir(), "backup destination must be a directory");
        ensure!(
            !path.starts_with(&database) && !database.starts_with(&path),
            "backup destination overlaps metadata database"
        );
        let marker = match kind {
            Kind::Export => export_marker(&path, system),
            Kind::Metadata => {
                let marker = metadata_marker(&path);
                ensure!(
                    marker.exists() || fs::read_dir(&path)?.next().is_none(),
                    "metadata backup needs an empty or registered repository"
                );
                marker
            }
        };
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(mut file) => {
                file.write_all(system.as_bytes())?;
                file.sync_all()?;
                fs::File::open(&path)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
                fs::read_to_string(marker)? == system,
                "backup repository belongs to another system"
            ),
            Err(e) => return Err(e.into()),
        }
        Ok(path)
    }

    pub(crate) fn backup_schedules(&self) -> Result<Vec<Value>> {
        let mut schedules = Vec::new();
        for kind in [Kind::Export, Kind::Metadata] {
            if let Some(config) = self.kv_get::<Schedule>(kind.key())? {
                schedules.push(json!({"kind":kind,"config":config}));
            }
        }
        Ok(schedules)
    }
    pub(crate) fn backup_status(&self) -> Result<Vec<Value>> {
        self.scan::<Status>(schema::BACKUP_STATUS)?
            .into_iter()
            .map(|(_, status)| Ok(serde_json::to_value(status)?))
            .collect()
    }
    pub fn backup_tick(&self, kind: Kind) -> Result<()> {
        self.dispatch_backups(kind, true)
    }
    pub(crate) fn schedule_backup(&self, kind: Kind) -> Result<()> {
        self.dispatch_backups(kind, false)
    }
    pub fn schedule_backups(&self) -> Result<()> {
        let mut errors = Vec::new();
        for kind in [Kind::Export, Kind::Metadata] {
            if let Err(error) = self.schedule_backup(kind) {
                errors.push(format!("{}: {error:#}", kind.as_str()));
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
    fn dispatch_backups(&self, kind: Kind, force: bool) -> Result<()> {
        let Some(schedule) = self.kv_get::<Schedule>(kind.key())? else {
            ensure!(!force, "backup schedule is not configured");
            return Ok(());
        };
        if !force && !schedule.enabled {
            return Ok(());
        }
        ensure!(
            !schedule.directories.is_empty(),
            "backup schedule has no destinations"
        );
        let time = crate::core::now()?;
        let system = self.require::<String>(schema::SYSTEM_ID)?;
        let mut jobs = Vec::new();
        for path in &schedule.directories {
            let key = schema::backup_status(kind.as_str(), path);
            let last = self.kv_get::<Status>(&key)?;
            let delay = if last.as_ref().is_some_and(|s| s.error.is_some()) {
                schedule.seconds.min(60)
            } else {
                schedule.seconds
            };
            if !force
                && last
                    .as_ref()
                    .is_some_and(|s| time.saturating_sub(s.attempted_at) < delay)
            {
                continue;
            }
            jobs.push((path.clone(), key));
        }
        if jobs.is_empty() {
            return Ok(());
        }
        let rows = if kind == Kind::Export {
            Some(crate::recovery::export_rows(self).map_err(|e| format!("{e:#}")))
        } else {
            None
        };
        thread::scope(|scope| -> Result<()> {
            let mut tasks = Vec::new();
            for (path, key) in jobs {
                let rows = &rows;
                let system = &system;
                let schedule = &schedule;
                tasks.push(scope.spawn(move || -> Result<()> {
                    let Some(_ticket) =
                        BackupTicket::acquire((system.clone(), kind, path.clone()), force)
                    else {
                        return Ok(());
                    };
                    let previous = self.kv_get::<Status>(&key)?;
                    let time = crate::core::now()?;
                    let delay = if previous.as_ref().is_some_and(|s| s.error.is_some()) {
                        schedule.seconds.min(60)
                    } else {
                        schedule.seconds
                    };
                    if !force
                        && previous
                            .as_ref()
                            .is_some_and(|s| time.saturating_sub(s.attempted_at) < delay)
                    {
                        return Ok(());
                    }
                    let result = match rows {
                        Some(Ok(rows)) => publish_export(&path, system, rows, schedule.keep),
                        Some(Err(error)) => Err(anyhow::anyhow!(error.clone())),
                        None => self.publish_metadata(&path, system, schedule.keep),
                    };
                    let status = Status {
                        kind,
                        path,
                        attempted_at: time,
                        time: if result.is_ok() {
                            time
                        } else {
                            previous.as_ref().map_or(0, |s| s.time)
                        },
                        latest: result
                            .as_ref()
                            .ok()
                            .map(|p| p.latest.clone())
                            .or_else(|| previous.and_then(|s| s.latest)),
                        error: match &result {
                            Ok(p) => p.cleanup_error.clone(),
                            Err(e) => Some(format!("{e:#}")),
                        },
                    };
                    self.put_record(&key, &status, false)?;
                    ensure!(
                        status.error.is_none(),
                        "{}",
                        status.error.unwrap_or_default()
                    );
                    Ok(())
                }));
            }
            let mut errors = Vec::new();
            for task in tasks {
                match task.join() {
                    Ok(Ok(())) => (),
                    Ok(Err(e)) => errors.push(format!("{e:#}")),
                    Err(_) => errors.push("backup worker panicked".into()),
                }
            }
            ensure!(errors.is_empty(), "{}", errors.join("; "));
            Ok(())
        })
    }
    fn publish_metadata(&self, path: &Path, system: &str, keep: usize) -> Result<Publication> {
        let pinned = fs::File::open(path)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
        let marker = fs::File::open(metadata_marker(&path))?;
        marker.lock()?;
        ensure!(
            fs::read_to_string(metadata_marker(&path))? == system,
            "backup repository identity changed"
        );
        let mut backup = open_engine(&path)?;
        self.inner.db.create_backup(&mut backup)?;
        let id = backup
            .get_backup_info()
            .last()
            .context("backup was not published")?
            .backup_id;
        backup.verify_backup(id)?;
        let cleanup = backup
            .purge_old_backups(keep)
            .map_err(anyhow::Error::from)
            .and_then(|_| Ok(pinned.sync_all()?));
        Ok(Publication {
            latest: id.to_string(),
            cleanup_error: cleanup
                .err()
                .map(|e| format!("backup created; retention failed: {e:#}")),
        })
    }
}

struct Publication {
    latest: String,
    cleanup_error: Option<String>,
}

fn publish_export(
    path: &Path,
    system: &str,
    rows: &[(String, String)],
    keep: usize,
) -> Result<Publication> {
    let pinned = fs::File::open(path)?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
    let marker = fs::File::open(export_marker(&path, system))?;
    marker.lock()?;
    ensure!(
        fs::read_to_string(export_marker(&path, system))? == system,
        "export destination identity changed"
    );
    let prefix = format!("tier-export-{system}-");
    let name = format!("{prefix}{}.csv", uuid::Uuid::new_v4());
    export::publish(rows, &path.join(&name))?;
    let cleanup = retain_exports(&path, &prefix, &name, keep).and_then(|_| Ok(pinned.sync_all()?));
    Ok(Publication {
        latest: name,
        cleanup_error: cleanup
            .err()
            .map(|e| format!("export created; retention failed: {e:#}")),
    })
}

fn retain_exports(path: &Path, prefix: &str, current: &str, keep: usize) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file = entry.file_name();
        if file != current
            && file.to_str().is_some_and(|n| {
                n.strip_prefix(prefix)
                    .and_then(|n| n.strip_suffix(".csv"))
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
            })
            && entry.file_type()?.is_file()
        {
            files.push((entry.metadata()?.modified()?, entry.path()));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, file) in files.into_iter().skip(keep.saturating_sub(1)) {
        fs::remove_file(file)?;
    }
    Ok(())
}

fn open_engine(path: &Path) -> Result<BackupEngine> {
    let mut options = BackupEngineOptions::new(path)?;
    options.set_sync(true);
    Ok(BackupEngine::open(&options, &Env::new()?)?)
}

struct ExistingBackup {
    _directory: fs::File,
    _lock: fs::File,
    engine: BackupEngine,
    identity: String,
    ids: BTreeSet<u32>,
}
impl ExistingBackup {
    fn open(path: &Path) -> Result<Self> {
        let directory = fs::File::open(path)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let lock = fs::File::open(metadata_marker(&path))?;
        lock.lock_shared()?;
        let identity = fs::read_to_string(metadata_marker(&path))?;
        uuid::Uuid::parse_str(&identity)?;
        // The Rust binding has no read-only BackupEngine. Existing complete
        // repositories open without mutation; never append or purge via this handle.
        for subdir in ["meta", "private", "shared_checksum"] {
            ensure!(
                path.join(subdir).is_dir(),
                "incomplete backup repository: missing {subdir}"
            );
        }
        let mut ids = BTreeSet::new();
        for entry in fs::read_dir(path.join("meta"))? {
            let entry = entry?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            {
                ids.insert(id);
            }
        }
        Ok(Self {
            engine: open_engine(&path)?,
            _directory: directory,
            _lock: lock,
            identity,
            ids,
        })
    }
}

pub fn list(path: &Path) -> Result<Vec<Value>> {
    let repository = ExistingBackup::open(path)?;
    let info = repository.engine.get_backup_info();
    Ok(repository.ids.iter().map(|id| {
        let backup = info.iter().find(|item| item.backup_id == *id);
        json!({"id":id,"recognized":backup.is_some(),"timestamp":backup.map(|b|b.timestamp),"size":backup.map(|b|b.size),"files":backup.map(|b|b.num_files)})
    }).collect())
}

pub(crate) fn restore_staging(path: &Path, requested: Option<u32>, dest: &Path) -> Result<u32> {
    ensure!(!dest.exists(), "restore staging directory already exists");
    let mut repository = ExistingBackup::open(path)?;
    let id = requested
        .or_else(|| repository.ids.last().copied())
        .context("backup repository is empty")?;
    ensure!(
        repository
            .engine
            .get_backup_info()
            .iter()
            .any(|b| b.backup_id == id),
        "backup {id} is missing or corrupt; choose another backup explicitly"
    );
    repository.engine.verify_backup(id)?;
    repository
        .engine
        .restore_from_backup(dest, dest, &RestoreOptions::default(), id)?;
    let database = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), dest, false)?;
    let identity = database
        .get(schema::SYSTEM_ID)?
        .context("restored database has no system identity")?;
    ensure!(
        schema::decode::<String>(&identity)? == repository.identity,
        "backup system identity mismatch"
    );
    Ok(id)
}

// Covers publication and its status together. Periodic work skips busy targets;
// explicit requests wait without holding a database or namespace lock.
type DestinationKey = (String, Kind, PathBuf);
static ACTIVE: LazyLock<(Mutex<HashSet<DestinationKey>>, Condvar)> =
    LazyLock::new(Default::default);
struct BackupTicket(DestinationKey);
impl BackupTicket {
    fn acquire(key: DestinationKey, wait: bool) -> Option<Self> {
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
        let (mutex, ready) = &*ACTIVE;
        mutex.lock().unwrap().remove(&self.0);
        ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_retention_keeps_the_publication_even_with_equal_or_future_timestamps() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let prefix = "tier-export-test-";
        let current = format!("{prefix}{}.csv", uuid::Uuid::new_v4());
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        for name in [
            current.clone(),
            format!("{prefix}{}.csv", uuid::Uuid::new_v4()),
            format!("{prefix}{}.csv", uuid::Uuid::new_v4()),
        ] {
            let file = fs::File::create(temp.path().join(&name))?;
            file.set_times(fs::FileTimes::new().set_modified(time))?;
        }
        retain_exports(temp.path(), prefix, &current, 1)?;
        assert!(temp.path().join(&current).exists());
        assert_eq!(fs::read_dir(temp.path())?.count(), 1);
        let other = temp
            .path()
            .join(format!("{prefix}{}.csv", uuid::Uuid::new_v4()));
        fs::File::create(other)?.set_times(
            fs::FileTimes::new().set_modified(time + std::time::Duration::from_secs(500)),
        )?;
        retain_exports(temp.path(), prefix, &current, 1)?;
        assert!(temp.path().join(current).exists());
        assert_eq!(fs::read_dir(temp.path())?.count(), 1);
        Ok(())
    }
}
