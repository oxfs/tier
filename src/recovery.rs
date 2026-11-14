//! Read-only diagnosis remains available when ordinary storage access fails closed.
use crate::config::Config;
use serde::Serialize;
use std::{collections::BTreeMap, fs, path::PathBuf};

/// Recover only the last agreed, committed mirrored state into an empty primary.
/// The caller must stop all writers; unreplicated writes cannot be recovered.
pub fn recover_primary(mut config: Config) -> anyhow::Result<usize> {
    use crate::{
        Store, descriptor,
        store::{copy_atomic, copy_file, create_parent},
    };
    use anyhow::{Context, ensure};
    use std::{io::Read, path::Path};
    config.validate()?;
    config.primary = fs::canonicalize(&config.primary)?;
    for root in config.replicas.values_mut() {
        *root = fs::canonicalize(&*root)?;
    }
    config.validate()?;
    ensure!(
        fs::read_dir(&config.primary)?.next().is_none(),
        "recovery primary must be empty"
    );
    ensure!(
        !config.replicas.is_empty(),
        "primary recovery requires local descriptor mirrors"
    );
    let mut identities = BTreeMap::new();
    let mut agreed: Option<BTreeMap<PathBuf, Vec<u8>>> = None;
    let mut directories = std::collections::BTreeSet::new();
    fn collect(
        root: &Path,
        relative: &Path,
        result: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> anyhow::Result<()> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            ensure!(
                !entry.file_type()?.is_symlink(),
                "symlink in recovery metadata"
            );
            if entry.file_type()?.is_dir() {
                collect(root, &path, result)?;
            } else {
                ensure!(entry.file_type()?.is_file(), "non-file recovery descriptor");
                let state = descriptor::load(&mut fs::File::open(entry.path())?)?
                    .context("empty recovery descriptor")?
                    .1;
                ensure!(
                    state.digest.is_some(),
                    "pending descriptor cannot be recovered"
                );
                result.insert(path, serde_json::to_vec(&state)?);
            }
        }
        Ok(())
    }
    fn collect_dirs(
        root: &Path,
        relative: &Path,
        result: &mut std::collections::BTreeSet<PathBuf>,
    ) -> anyhow::Result<()> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            if entry.file_name() == ".tier" {
                continue;
            }
            ensure!(
                !entry.file_type()?.is_symlink(),
                "symlink in recovery source"
            );
            if entry.file_type()?.is_dir() {
                let path = relative.join(entry.file_name());
                result.insert(path.clone());
                collect_dirs(root, &path, result)?;
            }
        }
        Ok(())
    }
    for root in config.replicas.values() {
        let identity = fs::read_to_string(Store::path(root, Path::new(".tier/disk-id"))?)?;
        ensure!(
            identity.len() == 64 && identity.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid survivor identity"
        );
        ensure!(
            !root.join(".tier/namespace.json").exists(),
            "unfinished namespace operation"
        );
        identities.insert(root.clone(), identity);
        let mut files = BTreeMap::new();
        collect(
            &Store::path(root, Path::new(".tier/meta"))?,
            Path::new(""),
            &mut files,
        )?;
        if let Some(previous) = &agreed {
            ensure!(
                previous == &files,
                "survivor descriptors disagree; manual reconciliation required"
            );
        } else {
            agreed = Some(files);
        }
        collect_dirs(root, Path::new(""), &mut directories)?;
    }
    config.validate()?;
    let store = Store { config, identities };
    let files = agreed.context("no descriptor source")?;
    let mut required = 0u64;
    for bytes in files.values() {
        let state: descriptor::State = serde_json::from_slice(bytes)?;
        required = required
            .checked_add(state.size)
            .context("recovery size overflow")?;
    }
    ensure!(
        fs2::available_space(store.root())? >= required,
        "insufficient primary space for recovery"
    );
    for directory in directories {
        create_parent(&Store::path(store.root(), &directory)?.join("placeholder"))?;
    }
    for (path, bytes) in &files {
        let state: descriptor::State = serde_json::from_slice(bytes)?;
        let mut source = store
            .replica(path, &state)
            .with_context(|| format!("recover {}", path.display()))?;
        copy_file(&mut source, &Store::path(store.root(), path)?, store.root())?;
        if let Some(attributes) = state.attributes {
            use std::os::unix::fs::PermissionsExt;
            let restored = fs::File::open(Store::path(store.root(), path)?)?;
            restored.set_permissions(fs::Permissions::from_mode(attributes.mode))?;
            if attributes.modified_secs >= 0 && attributes.modified_nanos >= 0 {
                let modified = std::time::UNIX_EPOCH
                    + std::time::Duration::new(
                        attributes.modified_secs as u64,
                        attributes.modified_nanos as u32,
                    );
                restored.set_times(fs::FileTimes::new().set_modified(modified))?;
            }
            restored.sync_all()?;
        }
        let metadata = Store::metadata(store.root(), path)?;
        create_parent(&metadata)?;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&metadata)?;
        descriptor::save(&mut file, &state)?;
        crate::store::sync_parent(&metadata)?;
    }
    let mut random = [0; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let identity: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    copy_atomic(identity.as_bytes(), &store.root().join(".tier/disk-id"))?;
    let mut identities = store.identities.clone();
    identities.insert(store.root().to_owned(), identity);
    copy_atomic(
        serde_json::to_vec(&identities)?.as_slice(),
        &store.root().join(".tier/storage.json"),
    )?;
    Ok(files.len())
}

/// Rebuild an empty replacement directory. Stop Samba and the daemon first.
/// On failure, preserve this directory for inspection and retry with an empty one.
pub fn replace_replica(config: Config, name: &str) -> anyhow::Result<()> {
    use crate::{
        Store, descriptor,
        store::{checksum, copy_atomic, copy_file, create_parent},
    };
    use anyhow::{Context, ensure};
    use fs2::FileExt;
    use std::{
        io::{Read, Seek},
        path::Path,
    };
    let store = Store::new(config)?;
    let destination = store.config.replicas.get(name).context("unknown replica")?;
    ensure!(destination.is_dir(), "replacement directory must exist");
    ensure!(
        fs::read_dir(destination)?.next().is_none(),
        "replacement directory must be empty; preserve failed rebuilds and use an empty destination"
    );
    store.verify_root(store.root())?;
    for (other, root) in &store.config.replicas {
        if other != name {
            store.verify_root(root)?;
        }
    }
    let namespace = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.root().join(".tier/namespace.lock"))?;
    FileExt::try_lock_exclusive(&namespace)?;
    ensure!(
        !store.root().join(".tier/namespace.json").exists(),
        "recover interrupted namespace operation before replacing disk"
    );
    let _handles = store.affected_locks(Path::new(""))?;
    fn copy_metadata(
        store: &Store,
        name: &str,
        destination: &Path,
        relative: &Path,
    ) -> anyhow::Result<()> {
        let directory = Store::metadata(store.root(), relative)?;
        if !directory.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            ensure!(!entry.file_type()?.is_symlink(), "symlink in metadata");
            if entry.file_type()?.is_dir() {
                create_parent(&Store::path(destination, &path)?.join("placeholder"))?;
                copy_metadata(store, name, destination, &path)?;
            } else {
                ensure!(entry.file_type()?.is_file(), "non-file replica descriptor");
                let mut metadata = fs::File::open(entry.path())?;
                let state = descriptor::load(&mut metadata)?
                    .context("file has no committed descriptor; reconcile before replacement")?
                    .1;
                if state.replicas.iter().any(|replica| replica == name) {
                    let digest = state.digest.as_ref().context("file awaits replication")?;
                    let primary = Store::path(store.root(), &path)?;
                    let candidate = fs::File::open(primary).ok().and_then(|mut file| {
                        (checksum(&mut file).ok() == Some((digest.clone(), state.size)))
                            .then_some(file)
                    });
                    let mut source = match candidate {
                        Some(file) => file,
                        None => store.replica(&path, &state)?,
                    };
                    source.rewind()?;
                    copy_file(&mut source, &Store::path(destination, &path)?, destination)?;
                }
                metadata.rewind()?;
                copy_atomic(metadata, &Store::metadata(destination, &path)?)?;
            }
        }
        Ok(())
    }
    fn mirror_directories(source: &Path, destination: &Path) -> anyhow::Result<()> {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if entry.file_name() == ".tier" {
                continue;
            }
            if entry.file_type()?.is_dir() {
                let directory = destination.join(entry.file_name());
                create_parent(&directory.join("placeholder"))?;
                mirror_directories(&entry.path(), &directory)?;
            }
        }
        Ok(())
    }
    mirror_directories(store.root(), destination)?;
    copy_metadata(&store, name, destination, Path::new(""))?;
    let mut random = [0; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let identity = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    copy_atomic(identity.as_bytes(), &destination.join(".tier/disk-id"))?;
    let mut identities = store.identities.clone();
    identities.insert(destination.clone(), identity);
    copy_atomic(
        serde_json::to_vec(&identities)?.as_slice(),
        &store.root().join(".tier/storage.json"),
    )?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct DiskStatus {
    pub name: String,
    pub path: PathBuf,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Diagnosis {
    pub disks: Vec<DiskStatus>,
    pub registry_error: Option<String>,
    pub unfinished_namespace_change: bool,
}

pub fn diagnose(config: &Config) -> Diagnosis {
    let registry = fs::read(config.primary.join(".tier/storage.json"))
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<BTreeMap<PathBuf, String>>(&bytes)
                .map_err(|error| error.to_string())
        });
    let disks = std::iter::once(("primary", &config.primary))
        .chain(
            config
                .replicas
                .iter()
                .map(|(name, path)| (name.as_str(), path)),
        )
        .map(|(name, path)| {
            let check = (|| -> Result<(), String> {
                let identities = registry.as_ref().map_err(Clone::clone)?;
                let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
                let expected = identities
                    .get(&canonical)
                    .ok_or("unregistered storage root")?;
                let actual = fs::read_to_string(path.join(".tier/disk-id"))
                    .map_err(|error| error.to_string())?;
                if &actual != expected {
                    return Err("storage identity changed".into());
                }
                Ok(())
            })();
            DiskStatus {
                name: name.into(),
                path: path.clone(),
                error: check.err(),
            }
        })
        .collect();
    Diagnosis {
        disks,
        registry_error: registry.err(),
        unfinished_namespace_change: config.primary.join(".tier/namespace.json").exists(),
    }
}
