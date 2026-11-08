use super::*;

pub(super) fn init(engine: &Engine) -> Result<()> {
    let tx = engine.write_transaction()?;
    let version: i32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='xattrs')",
            [],
            |r| r.get(0),
        )?;
        if existing {
            tx.execute_batch("ALTER TABLE xattrs RENAME TO old_xattrs")?;
        }
        tx.execute_batch("CREATE TABLE IF NOT EXISTS xattrs(node TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
      name TEXT NOT NULL CHECK((name='user.DOSATTRIB' OR name GLOB 'user.DosStream.?*') AND length(CAST(name AS BLOB))<=255 AND instr(name,char(0))=0),
      value BLOB NOT NULL CHECK(typeof(value)='blob' AND length(value)<=65536),PRIMARY KEY(node,name));")?;
        if existing {
            tx.execute_batch(
                "INSERT INTO xattrs SELECT node,name,value FROM old_xattrs; DROP TABLE old_xattrs",
            )?;
        }
        tx.execute_batch(
            "ALTER TABLE nodes ADD COLUMN display_mtime_nsec INTEGER CHECK(display_mtime_nsec BETWEEN 0 AND 999999999);
            ALTER TABLE nodes ADD COLUMN display_atime INTEGER;
            ALTER TABLE nodes ADD COLUMN display_atime_nsec INTEGER CHECK(display_atime_nsec BETWEEN 0 AND 999999999);
            PRAGMA user_version=1",
        )?;
    }
    tx.commit()?;
    Ok(())
}

const PREFIX: &str = "user.DosStream.";

fn check_name(name: &str) -> Result<()> {
    if name != "user.DOSATTRIB" && (!name.starts_with(PREFIX) || name.len() == PREFIX.len()) {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP).into());
    }
    Ok(())
}

impl Engine {
    pub(crate) fn stream_attr(&self, node: &str, name: &str) -> Result<Vec<u8>> {
        check_name(name)?;
        self.db
            .prepare_cached("SELECT value FROM xattrs WHERE node=? AND name=?")?
            .query_row((node, name), |r| r.get(0))
            .optional()?
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODATA).into())
    }

    pub(crate) fn set_stream_attr(
        &self,
        node: &str,
        name: &str,
        value: &[u8],
        flags: i32,
    ) -> Result<()> {
        check_name(name)?;
        if value.len() > 65536 {
            return Err(io::Error::from_raw_os_error(libc::E2BIG).into());
        }
        let query = match flags {
            libc::XATTR_CREATE => {
                "INSERT INTO xattrs(node,name,value) VALUES(?,?,?) ON CONFLICT(node,name) DO NOTHING"
            }
            libc::XATTR_REPLACE => {
                if self.stream_attr(node, name)? == value {
                    return Ok(());
                }
                "UPDATE xattrs SET value=?3 WHERE node=?1 AND name=?2"
            }
            0 => {
                "INSERT INTO xattrs(node,name,value) VALUES(?,?,?) ON CONFLICT(node,name) DO UPDATE SET value=excluded.value WHERE value != excluded.value"
            }
            _ => return Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
        };
        if self
            .db
            .prepare_cached(query)?
            .execute((node, name, value))?
            == 0
            && flags != 0
        {
            return Err(
                io::Error::from_raw_os_error(if flags == libc::XATTR_CREATE {
                    libc::EEXIST
                } else {
                    libc::ENODATA
                })
                .into(),
            );
        }
        Ok(())
    }
    pub(crate) fn list_stream_attrs(&self, node: &str) -> Result<Vec<u8>> {
        let mut names = Vec::new();
        let mut st = self
            .db
            .prepare_cached("SELECT name FROM xattrs WHERE node=? ORDER BY name")?;
        for name in st.query_map([node], |r| r.get::<_, String>(0))? {
            names.extend_from_slice(name?.as_bytes());
            names.push(0);
            if names.len() > 65536 {
                return Err(io::Error::from_raw_os_error(libc::E2BIG).into());
            }
        }
        Ok(names)
    }
    pub(crate) fn remove_stream_attr(&self, node: &str, name: &str) -> Result<()> {
        check_name(name)?;
        if self
            .db
            .execute("DELETE FROM xattrs WHERE node=? AND name=?", (node, name))?
            == 0
        {
            return Err(io::Error::from_raw_os_error(libc::ENODATA).into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_metadata_follows_node_and_roundtrips_through_recovery() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let engine = Engine::open(&tmp.path().join("source"))?;
        let disk = tmp.path().join("disk");
        fs::create_dir(&disk)?;
        engine.add_target("disk", &disk)?;
        engine.set_policy(
            "/",
            &Policy {
                write_cache: vec!["disk".into()],
                ..Default::default()
            },
        )?;
        engine.mkdir("/a")?;
        let node = engine.stat("/a")?.id;
        let name = "user.DosStream.AFP_AfpInfo";
        let before = engine.snapshot()?;
        assert!(before.get("xattrs").is_none());
        crate::recovery::import(
            &before,
            &tmp.path().join("legacy"),
            &Default::default(),
            false,
        )?;
        let bytes = [255, 0, 1, 128];
        engine.set_stream_attr(&node, name, &bytes, libc::XATTR_CREATE)?;
        assert!(
            engine
                .set_stream_attr(&node, name, &[], libc::XATTR_CREATE)
                .is_err()
        );
        assert!(
            engine
                .set_stream_attr(&node, "user.DOSATTRIB_OTHER", &[], 0)
                .is_err()
        );
        engine.set_stream_attr(&node, "user.DOSATTRIB", &bytes, 0)?;
        let revision = engine.snapshot()?["seq"].clone();
        engine.set_stream_attr(&node, name, &bytes, 0)?;
        engine.set_stream_attr(&node, name, &bytes, libc::XATTR_REPLACE)?;
        assert_eq!(engine.snapshot()?["seq"], revision);
        assert!(
            engine
                .set_stream_attr(&node, "user.DosStream.absent", &[], libc::XATTR_REPLACE)
                .is_err()
        );
        let time = SystemTime::now();
        engine.set_times(&node, Some(time), Some(time))?;
        let revision = engine.snapshot()?["seq"].clone();
        engine.set_times(&node, Some(time), None)?;
        engine.set_times(&node, None, Some(time))?;
        assert_eq!(engine.snapshot()?["seq"], revision);
        let after = engine.snapshot()?;
        assert!(after["seq"].as_u64() > before["seq"].as_u64());
        crate::recovery::import(&after, &tmp.path().join("new"), &Default::default(), false)?;
        let loaded = Engine::open(&tmp.path().join("new"))?;
        assert_eq!(loaded.stream_attr(&node, "user.DOSATTRIB")?, bytes);
        loaded.rename("/a", "/b")?;
        loaded.remove("/b")?;
        let id = loaded.snapshot()?["trash"][0]["id"].as_i64().unwrap();
        loaded.restore(id, "/c")?;
        assert_eq!(loaded.stream_attr(&loaded.stat("/c")?.id, name)?, bytes);
        loaded.remove("/c")?;
        loaded.purge(loaded.snapshot()?["trash"][0]["id"].as_i64().unwrap())?;
        assert!(loaded.snapshot()?.get("xattrs").is_none());
        Ok(())
    }
}
