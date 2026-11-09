use crate::{
    archive::{ArchiveCopy, ArchivePolicy, ArchiveTarget, configured_client},
    core::{Copy, Engine, Policy, Record, Target, Trash, safe_name},
    snapshot::{self, Image},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::PermissionsExt,
    },
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

struct Catalog {
    image: Image,
    nodes: BTreeMap<String, Record>,
    targets: BTreeMap<String, Target>,
    copies: Vec<Copy>,
    archives: BTreeMap<String, ArchiveTarget>,
    archived: Vec<ArchiveCopy>,
    trash: BTreeMap<String, Trash>,
    claimed: Vec<Copy>,
}

impl Catalog {
    fn parse(value: &Value) -> Result<Self> {
        let image = snapshot::decode(value)?;
        let mut result = Self {
            image,
            nodes: BTreeMap::new(),
            targets: BTreeMap::new(),
            copies: Vec::new(),
            archives: BTreeMap::new(),
            archived: Vec::new(),
            trash: BTreeMap::new(),
            claimed: Vec::new(),
        };
        let mut keys = BTreeSet::new();
        let mut entries = BTreeMap::new();
        let mut policies = Vec::new();
        let mut archive_policies = Vec::new();
        for row in &result.image.records {
            ensure!(keys.insert(row.key.clone()), "duplicate snapshot key");
            let bytes = row.bytes()?;
            let key = row.key.as_str();
            if let Some(id) = key.strip_prefix("node/") {
                let node: Record = decode_record(&bytes)?;
                ensure!(
                    node.node.id == id && (id == "root" || Uuid::parse_str(id).is_ok()),
                    "invalid node identity"
                );
                if id != "root" {
                    ensure!(
                        safe_name(&node.name) && node.name.len() <= 255,
                        "invalid node name"
                    );
                }
                if node.node.is_dir {
                    ensure!(
                        node.node.size == 0
                            && node.primary.is_none()
                            && node.hash.is_none()
                            && !node.dirty,
                        "invalid directory state"
                    );
                }
                if let Some(hash) = &node.hash {
                    digest(hash)?;
                }
                result.nodes.insert(id.into(), node);
            } else if key.starts_with("entry/") {
                entries.insert(key.to_owned(), decode_record::<String>(&bytes)?);
            } else if let Some(name) = key.strip_prefix("target/") {
                let target: Target = decode_record(&bytes)?;
                ensure!(
                    target.name == name && safe_name(name) && target.path.is_absolute(),
                    "invalid target"
                );
                Uuid::parse_str(&target.identity)?;
                result.targets.insert(name.into(), target);
            } else if key.starts_with("copy/") {
                let copy: Copy = decode_record(&bytes)?;
                ensure!(
                    key == format!("copy/{}/{}", copy.node, copy.target),
                    "invalid copy key"
                );
                object_key(&copy)?;
                if let Some(hash) = &copy.hash {
                    digest(hash)?;
                }
                result.copies.push(copy.clone());
                result.claimed.push(copy);
            } else if let Some(id) = key.strip_prefix("policy/") {
                policies.push((id.to_owned(), decode_record::<Policy>(&bytes)?));
            } else if let Some(id) = key.strip_prefix("archive_policy/") {
                archive_policies.push((id.to_owned(), decode_record::<ArchivePolicy>(&bytes)?));
            } else if let Some(name) = key.strip_prefix("archive_target/") {
                let target: ArchiveTarget = decode_record(&bytes)?;
                ensure!(
                    target.name == name && safe_name(name),
                    "invalid archive target"
                );
                ensure!(
                    (target.url.starts_with("http://") || target.url.starts_with("https://"))
                        && !target.url.contains(['?', '#', '@']),
                    "invalid archive URL"
                );
                ensure!(
                    target.user_env.is_some() == target.pass_env.is_some(),
                    "incomplete archive credentials"
                );
                result.archives.insert(name.into(), target);
            } else if key.starts_with("archive_copy/") || key.starts_with("archive_gc/") {
                let copy: ArchiveCopy = decode_record(&bytes)?;
                Uuid::parse_str(&copy.key)?;
                digest(&copy.hash)?;
                if key.starts_with("archive_copy/") {
                    ensure!(
                        key == format!("archive_copy/{}/{}", copy.node, copy.target),
                        "invalid archive copy key"
                    );
                    result.archived.push(copy);
                }
            } else if key.starts_with("trash/") {
                let trash: Trash = decode_record(&bytes)?;
                ensure!(
                    trash.id > 0 && key == format!("trash/{:020}", trash.id),
                    "invalid trash key"
                );
                let original = trash
                    .original
                    .strip_prefix('/')
                    .context("invalid trash path")?;
                ensure!(original.split('/').all(safe_name), "invalid trash path");
                ensure!(
                    result.trash.insert(trash.node.clone(), trash).is_none(),
                    "duplicate trash node"
                );
            } else if key.starts_with("garbage/") {
                let copy: Copy = decode_record(&bytes)?;
                object_key(&copy)?;
                result.claimed.push(copy);
            } else {
                match key {
                    "system/format" => ensure!(
                        decode_record::<String>(&bytes)? == "tier-rocksdb-v1",
                        "unsupported metadata format"
                    ),
                    "system/id" => ensure!(
                        decode_record::<String>(&bytes)? == result.image.system_id,
                        "system identity mismatch"
                    ),
                    "system/revision" => ensure!(
                        decode_record::<u64>(&bytes)? == result.image.seq,
                        "revision mismatch"
                    ),
                    "system/trash_seq" => {
                        ensure!(decode_record::<i64>(&bytes)? >= 0, "invalid trash sequence")
                    }
                    "backup/schedule" => {
                        let s: crate::backup::Schedule = decode_record(&bytes)?;
                        ensure!(
                            s.seconds > 0
                                && s.changes > 0
                                && s.keep > 0
                                && s.directories.iter().all(|p| p.is_absolute()),
                            "invalid snapshot schedule"
                        );
                    }
                    _ => bail!("unknown snapshot record: {key}"),
                }
            }
        }
        for required in ["system/format", "system/id", "system/revision", "node/root"] {
            ensure!(
                keys.contains(required),
                "missing snapshot record {required}"
            );
        }
        let root = result.nodes.get("root").context("missing root")?;
        ensure!(
            root.node.is_dir && root.parent.is_none() && root.name.is_empty(),
            "invalid root"
        );
        let mut expected = BTreeMap::new();
        for (id, node) in &result.nodes {
            if let Some(parent) = &node.parent {
                ensure!(
                    result.nodes.get(parent).is_some_and(|p| p.node.is_dir),
                    "invalid parent"
                );
                let key = format!(
                    "entry/{parent}/{}",
                    unicase::UniCase::new(&node.name).to_folded_case()
                );
                ensure!(
                    expected.insert(key, id.clone()).is_none(),
                    "duplicate sibling name"
                );
            }
            result.path(id)?;
        }
        ensure!(entries == expected, "directory index disagrees with nodes");
        for (id, trash) in &result.trash {
            ensure!(trash.expires >= trash.deleted, "invalid trash expiry");
            validate_policy(&trash.policy, &result.targets)?;
            if let Some(policy) = &trash.archive_policy {
                validate_archive_policy(policy, &result.archives)?;
            }
            ensure!(
                id != "root"
                    && result.nodes.get(id).is_some_and(|n| n.parent.is_none())
                    && trash.node == *id,
                "invalid trash root"
            );
        }
        let trash_seq = result
            .image
            .records
            .iter()
            .find(|row| row.key == "system/trash_seq")
            .map(|row| decode_record::<i64>(&row.bytes()?))
            .transpose()?
            .unwrap_or(0);
        ensure!(
            result.trash.values().all(|trash| trash.id <= trash_seq),
            "trash sequence precedes records"
        );
        let mut identities = BTreeSet::new();
        for target in result.targets.values() {
            ensure!(
                identities.insert(&target.identity),
                "duplicate target identity"
            );
        }
        for copy in &result.claimed {
            ensure!(
                result.targets.contains_key(&copy.target),
                "object target missing"
            );
        }
        for node in result.nodes.values() {
            if let Some(primary) = &node.primary {
                ensure!(
                    result.targets.contains_key(primary),
                    "primary target missing"
                );
            }
        }
        for copy in &result.copies {
            let node = result.nodes.get(&copy.node).context("copy without node")?;
            ensure!(
                !node.node.is_dir
                    && copy.version <= node.node.version
                    && result.targets.contains_key(&copy.target),
                "invalid copy reference"
            );
        }
        for copy in &result.archived {
            let node = result
                .nodes
                .get(&copy.node)
                .context("archive without node")?;
            ensure!(
                !node.node.is_dir
                    && copy.version <= node.node.version
                    && result.archives.contains_key(&copy.target),
                "invalid archive reference"
            );
        }
        for (id, policy) in policies {
            ensure!(
                result.nodes.get(&id).is_some_and(|n| n.node.is_dir),
                "policy without directory"
            );
            validate_policy(&policy, &result.targets)?;
        }
        for (id, policy) in archive_policies {
            ensure!(
                result.nodes.get(&id).is_some_and(|n| n.node.is_dir),
                "archive policy without directory"
            );
            validate_archive_policy(&policy, &result.archives)?;
        }
        Ok(result)
    }

    fn path(&self, id: &str) -> Result<(PathBuf, Option<i64>)> {
        let mut cursor = id;
        let mut parts = Vec::new();
        let mut visited = BTreeSet::new();
        loop {
            ensure!(visited.insert(cursor), "directory cycle");
            let node = self.nodes.get(cursor).context("missing parent")?;
            if cursor == "root" {
                return Ok((parts.iter().rev().collect(), None));
            }
            if let Some(parent) = node.parent.as_deref() {
                parts.push(node.name.as_str());
                cursor = parent;
            } else {
                let trash = self
                    .trash
                    .get(cursor)
                    .context("detached node has no trash entry")?;
                let mut path = PathBuf::from(trash.original.trim_start_matches('/'));
                for part in parts.iter().rev() {
                    path.push(part);
                }
                return Ok((path, Some(trash.id)));
            }
        }
    }
}

fn validate_policy(policy: &Policy, targets: &BTreeMap<String, Target>) -> Result<()> {
    ensure!(
        policy.write_cache.is_empty()
            || policy.low_water < policy.high_water && policy.high_water <= 100,
        "invalid cache watermarks"
    );
    ensure!(
        !policy.write_cache.is_empty() || policy.persistent_copies > 0,
        "empty storage policy"
    );
    ensure!(
        policy.persistent_copies <= policy.persistent_storage.len(),
        "invalid replica count"
    );
    ensure!(
        !policy.direct_fallback || !policy.persistent_storage.is_empty(),
        "fallback needs persistent storage"
    );
    for names in [&policy.write_cache, &policy.persistent_storage] {
        ensure!(
            names.iter().collect::<BTreeSet<_>>().len() == names.len(),
            "duplicate policy target"
        );
        for name in names {
            ensure!(targets.contains_key(name), "policy target missing");
        }
    }
    Ok(())
}
fn validate_archive_policy(
    policy: &ArchivePolicy,
    targets: &BTreeMap<String, ArchiveTarget>,
) -> Result<()> {
    ensure!(
        policy.copies <= policy.targets.len(),
        "invalid archive replica count"
    );
    ensure!(
        policy.targets.iter().collect::<BTreeSet<_>>().len() == policy.targets.len(),
        "duplicate archive policy target"
    );
    for name in &policy.targets {
        ensure!(targets.contains_key(name), "archive target missing");
    }
    Ok(())
}

fn decode_record<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, trailing) = postcard::take_from_bytes(bytes)?;
    ensure!(trailing.is_empty(), "trailing bytes in snapshot record");
    Ok(value)
}

fn digest(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid digest"
    );
    Ok(())
}
fn object_key(copy: &Copy) -> Result<&str> {
    let parts = copy.object.split('/').collect::<Vec<_>>();
    ensure!(
        parts.len() == 3
            && parts[0] == "objects"
            && parts[1] == copy.node
            && Uuid::parse_str(parts[1]).is_ok()
            && Uuid::parse_str(parts[2]).is_ok(),
        "invalid object path"
    );
    Ok(&copy.object)
}
fn section() -> Value {
    json!({"verified":[],"verified_remote":[],"missing":[],"damaged":[],"missing_copies":[],"unconfirmed":[],"remote_errors":[]})
}
fn label(path: &Path, trash: Option<i64>) -> Value {
    match trash {
        Some(id) => json!({"entry":id,"path":path}),
        None => json!(path),
    }
}
fn push(report: &mut Value, key: &str, value: Value) {
    report[key].as_array_mut().unwrap().push(value);
}

struct Device {
    path: PathBuf,
    handle: fs::File,
}
impl Device {
    fn open(&self, relative: &str) -> io::Result<fs::File> {
        let mut directory = self.handle.try_clone()?;
        let parts = relative.split('/').collect::<Vec<_>>();
        for (index, part) in parts.iter().enumerate() {
            if !safe_name(part) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            let name = std::ffi::CString::new(*part)
                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            let is_directory = index + 1 < parts.len();
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | if is_directory {
                    libc::O_DIRECTORY
                } else {
                    libc::O_NONBLOCK
                };
            // Each directory is pinned before the next component is resolved.
            let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            directory = unsafe { fs::File::from_raw_fd(fd) };
        }
        Ok(directory)
    }
    fn identity(&self) -> Result<String> {
        let mut identity = String::new();
        let file = self.open(".tier-id")?;
        ensure!(
            file.metadata()?.is_file(),
            "target identity must be a regular file"
        );
        file.take(128).read_to_string(&mut identity)?;
        Ok(Uuid::parse_str(identity.trim())?.to_string())
    }
}

fn inspect_catalog(
    catalog: &Catalog,
    roots: &[PathBuf],
    destination: Option<&Path>,
    remote: bool,
) -> Result<Value> {
    let mut devices = BTreeMap::new();
    for path in roots {
        let handle = fs::File::open(path)?;
        let device = Device {
            path: fs::canonicalize(path)?,
            handle,
        };
        ensure!(
            device.handle.metadata()?.is_dir(),
            "recovery target must be a directory"
        );
        let identity = device.identity()?;
        let target = catalog
            .targets
            .values()
            .find(|t| t.identity == identity.trim())
            .context("unknown target identity")?;
        ensure!(
            devices.insert(target.name.clone(), device).is_none(),
            "duplicate recovery target"
        );
    }
    if let Some(dest) = destination {
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs::canonicalize(parent)?;
        ensure!(
            devices.values().all(|d| !parent.starts_with(&d.path)),
            "extract outside the source devices"
        );
        fs::create_dir(dest).context("extraction requires a new output directory")?;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o700))?;
    }
    let mut live = section();
    let mut trash = section();
    let mut good_copies = Vec::new();
    let mut good_archives = Vec::new();
    for (id, node) in &catalog.nodes {
        let (path, trash_id) = catalog.path(id)?;
        let output = destination.filter(|_| trash_id.is_none());
        if node.node.is_dir {
            if let Some(out) = output {
                fs::create_dir_all(out.join(&path))?;
            }
            continue;
        }
        let report = if trash_id.is_some() {
            &mut trash
        } else {
            &mut live
        };
        let display = label(&path, trash_id);
        let mut found = false;
        if node.dirty || node.hash.is_none() {
            push(report, "unconfirmed", display.clone());
        }
        for copy in catalog
            .copies
            .iter()
            .filter(|c| c.node == *id && c.version == node.node.version)
        {
            let Some(device) = devices.get(&copy.target) else {
                continue;
            };
            let display_object = device.path.join(&copy.object);
            let mut source = match device.open(object_key(copy)?) {
                Ok(source) => source,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    push(report, "missing_copies", json!(display_object));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            ensure!(
                source.metadata()?.is_file(),
                "object must be a regular file"
            );
            let dest = output.filter(|_| !found).map(|root| root.join(&path));
            let mut stage = if let Some(dest) = &dest {
                let parent = dest.parent().context("invalid extract path")?;
                fs::create_dir_all(parent)?;
                Some(NamedTempFile::new_in(parent)?)
            } else {
                None
            };
            let mut hash = Sha256::new();
            let mut bytes = [0u8; 128 * 1024];
            let mut size = 0u64;
            loop {
                let n = source.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                hash.update(&bytes[..n]);
                if let Some(out) = &mut stage {
                    out.write_all(&bytes[..n])?;
                }
            }
            let hash = format!("{:x}", hash.finalize());
            let valid = !node.dirty
                && node.hash.as_ref() == Some(&hash)
                && size == node.node.size
                && copy.hash.as_ref() == Some(&hash);
            if !valid {
                push(report, "damaged", json!(display_object));
                continue;
            }
            good_copies.push(format!("copy/{}/{}", copy.node, copy.target));
            if !found {
                if let (Some(stage), Some(dest)) = (stage, dest) {
                    stage.as_file().sync_all()?;
                    stage.persist_noclobber(dest)?;
                }
                push(report, "verified", display.clone());
                found = true;
            }
        }
        if remote && !node.dirty {
            for copy in catalog.archived.iter().filter(|c| {
                c.node == *id
                    && c.version == node.node.version
                    && Some(&c.hash) == node.hash.as_ref()
            }) {
                let target = &catalog.archives[&copy.target];
                let result = configured_client(
                    &target.url,
                    target.user_env.as_deref(),
                    target.pass_env.as_deref(),
                )
                .and_then(|client| {
                    client.read_to(&copy.key, &copy.hash, node.node.size, &mut io::sink())
                });
                match result {
                    Ok(()) => {
                        good_archives.push(format!("archive_copy/{}/{}", copy.node, copy.target));
                        push(
                            report,
                            "verified_remote",
                            json!({"path":display,"target":copy.target}),
                        );
                        found = true;
                    }
                    Err(error) => push(
                        report,
                        "remote_errors",
                        json!({"path":display,"target":copy.target,"error":format!("{error:#}")}),
                    ),
                }
            }
        }
        if !found {
            push(report, "missing", display);
        }
    }
    let mut unclaimed = Vec::new();
    for (name, device) in &devices {
        let known = catalog
            .claimed
            .iter()
            .filter(|c| c.target == *name)
            .map(|c| c.object.as_str())
            .collect::<BTreeSet<_>>();
        let objects = match device.open("objects") {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        ensure!(
            objects.metadata()?.is_dir(),
            "object root must be a directory"
        );
        for node in fs::read_dir(format!("/proc/self/fd/{}", objects.as_raw_fd()))? {
            let node = node?;
            if !node.file_type()?.is_dir() {
                unclaimed.push(device.path.join("objects").join(node.file_name()));
                continue;
            }
            let relative = format!(
                "objects/{}",
                node.file_name()
                    .to_str()
                    .context("invalid object directory name")?
            );
            let directory = device.open(&relative)?;
            ensure!(
                directory.metadata()?.is_dir(),
                "object node must be a directory"
            );
            for file in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
                let file = file?;
                let relative = PathBuf::from("objects")
                    .join(node.file_name())
                    .join(file.file_name());
                if !relative.to_str().is_some_and(|p| known.contains(p)) {
                    unclaimed.push(device.path.join(relative));
                }
            }
        }
        ensure!(
            device.identity()? == catalog.targets[name].identity,
            "target changed during verification"
        );
    }
    if let Some(destination) = destination {
        for (id, node) in &catalog.nodes {
            let (path, trash) = catalog.path(id)?;
            if node.node.is_dir && trash.is_none() {
                fs::File::open(destination.join(path))?.sync_all()?;
            }
        }
        fs::File::open(
            destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()?;
    }
    live["trash"] = trash;
    live["offline"] = json!(
        catalog
            .targets
            .keys()
            .filter(|name| !devices.contains_key(*name))
            .collect::<Vec<_>>()
    );
    live["unclaimed"] = json!(unclaimed);
    live["verified_copies"] = json!(good_copies);
    live["verified_archives"] = json!(good_archives);
    Ok(live)
}

pub fn inspect(snapshot: &Value, roots: &[PathBuf], destination: Option<&Path>) -> Result<Value> {
    inspect_catalog(&Catalog::parse(snapshot)?, roots, destination, false)
}

pub fn preflight(snapshot: &Value, roots: &[PathBuf]) -> Result<Value> {
    inspect_catalog(&Catalog::parse(snapshot)?, roots, None, true)
}

pub fn import(
    snapshot: &Value,
    dest: &Path,
    targets: &BTreeMap<String, PathBuf>,
    allow_degraded: bool,
) -> Result<Value> {
    let catalog = Catalog::parse(snapshot)?;
    ensure!(!dest.exists(), "destination metadata already exists");
    for (name, path) in targets {
        let target = catalog
            .targets
            .get(name)
            .context("unknown target mapping")?;
        ensure!(
            path.is_absolute()
                && fs::read_to_string(path.join(".tier-id"))?.trim() == target.identity,
            "mapped target identity mismatch"
        );
    }
    let roots = catalog
        .targets
        .iter()
        .filter_map(|(name, target)| {
            let path = targets.get(name).unwrap_or(&target.path);
            fs::read_to_string(path.join(".tier-id"))
                .ok()
                .filter(|id| id.trim() == target.identity)
                .map(|_| path.clone())
        })
        .collect::<Vec<_>>();
    let report = inspect_catalog(&catalog, &roots, None, true)?;
    let clean = [
        "missing",
        "damaged",
        "missing_copies",
        "unconfirmed",
        "offline",
        "unclaimed",
        "remote_errors",
    ]
    .iter()
    .all(|key| {
        report[*key].as_array().is_none_or(Vec::is_empty)
            && report["trash"][*key].as_array().is_none_or(Vec::is_empty)
    });
    ensure!(
        clean || allow_degraded,
        "recovery requires --allow-degraded after reviewing:\n{}",
        serde_json::to_string_pretty(&report)?
    );
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = tempfile::tempdir_in(parent)?;
    let staging = temp.path().join("metadata");
    let engine = Engine::open(&staging)?;
    let mut options = rocksdb::WriteOptions::default();
    options.set_sync(true);
    let transaction = engine
        .inner
        .db
        .transaction_opt(&options, &rocksdb::TransactionOptions::default());
    for entry in engine.inner.db.iterator(rocksdb::IteratorMode::Start) {
        transaction.delete(entry?.0)?;
    }
    let verified = report["verified_copies"]
        .as_array()
        .context("invalid recovery report")?;
    let verified_archives = report["verified_archives"]
        .as_array()
        .context("invalid archive recovery report")?;
    for row in &catalog.image.records {
        let mut bytes = row.bytes()?;
        if let Some(name) = row.key.strip_prefix("target/")
            && let Some(path) = targets.get(name)
        {
            let mut target = catalog.targets[name].clone();
            target.path = fs::canonicalize(path)?;
            bytes = postcard::to_allocvec(&target)?;
        }
        if row.key.starts_with("copy/") && !verified.iter().any(|v| v.as_str() == Some(&row.key)) {
            continue;
        }
        if row.key.starts_with("archive_copy/")
            && !verified_archives
                .iter()
                .any(|value| value.as_str() == Some(&row.key))
        {
            continue;
        }
        if row.key.starts_with("archive_copy/") {
            let mut copy: ArchiveCopy = decode_record(&bytes)?;
            copy.checked = crate::core::now()?;
            copy.error = None;
            bytes = postcard::to_allocvec(&copy)?;
        }
        if row.key.starts_with("garbage/") || row.key.starts_with("archive_gc/") {
            continue;
        }
        if let Some(id) = row.key.strip_prefix("node/") {
            let mut node = catalog.nodes[id].clone();
            node.primary = catalog
                .copies
                .iter()
                .find(|copy| {
                    copy.node == id
                        && verified.iter().any(|v| {
                            v.as_str() == Some(&format!("copy/{}/{}", copy.node, copy.target))
                        })
                })
                .map(|c| c.target.clone());
            bytes = postcard::to_allocvec(&node)?;
        }
        transaction.put(&row.key, bytes)?;
    }
    transaction.put(
        "backup/lineage",
        postcard::to_allocvec(&Uuid::new_v4().to_string())?,
    )?;
    transaction.commit()?;
    engine.rebuild_work_queue()?;
    engine.sync_metadata()?;
    drop(engine);
    // RENAME_NOREPLACE makes publishing the new database safe against a concurrent import.
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(staging.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    fs::File::open(parent)?.sync_all()?;
    Ok(report)
}
