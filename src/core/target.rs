use super::*;
use std::{os::fd::AsRawFd, path::PathBuf};

/// Pins the directory whose identity was verified. All object operations use
/// this descriptor even if the configured mountpoint changes underneath us.
pub(crate) struct TargetDirectory {
    directory: fs::File,
    pub(crate) root: PathBuf,
}

impl TargetDirectory {
    fn open(registered: PathBuf, identity: &str) -> Result<Self> {
        let directory = fs::File::open(&registered)?;
        let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        ensure!(
            fs::read_to_string(root.join(".tier-id"))?.trim() == identity,
            "target identity mismatch"
        );
        Ok(Self { directory, root })
    }
    pub(crate) fn open_object(&self, key: &str) -> io::Result<fs::File> {
        fs::File::open(self.root.join(key))
    }
    pub(crate) fn remove_object(&self, key: &str) -> io::Result<()> {
        let path = self.root.join(key);
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        if let Some(parent) = path.parent() {
            match fs::File::open(parent) {
                Ok(directory) => directory.sync_all()?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        // Callers serialize object creation/deletion by node. Never recursively
        // remove directories: an unpublished candidate may still be copying.
        if key.starts_with("objects/")
            && let Some(parent) = path.parent()
        {
            match fs::remove_dir(parent) {
                Ok(()) => fs::File::open(self.root.join("objects"))?.sync_all()?,
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTEMPTY)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    pub(crate) fn stat(&self) -> Result<libc::statvfs> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // fstatvfs initializes stat on success and cannot resolve a replacement mount.
        if unsafe { libc::fstatvfs(self.directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { stat.assume_init() })
    }
}

fn detect_domain(device: u64) -> String {
    let number = format!("{}:{}", libc::major(device), libc::minor(device));
    let Ok(block) = fs::canonicalize(format!("/sys/dev/block/{number}")) else {
        return format!("fs:{number}");
    };
    let disk = if block.join("partition").is_file() {
        block.parent().unwrap_or(&block)
    } else {
        &block
    };
    format!("block:{}", disk.display())
}
impl Engine {
    pub fn add_target(&self, name: &str, path: &Path) -> Result<()> {
        self.register_target(name, path, None, false)
    }
    pub fn register_target(
        &self,
        name: &str,
        path: &Path,
        domain: Option<&str>,
        allow_shared: bool,
    ) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        ensure!(safe_name(name), "invalid target name");
        let path = fs::canonicalize(path)?;
        ensure!(path.is_dir(), "target must be directory");
        let device = fs::metadata(&path)?.dev();
        let detected = detect_domain(device);
        let domain = domain
            .filter(|d| !d.is_empty())
            .unwrap_or(&detected)
            .to_owned();
        let _guard = self.inner.namespace.write();
        ensure!(
            self.kv_get::<Target>(&schema::target(name))?.is_none(),
            "target already exists"
        );
        for (_, other) in self.scan::<Target>(schema::TARGETS)? {
            ensure!(
                !path.starts_with(&other.path) && !other.path.starts_with(&path),
                "overlapping targets"
            );
            let same = fs::metadata(&other.path)
                .is_ok_and(|m| m.dev() == device || detect_domain(m.dev()) == detected);
            ensure!(
                allow_shared || (!same && other.domain != domain),
                "target shares a device or fault domain; explicit override required"
            );
        }
        let marker = path.join(".tier-id");
        let identity = match fs::read_to_string(&marker) {
            Ok(id) => Uuid::parse_str(id.trim())?.to_string(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let id = Uuid::new_v4().to_string();
                let mut file = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(marker)?;
                file.write_all(id.as_bytes())?;
                file.sync_all()?;
                fs::File::open(&path)?.sync_all()?;
                id
            }
            Err(e) => return Err(e.into()),
        };
        ensure!(
            !self
                .scan::<Target>(schema::TARGETS)?
                .iter()
                .any(|(_, t)| t.identity == identity),
            "duplicate target identity"
        );
        self.put_record(
            &schema::target(name),
            &Target {
                name: name.into(),
                path,
                identity,
                domain,
            },
            true,
        )
    }
    pub(crate) fn target(&self, name: &str) -> Result<TargetDirectory> {
        let t: Target = self.require(&schema::target(name))?;
        TargetDirectory::open(t.path, &t.identity)
    }
    pub(crate) fn target_stat(&self, name: &str) -> Result<libc::statvfs> {
        self.target(name)?.stat()
    }
    pub(crate) fn filesystem_stat(&self) -> Result<(u64, u64)> {
        let mut total = 0;
        let mut free = 0;
        let mut devices = HashSet::new();
        for (_, t) in self.scan::<Target>(schema::TARGETS)? {
            if devices.insert(t.domain)
                && let Ok(s) = self.target_stat(&t.name)
            {
                total += s.f_blocks.saturating_mul(s.f_frsize);
                free += s.f_bavail.saturating_mul(s.f_frsize);
            }
        }
        Ok((total, free))
    }
    pub fn set_policy(&self, path: &str, p: &Policy) -> Result<()> {
        let _guard = self.inner.namespace.write();
        let n = self.lookup_unlocked(path)?;
        ensure!(n.is_dir, "policy requires directory");
        ensure!(
            p.persistent_copies <= p.persistent_storage.len(),
            "invalid replica count"
        );
        ensure!(
            !p.write_cache.is_empty() || p.persistent_copies > 0,
            "select a storage target"
        );
        ensure!(
            p.write_cache.is_empty() || p.low_water < p.high_water && p.high_water <= 100,
            "invalid watermarks"
        );
        ensure!(
            !p.direct_fallback || !p.persistent_storage.is_empty(),
            "fallback requires persistent storage"
        );
        for names in [&p.write_cache, &p.persistent_storage] {
            ensure!(
                names.iter().collect::<HashSet<_>>().len() == names.len(),
                "duplicate target"
            );
            for name in names {
                let _: Target = self.require(&schema::target(name))?;
            }
        }
        self.atomic(
            vec![
                Self::encoded(schema::policy(&n.id), p)?,
                Self::rescan_record(&n.id)?,
            ],
            vec![],
            true,
        )?;
        Ok(())
    }
    pub(crate) fn create_object(
        &self,
        name: &str,
        id: &str,
    ) -> Result<(TargetDirectory, String, fs::File)> {
        let root = self.target(name)?;
        let label = self.record(id)?.name;
        let dir = root.root.join("objects").join(id);
        fs::create_dir_all(&dir)?;
        let object = format!("objects/{id}/{}", objects::name(&label));
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(root.root.join(&object))
        {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_dir(&dir);
                return Err(error.into());
            }
        };
        Ok((root, object, file))
    }
    pub(crate) fn sync_object(root: &TargetDirectory, object: &str, file: &fs::File) -> Result<()> {
        file.sync_all()?;
        let path = root.root.join(object);
        for dir in [
            path.parent().context("invalid object")?.to_path_buf(),
            root.root.join("objects"),
            root.root.clone(),
        ] {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }
    pub(crate) fn matches_copy(&self, target: &str, object: &str, hash: &str) -> bool {
        self.target(target)
            .and_then(|t| Ok(fingerprint(&mut t.open_object(object)?)?.1 == hash))
            .unwrap_or(false)
    }
    pub(crate) fn relieve_cache(
        &self,
        name: &str,
        exclude: &str,
        additional: u64,
        high_water: u8,
    ) -> Result<bool> {
        let fits = || -> Result<bool> {
            let stat = self.target_stat(name)?;
            let free = stat.f_bavail.saturating_mul(stat.f_frsize);
            let used = 100
                - stat
                    .f_bavail
                    .saturating_sub(additional.div_ceil(stat.f_frsize.max(1)))
                    .saturating_mul(100)
                    / stat.f_blocks.max(1);
            Ok(free >= additional && used < u64::from(high_water))
        };
        if fits()? {
            return Ok(true);
        }
        for (_, candidate) in self.scan::<Record>(schema::NODES)? {
            let id = &candidate.node.id;
            if id == exclude || candidate.node.is_dir || candidate.dirty || candidate.hash.is_none()
            {
                continue;
            }
            let lock = self.file_lock(id);
            let Some(mut state) = lock.try_lock() else {
                continue;
            };
            if state.handles != 0 {
                continue;
            }
            let mut record = self.record(id)?;
            if record.dirty || record.node.version != candidate.node.version {
                continue;
            }
            let policy = self.policy(id)?;
            if !policy.write_cache.contains(&name.to_owned())
                || policy.persistent_storage.contains(&name.to_owned())
                || policy.persistent_copies == 0
            {
                continue;
            }
            let copies = self.copies(id)?;
            let Some(old) = copies.iter().find(|c| c.target == name) else {
                continue;
            };
            let hash = record.hash.as_deref().context("missing protected digest")?;
            let verified = copies
                .iter()
                .filter(|c| {
                    c.target != name
                        && c.version == record.node.version
                        && policy.persistent_storage.contains(&c.target)
                        && self.matches_copy(&c.target, &c.object, hash)
                })
                .collect::<Vec<_>>();
            if verified.len() < policy.persistent_copies {
                continue;
            }
            let _namespace = self.inner.namespace.read();
            if self.policy(id)? != policy {
                continue;
            }
            if record.primary.as_deref() == Some(name) {
                record.primary = Some(verified[0].target.clone());
            }
            let garbage = schema::garbage(name);
            self.atomic(
                vec![
                    Self::encoded(schema::node(id), &record)?,
                    Self::encoded(garbage.clone(), old)?,
                ],
                vec![schema::copy(id, name)],
                true,
            )?;
            state.body = None;
            drop(_namespace);
            if let Ok(root) = self.target(name)
                && root.remove_object(&old.object).is_ok()
            {
                self.delete_record(&garbage)?;
            }
            if fits()? {
                return Ok(true);
            }
        }
        fits()
    }

    pub(crate) fn choose_target(&self, policy: &Policy, id: &str) -> Result<String> {
        let mut cache = policy.write_cache.clone();
        rank(&mut cache, id);
        let mut persistent = policy.persistent_storage.clone();
        rank(&mut persistent, id);
        for name in cache {
            if let Ok(s) = self.target_stat(&name) {
                let used = 100 - s.f_bavail.saturating_mul(100) / s.f_blocks.max(1);
                if used < u64::from(policy.high_water)
                    || self
                        .relieve_cache(&name, id, 0, policy.high_water)
                        .unwrap_or(false)
                {
                    return Ok(name);
                }
            }
        }
        if policy.write_cache.is_empty() || policy.direct_fallback {
            for name in persistent {
                if let Ok(s) = self.target_stat(&name)
                    && s.f_bavail > 0
                {
                    return Ok(name);
                }
            }
        }
        Err(failure(libc::ENOSPC))
    }
    pub(crate) fn collect_garbage(&self) -> Result<()> {
        for (k, c) in self.scan::<Copy>(schema::GARBAGE)? {
            let lock = self.file_lock(&c.node);
            let _guard = lock.lock();
            // A -> B -> A may reuse a still-existing hard-link name. Never
            // unlink a path now selected by the authoritative copy record.
            if self
                .kv_get::<Copy>(&schema::copy(&c.node, &c.target))?
                .is_some_and(|current| current.object == c.object)
            {
                self.delete_record(&k)?;
                continue;
            }
            if let Ok(t) = self.target(&c.target) {
                match t.remove_object(&c.object) {
                    Ok(()) => self.delete_record(&k)?,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => self.delete_record(&k)?,
                    Err(_) => {}
                }
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remount_cannot_redirect_reads_or_deletes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let mount = tmp.path().join("mount");
        let original = tmp.path().join("original");
        fs::create_dir(&mount)?;
        fs::write(mount.join(".tier-id"), "first")?;
        fs::write(mount.join("object"), "first")?;
        let target = TargetDirectory::open(mount.clone(), "first")?;
        fs::rename(&mount, &original)?;
        fs::create_dir(&mount)?;
        fs::write(mount.join(".tier-id"), "second")?;
        fs::write(mount.join("object"), "second")?;
        assert!(TargetDirectory::open(mount.clone(), "first").is_err());
        let mut contents = String::new();
        target
            .open_object("object")?
            .read_to_string(&mut contents)?;
        assert_eq!(contents, "first");
        target.remove_object("object")?;
        assert!(!original.join("object").exists());
        assert_eq!(fs::read_to_string(mount.join("object"))?, "second");
        Ok(())
    }
}
