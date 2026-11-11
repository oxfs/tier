//! Physical names carry a stable object UUID and a non-authoritative name hint.
//! Full logical names live in metadata; UTF-8 hints fit Linux's 255-byte limit.
use super::*;

fn hint(label: &str) -> std::borrow::Cow<'_, str> {
    if label.len() <= 255 - 38 {
        return label.into();
    }
    let extension = Path::new(label)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && s.len() <= 32);
    let mut length = 255 - 38 - extension.map_or(0, |s| s.len() + 1);
    while !label.is_char_boundary(length) {
        length -= 1;
    }
    match extension {
        Some(extension) => format!("{}.{extension}", &label[..length]).into(),
        None => (&label[..length]).into(),
    }
}

pub(crate) fn name(label: &str) -> String {
    format!("{}--{}", Uuid::new_v4(), hint(label))
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.len() <= 255
        && name.get(..36).is_some_and(|id| Uuid::parse_str(id).is_ok())
        && name.get(36..38) == Some("--")
        && name.get(38..).is_some_and(safe_name)
}

pub(crate) fn matches_name(object: &str, label: &str) -> bool {
    object.rsplit('/').next().and_then(|s| s.get(38..)) == Some(hint(label).as_ref())
}

impl Engine {
    /// Add a new hard link before committing its path. The old link remains
    /// valid across a crash until the synchronous metadata transaction commits.
    pub(crate) fn refresh_object_names(&self, id: &str) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let lock = self.file_lock(id);
        let Some(_pin) = self.try_idle_pin(id)? else {
            return Ok(());
        };
        let mut state = lock.lock();
        if state.handles != 0 {
            return Ok(());
        }
        let record = self.record(id)?;
        for mut copy in self.copies(id)? {
            if copy.version != record.node.version || matches_name(&copy.object, &record.name) {
                continue;
            }
            let Ok(target) = self.target(&copy.target) else {
                continue;
            };
            let (parent, old_name) = copy
                .object
                .rsplit_once('/')
                .context("invalid object path")?;
            ensure!(valid_name(old_name), "invalid object name");
            let next = format!("{parent}/{}--{}", &old_name[..36], hint(&record.name));
            let parent_dir = target.root.join(parent);
            let old_path = target.root.join(&copy.object);
            let new_path = target.root.join(&next);
            let exists = match fs::symlink_metadata(&new_path) {
                Ok(new) => {
                    let old = fs::symlink_metadata(&old_path)?;
                    ensure!(
                        old.is_file()
                            && new.is_file()
                            && old.dev() == new.dev()
                            && old.ino() == new.ino(),
                        "object name collision"
                    );
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            let old = copy.clone();
            copy.object = next;
            // A crash before publication must leave a discoverable cleanup job,
            // even if the logical name changes again before the next retry.
            let candidate_gc = schema::garbage(&copy.target);
            self.put_record(&candidate_gc, &copy, true)?;
            if !exists && let Err(error) = fs::hard_link(&old_path, &new_path) {
                self.delete_record(&candidate_gc)?;
                if error.kind() == io::ErrorKind::NotFound {
                    continue;
                }
                return Err(error.into());
            }
            fs::File::open(parent_dir)?.sync_all()?;
            self.atomic(
                vec![
                    Self::encoded(schema::copy(id, &copy.target), &copy)?,
                    Self::encoded(schema::garbage(&old.target), &old)?,
                ],
                vec![candidate_gc],
                true,
            )?;
            if let Some(body) = &mut state.body
                && body.copy.target == old.target
                && body.copy.object == old.object
            {
                body.copy = copy;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_bounded_and_keep_uuid_identity_separate_from_unicode() {
        let label = format!("{}照片.jpg", "海".repeat(100));
        let name = name(&label);
        assert!(valid_name(&name));
        assert!(matches_name(&name, &label));
        assert!(name.len() <= 255);
        assert!(name.ends_with(".jpg"));
        assert!(!valid_name(&format!("{}--../escape", Uuid::new_v4())));
    }

    #[test]
    fn rename_restarts_and_aba_gc_preserve_the_current_object() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir()?;
        let disk = temp.path().join("disk");
        fs::create_dir(&disk)?;
        let db = temp.path().join("metadata");
        let engine = Engine::open(&db)?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                persistent_storage: vec!["disk".into()],
                persistent_copies: 1,
                ..Default::default()
            },
        )?;
        engine.put("/原名.txt", b"contents")?;
        engine.reconcile()?;
        let id = engine.stat("/原名.txt")?.id;
        let original = engine.copies(&id)?.remove(0).object;
        assert!(original.ends_with("--原名.txt"));
        engine.rename("/原名.txt", "/改名.txt")?;
        assert!(disk.join(&original).exists()); // foreground rename did no body work
        engine.refresh_object_names(&id)?;
        let renamed = engine.copies(&id)?.remove(0).object;
        assert!(renamed.ends_with("--改名.txt"));
        assert_eq!(
            fs::metadata(disk.join(&original))?.ino(),
            fs::metadata(disk.join(&renamed))?.ino()
        );
        drop(engine); // restart after publication, before old-name GC
        let engine = Engine::open(&db)?;
        assert_eq!(engine.get("/改名.txt")?, b"contents");
        engine.rename("/改名.txt", "/原名.txt")?;
        engine.refresh_object_names(&id)?;
        assert_eq!(engine.copies(&id)?[0].object, original);
        engine.collect_garbage()?;
        assert_eq!(engine.get("/原名.txt")?, b"contents");
        assert!(!disk.join(renamed).exists());
        engine.remove("/原名.txt")?;
        engine.purge(1)?;
        engine.collect_garbage()?;
        assert!(!disk.join("objects").join(id).exists());
        assert!(disk.join("objects").is_dir());
        Ok(())
    }

    #[test]
    fn interrupted_link_and_active_writer_are_safe_to_retry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let disk = temp.path().join("disk");
        fs::create_dir(&disk)?;
        let db = temp.path().join("metadata");
        let engine = Engine::open(&db)?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                persistent_storage: vec!["disk".into()],
                persistent_copies: 1,
                ..Default::default()
            },
        )?;
        engine.put("/before", b"contents")?;
        let id = engine.stat("/before")?.id;
        let original = engine.copies(&id)?[0].object.clone();
        let writer = engine.open_file("/before", true, false, false)?;
        engine.rename("/before", "/after")?;
        engine.refresh_object_names(&id)?;
        assert_eq!(engine.copies(&id)?[0].object, original);
        writer.close()?;
        let prefix = original.rsplit_once("--").unwrap().0;
        let new_name = format!("{prefix}--after");
        let mut candidate = engine.copies(&id)?[0].clone();
        candidate.object = new_name.clone();
        engine.put_record(&schema::garbage("disk"), &candidate, true)?;
        fs::hard_link(disk.join(&original), disk.join(&new_name))?;
        drop(engine); // restart after linking, before metadata publication
        let engine = Engine::open(&db)?;
        assert_eq!(engine.get("/after")?, b"contents");
        engine.rename("/after", "/third")?;
        engine.refresh_object_names(&id)?;
        engine.collect_garbage()?;
        assert!(engine.copies(&id)?[0].object.ends_with("--third"));
        assert!(!disk.join(new_name).exists());
        assert!(!disk.join(original).exists());
        assert_eq!(engine.get("/third")?, b"contents");
        Ok(())
    }
}
