use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

// Two checksummed slots preserve the previous committed state during a torn
// update, without replacing the inode used by all foreground/background locks.
const SLOT: usize = 64 * 1024;
const HEADER: usize = 44;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct State {
    pub generation: u64,
    pub replicas: Vec<String>,
    pub digest: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub attributes: Option<FileAttributes>,
    #[serde(default)]
    pub archives: Vec<ArchiveCopy>,
    #[serde(default)]
    pub archived_at: Option<u64>,
    #[serde(default)]
    pub retiring: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FileAttributes {
    pub size: u64,
    pub modified_secs: i64,
    pub modified_nanos: i64,
    pub mode: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArchiveCopy {
    pub target: String,
    pub endpoint: String,
    pub key: String,
}

impl State {
    pub fn pending(generation: u64) -> Self {
        Self {
            generation,
            replicas: Vec::new(),
            digest: None,
            size: 0,
            attributes: None,
            archives: Vec::new(),
            archived_at: None,
            retiring: Vec::new(),
        }
    }
}

pub(crate) fn load(file: &mut File) -> Result<Option<(u64, State)>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take((2 * SLOT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(bytes.len() <= 2 * SLOT, "oversized descriptor");
    let mut latest = None;
    for offset in [0, SLOT] {
        let Some(header) = bytes.get(offset..offset + HEADER) else {
            continue;
        };
        let revision = u64::from_le_bytes(header[..8].try_into()?);
        let length = u32::from_le_bytes(header[8..12].try_into()?) as usize;
        if revision == 0 || length == 0 || length > SLOT - HEADER {
            continue;
        }
        let Some(payload) = bytes.get(offset + HEADER..offset + HEADER + length) else {
            continue;
        };
        let mut hash = Sha256::new();
        hash.update(&header[..12]);
        hash.update(payload);
        if hash.finalize().as_slice() != &header[12..HEADER] {
            continue;
        }
        let state: State = serde_json::from_slice(payload)?;
        if latest
            .as_ref()
            .is_none_or(|(previous, _)| revision > *previous)
        {
            latest = Some((revision, state));
        }
    }
    if latest.is_none() {
        bail!("descriptor has no valid committed state");
    }
    Ok(latest)
}

pub(crate) fn save(file: &mut File, state: &State) -> Result<()> {
    let revision = load(file)?.map_or(Ok(1), |(n, _)| {
        n.checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("descriptor revision exhausted"))
    })?;
    let payload = serde_json::to_vec(state)?;
    ensure!(payload.len() <= SLOT - HEADER, "descriptor too large");
    let mut frame = Vec::with_capacity(HEADER + payload.len());
    frame.extend(revision.to_le_bytes());
    frame.extend((payload.len() as u32).to_le_bytes());
    let mut hash = Sha256::new();
    hash.update(&frame);
    hash.update(&payload);
    frame.extend(hash.finalize());
    frame.extend(payload);
    file.seek(SeekFrom::Start(((revision - 1) % 2) * SLOT as u64))?;
    file.write_all(&frame)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torn_update_preserves_previous_commit() -> Result<()> {
        let mut file = tempfile::tempfile()?;
        save(&mut file, &State::pending(1))?;
        save(&mut file, &State::pending(2))?;
        file.seek(SeekFrom::Start(SLOT as u64 + HEADER as u64))?;
        file.write_all(b"broken")?;
        assert_eq!(load(&mut file)?.unwrap().1.generation, 1);
        save(&mut file, &State::pending(3))?;
        assert_eq!(load(&mut file)?.unwrap().1.generation, 3);
        Ok(())
    }

    #[test]
    fn corruption_is_not_an_empty_file() -> Result<()> {
        let mut file = tempfile::tempfile()?;
        file.write_all(b"corrupt")?;
        assert!(load(&mut file).is_err());
        Ok(())
    }
}
