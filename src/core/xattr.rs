use super::*;

const PREFIX: &str = "user.DosStream.";

fn check_name(name: &str) -> Result<()> {
    if !name.starts_with(PREFIX) || name.len() == PREFIX.len() {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP).into());
    }
    Ok(())
}

impl Engine {
    pub(crate) fn stream_attr(&self, node: &str, name: &str) -> Result<Vec<u8>> {
        check_name(name)?;
        self.db
            .query_row(
                "SELECT value FROM xattrs WHERE node=? AND name=?",
                (node, name),
                |r| r.get(0),
            )
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
            libc::XATTR_REPLACE => "UPDATE xattrs SET value=?3 WHERE node=?1 AND name=?2",
            0 => {
                "INSERT INTO xattrs(node,name,value) VALUES(?,?,?) ON CONFLICT(node,name) DO UPDATE SET value=excluded.value"
            }
            _ => return Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
        };
        if self.db.execute(query, (node, name, value))? == 0 {
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
            .prepare("SELECT name FROM xattrs WHERE node=? ORDER BY name")?;
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
                .set_stream_attr(&node, "user.DOSATTRIB", &[], 0)
                .is_err()
        );
        let after = engine.snapshot()?;
        assert!(after["seq"].as_u64() > before["seq"].as_u64());
        crate::recovery::import(&after, &tmp.path().join("new"), &Default::default(), false)?;
        let loaded = Engine::open(&tmp.path().join("new"))?;
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
