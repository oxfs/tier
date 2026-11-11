use super::*;
use serde_json::{Value, json};
impl Engine {
    pub fn trash_page(&self, requested: u64, size: u64) -> Result<Value> {
        ensure!(
            requested > 0 && (1..=100).contains(&size),
            "invalid trash page"
        );
        let mut trash = self.scan::<Trash>(schema::TRASH)?;
        trash.reverse();
        let total = trash.len() as u64;
        let pages = total.div_ceil(size);
        let page = requested.min(pages.max(1));
        let mut items = vec![];
        for (_, t) in trash
            .into_iter()
            .skip(((page - 1) * size) as usize)
            .take(size as usize)
        {
            let mut logical = 0u64;
            let mut bytes = std::collections::BTreeMap::<String, Option<u64>>::new();
            for r in self.subtree(&t.node)? {
                logical = logical.saturating_add(self.stat_id(&r.node.id)?.size);
                for c in self.copies(&r.node.id)? {
                    let amount = self
                        .target(&c.target)
                        .and_then(|t| Ok(t.open_object(&c.object)?.metadata()?.len()))
                        .ok();
                    let current = bytes.entry(c.target).or_insert(Some(0));
                    *current = current.zip(amount).map(|(a, b)| a.saturating_add(b));
                }
            }
            let mut row = serde_json::to_value(t)?;
            row["logical_bytes"] = logical.into();
            row["target_bytes"] = serde_json::to_value(bytes)?;
            items.push(row);
        }
        Ok(json!({"items":items,"total":total,"page":page,"page_size":size,"pages":pages}))
    }
    pub fn state(&self) -> Result<Value> {
        use std::os::unix::fs::MetadataExt;
        let mut targets = vec![];
        for (_, t) in self.scan::<Target>(schema::TARGETS)? {
            let stat = self.target_stat(&t.name).ok();
            let mut row = serde_json::to_value(&t)?;
            row["online"] = stat.is_some().into();
            row["device"] = stat
                .as_ref()
                .and_then(|_| fs::metadata(&t.path).ok())
                .map(|m| m.dev())
                .into();
            row["total_bytes"] = stat
                .as_ref()
                .map(|s| s.f_blocks.saturating_mul(s.f_frsize))
                .into();
            row["available_bytes"] = stat.map(|s| s.f_bavail.saturating_mul(s.f_frsize)).into();
            targets.push(row);
        }
        let mut policies = vec![];
        for (k, p) in self.scan::<Policy>(schema::POLICIES)? {
            let id = k.trim_start_matches(schema::POLICIES);
            if let Some(path) = self.path_for(id)? {
                policies.push(json!({"node":id,"path":path,"policy":p}));
            }
        }
        let mut archive_policy = vec![];
        for (k, p) in self.scan::<crate::archive::ArchivePolicy>(schema::ARCHIVE_POLICIES)? {
            let id = k.trim_start_matches(schema::ARCHIVE_POLICIES);
            if let Some(path) = self.path_for(id)? {
                archive_policy.push(json!({"node":id,"path":path,"settings":p}));
            }
        }
        let copies = self.scan::<crate::archive::ArchiveCopy>(schema::ARCHIVE_COPIES)?;
        let mut archive_targets = vec![];
        for (_, t) in self.scan::<crate::archive::ArchiveTarget>(schema::ARCHIVE_TARGETS)? {
            archive_targets.push(json!({"name":t.name,"url":t.url,"object_count":copies.iter().filter(|(_,c)|c.target==t.name).count()}));
        }
        let mut errors = vec![];
        for (k, error) in self.scan::<String>(schema::ARCHIVE_ERRORS)? {
            let id = k.trim_start_matches(schema::ARCHIVE_ERRORS);
            if let Some(path) = self.path_for(id)? {
                errors.push(json!({"node":id,"path":path,"error":error}));
            }
        }
        let mut health = vec![];
        for (_, r) in self.scan::<Record>(schema::NODES)? {
            if r.node.is_dir {
                continue;
            }
            let Some(path) = self.path_for(&r.node.id)? else {
                continue;
            };
            let Ok(p) = self.policy(&r.node.id) else {
                continue;
            };
            let (targets, required) = p.desired();
            let required = self.archive_retained(&r.node.id, required)?;
            let mut locations = vec![];
            let mut domains = HashSet::new();
            let mut current = 0;
            for c in self.copies(&r.node.id)? {
                if c.version == r.node.version
                    && self
                        .target(&c.target)
                        .and_then(|t| Ok(t.open_object(&c.object)?))
                        .is_ok()
                {
                    if targets.contains(&c.target) {
                        current += 1
                    }
                    if let Some(t) = self.kv_get::<Target>(&schema::target(&c.target))? {
                        domains.insert(t.domain);
                    }
                    locations.push(c.target);
                }
            }
            let lock = self.file_lock(&r.node.id);
            let writing = lock.lock().writer;
            let status = if writing {
                "writing"
            } else if r.error.is_some() {
                "needs_attention"
            } else if r.dirty {
                "waiting"
            } else if required == 0 && locations.is_empty() {
                "remote_only"
            } else if current >= required {
                "healthy"
            } else if now()?.saturating_sub(r.closed_at) < p.persist_delay_secs {
                "waiting"
            } else if p.manual_repair {
                "needs_attention"
            } else {
                "under_replicated"
            };
            health.push(json!({"path":path,"status":status,"current":current,"required":required,"readable_local":locations,"readable_domains":domains.len(),"repair":if p.manual_repair{"manual"}else{"auto"},"error":r.error}));
        }
        Ok(
            json!({"targets":targets,"trash_count":self.scan::<Trash>(schema::TRASH)?.len(),"archive_targets":archive_targets,"policies":policies,"archive_policy":archive_policy,"archive_errors":errors,"health":health,"backup_schedules":self.backup_schedules()?,"backup_status":self.backup_status()?}),
        )
    }
}
