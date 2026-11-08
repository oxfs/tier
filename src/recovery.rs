use crate::core::Engine;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{params_from_iter, types::Value as Sql};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Seek, Write},
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
fn safe_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', '\0']) && name != "." && name != ".."
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

fn tree(snapshot: &Value) -> Result<BTreeMap<String, Option<PathBuf>>> {
    let mut links = BTreeMap::new();
    for row in rows(snapshot, "nodes")? {
        let id = field(row, "id")?;
        if id != "root" {
            ensure!(
                Uuid::parse_str(id).is_ok() && safe_name(field(row, "name")?),
                "invalid namespace entry"
            )
        }
        ensure!(
            links
                .insert(id, (row["parent"].as_str(), field(row, "name")?))
                .is_none(),
            "duplicate node"
        );
    }
    ensure!(links.get("root") == Some(&(None, "")), "invalid root");
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
                break Some(path);
            }
            ensure!(seen.insert(cursor), "directory cycle");
            let &(parent, name) = links.get(cursor).context("missing parent")?;
            let Some(parent) = parent else { break None };
            parts.push(name);
            cursor = parent;
        };
        paths.insert((*id).into(), path);
    }
    Ok(paths)
}

pub fn import(snapshot: &Value, dest: &Path, targets: &BTreeMap<String, PathBuf>) -> Result<()> {
    tree(snapshot)?;
    for copy in rows(snapshot, "copies")? {
        object_key(copy)?;
    }
    ensure!(!dest.exists(), "destination database already exists");
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
    Ok(())
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
        devices.insert(field(target, "name")?.to_owned(), fs::canonicalize(path)?);
    }
    if let Some(dest) = destination {
        fs::create_dir(dest).context("extraction requires a new output directory")?;
    }
    let mut good = Vec::new();
    let mut missing = Vec::new();
    let mut damaged = Vec::new();
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
        let Some(path) = &paths[id] else { continue };
        if node["dir"] == 1 {
            if let Some(dest) = destination {
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
            let Ok(actual) = fs::canonicalize(&obj) else {
                continue;
            };
            ensure!(actual.starts_with(root), "object outside device");
            let mut source = fs::File::open(actual)?;
            let mut checksum = Sha256::new();
            io::copy(&mut source, &mut checksum)?;
            if format!("{:x}", checksum.finalize()) != field(node, "hash")? {
                damaged.push(obj);
                continue;
            }
            if !found {
                if let Some(dest) = destination {
                    let out = dest.join(path);
                    fs::create_dir_all(out.parent().context("invalid path")?)?;
                    source.rewind()?;
                    io::copy(
                        &mut source,
                        &mut fs::OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(out)?,
                    )?;
                }
                good.push(path);
                found = true;
            }
        }
        if !found {
            missing.push(path);
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
    Ok(json!({"verified":good,"missing":missing,"damaged":damaged,"unclaimed":unknown}))
}
