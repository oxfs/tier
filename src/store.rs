use crate::{
    config::{Backup, Config, validate_relative},
    descriptor::{self, FileAttributes, State},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::fd::{AsRawFd, RawFd},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub struct Store {
    pub(crate) config: Config,
    pub(crate) identities: std::collections::BTreeMap<PathBuf, String>,
}

// The lock lives at least as long as the content fd, including caller reads.
pub struct Reader {
    file: File,
    _lock: File,
}
pub struct Writer {
    file: File,
    _lock: File,
}

impl AsRawFd for Reader {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
impl AsRawFd for Writer {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}
impl Seek for Reader {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl Seek for Writer {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}
impl Writer {
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        self.file.metadata()
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        self.file.set_len(size)
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub completed: usize,
    pub busy: usize,
    pub errors: Vec<String>,
}

struct MaintenancePlan {
    local_replicas: usize,
    remove_primary: bool,
    archive_due: bool,
}

impl MaintenancePlan {
    fn new(config: &Config, backup: &Backup, state: Option<&State>, now: u64) -> Self {
        let archive_ready = state.is_some_and(|state| {
            state.archived_at.is_some()
                && crate::archive::satisfied(config, backup.archive.as_ref(), state)
        });
        let retention_elapsed = backup.archive.as_ref().is_none_or(|archive| {
            archive_ready
                && state
                    .and_then(|state| state.archived_at)
                    .is_some_and(|at| now >= at && now - at >= archive.local_ttl_secs)
        });
        let archive_due = backup.archive.as_ref().is_some_and(|archive| {
            let modified = state
                .and_then(|state| state.attributes)
                .map_or(now, |attributes| attributes.modified_secs.max(0) as u64);
            !archive_ready && now >= modified.saturating_add(archive.after_secs)
        });
        Self {
            local_replicas: if retention_elapsed {
                backup.local_replicas()
            } else {
                backup.count.saturating_sub(1)
            },
            remove_primary: backup.gc && retention_elapsed,
            archive_due,
        }
    }

    fn needs_work(&self, config: &Config, state: &State, primary_exists: bool) -> bool {
        state.digest.is_none()
            || !state.retiring.is_empty()
            || self.archive_due
            || state.replicas.len()
                != self.local_replicas
                    + usize::from(!self.remove_primary && state.local_primary.is_some())
            || state
                .replicas
                .iter()
                .any(|name| !config.replicas.contains_key(name))
            || (state.local_primary.is_none() && primary_exists == self.remove_primary)
            || (state.local_primary.is_some() && primary_exists)
            || (state.local_primary.is_none() && !primary_exists && !state.replicas.is_empty())
    }
}

impl Store {
    pub fn new(config: Config) -> Result<Self> {
        Self::open(config, false)
    }

    pub fn initialize(config: Config) -> Result<Self> {
        Self::open(config, true)
    }

    fn open(mut config: Config, initialize: bool) -> Result<Self> {
        config.validate()?;
        // Existing roots are intentional: never recreate a missing mounted disk.
        config.primary = fs::canonicalize(&config.primary)?;
        for root in config.replicas.values_mut() {
            if root.exists() {
                *root = fs::canonicalize(&*root)?;
            }
        }
        config.validate()?;
        ensure!(config.primary.is_dir(), "primary root is not a directory");
        let mut store = Self {
            config,
            identities: Default::default(),
        };
        if initialize {
            store.initialize_roots()?;
        } else {
            store.identities = serde_json::from_slice(
                &fs::read(store.root().join(".tier/storage.json"))
                    .context("primary storage is not initialized or unavailable")?,
            )?;
            store.verify_root(store.root())?;
            for root in store.config.replicas.values() {
                ensure!(
                    store.identities.contains_key(root),
                    "new replica requires explicit initialization: {}",
                    root.display()
                );
            }
        }
        if store.root().join(".tier/namespace.json").exists() {
            let _namespace = store.namespace(true)?;
            store.recover_namespace()?;
        }
        Ok(store)
    }

    pub(crate) fn path(root: &Path, relative: &Path) -> Result<PathBuf> {
        ensure!(
            root.is_dir(),
            "storage root unavailable: {}",
            root.display()
        );
        let mut path = root.to_owned();
        for component in relative.components() {
            ensure!(
                matches!(component, std::path::Component::Normal(_)),
                "invalid relative path"
            );
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(meta) => ensure!(
                    !meta.file_type().is_symlink(),
                    "symlinks are not supported: {}",
                    path.display()
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(path)
    }

    pub(crate) fn content(&self, relative: &Path) -> Result<PathBuf> {
        validate_relative(relative)?;
        ensure!(!relative.as_os_str().is_empty(), "file path is empty");
        Self::path(&self.config.primary, relative)
    }

    pub(crate) fn metadata(root: &Path, relative: &Path) -> Result<PathBuf> {
        Self::path(root, &Path::new(".tier/meta").join(relative))
    }

    pub(crate) fn lock(&self, relative: &Path) -> Result<File> {
        self.content(relative)?;
        let path = Self::metadata(&self.config.primary, relative)?;
        create_parent(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        Ok(file)
    }

    pub fn read(&self, relative: &Path) -> Result<Reader> {
        let _namespace = self.namespace(false)?;
        if !self.content(relative)?.exists() && !Self::metadata(self.root(), relative)?.is_file() {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        let mut lock = self.lock(relative)?;
        FileExt::try_lock_shared(&lock)?;
        let _state = self.state_lock(relative, false)?;
        let state = descriptor::load(&mut lock)?.map(|(_, state)| state);
        drop(_namespace);
        let primary = self.content(relative)?;
        let file = match state.as_ref() {
            Some(state) => self.read_content(relative, state)?,
            None => File::open(primary)?,
        };
        Ok(Reader { file, _lock: lock })
    }

    // All modifications must go through this entry, including after primary GC.
    // Native uncoordinated writes to the backing directories are unsupported.
    pub fn write(&self, relative: &Path) -> Result<Writer> {
        self.open_write(relative, true, false, false)
    }

    pub fn open_write(
        &self,
        relative: &Path,
        create: bool,
        exclusive: bool,
        truncate: bool,
    ) -> Result<Writer> {
        let _namespace = self.namespace(false)?;
        if !create
            && !self.content(relative)?.exists()
            && !Self::metadata(self.root(), relative)?.is_file()
        {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        let mut lock = self.lock(relative)?;
        FileExt::try_lock_shared(&lock)?;
        let _state = self.state_lock(relative, true)?;
        let state = descriptor::load(&mut lock)?.map(|(_, state)| state);
        drop(_namespace);
        let primary = self.local_content(relative, state.as_ref())?;
        let exists = primary.exists() || state.is_some();
        if state
            .as_ref()
            .is_some_and(|state| !state.archives.is_empty())
        {
            return Err(io::Error::from_raw_os_error(libc::EROFS).into());
        }
        if exists && create && exclusive {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        if !exists && !create {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        if state.is_some() && !primary.exists() {
            bail!("designated local primary is unavailable");
        }
        if state.as_ref().is_some_and(|state| state.digest.is_some()) {
            let mut pending = State::pending();
            if let Some(old) = state {
                pending.local_primary = old.local_primary;
                pending.retiring = old.retiring;
                for name in old.replicas {
                    if pending.local_primary.as_ref() != Some(&name)
                        && !pending.retiring.contains(&name)
                    {
                        pending.retiring.push(name);
                    }
                }
            }
            descriptor::save(&mut lock, &pending)?;
        }
        create_parent(&primary)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate)
            .open(&primary)?;
        FileExt::lock_shared(&lock)?;
        Ok(Writer { file, _lock: lock })
    }

    pub(crate) fn local_content(&self, relative: &Path, state: Option<&State>) -> Result<PathBuf> {
        let root = match state.and_then(|state| state.local_primary.as_ref()) {
            Some(name) => self
                .config
                .replicas
                .get(name)
                .context("unknown local primary")?,
            None => self.root(),
        };
        self.verify_root(root)?;
        Self::path(root, relative)
    }

    fn read_content(&self, relative: &Path, state: &State) -> Result<File> {
        let candidate = self
            .local_content(relative, Some(state))
            .and_then(|path| Ok(File::open(path)?));
        match candidate {
            Ok(file) => Ok(file),
            Err(error) if state.digest.is_none() => Err(error),
            Err(_) => self.replica(relative, state),
        }
    }

    pub(crate) fn replica(&self, relative: &Path, state: &State) -> Result<File> {
        self.find_replica(relative, state, false)
    }

    pub(crate) fn verified_replica(&self, relative: &Path, state: &State) -> Result<File> {
        self.find_replica(relative, state, true)
    }

    fn find_replica(&self, relative: &Path, state: &State, verify: bool) -> Result<File> {
        let digest = state
            .digest
            .as_ref()
            .context("pending file has no recoverable replica")?;
        for name in &state.replicas {
            let Some(root) = self.config.replicas.get(name) else {
                continue;
            };
            let candidate = (|| -> Result<File> {
                self.verify_root(root)?;
                let mut file = File::open(Self::path(root, relative)?)?;
                if verify {
                    ensure!(
                        checksum(&mut file)? == (digest.clone(), state.size),
                        "replica content mismatch"
                    );
                    file.rewind()?;
                }
                Ok(file)
            })();
            if let Ok(file) = candidate {
                return Ok(file);
            }
        }
        if !state.archives.is_empty() {
            return crate::archive::read(&self.config, state);
        }
        bail!("no available replica for {}", relative.display())
    }

    pub fn reconcile(&self, relative: &Path) -> Result<bool> {
        let _namespace = self.namespace(false)?;
        if !self.content(relative)?.exists() && !Self::metadata(self.root(), relative)?.exists() {
            return Ok(false);
        }
        let Some(_state) = self.try_state_lock(relative)? else {
            return Ok(false);
        };
        let mut lock = self.lock(relative)?;
        if !try_lock(&lock, true)? {
            return Ok(false);
        }
        drop(_namespace);
        let current = descriptor::load(&mut lock)?.map(|(_, state)| state);
        let primary = self.content(relative)?;
        let backup = self.config.backup_for(relative)?;
        let now = now_secs();
        let plan = MaintenancePlan::new(&self.config, backup, current.as_ref(), now);
        let local_count = plan.local_replicas
            + usize::from(
                !plan.remove_primary && current.as_ref().is_some_and(|s| s.local_primary.is_some()),
            );
        let gc = plan.remove_primary;
        if let Some(state) = &current
            && state.digest.is_some()
            && state.replicas.len() == local_count
            && state
                .replicas
                .iter()
                .all(|name| self.config.replicas.contains_key(name))
            && crate::archive::satisfied(&self.config, backup.archive.as_ref(), state)
            && (!gc || state.local_primary.is_some() || state.replicas.is_empty())
            && (gc || self.local_content(relative, current.as_ref())?.exists())
            && (backup.archive.is_none() || state.archived_at.is_some())
        {
            self.verify_content(relative, state)?;
            self.mirror(relative, &mut lock)?;
            self.cleanup_verified(relative, &mut lock, state.clone(), gc)?;
            return Ok(true);
        }
        let source_path = self.local_content(relative, current.as_ref())?;
        let mut source = if source_path.exists() {
            File::open(&source_path)?
        } else {
            self.verified_replica(relative, current.as_ref().context("missing primary")?)?
        };
        source.sync_all()?;
        if source_path.exists() {
            sync_parent(&source_path)?;
        }
        let (digest, size) = checksum(&mut source)?;
        if let Some(expected) = current.as_ref().and_then(|state| state.digest.as_ref()) {
            ensure!(
                *expected == digest,
                "content changed outside the writer protocol"
            );
        }
        // A retained primary counts toward the total and must exist before
        // publishing a smaller set of copies on other disks.
        if !gc && current.as_ref().is_none_or(|s| s.local_primary.is_none()) && !primary.exists() {
            source.rewind()?;
            copy_file(&mut source, &primary, self.root())?;
        }
        let mut replicas = Vec::new();
        let selected = self.select_replicas(relative, current.as_ref(), local_count, size)?;
        for name in &selected {
            let root = &self.config.replicas[name];
            self.verify_root(root)?;
            let destination = Self::path(root, relative)?;
            let matches = File::open(&destination)
                .ok()
                .and_then(|mut file| checksum(&mut file).ok())
                .is_some_and(|value| value == (digest.clone(), size));
            if !matches {
                source.rewind()?;
                copy_file(&mut source, &destination, root)?;
            }
            replicas.push(name.clone());
        }
        let local_primary = if gc {
            current
                .as_ref()
                .and_then(|s| s.local_primary.clone())
                .filter(|name| replicas.contains(name))
                .or_else(|| replicas.first().cloned())
        } else {
            current.as_ref().and_then(|s| s.local_primary.clone())
        };
        let mut state = State {
            local_primary,
            replicas,
            digest: Some(digest),
            size,
            archives: current
                .as_ref()
                .map_or_else(Vec::new, |s| s.archives.clone()),
            archived_at: current.as_ref().and_then(|s| s.archived_at),
            retiring: current
                .as_ref()
                .map_or_else(Vec::new, |s| s.retiring.clone()),
            attributes: Some({
                let metadata = source.metadata()?;
                FileAttributes {
                    size,
                    modified_secs: metadata.mtime(),
                    modified_nanos: metadata.mtime_nsec(),
                    mode: metadata.mode(),
                }
            }),
        };
        if let Some(attributes) = current.as_ref().and_then(|old| old.attributes) {
            state.attributes = Some(attributes);
        }
        if let Some(old) = &current {
            for name in old
                .replicas
                .iter()
                .filter(|name| !state.replicas.contains(name))
            {
                if !state.retiring.contains(name) {
                    state.retiring.push(name.clone());
                }
            }
        }
        state.retiring.retain(|name| !state.replicas.contains(name));
        if gc || !state.retiring.is_empty() {
            self.verify_local_content(relative, &state)?;
            if !plan.archive_due {
                crate::archive::verify(&self.config, &state)?;
            }
        }
        // Publish local protection before any remote I/O. A failed upload must
        // leave independently recoverable local copies and descriptors.
        descriptor::save(&mut lock, &state)?;
        // Publish the mapping durably before mirrors or GC can rely on it.
        sync_parent(&Self::metadata(self.root(), relative)?)?;
        // Mirror descriptors to every configured disk, even disks without content.
        // A failed mirror aborts GC. Replica descriptors are never active locks.
        self.mirror(relative, &mut lock)?;
        if let Some(policy) = &backup.archive
            && MaintenancePlan::new(&self.config, backup, Some(&state), now).archive_due
        {
            state.archives = crate::archive::upload(
                &self.config,
                policy,
                &mut source,
                state.digest.as_ref().unwrap(),
                size,
            )?;
            state.archived_at = Some(now_secs());
            descriptor::save(&mut lock, &state)?;
            self.mirror(relative, &mut lock)?;
        }
        // Only retire old local copies after the newly published set is durable.
        self.cleanup_verified(relative, &mut lock, state, gc)?;
        Ok(true)
    }

    // Caller has verified the retained content while holding the exclusive file
    // lock and mirrored the new state. No second full local/remote read is needed.
    fn cleanup_verified(
        &self,
        relative: &Path,
        lock: &mut File,
        mut state: State,
        remove_primary: bool,
    ) -> Result<()> {
        let primary = self.content(relative)?;
        if (remove_primary || state.local_primary.is_some()) && primary.exists() {
            fs::remove_file(&primary)?;
            sync_parent(&primary)?;
        }
        if state.retiring.is_empty() {
            return Ok(());
        }
        for name in &state.retiring {
            let root = self
                .config
                .replicas
                .get(name)
                .context("retiring disk removed from configuration")?;
            self.verify_root(root)?;
            let path = Self::path(root, relative)?;
            if path.exists() {
                fs::remove_file(&path)?;
                sync_parent(&path)?;
            }
        }
        state.retiring.clear();
        descriptor::save(lock, &state)?;
        self.mirror(relative, lock)
    }

    fn select_replicas(
        &self,
        relative: &Path,
        current: Option<&State>,
        count: usize,
        size: u64,
    ) -> Result<Vec<String>> {
        let mut candidates = Vec::new();
        for (name, root) in &self.config.replicas {
            self.verify_root(root)?;
            let designated =
                current.is_some_and(|state| state.local_primary.as_ref() == Some(name));
            let retained = current.is_some_and(|state| state.replicas.contains(name));
            let available = fs2::available_space(root)?;
            if designated || retained || available >= size {
                let mut hash = Sha256::new();
                hash.update(relative.as_os_str().as_encoded_bytes());
                hash.update(name.as_bytes());
                candidates.push((
                    designated,
                    retained,
                    available / (1024 * 1024 * 1024),
                    hash.finalize().to_vec(),
                    name.clone(),
                ));
            }
        }
        candidates.sort_by(|a, b| b.cmp(a));
        ensure!(
            candidates.len() >= count,
            "not enough local space for required copies"
        );
        let mut selected: Vec<_> = candidates
            .into_iter()
            .take(count)
            .map(|(_, _, _, _, name)| name)
            .collect();
        selected.sort();
        Ok(selected)
    }

    pub fn scan(&self) -> Result<Report> {
        self.scan_with_audit(true, true)
    }

    pub fn scan_local_audit(&self) -> Result<Report> {
        self.scan_with_audit(true, false)
    }

    pub fn scan_pending(&self) -> Result<Report> {
        self.scan_with_audit(false, false)
    }

    fn scan_with_audit(&self, audit: bool, remote: bool) -> Result<Report> {
        let namespace = self.namespace(false)?;
        let mut files = BTreeSet::new();
        self.walk(&self.config.primary, Path::new(""), &mut files, true)?;
        let metadata = Self::path(&self.config.primary, Path::new(".tier/meta"))?;
        if metadata.exists() {
            self.walk(&metadata, Path::new(""), &mut files, false)?;
        }
        drop(namespace);
        let queue = std::sync::Mutex::new(files.into_iter());
        let report = std::sync::Mutex::new(Report::default());
        std::thread::scope(|scope| {
            for _ in 0..self.config.background.workers {
                scope.spawn(|| {
                    loop {
                        let Some(relative) = queue.lock().unwrap().next() else {
                            break;
                        };
                        let result = self.maintain_file(&relative, audit, remote);
                        let mut report = report.lock().unwrap();
                        match result {
                            Ok(true) => report.completed += 1,
                            Ok(false) => report.busy += 1,
                            Err(error) => report
                                .errors
                                .push(format!("{}: {error:#}", relative.display())),
                        }
                    }
                });
            }
        });
        Ok(report.into_inner().unwrap())
    }

    fn maintain_file(&self, relative: &Path, audit: bool, remote: bool) -> Result<bool> {
        let pending = {
            let _namespace = self.namespace(false)?;
            let primary_exists = self.content(relative)?.exists();
            if !primary_exists && !Self::metadata(self.root(), relative)?.exists() {
                return Ok(false);
            }
            let Some(_state) = self.try_state_lock(relative)? else {
                return Ok(false);
            };
            let mut lock = self.lock(relative)?;
            if !try_lock(&lock, true)? {
                return Ok(false);
            }
            let current = descriptor::load(&mut lock)?.map(|(_, state)| state);
            let policy = self.config.backup_for(relative)?;
            let plan = MaintenancePlan::new(&self.config, policy, current.as_ref(), now_secs());
            current
                .as_ref()
                .is_none_or(|state| plan.needs_work(&self.config, state, primary_exists))
        };
        if pending {
            self.reconcile(relative)
        } else if audit {
            self.check_with_remote(relative, remote)
        } else {
            Ok(true)
        }
    }

    fn walk(
        &self,
        root: &Path,
        relative: &Path,
        files: &mut BTreeSet<PathBuf>,
        mirror_dirs: bool,
    ) -> Result<()> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            if relative.as_os_str().is_empty() && entry.file_name() == ".tier" {
                continue;
            }
            let path = relative.join(entry.file_name());
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "symlink is unsupported: {}",
                path.display()
            );
            if kind.is_dir() {
                if mirror_dirs {
                    for replica in self.config.replicas.values() {
                        self.verify_root(replica)?;
                        let directory = Self::path(replica, &path)?;
                        create_parent(&directory.join("placeholder"))?;
                    }
                }
                self.walk(root, &path, files, mirror_dirs)?;
            } else {
                ensure!(kind.is_file(), "special files are unsupported");
                files.insert(path);
            }
        }
        Ok(())
    }

    // Inspection never repairs or deletes: inconsistencies are explicit alerts.
    pub fn check(&self, relative: &Path) -> Result<bool> {
        self.check_with_remote(relative, true)
    }

    fn check_with_remote(&self, relative: &Path, remote: bool) -> Result<bool> {
        let _namespace = self.namespace(false)?;
        if !self.content(relative)?.exists() && !Self::metadata(self.root(), relative)?.exists() {
            return Ok(false);
        }
        let Some(_state) = self.try_state_lock(relative)? else {
            return Ok(false);
        };
        let mut lock = self.lock(relative)?;
        if !try_lock(&lock, true)? {
            return Ok(false);
        }
        let state = descriptor::load(&mut lock)?
            .context("file has not been replicated")?
            .1;
        drop(_namespace);
        self.verify(relative, &state, remote)?;
        Ok(true)
    }

    fn verify(&self, relative: &Path, state: &State, remote: bool) -> Result<()> {
        self.verify_local_content(relative, state)?;
        if remote {
            crate::archive::verify(&self.config, state)?;
        }
        for (name, root) in &self.config.replicas {
            self.verify_root(root)?;
            let mut file = File::open(Self::metadata(root, relative)?)?;
            let mirrored = descriptor::load(&mut file)?
                .context("empty mirrored descriptor")?
                .1;
            ensure!(
                serde_json::to_vec(&mirrored)? == serde_json::to_vec(&state)?,
                "descriptor mismatch on {name}"
            );
        }
        Ok(())
    }

    fn verify_content(&self, relative: &Path, state: &State) -> Result<()> {
        self.verify_local_content(relative, state)?;
        crate::archive::verify(&self.config, state)
    }

    fn verify_local_content(&self, relative: &Path, state: &State) -> Result<()> {
        let digest = state.digest.as_ref().context("file awaits replication")?;
        let primary = self.content(relative)?;
        ensure!(
            primary.exists() || !state.replicas.is_empty() || !state.archives.is_empty(),
            "no content locations remain"
        );
        if state.local_primary.is_none() && primary.exists() {
            let mut file = File::open(primary)?;
            ensure!(
                checksum(&mut file)? == (digest.clone(), state.size),
                "primary content mismatch"
            );
        }
        for name in &state.replicas {
            let root = self
                .config
                .replicas
                .get(name)
                .context("unknown replica disk")?;
            self.verify_root(root)?;
            let mut file = File::open(Self::path(root, relative)?)?;
            ensure!(
                checksum(&mut file)? == (digest.clone(), state.size),
                "content mismatch on {name}"
            );
        }
        Ok(())
    }

    fn mirror(&self, relative: &Path, lock: &mut File) -> Result<()> {
        for root in self.config.replicas.values() {
            self.verify_root(root)?;
            lock.rewind()?;
            copy_atomic(&mut *lock, &Self::metadata(root, relative)?)?;
        }
        Ok(())
    }
}

fn try_lock(file: &File, exclusive: bool) -> Result<bool> {
    let result = if exclusive {
        FileExt::try_lock_exclusive(file)
    } else {
        FileExt::try_lock_shared(file)
    };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn checksum(file: &mut File) -> Result<(String, u64)> {
    file.rewind()?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut size = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((format!("{:x}", digest.finalize()), size))
}

pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("missing parent directory")?)?.sync_all()?;
    Ok(())
}

pub(crate) fn create_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    if !parent.exists() {
        create_parent(parent)?;
        match fs::create_dir(parent) {
            Ok(()) => sync_parent(parent)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn copy_atomic(mut source: impl Read, destination: &Path) -> Result<()> {
    create_parent(destination)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(destination.parent().context("missing parent")?)?;
    io::copy(&mut source, &mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary.persist(destination)?;
    sync_parent(destination)
}

pub(crate) fn copy_file(source: &mut File, destination: &Path, root: &Path) -> Result<()> {
    let metadata = source.metadata()?;
    create_parent(destination)?;
    let staging = Store::path(root, Path::new(".tier/tmp"))?;
    create_parent(&staging.join("placeholder"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(staging)?;
    io::copy(source, &mut temporary)?;
    temporary
        .as_file()
        .set_permissions(metadata.permissions())?;
    temporary.as_file().set_times(
        fs::FileTimes::new()
            .set_accessed(metadata.accessed()?)
            .set_modified(metadata.modified()?),
    )?;
    temporary.as_file().sync_all()?;
    temporary.persist(destination)?;
    sync_parent(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_assigns_a_local_primary_to_older_gc_descriptors() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let primary = temporary.path().join("primary");
        let replica = temporary.path().join("replica");
        fs::create_dir(&primary)?;
        fs::create_dir(&replica)?;
        let store = Store::initialize(toml::from_str(&format!(
            "primary={primary:?}\n[replicas]\nbackup={replica:?}\n[[backup]]\npath=''\ncount=1\ngc=true\n"
        ))?)?;
        let path = Path::new("file");
        store.write(path)?.write_all(b"before")?;
        store.reconcile(path)?;
        let mut lock = store.lock(path)?;
        let state = descriptor::load(&mut lock)?.unwrap().1;
        let mut json = serde_json::to_value(state)?;
        json.as_object_mut().unwrap().remove("local_primary");
        let old: State = serde_json::from_value(json)?;
        assert!(old.local_primary.is_none());
        descriptor::save(&mut lock, &old)?;
        drop(lock);
        assert!(store.scan_pending()?.errors.is_empty());
        store.write(path)?.write_all(b"after!")?;
        assert!(!primary.join(path).exists());
        assert_eq!(fs::read(replica.join(path))?, b"after!");
        Ok(())
    }
}
