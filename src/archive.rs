mod webdav;
use self::webdav::WebDav;
use crate::core::{Engine, Record, fingerprint, key, now};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, Seek},
};
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ArchivePolicy {
    pub targets: Vec<String>,
    pub copies: usize,
    pub delay_secs: u64,
    pub retain_local: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ArchiveTarget {
    pub name: String,
    pub url: String,
    pub user_env: Option<String>,
    pub pass_env: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ArchiveCopy {
    pub node: String,
    pub target: String,
    pub key: String,
    pub hash: String,
    pub version: u64,
    pub checked: u64,
    pub error: Option<String>,
}
pub(crate) fn configured_client(
    url: &str,
    user: Option<&str>,
    pass: Option<&str>,
) -> Result<WebDav> {
    let credentials = user
        .zip(pass)
        .map(|(u, p)| {
            Ok::<_, anyhow::Error>((
                std::env::var(u).context("missing WebDAV username environment variable")?,
                std::env::var(p).context("missing WebDAV password environment variable")?,
            ))
        })
        .transpose()?;
    WebDav::new(
        url,
        credentials.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
    )
}
impl Engine {
    pub(crate) fn archive_client(&self, target: &str) -> Result<WebDav> {
        let t: ArchiveTarget = self.require(&key("archive_target", target))?;
        configured_client(&t.url, t.user_env.as_deref(), t.pass_env.as_deref())
    }
    pub(crate) fn archive_policy(&self, id: &str) -> Result<Option<ArchivePolicy>> {
        let view = self.inner.db.snapshot();
        let mut id = id.to_owned();
        let mut seen = HashSet::new();
        loop {
            ensure!(seen.insert(id.clone()), "namespace cycle");
            if let Some(policy) = Self::view_get(&view, &key("archive_policy", &id))? {
                return Ok(Some(policy));
            }
            let node: Record =
                Self::view_get(&view, &key("node", &id))?.context("missing policy node")?;
            if let Some(parent) = node.parent {
                id = parent;
            } else {
                for item in view.iterator(rocksdb::IteratorMode::From(
                    b"trash/",
                    rocksdb::Direction::Forward,
                )) {
                    let (key, value) = item?;
                    if !key.starts_with(b"trash/") {
                        break;
                    }
                    let trash: crate::core::Trash = Self::decode(&value)?;
                    if trash.node == id {
                        return Ok(trash.archive_policy);
                    }
                }
                return Ok(None);
            }
        }
    }
    pub fn add_archive(
        &self,
        name: &str,
        url: &str,
        user_env: Option<&str>,
        pass_env: Option<&str>,
    ) -> Result<()> {
        ensure!(crate::core::safe_name(name), "invalid archive name");
        ensure!(
            user_env.is_some() == pass_env.is_some(),
            "supply both credential variables"
        );
        WebDav::new(url, None)?;
        ensure!(
            self.kv_get::<ArchiveTarget>(&key("archive_target", name))?
                .is_none(),
            "archive already exists"
        );
        self.put_record(
            &key("archive_target", name),
            &ArchiveTarget {
                name: name.into(),
                url: url.into(),
                user_env: user_env.map(Into::into),
                pass_env: pass_env.map(Into::into),
            },
            true,
        )
    }
    pub fn set_archive_policy(&self, path: &str, p: &ArchivePolicy) -> Result<()> {
        let node = self.stat(path)?;
        ensure!(
            node.is_dir
                && p.copies <= p.targets.len()
                && p.targets.iter().collect::<HashSet<_>>().len() == p.targets.len(),
            "invalid archive policy"
        );
        for t in &p.targets {
            let _: ArchiveTarget = self.require(&key("archive_target", t))?;
        }
        let _guard = self.inner.namespace.write();
        self.put_record(&key("archive_policy", &node.id), p, true)?;
        for r in self.subtree(&node.id)? {
            if !r.node.is_dir {
                self.schedule_file(&r.node.id)?;
            }
        }
        Ok(())
    }
    pub(crate) fn archive_copies(&self, id: &str) -> Result<Vec<ArchiveCopy>> {
        let r = self.record(id)?;
        Ok(self
            .scan::<ArchiveCopy>(&format!("archive_copy/{id}/"))?
            .into_iter()
            .map(|(_, v)| v)
            .filter(|v| v.version == r.node.version && r.hash.as_ref() == Some(&v.hash))
            .collect())
    }
    pub(crate) fn recall_archive(&self, id: &str, out: &mut fs::File) -> Result<()> {
        let r = self.record(id)?;
        let mut errors = vec![];
        for c in self.archive_copies(id)? {
            out.set_len(0)?;
            out.rewind()?;
            match self
                .archive_client(&c.target)
                .and_then(|client| client.read_to(&c.key, &c.hash, r.node.size, out))
            {
                Ok(()) => {
                    out.rewind()?;
                    return Ok(());
                }
                Err(e) => errors.push(format!("{}: {e:#}", c.target)),
            }
        }
        bail!("no readable current copy: {}", errors.join("; "))
    }
    pub(crate) fn archive_retained(&self, id: &str, default: usize) -> Result<usize> {
        let Some(p) = self.archive_policy(id)? else {
            return Ok(default);
        };
        let count = self
            .archive_copies(id)?
            .iter()
            .filter(|c| p.targets.contains(&c.target) && c.error.is_none())
            .count();
        Ok(if p.copies > 0 && count >= p.copies {
            p.retain_local
        } else {
            default
        })
    }
    pub fn archive_tick(&self) -> Result<()> {
        self.run_archive(true)
    }
    pub fn automatic_archive(&self) -> Result<()> {
        self.run_archive(false)
    }
    fn run_archive(&self, manual: bool) -> Result<()> {
        let Some(_maintenance) = self.inner.maintenance.try_lock() else {
            return Ok(());
        };
        let mut errors = vec![];
        for id in self.due_work("archive", manual)? {
            if !self.record(&id).is_ok_and(|r| !r.node.is_dir) || !self.live(&id)? {
                self.reschedule_work("archive", &id, false)?;
                continue;
            }
            let result = self.archive_one(&id, manual);
            let failed = result.is_err();
            if let Err(e) = result {
                self.put_record(&key("status/archive_error", &id), &format!("{e:#}"), false)?;
                errors.push(format!("{id}: {e:#}"));
            } else {
                self.delete_record(&key("status/archive_error", &id))?;
            }
            self.reschedule_work("archive", &id, failed)?;
        }
        for (k, c) in self.scan::<ArchiveCopy>("archive_gc/")? {
            if let Err(e) = self
                .archive_client(&c.target)
                .and_then(|client| client.remove(&c.key))
            {
                errors.push(format!("archive cleanup: {e:#}"));
            } else {
                self.delete_record(&k)?;
            }
        }
        self.collect_garbage()?;
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
    fn archive_one(&self, id: &str, manual: bool) -> Result<()> {
        let Some(r) = self.prepare_protected(id)? else {
            return Ok(());
        };
        let configured = self.archive_policy(id)?;
        let current = self.archive_copies(id)?;
        let Some(p) = configured.filter(|p| p.copies > 0) else {
            let current: Vec<_> = self
                .scan::<ArchiveCopy>(&format!("archive_copy/{id}/"))?
                .into_iter()
                .map(|(_, c)| c)
                .collect();
            if !current.is_empty() {
                let policy = self.policy(id)?;
                let (targets, count) = policy.desired();
                let n = self
                    .copies(id)?
                    .iter()
                    .filter(|c| {
                        c.version == r.node.version
                            && targets.contains(&c.target)
                            && r.hash
                                .as_ref()
                                .is_some_and(|h| self.matches_copy(&c.target, &c.object, h))
                    })
                    .count();
                if n >= count {
                    let lock = self.file_lock(id);
                    let guard = lock.lock();
                    let _namespace = self.inner.namespace.read();
                    if !guard.writer && self.record(id)?.node.version == r.node.version {
                        let mut puts = vec![];
                        let mut dels = vec![];
                        for c in current {
                            puts.push(Self::encoded(
                                format!("archive_gc/{}", uuid::Uuid::new_v4()),
                                &c,
                            )?);
                            dels.push(format!("archive_copy/{id}/{}", c.target));
                        }
                        self.atomic(puts, dels, true)?;
                    }
                }
            }
            return Ok(());
        };
        if now()?.saturating_sub(r.closed_at) < p.delay_secs {
            return Ok(());
        }
        let hash = r.hash.as_deref().context("missing hash")?;
        let mut verified = vec![];
        for mut c in current {
            if !p.targets.contains(&c.target) {
                continue;
            }
            let result = self
                .archive_client(&c.target)
                .and_then(|client| client.read_to(&c.key, hash, r.node.size, &mut io::sink()));
            match result {
                Ok(()) => {
                    c.checked = now()?;
                    c.error = None;
                    verified.push(c)
                }
                Err(e) => {
                    if c.error.is_none() {
                        c.checked = now()?;
                    }
                    c.error = Some(format!("{e:#}"));
                    self.put_record(&format!("archive_copy/{id}/{}", c.target), &c, false)?;
                    let policy = self.policy(id)?;
                    if !manual
                        && (policy.manual_repair
                            || now()?.saturating_sub(c.checked) < policy.repair_grace_secs)
                    {
                        return Err(e.context("archive requires manual repair"));
                    }
                }
            }
        }
        for name in &p.targets {
            if verified.len() >= p.copies {
                break;
            }
            if verified.iter().any(|c| &c.target == name) {
                continue;
            }
            let mut source = tempfile::tempfile()?;
            let local = self.copies(id)?.into_iter().find(|c| {
                c.version == r.node.version && self.matches_copy(&c.target, &c.object, hash)
            });
            if let Some(c) = local {
                io::copy(
                    &mut self.target(&c.target)?.open_object(&c.object)?,
                    &mut source,
                )?;
            } else {
                self.recall_archive(id, &mut source)?;
            }
            ensure!(
                fingerprint(&mut source)?.1 == hash,
                "archive source changed"
            );
            source.rewind()?;
            let client = self.archive_client(name)?;
            let object = client.upload(&mut source, hash)?;
            let lock = self.file_lock(id);
            let guard = lock.lock();
            let _namespace = self.inner.namespace.read();
            if guard.writer
                || self.record(id)?.node.version != r.node.version
                || self.archive_policy(id)?.as_ref() != Some(&p)
            {
                drop(guard);
                client.remove(&object)?;
                return Ok(());
            }
            let c = ArchiveCopy {
                node: id.into(),
                target: name.clone(),
                key: object,
                hash: hash.into(),
                version: r.node.version,
                checked: now()?,
                error: None,
            };
            let mut puts = vec![Self::encoded(format!("archive_copy/{id}/{name}"), &c)?];
            if let Some(old) = self.kv_get::<ArchiveCopy>(&format!("archive_copy/{id}/{name}"))? {
                puts.push(Self::encoded(
                    format!("archive_gc/{}", uuid::Uuid::new_v4()),
                    &old,
                )?);
            }
            self.atomic(puts, vec![], true)?;
            verified.push(c);
        }
        ensure!(verified.len() >= p.copies, "archive placement incomplete");
        let verified_local: HashSet<String> = self
            .copies(id)?
            .into_iter()
            .filter(|c| {
                c.version == r.node.version && self.matches_copy(&c.target, &c.object, hash)
            })
            .map(|c| c.object)
            .collect();
        ensure!(
            verified_local.len() >= p.retain_local,
            "archive local retention is not satisfied"
        );
        // Each selected remote was read back immediately above, before local eviction.
        let lock = self.file_lock(id);
        let mut guard = lock.lock();
        let _namespace = self.inner.namespace.read();
        let mut latest = self.record(id)?;
        if guard.writer
            || latest.node.version != r.node.version
            || self.archive_policy(id)?.as_ref() != Some(&p)
        {
            return Ok(());
        }
        let policy = self.policy(id)?;
        let mut locals = self.copies(id)?;
        locals.sort_by_key(|c| !policy.persistent_storage.contains(&c.target));
        let mut keep = vec![];
        let mut puts = vec![];
        let mut deletes = vec![];
        for c in locals {
            if c.version == r.node.version
                && verified_local.contains(&c.object)
                && keep.len() < p.retain_local
            {
                keep.push(c);
                continue;
            }
            puts.push(Self::encoded(
                format!("garbage/{}/{}", c.target, uuid::Uuid::new_v4()),
                &c,
            )?);
            deletes.push(format!("copy/{id}/{}", c.target));
        }
        latest.primary = keep.first().map(|c| c.target.clone());
        puts.push(Self::encoded(key("node", id), &latest)?);
        self.atomic(puts, deletes, true)?;
        if guard.body.as_ref().is_some_and(|b| {
            !keep
                .iter()
                .any(|c| c.object == b.copy.object && c.target == b.copy.target)
        }) {
            guard.body = None;
        }
        Ok(())
    }
}
