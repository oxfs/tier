use super::*;
impl Engine {
    pub fn reconcile(&self) -> Result<()> {
        self.run_reconcile(true)
    }
    pub fn automatic_reconcile(&self) -> Result<()> {
        self.run_reconcile(false)
    }
    fn run_reconcile(&self, manual: bool) -> Result<()> {
        let Some(_maintenance) = self.inner.maintenance.try_lock() else {
            return Ok(());
        };
        self.resume_rescans()?;
        let mut errors = vec![];
        for id in self.due_work(schema::WorkKind::Local, manual)? {
            if !self.record(&id).is_ok_and(|r| !r.node.is_dir) || !self.live(&id)? {
                self.reschedule_work(schema::WorkKind::Local, &id, false)?;
                continue;
            }
            let result = self.reconcile_one(&id, manual);
            let failed = result.is_err();
            if let Err(e) = result {
                let lock = self.file_lock(&id);
                let _guard = lock.lock();
                if let Ok(mut n) = self.record(&id) {
                    n.error = Some(format!("{e:#}"));
                    n.incident = Some(n.incident.unwrap_or(now()?));
                    self.put_record(&schema::node(&id), &n, false)?;
                }
                errors.push(format!("{id}: {e:#}"));
            }
            self.reschedule_work(schema::WorkKind::Local, &id, failed)?;
        }
        for (_, t) in self.scan::<Trash>(schema::TRASH)? {
            if t.expires <= now()?
                && let Err(e) = self.purge(t.id)
            {
                errors.push(format!("trash {}: {e:#}", t.id));
            }
        }
        self.collect_garbage()?;
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }
    pub(crate) fn prepare_protected(&self, id: &str) -> Result<Option<Record>> {
        let lock = self.file_lock(id);
        let Some(pin) = self.try_idle_pin(id)? else {
            return Ok(None);
        };
        let guard = lock.lock();
        let r = self.record(id)?;
        let p = self.policy(id)?;
        if guard.writer || now()?.saturating_sub(r.closed_at) < p.persist_delay_secs {
            return Ok(None);
        }
        if !r.dirty && r.hash.is_some() {
            return Ok(Some(r));
        }
        let c = self
            .copies(id)?
            .into_iter()
            .find(|c| Some(&c.target) == r.primary.as_ref() && c.version == r.node.version)
            .context("no current primary")?;
        let target = self.target(&c.target)?;
        let mut source = target.open_object(&c.object)?;
        drop(pin);
        drop(guard);
        let (size, hash) = fingerprint(&mut source)?;
        Self::sync_object(&target, &c.object, &source)?;
        let Some(_pin) = self.try_idle_pin(id)? else {
            return Ok(None);
        };
        let guard = lock.lock();
        let mut latest = self.record(id)?;
        if guard.writer || latest.node.version != r.node.version {
            return Ok(None);
        }
        Self::apply_physical_attrs(&mut latest.node, &source.metadata()?)?;
        ensure!(
            latest.node.size == size,
            "source size changed during verification"
        );
        latest.hash = Some(hash.clone());
        latest.dirty = false;
        latest.error = None;
        let mut copy = c;
        copy.hash = Some(hash);
        self.atomic(
            vec![
                Self::encoded(schema::node(id), &latest)?,
                Self::encoded(schema::copy(id, &copy.target), &copy)?,
            ],
            vec![],
            true,
        )?;
        Ok(Some(latest))
    }
    fn reconcile_one(&self, id: &str, manual: bool) -> Result<()> {
        self.refresh_object_names(id)?;
        let Some(r) = self.prepare_protected(id)? else {
            return Ok(());
        };
        let p = self.policy(id)?;
        let hash = r.hash.as_deref().context("missing checksum")?;
        let mut valid = vec![];
        for c in self.copies(id)? {
            if c.version == r.node.version && self.matches_copy(&c.target, &c.object, hash) {
                valid.push(c)
            }
        }
        let (targets, count) = p.desired();
        let required = self.archive_retained(id, count)?;
        let placement = model::Placement {
            version: r.node.version,
            targets: targets.to_vec(),
            copies: count,
        };
        let have = valid.iter().filter(|c| targets.contains(&c.target)).count();
        if have < required && r.placement.as_ref() == Some(&placement) && !manual {
            let incident = r.incident.unwrap_or(now()?);
            if p.manual_repair || now()?.saturating_sub(incident) < p.repair_grace_secs {
                bail!("repair deferred by policy")
            }
        }
        if valid.is_empty() && required > 0 {
            if !self.recall_idle(id)? {
                return Ok(());
            }
            valid = self
                .copies(id)?
                .into_iter()
                .filter(|c| {
                    c.version == r.node.version && self.matches_copy(&c.target, &c.object, hash)
                })
                .collect();
        }
        if valid.is_empty() {
            ensure!(required == 0, "no verified source remains");
            return Ok(());
        }
        let mut desired = targets.to_vec();
        rank(&mut desired, id);
        let mut domains: HashSet<String> = valid
            .iter()
            .filter(|c| targets.contains(&c.target))
            .filter_map(|c| {
                self.kv_get::<Target>(&schema::target(&c.target))
                    .ok()
                    .flatten()
                    .map(|t| t.domain)
            })
            .collect();
        while !desired.is_empty() {
            let index = desired
                .iter()
                .position(|name| {
                    self.kv_get::<Target>(&schema::target(name))
                        .ok()
                        .flatten()
                        .is_some_and(|t| !domains.contains(&t.domain))
                })
                .unwrap_or(0);
            let name = desired.remove(index);
            if valid.iter().filter(|c| targets.contains(&c.target)).count() >= required {
                break;
            }
            if valid.iter().any(|c| c.target == name) {
                continue;
            }
            let source = &valid[0];
            let mut input = self.target(&source.target)?.open_object(&source.object)?;
            let lock = self.file_lock(id);
            let candidate = {
                let Some(_pin) = self.try_idle_pin(id)? else {
                    return Ok(());
                };
                let _guard = lock.lock();
                self.create_object(&name, id)
            };
            let (root, object, mut output) = match candidate {
                Ok(v) => v,
                Err(_) => continue,
            };
            // The maintenance gate excludes GC while this candidate is live.
            // Persist its cleanup job before copying data; publication removes
            // the job atomically, including when a concurrent purge wins.
            let candidate_gc = schema::garbage(&name);
            self.put_record(
                &candidate_gc,
                &Copy {
                    node: id.into(),
                    target: name.clone(),
                    object: object.clone(),
                    version: r.node.version,
                    hash: None,
                },
                true,
            )?;
            let copied = (|| -> Result<()> {
                io::copy(&mut input, &mut output)?;
                ensure!(
                    fingerprint(&mut output)?.1 == hash,
                    "replica verification failed"
                );
                Self::copy_physical_times(&input, &output)?;
                Self::sync_object(&root, &object, &output)
            })();
            if copied.is_err() {
                if root.remove_object(&object).is_ok() {
                    self.delete_record(&candidate_gc)?;
                }
                continue;
            }
            let Some(_pin) = self.try_idle_pin(id)? else {
                if root.remove_object(&object).is_ok() {
                    self.delete_record(&candidate_gc)?;
                }
                return Ok(());
            };
            let state = lock.lock();
            let _namespace = self.inner.namespace.read();
            let publish = (|| -> Result<bool> {
                let latest = self.record(id)?;
                Ok(!state.writer
                    && latest.node.version == r.node.version
                    && self.policy(id)? == p
                    && self.live(id)?)
            })();
            if !matches!(publish, Ok(true)) {
                if root.remove_object(&object).is_ok() {
                    self.delete_record(&candidate_gc)?;
                }
                return publish.map(|_| ());
            }
            let copy = Copy {
                node: id.into(),
                target: name.clone(),
                object,
                version: r.node.version,
                hash: Some(hash.into()),
            };
            let mut puts = vec![Self::encoded(schema::copy(id, &name), &copy)?];
            if let Some(old) = self.kv_get::<Copy>(&schema::copy(id, &name))? {
                puts.push(Self::encoded(schema::garbage(&name), &old)?)
            }
            self.atomic(puts, vec![candidate_gc], true)?;
            domains.insert(self.require::<Target>(&schema::target(&name))?.domain);
            valid.push(copy);
        }
        ensure!(
            valid.iter().filter(|c| targets.contains(&c.target)).count() >= required,
            "persistent storage placement incomplete"
        );
        let lock = self.file_lock(id);
        let Some(_pin) = self.try_idle_pin(id)? else {
            return Ok(());
        };
        let mut state = lock.lock();
        let _namespace = self.inner.namespace.read();
        let mut latest = self.record(id)?;
        if state.writer || latest.node.version != r.node.version || self.policy(id)? != p {
            return Ok(());
        }
        let mut keep: HashSet<String> = valid
            .iter()
            .filter(|c| targets.contains(&c.target))
            .take(required)
            .map(|c| c.target.clone())
            .collect();
        // Only archive reconciliation may release local protection based on
        // remote copies, after immediate read-back verification.
        if required < count {
            keep.extend(valid.iter().map(|c| c.target.clone()));
        }
        for c in &valid {
            if p.write_cache.contains(&c.target) {
                let pressure = self.target_stat(&c.target).is_ok_and(|s| {
                    100 - s.f_bavail.saturating_mul(100) / s.f_blocks.max(1)
                        >= u64::from(p.low_water)
                });
                if !pressure {
                    keep.insert(c.target.clone());
                }
            }
        }
        if keep.is_empty() && required > 0 {
            keep.insert(valid[0].target.clone());
        }
        let mut puts = vec![];
        let mut deletes = vec![];
        for c in self.copies(id)? {
            if !keep.contains(&c.target) {
                puts.push(Self::encoded(schema::garbage(&c.target), &c)?);
                deletes.push(schema::copy(id, &c.target));
            }
        }
        latest.primary = valid
            .iter()
            .find(|c| keep.contains(&c.target) && Some(&c.target) == latest.primary.as_ref())
            .or_else(|| valid.iter().find(|c| keep.contains(&c.target)))
            .map(|c| c.target.clone());
        latest.error = None;
        latest.incident = None;
        latest.placement = Some(placement);
        puts.push(Self::encoded(schema::node(id), &latest)?);
        self.atomic(puts, deletes, true)?;
        if state.body.as_ref().is_some_and(|b| {
            !valid.iter().any(|c| {
                keep.contains(&c.target) && c.target == b.copy.target && c.object == b.copy.object
            })
        }) {
            state.body = None;
        }
        Ok(())
    }
}
