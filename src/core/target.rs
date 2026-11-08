use super::*;
use std::os::fd::AsRawFd;

/// Pins the directory whose identity was verified. All object operations use
/// this descriptor even if the configured mountpoint changes underneath us.
pub(crate) struct TargetDirectory {
    directory: fs::File,
    root: PathBuf,
    registered: PathBuf,
}

impl TargetDirectory {
    fn open(registered: PathBuf, identity: &str) -> Result<Self> {
        let directory = fs::File::open(&registered)?;
        let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        ensure!(
            fs::read_to_string(root.join(".tier-id"))?.trim() == identity,
            "target identity mismatch"
        );
        Ok(Self {
            directory,
            root,
            registered,
        })
    }
    pub(super) fn open_object(&self, key: &str) -> io::Result<fs::File> {
        fs::File::open(self.root.join(key))
    }
    pub(super) fn remove_object(&self, key: &str) -> io::Result<()> {
        fs::remove_file(self.root.join(key))
    }
    pub(super) fn cache_key(&self, key: &str) -> PathBuf {
        self.registered.join(key)
    }
    pub(super) fn stat(&self) -> Result<libc::statvfs> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // fstatvfs initializes stat on success and cannot resolve a replacement mount.
        if unsafe { libc::fstatvfs(self.directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { stat.assume_init() })
    }
}

pub(super) struct Candidates<'a> {
    remaining: Vec<(&'a String, String)>,
    used: HashSet<String>,
}
impl<'a> Candidates<'a> {
    pub(super) fn next(&mut self) -> Option<(&'a String, String)> {
        if self.remaining.is_empty() {
            return None;
        }
        let index = self
            .remaining
            .iter()
            .position(|(_, domain)| !self.used.contains(domain))
            .unwrap_or(0);
        Some(self.remaining.remove(index))
    }
    pub(super) fn confirmed(&mut self, domain: String) {
        self.used.insert(domain);
    }
}
impl Engine {
    pub(super) fn candidates<'a>(&self, targets: &'a [String]) -> Result<Candidates<'a>> {
        let mut query = self
            .db
            .prepare_cached("SELECT domain FROM targets WHERE name=?")?;
        let remaining = targets
            .iter()
            .map(|name| Ok((name, query.query_row([name], |r| r.get(0))?)))
            .collect::<Result<_>>()?;
        Ok(Candidates {
            remaining,
            used: HashSet::new(),
        })
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
        ensure!(!name.is_empty(), "empty target name");
        let device = fs::metadata(path)?.dev();
        let detected = detect_domain(device);
        let domain = domain
            .filter(|value| !value.is_empty())
            .unwrap_or(&detected);
        let others = self
            .db
            .prepare("SELECT path,domain FROM targets")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let proposed = fs::canonicalize(path)?;
        for (other, old_domain) in others {
            let other = PathBuf::from(other);
            ensure!(
                !proposed.starts_with(&other) && !other.starts_with(&proposed),
                "overlapping targets"
            );
            let shared_device = fs::metadata(&other)
                .is_ok_and(|info| info.dev() == device || detect_domain(info.dev()) == detected);
            ensure!(
                allow_shared || (!shared_device && old_domain != domain),
                "target shares a device or fault domain; explicit override required"
            );
        }
        ensure!(path.is_dir(), "target directory must exist");
        let path = fs::canonicalize(path)?;
        let marker = path.join(".tier-id");
        let identity = match fs::read_to_string(&marker) {
            Ok(id) => Uuid::parse_str(id.trim())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Uuid::new_v4();
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(marker)?;
                write!(file, "{id}")?;
                file.sync_all()?;
                fs::File::open(&path)?.sync_all()?;
                id
            }
            Err(e) => return Err(e.into()),
        };
        self.db.execute(
            "INSERT INTO targets VALUES(?,?,?,?)",
            params![
                name,
                path.to_str().context("invalid path encoding")?,
                identity.to_string(),
                domain
            ],
        )?;
        Ok(())
    }
    pub(crate) fn target(&self, name: &str) -> Result<TargetDirectory> {
        let (path, id): (String, String) = self.db.query_row(
            "SELECT path,identity FROM targets WHERE name=?",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        TargetDirectory::open(path.into(), &id)
    }
    pub(crate) fn target_stat(&self, name: &str) -> Result<libc::statvfs> {
        self.target(name)?.stat()
    }
    pub fn set_policy(&self, path: &str, p: &Policy) -> Result<()> {
        let n = self.stat(path)?;
        ensure!(n.is_dir, "policy requires directory");
        let mut p = p.clone();
        if p.write_cache.is_empty() {
            p.cache_copies = 0;
            p.direct_fallback = false;
            p.fallback_count = None;
        }
        for (names, count) in [
            (&p.write_cache, p.cache_copies),
            (&p.persistent_storage, p.persistent_copies),
        ] {
            ensure!(
                count <= names.len() && names.iter().collect::<HashSet<_>>().len() == names.len(),
                "invalid target count"
            );
            for name in names {
                self.db
                    .query_row("SELECT 1 FROM targets WHERE name=?", [name], |_| Ok(()))?;
            }
        }
        ensure!(
            p.write_cache.is_empty() || (p.low_water < p.high_water && p.high_water <= 100),
            "invalid watermarks"
        );
        if p.direct_fallback {
            ensure!(
                (1..=p.persistent_storage.len())
                    .contains(&p.fallback_count.unwrap_or(p.persistent_copies)),
                "invalid direct fallback count"
            );
        }
        ensure!(
            if p.write_cache.is_empty() {
                p.persistent_copies > 0
            } else {
                p.cache_copies > 0
            },
            "select at least one confirmation target"
        );
        self.db.execute(
            "INSERT OR REPLACE INTO policies VALUES(?,?)",
            params![n.id, serde_json::to_string(&p)?],
        )?;
        Ok(())
    }
    pub(crate) fn policy(&self, id: &str) -> Result<Policy> {
        inherited_policy(&self.db, id)
    }
    pub(super) fn store<R: Read + Seek>(
        &self,
        target: &str,
        id: &str,
        data: &mut R,
    ) -> Result<String> {
        let target = self.target(target)?;
        let root = &target.root;
        let dir = root.join("objects").join(id);
        fs::create_dir_all(&dir)?;
        let key = format!("objects/{id}/{}", Uuid::new_v4());
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.join(&key))?;
        let write = (|| -> Result<()> {
            let len = data.seek(io::SeekFrom::End(0))?;
            data.rewind()?;
            if len > 0 {
                // Reserve actual blocks on the verified target before copying bytes.
                let result =
                    unsafe { libc::posix_fallocate(f.as_raw_fd(), 0, i64::try_from(len)?) };
                if result != 0 {
                    return Err(io::Error::from_raw_os_error(result).into());
                }
            }
            ensure!(io::copy(data, &mut f)? == len, "source length changed");
            f.sync_all()?;
            for d in [&dir, &root.join("objects"), root] {
                fs::File::open(d)?.sync_all()?;
            }
            Ok(())
        })();
        if let Err(error) = write {
            let _ = fs::remove_file(root.join(&key));
            return Err(error);
        }
        Ok(key)
    }
    pub(crate) fn matches_copy(&self, target: &str, key: &str, expected: &str) -> bool {
        self.target(target)
            .and_then(|r| Ok(fingerprint(&mut r.open_object(key)?)?.1 == expected))
            .unwrap_or(false)
    }
    pub(super) fn relieve_pressure(
        &self,
        name: &str,
        threshold: u8,
        incoming: Option<u64>,
    ) -> Result<()> {
        let root = self.target(name)?;
        let utilization = || -> Result<u64> {
            let stat = root.stat()?;
            let required_blocks = incoming.unwrap_or(0).div_ceil(stat.f_frsize.max(1));
            Ok(100
                - stat
                    .f_bavail
                    .saturating_sub(required_blocks)
                    .saturating_mul(100)
                    / stat.f_blocks.max(1))
        };
        if utilization()? < threshold as u64 {
            return Ok(());
        }
        // Cursor is local to this connection. Each attempt inspects a bounded window.
        self.db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS reclaim_cursor(target TEXT PRIMARY KEY,node TEXT NOT NULL)")?;
        let cursor: String = self
            .db
            .query_row(
                "SELECT node FROM reclaim_cursor WHERE target=?",
                [name],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default();
        let limit = if incoming.is_some() { 8 } else { 32 };
        let keys = self
            .db
            .prepare(
                "SELECT node,object FROM copies WHERE target=? AND node>? ORDER BY node LIMIT ?",
            )?
            .query_map(params![name, cursor, limit], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next = if keys.len() == limit {
            keys.last().map(|(id, _)| id.as_str()).unwrap_or("")
        } else {
            ""
        };
        self.db.execute(
            "INSERT OR REPLACE INTO reclaim_cursor VALUES(?,?)",
            params![name, next],
        )?;
        for (id, key) in keys {
            let tx = self.db.unchecked_transaction()?;
            let pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_writes WHERE node=?)",
                [&id],
                |r| r.get(0),
            )?;
            if pending {
                continue;
            }
            let Ok(p) = self.policy(&id) else { continue };
            if p.persistent_storage.iter().any(|v| v == name) || p.persistent_copies == 0 {
                continue;
            }
            let hash: String =
                tx.query_row("SELECT hash FROM nodes WHERE id=?", [&id], |r| r.get(0))?;
            let mut available = 0;
            for target in &p.persistent_storage {
                let sql = "SELECT object
                    FROM copies c
                    JOIN nodes n ON c.node=n.id
                    WHERE node=? AND target=? AND c.hash=n.hash";
                let other: Option<String> = tx
                    .query_row(sql, params![id, target], |r| r.get(0))
                    .optional()?;
                if other.is_some_and(|k| self.cached_verified(target, &k, &hash)) {
                    available += 1;
                    if available >= p.persistent_copies {
                        break;
                    }
                }
            }
            if available < p.persistent_copies {
                continue;
            }
            if tx.execute(
                "DELETE FROM copies WHERE node=? AND target=? AND object=?",
                params![id, name, key],
            )? > 0
            {
                tx.commit()?;
                root.remove_object(&key)?;
                if incoming.is_none() {
                    return Ok(());
                }
            }
            if utilization()? < threshold as u64 {
                return Ok(());
            }
        }
        Err(io::Error::from_raw_os_error(libc::ENOSPC)).context("target above watermark")
    }
}

#[cfg(test)]
mod target_directory_tests {
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
