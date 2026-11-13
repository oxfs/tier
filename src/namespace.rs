use crate::{
    Store,
    config::validate_relative,
    descriptor,
    store::{copy_atomic, create_parent, sync_parent},
};
use anyhow::{Result, ensure};
use fs2::FileExt;
use std::os::unix::fs::MetadataExt;
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[derive(serde::Serialize, serde::Deserialize)]
struct Change {
    source: PathBuf,
    destination: Option<PathBuf>,
    directory: bool,
    // Once true recovery must never clear the destination again: a previous
    // attempt may already have moved the source there on some disks.
    prepared: bool,
}

impl Store {
    pub fn root(&self) -> &Path {
        &self.config.primary
    }

    // Namespace lookup/open holds SH briefly. Mutations hold EX and lock only
    // affected descriptors, leaving unrelated open files completely independent.
    pub(crate) fn namespace(&self, exclusive: bool) -> Result<File> {
        self.verify_root(self.root())?;
        let path = Self::path(self.root(), Path::new(".tier/namespace.lock"))?;
        create_parent(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if exclusive {
            FileExt::try_lock_exclusive(&file)?;
        } else {
            FileExt::lock_shared(&file)?;
            ensure!(
                !self.root().join(".tier/namespace.json").exists(),
                "unfinished namespace change: reopen store to recover"
            );
        }
        Ok(file)
    }

    pub(crate) fn initialize_roots(&mut self) -> Result<()> {
        use std::io::Read;
        let path = self.root().join(".tier/storage.json");
        create_parent(&path)?;
        let bootstrap = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root().join(".tier/initialize.lock"))?;
        FileExt::lock_exclusive(&bootstrap)?;
        self.identities = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Default::default(),
            Err(error) => return Err(error.into()),
        };
        for root in std::iter::once(&self.config.primary).chain(self.config.replicas.values()) {
            if self.identities.contains_key(root) {
                self.verify_root(root)?;
                continue;
            }
            ensure!(
                root.is_dir(),
                "new storage root unavailable: {}",
                root.display()
            );
            let marker = Self::path(root, Path::new(".tier/disk-id"))?;
            let id = match fs::read_to_string(&marker) {
                Ok(id) => id,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let mut random = [0; 32];
                    File::open("/dev/urandom")?.read_exact(&mut random)?;
                    let id = random
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    copy_atomic(id.as_bytes(), &marker)?;
                    id
                }
                Err(error) => return Err(error.into()),
            };
            ensure!(
                id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid disk identity"
            );
            self.identities.insert(root.clone(), id);
        }
        self.verify_root(self.root())?;
        copy_atomic(serde_json::to_vec(&self.identities)?.as_slice(), &path)
    }

    pub(crate) fn verify_root(&self, root: &Path) -> Result<()> {
        let expected = self
            .identities
            .get(root)
            .ok_or_else(|| anyhow::anyhow!("unregistered storage root"))?;
        let actual = fs::read_to_string(Self::path(root, Path::new(".tier/disk-id"))?)?;
        ensure!(
            &actual == expected,
            "storage identity changed: {}",
            root.display()
        );
        Ok(())
    }

    pub fn logical_id(&self, relative: &Path) -> Result<u64> {
        validate_relative(relative)?;
        let _namespace = self.namespace(false)?;
        let path = Self::path(self.root(), relative)?;
        if path.is_dir() {
            return Ok(fs::metadata(path)?.ino());
        }
        Ok(self.lock(relative)?.metadata()?.ino())
    }

    pub fn metadata_fd(&self, relative: &Path) -> Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        validate_relative(relative)?;
        let _namespace = self.namespace(false)?;
        let content = Self::path(self.root(), relative)?;
        if content.is_dir() {
            return Ok(File::open(content)?);
        }
        let descriptor = Self::metadata(self.root(), relative)?;
        if !content.exists() && !descriptor.is_file() {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        drop(self.lock(relative)?);
        Ok(OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
            .open(descriptor)?)
    }

    // Serialize descriptor publication without excluding other foreground
    // content handles. The descriptor flock itself excludes background work.
    pub(crate) fn state_lock(&self, relative: &Path, exclusive: bool) -> Result<File> {
        let file = self.state_file(relative)?;
        if exclusive {
            FileExt::lock_exclusive(&file)?;
        } else {
            FileExt::lock_shared(&file)?;
        }
        Ok(file)
    }

    pub(crate) fn try_state_lock(&self, relative: &Path) -> Result<Option<File>> {
        let file = self.state_file(relative)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn state_file(&self, relative: &Path) -> Result<File> {
        let path = Self::path(self.root(), &Path::new(".tier/state").join(relative))?;
        create_parent(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(file)
    }

    fn affected_locks(&self, relative: &Path) -> Result<Vec<File>> {
        fn collect(path: &Path, locks: &mut Vec<File>) -> Result<()> {
            if !path.exists() {
                return Ok(());
            }
            if path.is_dir() {
                for entry in fs::read_dir(path)? {
                    collect(&entry?.path(), locks)?;
                }
            } else {
                let file = OpenOptions::new().read(true).write(true).open(path)?;
                FileExt::try_lock_exclusive(&file)?;
                locks.push(file);
            }
            Ok(())
        }
        let mut locks = Vec::new();
        collect(&Self::metadata(self.root(), relative)?, &mut locks)?;
        Ok(locks)
    }

    /// Archive-only attributes come from the descriptor without recalling data.
    /// Archived attributes override the descriptor file's physical attributes.
    pub fn stat(&self, relative: &Path) -> Result<(Metadata, Option<crate::FileAttributes>)> {
        validate_relative(relative)?;
        let _namespace = self.namespace(false)?;
        let content = Self::path(self.root(), relative)?;
        if content.is_dir() {
            return Ok((fs::metadata(content)?, None));
        }
        if !content.exists() && !Self::metadata(self.root(), relative)?.is_file() {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        let mut lock = self.lock(relative)?;
        FileExt::try_lock_shared(&lock)?;
        let _state = self.state_lock(relative, false)?;
        if content.exists() {
            return Ok((fs::metadata(content)?, None));
        }
        let state = descriptor::load(&mut lock)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?
            .1;
        if !state.archives.is_empty() {
            return Ok((
                lock.metadata()?,
                Some(
                    state
                        .attributes
                        .ok_or_else(|| anyhow::anyhow!("archive attributes missing"))?,
                ),
            ));
        }
        // Attribute queries must not read file contents or recall remote data.
        for name in &state.replicas {
            let Some(root) = self.config.replicas.get(name) else {
                continue;
            };
            let candidate = (|| -> Result<Metadata> {
                self.verify_root(root)?;
                let metadata = fs::metadata(Self::path(root, relative)?)?;
                ensure!(
                    metadata.is_file() && metadata.len() == state.size,
                    "replica size mismatch"
                );
                Ok(metadata)
            })();
            if let Ok(metadata) = candidate {
                return Ok((metadata, None));
            }
        }
        Err(io::Error::from_raw_os_error(libc::EIO).into())
    }

    pub fn list(&self, relative: &Path) -> Result<Vec<PathBuf>> {
        validate_relative(relative)?;
        let _namespace = self.namespace(false)?;
        let content = Self::path(self.root(), relative)?;
        if !content.is_dir() {
            return Err(io::Error::from(io::ErrorKind::NotADirectory).into());
        }
        let mut names = BTreeSet::new();
        for root in [content, Self::metadata(self.root(), relative)?] {
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(root)? {
                let entry = entry?;
                if entry.file_name() == ".tier" {
                    continue;
                }
                ensure!(!entry.file_type()?.is_symlink(), "symlinks are unsupported");
                names.insert(PathBuf::from(entry.file_name()));
            }
        }
        Ok(names.into_iter().collect())
    }

    pub fn mkdir(&self, relative: &Path) -> Result<()> {
        validate_relative(relative)?;
        let _namespace = self.namespace(true)?;
        self.recover_namespace()?;
        let path = self.content(relative)?;
        if Self::metadata(self.root(), relative)?.is_file() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
        }
        fs::create_dir(&path)?;
        sync_parent(&path)
    }

    pub fn rename(&self, source: &Path, destination: &Path) -> Result<()> {
        validate_relative(source)?;
        validate_relative(destination)?;
        ensure!(
            !source.as_os_str().is_empty() && !destination.as_os_str().is_empty(),
            "cannot rename root"
        );
        if source == destination {
            self.stat(source)?;
            return Ok(());
        }
        ensure!(
            !destination.starts_with(source),
            "destination is inside source"
        );
        let _namespace = self.namespace(true)?;
        self.recover_namespace()?;
        let _files = self.affected_locks(source)?;
        let _destination_files = self.affected_locks(destination)?;
        let source_content = self.content(source)?;
        let source_meta = Self::metadata(self.root(), source)?;
        if !source_content.exists() && !source_meta.is_file() {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        let destination_content = self.content(destination)?;
        let destination_meta = Self::metadata(self.root(), destination)?;
        if destination_content.exists() || destination_meta.exists() {
            if source_content.is_dir() != destination_content.is_dir() {
                return Err(io::Error::from(if source_content.is_dir() {
                    io::ErrorKind::NotADirectory
                } else {
                    io::ErrorKind::IsADirectory
                })
                .into());
            }
            if destination_content.is_dir() {
                for path in [&destination_content, &destination_meta] {
                    if path.exists() && fs::read_dir(path)?.next().is_some() {
                        return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty).into());
                    }
                }
            }
        }
        ensure!(
            self.content(destination)?
                .parent()
                .is_some_and(Path::is_dir),
            "destination parent missing"
        );
        self.commit_namespace(Change {
            source: source.into(),
            destination: Some(destination.into()),
            directory: source_content.is_dir(),
            prepared: false,
        })
    }

    pub fn remove(&self, relative: &Path, directory: bool) -> Result<()> {
        validate_relative(relative)?;
        ensure!(!relative.as_os_str().is_empty(), "cannot remove root");
        let _namespace = self.namespace(true)?;
        self.recover_namespace()?;
        let _files = self.affected_locks(relative)?;
        let content = self.content(relative)?;
        let metadata = Self::metadata(self.root(), relative)?;
        if directory {
            if !content.is_dir() {
                return Err(io::Error::from(io::ErrorKind::NotADirectory).into());
            }
            for path in [&content, &metadata] {
                if path.exists() && fs::read_dir(path)?.next().is_some() {
                    return Err(io::Error::from(io::ErrorKind::DirectoryNotEmpty).into());
                }
            }
        } else {
            if content.is_dir() {
                return Err(io::Error::from(io::ErrorKind::IsADirectory).into());
            }
            if !content.exists() && !metadata.is_file() {
                return Err(io::Error::from(io::ErrorKind::NotFound).into());
            }
        }
        self.commit_namespace(Change {
            source: relative.into(),
            destination: None,
            directory,
            prepared: true,
        })
    }

    fn commit_namespace(&self, change: Change) -> Result<()> {
        // A disk already offline must not leave an intent that blocks otherwise
        // healthy foreground I/O. Mid-operation failures retain recovery intent.
        for root in std::iter::once(&self.config.primary).chain(self.config.replicas.values()) {
            self.verify_root(root)?;
        }
        let journal = self.root().join(".tier/namespace.json");
        copy_atomic(serde_json::to_vec(&change)?.as_slice(), &journal)?;
        self.recover_namespace()
    }

    // Durable intent makes interrupted cross-disk renames/deletes roll forward.
    // Failure retains the intent and blocks opening this Store until repaired.
    pub(crate) fn recover_namespace(&self) -> Result<()> {
        let journal = self.root().join(".tier/namespace.json");
        let bytes = match fs::read(&journal) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut change: Change = serde_json::from_slice(&bytes)?;
        validate_relative(&change.source)?;
        if let Some(destination) = &change.destination {
            validate_relative(destination)?;
        }
        if !change.prepared {
            let destination = change
                .destination
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("invalid rename intent"))?;
            for root in self
                .config
                .replicas
                .values()
                .chain(std::iter::once(&self.config.primary))
            {
                self.verify_root(root)?;
                for path in [
                    Self::path(root, destination)?,
                    Self::metadata(root, destination)?,
                    Self::path(root, &Path::new(".tier/state").join(destination))?,
                ] {
                    if !path.exists() {
                        continue;
                    }
                    if path.is_dir() {
                        fs::remove_dir(&path)?;
                    } else {
                        fs::remove_file(&path)?;
                    }
                    sync_parent(&path)?;
                }
            }
            change.prepared = true;
            copy_atomic(serde_json::to_vec(&change)?.as_slice(), &journal)?;
        }
        for root in self
            .config
            .replicas
            .values()
            .chain(std::iter::once(&self.config.primary))
        {
            self.verify_root(root)?;
            for kind in 0..3 {
                if kind == 2 && root != &self.config.primary {
                    continue;
                }
                let path = |relative: &Path| match kind {
                    1 => Self::metadata(root, relative),
                    2 => Self::path(root, &Path::new(".tier/state").join(relative)),
                    _ => Self::path(root, relative),
                };
                let source = path(&change.source)?;
                if !source.exists() {
                    continue;
                }
                if let Some(destination) = &change.destination {
                    let destination = path(destination)?;
                    create_parent(&destination)?;
                    fs::rename(&source, &destination)?;
                    sync_parent(&destination)?;
                } else if change.directory {
                    fs::remove_dir(&source)?;
                } else {
                    fs::remove_file(&source)?;
                }
                sync_parent(&source)?;
            }
        }
        fs::remove_file(&journal)?;
        sync_parent(&journal)
    }
}
