use crate::core::{Engine, safe_name};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{params_from_iter, types::Value as Sql};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

pub fn write_snapshot(engine: &Engine, path: &Path) -> Result<()> {
    let data = engine.snapshot()?;
    publish(&data, path)
}

pub(crate) fn publish(data: &Value, path: &Path) -> Result<()> {
    let checksum = format!("{:x}", Sha256::digest(serde_json::to_vec(&data)?));
    let mut file = NamedTempFile::new_in(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    serde_json::to_writer_pretty(&mut file, &json!({"checksum":checksum,"metadata":data}))?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    fs::File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Value> {
    let envelope: Value = serde_json::from_reader(fs::File::open(path)?)?;
    let metadata = &envelope["metadata"];
    ensure!(
        envelope["checksum"].as_str()
            == Some(&format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(metadata)?)
            )),
        "snapshot checksum mismatch"
    );
    ensure!(metadata["format"] == 1, "unsupported snapshot format");
    Ok(metadata.clone())
}

fn rows<'a>(snapshot: &'a Value, table: &str) -> Result<&'a Vec<Value>> {
    snapshot[table]
        .as_array()
        .with_context(|| format!("missing table {table}"))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().with_context(|| format!("missing {key}"))
}
fn object_key(v: &Value) -> Result<&str> {
    let key = field(v, "object")?;
    let parts: Vec<_> = key.split('/').collect();
    ensure!(
        parts.len() == 3
            && parts[0] == "objects"
            && parts[1] == field(v, "node")?
            && Uuid::parse_str(parts[1]).is_ok()
            && Uuid::parse_str(parts[2]).is_ok(),
        "invalid object path"
    );
    Ok(key)
}

struct Entry {
    path: PathBuf,
    trash: Option<i64>,
}
impl Entry {
    fn label(&self) -> Value {
        match self.trash {
            Some(id) => json!({"entry":id,"path":self.path}),
            None => json!(self.path),
        }
    }
}

fn tree(snapshot: &Value) -> Result<BTreeMap<String, Entry>> {
    let mut links = BTreeMap::new();
    let mut directories = HashSet::new();
    let mut siblings = HashSet::new();
    for row in rows(snapshot, "nodes")? {
        let id = field(row, "id")?;
        match row["dir"].as_i64() {
            Some(1) => {
                directories.insert(id);
            }
            Some(0) => {
                ensure!(row["size"].as_u64().is_some(), "invalid file size");
                let hash = field(row, "hash")?;
                ensure!(
                    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid file digest"
                );
            }
            _ => bail!("invalid node type"),
        }
        ensure!(
            row["parent"].is_null() || row["parent"].is_string(),
            "invalid parent ID"
        );
        if id != "root" {
            ensure!(
                Uuid::parse_str(id).is_ok() && safe_name(field(row, "name")?),
                "invalid namespace entry"
            )
        }
        if let Some(parent) = row["parent"].as_str() {
            ensure!(
                siblings.insert((parent, unicase::UniCase::new(field(row, "name")?))),
                "duplicate sibling name"
            );
        }
        ensure!(
            links
                .insert(id, (row["parent"].as_str(), field(row, "name")?))
                .is_none(),
            "duplicate node"
        );
    }
    ensure!(
        links.get("root") == Some(&(None, "")) && directories.contains("root"),
        "invalid root"
    );
    let mut trash = BTreeMap::new();
    for item in rows(snapshot, "trash")? {
        let id = field(item, "node")?;
        ensure!(
            id != "root" && links.get(id).is_some_and(|(p, _)| p.is_none()),
            "invalid trash root"
        );
        let original = field(item, "original")?
            .strip_prefix('/')
            .context("invalid trash path")?;
        ensure!(original.split('/').all(safe_name), "invalid trash path");
        ensure!(
            trash
                .insert(
                    id,
                    (
                        item["id"].as_i64().context("invalid trash entry ID")?,
                        original
                    )
                )
                .is_none(),
            "duplicate trash root"
        );
    }
    let mut paths = BTreeMap::new();
    for id in links.keys() {
        let mut parts = Vec::new();
        let mut cursor = *id;
        let mut seen = HashSet::new();
        let path = loop {
            if cursor == "root" {
                let mut path = PathBuf::new();
                for name in parts.iter().rev() {
                    path.push(name)
                }
                break Entry { path, trash: None };
            }
            ensure!(seen.insert(cursor), "directory cycle");
            let &(parent, name) = links.get(cursor).context("missing parent")?;
            let Some(parent) = parent else {
                let &(entry, original) = trash
                    .get(cursor)
                    .context("detached node has no trash entry")?;
                let mut path = PathBuf::from(original);
                for name in parts.iter().rev() {
                    path.push(name)
                }
                break Entry {
                    path,
                    trash: Some(entry),
                };
            };
            ensure!(directories.contains(parent), "parent must be a directory");
            parts.push(name);
            cursor = parent;
        };
        paths.insert((*id).into(), path);
    }
    Ok(paths)
}

pub fn import(
    snapshot: &Value,
    dest: &Path,
    targets: &BTreeMap<String, PathBuf>,
    allow_degraded: bool,
) -> Result<Value> {
    tree(snapshot)?;
    for copy in rows(snapshot, "copies")? {
        object_key(copy)?;
    }
    ensure!(!dest.exists(), "destination database already exists");
    let mut roots = Vec::new();
    let configured = rows(snapshot, "targets")?;
    for name in targets.keys() {
        ensure!(
            configured.iter().any(|t| t["name"] == *name),
            "unknown target mapping: {name}"
        );
    }
    for target in configured {
        let name = field(target, "name")?;
        let mapped = targets.get(name);
        let root = mapped
            .map(PathBuf::as_path)
            .unwrap_or(Path::new(field(target, "path")?));
        match fs::read_to_string(root.join(".tier-id")) {
            Ok(id) if id.trim() == field(target, "identity")? => roots.push(root.to_owned()),
            _ if mapped.is_none() => continue,
            _ => bail!("mapped target identity mismatch: {name}"),
        }
    }
    let report = preflight(snapshot, &roots)?;
    let clean = [
        "missing",
        "missing_copies",
        "offline",
        "unconfirmed",
        "remote_errors",
        "damaged",
        "unclaimed",
    ]
    .iter()
    .all(|key| {
        report[key].as_array().is_some_and(Vec::is_empty)
            && report["trash"][key].as_array().is_none_or(Vec::is_empty)
    });
    ensure!(
        clean || allow_degraded,
        "recovery requires explicit --allow-degraded after review:\n{}",
        serde_json::to_string_pretty(&report)?
    );
    // Build and validate privately; publication cannot replace an existing database.
    let temp = NamedTempFile::new_in(
        dest.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let engine = Engine::open(temp.path())?;
    let tx = engine.db.unchecked_transaction()?;
    tx.execute_batch("PRAGMA defer_foreign_keys=ON; DELETE FROM nodes;")?;
    let system_id = Uuid::parse_str(field(snapshot, "system_id")?)?;
    tx.execute(
        "UPDATE system_identity SET uuid=? WHERE id=1",
        [system_id.to_string()],
    )?;
    for table in [
        "nodes",
        "targets",
        "copies",
        "policies",
        "trash",
        "protection",
        "pending_writes",
        "backup_schedule",
        "cloud_targets",
        "cloud_policy",
        "cloud_copies",
    ] {
        let st = tx.prepare(&format!("SELECT * FROM {table} LIMIT 0"))?;
        let columns = st
            .column_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>();
        drop(st);
        let query = format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            columns.join(","),
            vec!["?"; columns.len()].join(",")
        );
        let mut insert = tx.prepare(&query)?;
        for row in rows(snapshot, table)? {
            let values = columns
                .iter()
                .map(|key| {
                    if table == "targets"
                        && key == "path"
                        && let Some(path) = targets.get(field(row, "name")?)
                    {
                        ensure!(
                            fs::read_to_string(path.join(".tier-id"))?.trim()
                                == field(row, "identity")?,
                            "target identity mismatch"
                        );
                        return Ok(Sql::Text(
                            path.to_str().context("invalid target path")?.into(),
                        ));
                    }
                    Ok(match &row[key] {
                        Value::Null => Sql::Null,
                        Value::String(v) => Sql::Text(v.clone()),
                        Value::Number(v) => Sql::Integer(v.as_i64().context("invalid integer")?),
                        _ => bail!("invalid database value"),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            insert.execute(params_from_iter(values))?;
        }
    }
    tx.execute(
        "UPDATE revision SET seq=? WHERE id=1",
        [snapshot["seq"]
            .as_i64()
            .context("invalid snapshot sequence")?],
    )?;
    tx.commit()?;
    engine
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    drop(engine);
    temp.as_file().sync_all()?;
    temp.persist_noclobber(dest)?;
    fs::File::open(
        dest.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()?;
    Ok(report)
}

pub fn inspect(snapshot: &Value, roots: &[PathBuf], destination: Option<&Path>) -> Result<Value> {
    let paths = tree(snapshot)?;
    let mut devices = BTreeMap::new();
    for path in roots {
        let id = fs::read_to_string(path.join(".tier-id"))?;
        let target = rows(snapshot, "targets")?
            .iter()
            .find(|v| v["identity"] == id.trim())
            .context("unknown target ID")?;
        let previous = devices.insert(field(target, "name")?.to_owned(), fs::canonicalize(path)?);
        ensure!(
            previous.is_none(),
            "duplicate target identity in recovery devices"
        );
    }
    if let Some(dest) = destination {
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let resolved = fs::canonicalize(parent)?;
        ensure!(
            devices.values().all(|root| !resolved.starts_with(root)),
            "extract outside the source devices"
        );
        fs::create_dir(dest).context("extraction requires a new output directory")?;
    }
    let section = || json!({"verified":[],"missing":[],"damaged":[],"missing_copies":[]});
    let mut live = section();
    let mut trash = section();
    let offline = rows(snapshot, "targets")?
        .iter()
        .filter_map(|t| t["name"].as_str())
        .filter(|name| !devices.contains_key(*name))
        .collect::<Vec<_>>();
    let mut known = HashSet::new();
    let mut by_node = BTreeMap::<&str, Vec<&Value>>::new();
    for copy in rows(snapshot, "copies")? {
        if let Some(root) = devices.get(field(copy, "target")?) {
            known.insert(root.join(object_key(copy)?));
            by_node.entry(field(copy, "node")?).or_default().push(copy);
        }
    }
    for node in rows(snapshot, "nodes")? {
        let id = field(node, "id")?;
        let entry = &paths[id];
        let path = &entry.path;
        let output = destination.filter(|_| entry.trash.is_none());
        let report = if entry.trash.is_some() {
            &mut trash
        } else {
            &mut live
        };
        if node["dir"] == 1 {
            if let Some(dest) = output {
                fs::create_dir_all(dest.join(path))?;
            }
            continue;
        }
        let mut found = false;
        for c in by_node
            .get(id)
            .into_iter()
            .flatten()
            .filter(|c| c["hash"] == node["hash"])
        {
            let Some(root) = devices.get(field(c, "target")?) else {
                continue;
            };
            let obj = root.join(object_key(c)?);
            let actual = match fs::canonicalize(&obj) {
                Ok(path) => path,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    report["missing_copies"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!(obj));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            ensure!(actual.starts_with(root), "object outside device");
            let mut source = fs::File::open(actual)?;
            let out = output.filter(|_| !found).map(|dest| dest.join(path));
            let mut stage = if let Some(file) = &out {
                let parent = file.parent().context("invalid output path")?;
                fs::create_dir_all(parent)?;
                Some(NamedTempFile::new_in(parent)?)
            } else {
                None
            };
            let mut checksum = Sha256::new();
            let mut length = 0u64;
            let mut buffer = vec![0; 128 * 1024];
            loop {
                let n = source.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                checksum.update(&buffer[..n]);
                if let Some(stage) = &mut stage {
                    stage.write_all(&buffer[..n])?;
                }
                length += n as u64;
            }
            if Some(length) != node["size"].as_u64()
                || format!("{:x}", checksum.finalize()) != field(node, "hash")?
            {
                report["damaged"].as_array_mut().unwrap().push(json!(obj));
                continue;
            }
            if !found {
                if let (Some(stage), Some(out)) = (stage, out) {
                    stage.persist_noclobber(out)?;
                }
                report["verified"]
                    .as_array_mut()
                    .unwrap()
                    .push(entry.label());
                found = true;
            }
        }
        if !found {
            report["missing"]
                .as_array_mut()
                .unwrap()
                .push(entry.label());
        }
    }
    let mut unknown = Vec::new();
    for root in devices.values() {
        let objects = root.join("objects");
        if !objects.is_dir() {
            continue;
        }
        for dir in fs::read_dir(objects)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                unknown.push(dir.path());
                continue;
            }
            for entry in fs::read_dir(dir.path())? {
                let p = entry?.path();
                if !known.contains(&p) {
                    unknown.push(p);
                }
            }
        }
    }
    for t in rows(snapshot, "targets")? {
        if let Some(root) = devices.get(field(t, "name")?) {
            ensure!(
                fs::read_to_string(root.join(".tier-id"))?.trim() == field(t, "identity")?,
                "target changed during verification"
            );
        }
    }
    live["trash"] = trash;
    live["offline"] = json!(offline);
    live["unclaimed"] = json!(unknown);
    Ok(live)
}

/// Check registered WebDAV objects as well as the supplied physical devices.
pub fn preflight(snapshot: &Value, roots: &[PathBuf]) -> Result<Value> {
    let mut report = inspect(snapshot, roots, None)?;
    let paths = tree(snapshot)?;
    for key in ["unconfirmed", "verified_remote", "remote_errors"] {
        report[key] = json!([]);
        report["trash"][key] = json!([]);
    }
    for w in rows(snapshot, "pending_writes")? {
        let entry = paths
            .get(field(w, "node")?)
            .context("unknown pending write")?;
        let section = if entry.trash.is_some() {
            &mut report["trash"]
        } else {
            &mut report
        };
        section["unconfirmed"]
            .as_array_mut()
            .unwrap()
            .push(entry.label());
    }
    let nodes: BTreeMap<_, _> = rows(snapshot, "nodes")?
        .iter()
        .map(|n| Ok((field(n, "id")?, n)))
        .collect::<Result<_>>()?;
    let targets: BTreeMap<_, _> = rows(snapshot, "cloud_targets")?
        .iter()
        .map(|t| Ok((field(t, "name")?, t)))
        .collect::<Result<_>>()?;
    for c in rows(snapshot, "cloud_copies")? {
        let id = field(c, "node")?;
        let target = field(c, "target")?;
        let hash = field(c, "hash")?;
        let node = nodes.get(id).context("unknown archived node")?;
        if node["hash"] != hash {
            continue;
        }
        let entry = paths.get(id).context("unknown archived path")?;
        let section = if entry.trash.is_some() {
            &mut report["trash"]
        } else {
            &mut report
        };
        let t = targets.get(target).context("unknown archived target")?;
        let checked = crate::cloud::configured_client(
            field(t, "url")?,
            t["user_env"].as_str(),
            t["pass_env"].as_str(),
        )
        .and_then(|client| {
            client.read_to(
                field(c, "key")?,
                hash,
                node["size"].as_u64().context("invalid file size")?,
                &mut io::sink(),
            )
        });
        match checked {
            Ok(()) => {
                section["verified_remote"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"path":entry.label(),"target":target}));
                section["missing"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|label| label != &entry.label());
            }
            Err(err) => section["remote_errors"]
                .as_array_mut()
                .unwrap()
                .push(json!({"path":entry.label(),"target":target,"error":err.to_string()})),
        }
    }
    Ok(report)
}
