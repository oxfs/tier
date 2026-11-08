use super::*;
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
    pub(crate) fn target(&self, name: &str) -> Result<PathBuf> {
        let (path, id): (String, String) = self.db.query_row(
            "SELECT path,identity FROM targets WHERE name=?",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let path = PathBuf::from(path);
        ensure!(
            fs::read_to_string(path.join(".tier-id"))?.trim() == id,
            "target identity mismatch"
        );
        Ok(path)
    }
    pub fn set_policy(&self, path: &str, p: &Policy) -> Result<()> {
        let n = self.stat(path)?;
        ensure!(n.is_dir, "policy requires directory");
        for (names, count) in [(&p.front, p.front_count), (&p.long, p.long_count)] {
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
            p.low_water < p.high_water && p.high_water <= 100,
            "invalid watermarks"
        );
        ensure!(p.front_count > 0, "empty confirmation set");
        self.db.execute(
            "INSERT OR REPLACE INTO policies VALUES(?,?)",
            params![n.id, serde_json::to_string(p)?],
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
        use std::os::fd::AsRawFd;
        let (registered, identity): (String, String) = self.db.query_row(
            "SELECT path,identity FROM targets WHERE name=?",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let directory = fs::File::open(registered)?;
        let root = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        ensure!(
            fs::read_to_string(root.join(".tier-id"))?.trim() == identity,
            "target identity mismatch"
        );
        let dir = root.join("objects").join(id);
        fs::create_dir_all(&dir)?;
        let key = format!("objects/{id}/{}", Uuid::new_v4());
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.join(&key))?;
        data.rewind()?;
        if let Err(error) = io::copy(data, &mut f) {
            let _ = fs::remove_file(root.join(&key));
            return Err(error.into());
        }
        f.sync_all()?;
        for d in [&dir, &root.join("objects"), &root] {
            fs::File::open(d)?.sync_all()?;
        }
        Ok(key)
    }
    pub(crate) fn matches_copy(&self, target: &str, key: &str, expected: &str) -> bool {
        self.target(target)
            .and_then(|r| Ok(fingerprint(&mut fs::File::open(r.join(key))?)?.1 == expected))
            .unwrap_or(false)
    }
    pub(super) fn relieve_pressure(&self, name: &str, threshold: u8) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let root = self.target(name)?;
        let cpath = std::ffi::CString::new(root.as_os_str().as_bytes())?;
        let utilization = || -> Result<u64> {
            let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // statvfs initializes the entire output structure on success.
            if unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let stat = unsafe { stat.assume_init() };
            Ok(100 - stat.f_bavail.saturating_mul(100) / stat.f_blocks.max(1))
        };
        if utilization()? < threshold as u64 {
            return Ok(());
        }
        let keys=self.db.prepare("SELECT c.node,c.object FROM copies c JOIN nodes n ON n.id=c.node WHERE c.target=? ORDER BY n.mtime")?.query_map([name],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, key) in keys {
            let tx = self.db.unchecked_transaction()?;
            let Ok(p) = self.policy(&id) else { continue };
            if p.long.iter().any(|v| v == name) || p.long_count == 0 {
                continue;
            }
            let hash: String =
                tx.query_row("SELECT hash FROM nodes WHERE id=?", [&id], |r| r.get(0))?;
            let mut available = 0;
            for target in &p.long {
                let other:Option<String>=tx.query_row("SELECT object FROM copies c JOIN nodes n ON c.node=n.id WHERE node=? AND target=? AND c.hash=n.hash",params![id,target],|r|r.get(0)).optional()?;
                if other.is_some_and(|k| self.matches_copy(target, &k, &hash)) {
                    available += 1
                }
            }
            if available < p.long_count {
                continue;
            }
            if tx.execute(
                "DELETE FROM copies WHERE node=? AND target=? AND object=?",
                params![id, name, key],
            )? > 0
            {
                tx.commit()?;
                fs::remove_file(root.join(key))?;
            }
            if utilization()? < threshold as u64 {
                return Ok(());
            }
        }
        bail!("target above watermark")
    }
}
