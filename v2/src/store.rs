use crate::{
    config::{Config, validate_relative},
    descriptor::{self, State},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
};

pub struct Store {
    config: Config,
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

impl Store {
    pub fn new(mut config: Config) -> Result<Self> {
        config.validate()?;
        // Existing roots are intentional: never recreate a missing mounted disk.
        config.primary = fs::canonicalize(&config.primary)?;
        for root in config.replicas.values_mut() {
            *root = fs::canonicalize(&*root)?;
        }
        config.validate()?;
        for root in std::iter::once(&config.primary).chain(config.replicas.values()) {
            ensure!(root.is_dir(), "storage root is not a directory");
        }
        Ok(Self { config })
    }

    fn path(root: &Path, relative: &Path) -> Result<PathBuf> {
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

    fn content(&self, relative: &Path) -> Result<PathBuf> {
        validate_relative(relative)?;
        ensure!(!relative.as_os_str().is_empty(), "file path is empty");
        Self::path(&self.config.primary, relative)
    }

    fn metadata(root: &Path, relative: &Path) -> Result<PathBuf> {
        Self::path(root, &Path::new(".tier/meta").join(relative))
    }

    fn lock(&self, relative: &Path) -> Result<File> {
        self.content(relative)?;
        let path = Self::metadata(&self.config.primary, relative)?;
        create_parent(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        sync_parent(&path)?;
        Ok(file)
    }

    pub fn read(&self, relative: &Path) -> Result<Reader> {
        let mut lock = self.lock(relative)?;
        FileExt::lock_shared(&lock)?;
        let state = descriptor::load(&mut lock)?.map(|(_, state)| state);
        let primary = self.content(relative)?;
        let file = if primary.exists() {
            File::open(primary)?
        } else {
            self.replica(
                relative,
                state.as_ref().context("missing content and descriptor")?,
            )?
        };
        Ok(Reader { file, _lock: lock })
    }

    // All modifications must go through this entry, including after primary GC.
    // Native uncoordinated writes to the backing directories are unsupported.
    pub fn write(&self, relative: &Path) -> Result<Writer> {
        let mut lock = self.lock(relative)?;
        FileExt::lock_exclusive(&lock)?;
        let state = descriptor::load(&mut lock)?.map(|(_, state)| state);
        let primary = self.content(relative)?;
        if !primary.exists()
            && let Some(state) = &state
        {
            let source = self.replica(relative, state)?;
            copy_atomic(source, &primary)?;
        }
        let generation = state.map_or(Ok(1), |state| {
            state
                .generation
                .checked_add(1)
                .context("generation exhausted")
        })?;
        descriptor::save(&mut lock, &State::pending(generation))?;
        create_parent(&primary)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&primary)?;
        sync_parent(&primary)?;
        Ok(Writer { file, _lock: lock })
    }

    fn replica(&self, relative: &Path, state: &State) -> Result<File> {
        let digest = state
            .digest
            .as_ref()
            .context("pending file has no recoverable replica")?;
        for name in &state.replicas {
            let Some(root) = self.config.replicas.get(name) else {
                continue;
            };
            let candidate = (|| -> Result<File> {
                let mut file = File::open(Self::path(root, relative)?)?;
                ensure!(
                    checksum(&mut file)? == (digest.clone(), state.size),
                    "replica content mismatch"
                );
                file.rewind()?;
                Ok(file)
            })();
            if let Ok(file) = candidate {
                return Ok(file);
            }
        }
        bail!("no verified replica for {}", relative.display())
    }

    pub fn reconcile(&self, relative: &Path) -> Result<bool> {
        let mut lock = self.lock(relative)?;
        if !try_lock(&lock, true)? {
            return Ok(false);
        }
        let current = descriptor::load(&mut lock)?.map(|(_, state)| state);
        let primary = self.content(relative)?;
        let policy = self.config.policy_for(relative)?;
        let selected: Vec<_> = self
            .config
            .replicas
            .keys()
            .take(policy.copies)
            .cloned()
            .collect();
        if let Some(state) = &current
            && state.digest.is_some()
            && state.replicas == selected
        {
            self.verify_content(relative, state)?;
            self.mirror(relative, &mut lock)?;
            if primary.exists() && policy.gc {
                fs::remove_file(&primary)?;
                sync_parent(&primary)?;
            }
            return Ok(true);
        }
        let mut source = if primary.exists() {
            File::open(&primary)?
        } else {
            self.replica(relative, current.as_ref().context("missing primary")?)?
        };
        source.sync_all()?;
        let (digest, size) = checksum(&mut source)?;
        if let Some(expected) = current.as_ref().and_then(|state| state.digest.as_ref()) {
            ensure!(
                *expected == digest,
                "content changed outside the writer protocol"
            );
        }
        // Restore the primary before dropping the last recorded replica.
        if policy.copies == 0 && !primary.exists() {
            source.rewind()?;
            copy_atomic(&mut source, &primary)?;
        }
        let mut replicas = Vec::new();
        for (name, root) in self.config.replicas.iter().take(policy.copies) {
            let destination = Self::path(root, relative)?;
            let matches = File::open(&destination)
                .ok()
                .and_then(|mut file| checksum(&mut file).ok())
                .is_some_and(|value| value == (digest.clone(), size));
            if !matches {
                source.rewind()?;
                copy_atomic(&mut source, &destination)?;
            }
            replicas.push(name.clone());
        }
        let state = State {
            generation: current.map_or(1, |state| state.generation),
            replicas,
            digest: Some(digest),
            size,
        };
        descriptor::save(&mut lock, &state)?;
        // Mirror descriptors to every configured disk, even disks without content.
        // A failed mirror aborts GC. Replica descriptors are never active locks.
        self.mirror(relative, &mut lock)?;
        if policy.gc && primary.exists() {
            fs::remove_file(&primary)?;
            sync_parent(&primary)?;
        }
        Ok(true)
    }

    pub fn scan(&self) -> Result<Report> {
        let mut files = BTreeSet::new();
        self.walk(&self.config.primary, Path::new(""), &mut files, true)?;
        let metadata = Self::path(&self.config.primary, Path::new(".tier/meta"))?;
        if metadata.exists() {
            self.walk(&metadata, Path::new(""), &mut files, false)?;
        }
        let mut report = Report::default();
        let mut pending = Vec::new();
        let mut ready = Vec::new();
        for relative in files {
            let classification = (|| -> Result<Option<bool>> {
                let mut lock = self.lock(&relative)?;
                if !try_lock(&lock, false)? {
                    return Ok(None);
                }
                let policy = self.config.policy_for(&relative)?;
                let selected: Vec<_> = self
                    .config
                    .replicas
                    .keys()
                    .take(policy.copies)
                    .cloned()
                    .collect();
                Ok(Some(descriptor::load(&mut lock)?.is_none_or(
                    |(_, state)| {
                        state.digest.is_none()
                            || state.replicas != selected
                            || (policy.gc && self.config.primary.join(&relative).exists())
                    },
                )))
            })();
            match classification {
                Ok(Some(true)) => pending.push(relative),
                Ok(Some(false)) => ready.push(relative),
                Ok(None) => report.busy += 1,
                Err(error) => report
                    .errors
                    .push(format!("{}: {error:#}", relative.display())),
            }
        }
        for relative in pending {
            match self.reconcile(&relative) {
                Ok(true) => report.completed += 1,
                Ok(false) => report.busy += 1,
                Err(error) => report
                    .errors
                    .push(format!("{}: {error:#}", relative.display())),
            }
        }
        // Only inspect old copies once foreground replication has caught up.
        if report.errors.is_empty() && report.busy == 0 {
            for relative in ready {
                match self.check(&relative) {
                    Ok(true) => report.completed += 1,
                    Ok(false) => report.busy += 1,
                    Err(error) => report
                        .errors
                        .push(format!("{}: {error:#}", relative.display())),
                }
            }
        }
        Ok(report)
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
        let mut lock = self.lock(relative)?;
        if !try_lock(&lock, false)? {
            return Ok(false);
        }
        let state = descriptor::load(&mut lock)?
            .context("file has not been replicated")?
            .1;
        self.verify(relative, &state)?;
        Ok(true)
    }

    fn verify(&self, relative: &Path, state: &State) -> Result<()> {
        self.verify_content(relative, state)?;
        for (name, root) in &self.config.replicas {
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
        let digest = state.digest.as_ref().context("file awaits replication")?;
        let primary = self.content(relative)?;
        ensure!(
            primary.exists() || !state.replicas.is_empty(),
            "no content locations remain"
        );
        if primary.exists() {
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

fn checksum(file: &mut File) -> Result<(String, u64)> {
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

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("missing parent directory")?)?.sync_all()?;
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
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

fn copy_atomic(mut source: impl Read, destination: &Path) -> Result<()> {
    create_parent(destination)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(destination.parent().context("missing parent")?)?;
    io::copy(&mut source, &mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary.persist(destination)?;
    sync_parent(destination)
}
