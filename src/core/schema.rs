//! RocksDB's single-column-family schema. All persisted key layouts live here.
//!
//! Values use named-field MessagePack. Preserve
//! storage names with `serde(rename)` when renaming Rust fields. Added fields
//! need safe defaults. Unknown fields are discarded on read/modify/write, so
//! incompatible semantics or unsafe downgrades require a new FORMAT gate.

use anyhow::{Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{io::Cursor, os::unix::ffi::OsStrExt, path::Path};

pub(crate) const FORMAT: &str = "tier-rocksdb-v1";
/// Format gate, system UUID and monotonic trash sequence.
pub(crate) const SYSTEM_FORMAT: &str = "system/format";
pub(crate) const SYSTEM_ID: &str = "system/id";
pub(crate) const TRASH_SEQ: &str = "system/trash_seq";
pub(crate) const ROOT: &str = "node/root";
/// Independent metadata-backup/export policies and per-destination status.
pub(crate) const EXPORT_SCHEDULE: &str = "backup/schedule/export";
pub(crate) const METADATA_SCHEDULE: &str = "backup/schedule/metadata";
pub(crate) const BACKUP_STATUS: &str = "backup/status/";
/// Runtime diagnostics, discarded when restoring a metadata backup.
pub(crate) const STATUS: &str = "status/";
pub(crate) const ARCHIVE_ERRORS: &str = "status/archive_error/";
/// UUID -> Record: namespace attributes, current generation and placement state.
pub(crate) const NODES: &str = "node/";
/// Parent UUID + case-folded name -> child UUID. Updated atomically with Record.
pub(crate) const ENTRIES: &str = "entry/";
/// Target name -> Target: local path, disk identity and failure domain.
pub(crate) const TARGETS: &str = "target/";
/// Node UUID + target name -> Copy: object path, generation and optional digest.
pub(crate) const COPIES: &str = "copy/";
/// Node UUID -> explicit Policy; absent entries inherit from the parent.
pub(crate) const POLICIES: &str = "policy/";
/// Zero-padded sequence -> Trash, including policies of the detached subtree.
pub(crate) const TRASH: &str = "trash/";
/// Target name + random UUID -> Copy awaiting physical deletion.
pub(crate) const GARBAGE: &str = "garbage/";
/// Archive target name -> ArchiveTarget (credential env names, never secrets).
pub(crate) const ARCHIVE_TARGETS: &str = "archive_target/";
/// Node UUID -> ArchivePolicy; node UUID + target -> verified ArchiveCopy.
pub(crate) const ARCHIVE_POLICIES: &str = "archive_policy/";
pub(crate) const ARCHIVE_COPIES: &str = "archive_copy/";
/// Random UUID -> ArchiveCopy awaiting remote deletion.
pub(crate) const ARCHIVE_GC: &str = "archive_gc/";
/// Kind + due seconds (20 digits) + node UUID -> node UUID, ordered by deadline.
pub(crate) const QUEUE: &str = "queue/";
/// Kind + node UUID -> due seconds; atomically replaces old queue entries.
pub(crate) const QUEUE_AT: &str = "queue_at/";
/// Node UUID -> Rescan: restartable directory cursor after policy/namespace changes.
pub(crate) const RESCANS: &str = "rescan/";
pub(crate) fn rescan(id: &str) -> String {
    format!("{RESCANS}{id}")
}

pub(crate) fn node(id: &str) -> String {
    format!("{NODES}{id}")
}
pub(crate) fn target(name: &str) -> String {
    format!("{TARGETS}{name}")
}
pub(crate) fn policy(id: &str) -> String {
    format!("{POLICIES}{id}")
}
pub(crate) fn archive_policy(id: &str) -> String {
    format!("{ARCHIVE_POLICIES}{id}")
}
pub(crate) fn archive_target(name: &str) -> String {
    format!("{ARCHIVE_TARGETS}{name}")
}
pub(crate) fn archive_error(id: &str) -> String {
    format!("{ARCHIVE_ERRORS}{id}")
}
pub(crate) fn entries(parent: &str) -> String {
    format!("{ENTRIES}{parent}/")
}
pub(crate) fn entry(parent: &str, name: &str) -> String {
    format!(
        "{ENTRIES}{parent}/{}",
        unicase::UniCase::new(name).to_folded_case()
    )
}
pub(crate) fn copies(id: &str) -> String {
    format!("{COPIES}{id}/")
}
pub(crate) fn copy(id: &str, target: &str) -> String {
    format!("{COPIES}{id}/{target}")
}
pub(crate) fn archive_copies(id: &str) -> String {
    format!("{ARCHIVE_COPIES}{id}/")
}
pub(crate) fn archive_copy(id: &str, target: &str) -> String {
    format!("{ARCHIVE_COPIES}{id}/{target}")
}
pub(crate) fn trash(id: i64) -> String {
    format!("{TRASH}{id:020}")
}
pub(crate) fn garbage(target: &str) -> String {
    format!("{GARBAGE}{target}/{}", uuid::Uuid::new_v4())
}
pub(crate) fn archive_gc() -> String {
    format!("{ARCHIVE_GC}{}", uuid::Uuid::new_v4())
}
pub(crate) fn backup_status(kind: &str, path: &Path) -> String {
    format!(
        "{BACKUP_STATUS}{kind}/{:x}",
        Sha256::digest(path.as_os_str().as_bytes())
    )
}
/// The queue discriminator is part of its persisted key, not arbitrary text.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WorkKind {
    Local,
    Archive,
}
impl WorkKind {
    fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Archive => "archive",
        }
    }
}

pub(crate) fn queue_prefix(kind: WorkKind) -> String {
    format!("{QUEUE}{}/", kind.name())
}
pub(crate) fn queue(kind: WorkKind, due: u64, id: &str) -> String {
    format!("{QUEUE}{}/{due:020}/{id}", kind.name())
}
pub(crate) fn queue_at(kind: WorkKind, id: &str) -> String {
    format!("{QUEUE_AT}{}/{id}", kind.name())
}

pub(crate) fn ephemeral(key: &str) -> bool {
    key.starts_with(QUEUE)
        || key.starts_with(QUEUE_AT)
        || key.starts_with(RESCANS)
        || key.starts_with(STATUS)
        || key.starts_with(BACKUP_STATUS)
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(rmp_serde::to_vec_named(value)?)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut reader = Cursor::new(bytes);
    let value = T::deserialize(&mut rmp_serde::Deserializer::new(&mut reader))?;
    ensure!(
        reader.position() == bytes.len() as u64,
        "trailing bytes in metadata value"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Original {
        id: String,
        size: u64,
    }
    #[derive(Serialize, Deserialize)]
    struct Extended {
        size: u64,
        id: String,
        #[serde(default)]
        checksum: Option<String>,
    }

    #[test]
    fn named_fields_survive_reordering_and_safe_additions() -> Result<()> {
        let old = Original {
            id: "file".into(),
            size: 42,
        };
        let mut new: Extended = decode(&encode(&old)?)?;
        assert!(new.checksum.is_none());
        new.checksum = Some("digest".into());
        assert_eq!(decode::<Original>(&encode(&new)?)?, old);
        // Ignoring an unknown field is not preserving it: downgrades need a gate.
        let rewritten: Extended = decode(&encode(&decode::<Original>(&encode(&new)?)?)?)?;
        assert!(rewritten.checksum.is_none());
        Ok(())
    }

    #[test]
    fn invalid_values_fail_closed() -> Result<()> {
        let value = Original {
            id: "file".into(),
            size: 42,
        };
        let bytes = encode(&value)?;
        assert!(decode::<Original>(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode::<Original>(&trailing).is_err());
        assert!(decode::<Original>(&encode(&serde_json::json!({"id": "file"}))?).is_err());
        assert!(
            decode::<Original>(&encode(&serde_json::json!({"id": "file", "size": "42"}))?).is_err()
        );
        Ok(())
    }

    #[test]
    fn key_order_and_case_folding_are_stable() {
        assert_eq!(entry("parent", "Mixed"), entry("parent", "mixed"));
        assert!(queue(WorkKind::Local, 9, "a") < queue(WorkKind::Local, 10, "a"));
        assert!(trash(9) < trash(10));
        assert!(!copies("a").starts_with(&copies("ab")));
    }
}
