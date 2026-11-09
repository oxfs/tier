//! Persisted domain types; key and encoding contracts are in `schema`.
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::SystemTime};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub write_cache: Vec<String>,
    #[serde(default)]
    pub persistent_storage: Vec<String>,
    #[serde(default)]
    pub persistent_copies: usize,
    #[serde(default)]
    pub persist_delay_secs: u64,
    #[serde(default)]
    pub direct_fallback: bool,
    #[serde(default)]
    pub manual_repair: bool,
    #[serde(default)]
    pub repair_grace_secs: u64,
    #[serde(default = "default_low")]
    pub low_water: u8,
    #[serde(default = "default_high")]
    pub high_water: u8,
    #[serde(default = "default_ttl")]
    pub trash_ttl_secs: u64,
}
fn default_low() -> u8 {
    70
}
fn default_high() -> u8 {
    90
}
fn default_ttl() -> u64 {
    604800
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            write_cache: vec![],
            persistent_storage: vec![],
            persistent_copies: 0,
            persist_delay_secs: 0,
            direct_fallback: false,
            manual_repair: false,
            repair_grace_secs: 0,
            low_water: 70,
            high_water: 90,
            trash_ttl_secs: 604800,
        }
    }
}
impl Policy {
    pub(crate) fn desired(&self) -> (&[String], usize) {
        if self.persistent_copies > 0 {
            (&self.persistent_storage, self.persistent_copies)
        } else {
            (&self.write_cache, 1)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    pub btime: SystemTime,
    pub version: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub node: Node,
    pub parent: Option<String>,
    pub name: String,
    pub primary: Option<String>,
    pub hash: Option<String>,
    pub dirty: bool,
    pub placement: Option<Placement>,
    pub closed_at: u64,
    pub incident: Option<u64>,
    pub error: Option<String>,
}
/// Last fully satisfied local placement, used to distinguish initial copying
/// from a later repair subject to grace/manual-repair policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Placement {
    pub version: u64,
    pub targets: Vec<String>,
    pub copies: usize,
}
impl Record {
    pub(super) fn new(id: String, parent: Option<String>, name: String, is_dir: bool) -> Self {
        let time = SystemTime::now();
        Self {
            node: Node {
                id,
                is_dir,
                size: 0,
                mtime: time,
                atime: time,
                ctime: time,
                btime: time,
                version: 0,
            },
            parent,
            name,
            primary: None,
            hash: None,
            dirty: false,
            placement: None,
            closed_at: 0,
            incident: None,
            error: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Copy {
    pub node: String,
    pub target: String,
    pub object: String,
    pub version: u64,
    pub hash: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Target {
    pub name: String,
    pub path: PathBuf,
    pub identity: String,
    pub domain: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Trash {
    pub id: i64,
    pub node: String,
    pub original: String,
    pub deleted: u64,
    pub expires: u64,
    pub policy: Policy,
    pub archive_policy: Option<crate::archive::ArchivePolicy>,
}
