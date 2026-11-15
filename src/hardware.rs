use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Seek, SeekFrom},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn physical(path: &Path) -> Result<BTreeSet<String>> {
    let path = fs::canonicalize(path)?;
    if path.join("partition").exists() {
        return physical(path.parent().context("partition has no parent")?);
    }
    let mut disks = BTreeSet::new();
    let slaves = path.join("slaves");
    if slaves.exists() {
        for slave in fs::read_dir(slaves)? {
            disks.extend(physical(&slave?.path())?);
        }
    }
    if disks.is_empty() && path.join("device").exists() {
        disks.insert(
            path.file_name()
                .context("missing device name")?
                .to_string_lossy()
                .into_owned(),
        );
    }
    Ok(disks)
}

fn normalize(data: &Value, code: i32) -> Value {
    let attribute = |id| {
        data["ata_smart_attributes"]["table"]
            .as_array()
            .and_then(|table| table.iter().find(|entry| entry["id"] == id))
            .map_or(Value::Null, |entry| entry["raw"]["value"].clone())
    };
    let mut result = json!({
        "model":data.get("model_name").or_else(|| data.get("product")),
        "serial":data["serial_number"], "temperature":data["temperature"]["current"],
        "passed":data["smart_status"]["passed"], "power_on_hours":data["power_on_time"]["hours"],
        "percentage_used":data["nvme_smart_health_information_log"]["percentage_used"],
        "media_errors":data["nvme_smart_health_information_log"]["media_errors"],
        "reallocated":attribute(5), "pending":attribute(197), "uncorrectable":attribute(198),
        "warning":code & 248 != 0
    });
    if code & 7 != 0 {
        result["error"] = json!("SMART 读取不完整或设备不支持");
    }
    if code & 8 != 0 {
        result["passed"] = json!(false);
    }
    result
}

fn read_disk(program: &Path, name: &str, timeout: Duration) -> Value {
    let device = format!("/dev/{name}");
    let reading = (|| -> Result<Value> {
        // A file avoids blocking on a full stdout pipe while waiting for smartctl.
        let mut output = tempfile::tempfile()?;
        let mut child = Command::new(program)
            .args(["--all", "--json", &device])
            .stdout(output.try_clone()?)
            .stderr(Stdio::null())
            .spawn()
            .context("无法运行 smartctl，请检查 smartmontools 安装及设备权限")?;
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                bail!("SMART 读取超时");
            }
            thread::sleep(Duration::from_millis(50));
        };
        let code = status.code().context("smartctl 被信号终止")?;
        output.seek(SeekFrom::Start(0))?;
        let data: Value = serde_json::from_reader(output).context("无法解析 SMART 信息")?;
        Ok(normalize(&data, code))
    })();
    let mut result = reading.unwrap_or_else(|error| json!({"error":format!("{error:#}")}));
    result["device"] = json!(device);
    result
}

fn collect(sysfs: &Path, program: &Path, timeout: Duration) -> Result<Value> {
    let mut devices = BTreeMap::new();
    let mut readings = BTreeMap::new();
    for entry in fs::read_dir(sysfs)? {
        let path = entry?.path();
        // A device can disappear during enumeration; retry it next round.
        let (Ok(names), Ok(id)) = (physical(&path), fs::read_to_string(path.join("dev"))) else {
            continue;
        };
        let values: Vec<_> = names
            .into_iter()
            .map(|name| {
                readings
                    .entry(name.clone())
                    .or_insert_with(|| read_disk(program, &name, timeout))
                    .clone()
            })
            .collect();
        devices.insert(id.trim().to_owned(), values);
    }
    Ok(
        json!({"updated_at":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(), "devices":devices}),
    )
}

pub fn run() -> Result<()> {
    let snapshot = collect(
        Path::new("/sys/class/block"),
        Path::new("/usr/sbin/smartctl"),
        Duration::from_secs(10),
    )?;
    let directory = Path::new("/run/tier-hardware");
    fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, &snapshot)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    file.persist(directory.join("status.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stacked_devices_share_one_physical_reading() -> Result<()> {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let disk = root.join("devices/sda");
        let partition = disk.join("sda1");
        let stacked = root.join("devices/dm-0");
        fs::create_dir_all(disk.join("device"))?;
        fs::create_dir_all(&partition)?;
        fs::write(partition.join("partition"), "1")?;
        fs::create_dir_all(stacked.join("slaves"))?;
        symlink(&partition, stacked.join("slaves/sda1"))?;
        let sysfs = root.join("block");
        fs::create_dir(&sysfs)?;
        for (name, path, id) in [
            ("sda", &disk, "8:0"),
            ("sda1", &partition, "8:1"),
            ("dm-0", &stacked, "253:0"),
        ] {
            fs::write(path.join("dev"), id)?;
            symlink(path, sysfs.join(name))?;
        }
        let script = root.join("smartctl");
        fs::write(
            &script,
            "#!/bin/sh\necho call >> \"$0.calls\"\nprintf '{\"model_name\":\"physical disk\"}'\n",
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        let snapshot = collect(&sysfs, &script, Duration::from_secs(1))?;
        for id in ["8:0", "8:1", "253:0"] {
            let readings = snapshot["devices"][id].as_array().unwrap();
            assert_eq!(readings.len(), 1);
            assert_eq!(readings[0]["device"], "/dev/sda");
            assert_eq!(readings[0]["model"], "physical disk");
        }
        assert_eq!(fs::read_to_string(root.join("smartctl.calls"))?, "call\n");
        Ok(())
    }

    #[test]
    fn smart_status_bits_and_optional_values() {
        let data = json!({"temperature":{"current":0},"smart_status":{"passed":true},"ata_smart_attributes":{"table":[{"id":197,"raw":{"value":3}}]},"nvme_smart_health_information_log":{"percentage_used":105}});
        let result = normalize(&data, 8);
        assert_eq!(result["passed"], false);
        assert_eq!(result["temperature"], 0);
        assert_eq!(result["pending"], 3);
        assert_eq!(result["percentage_used"], 105);
        assert!(result["reallocated"].is_null());
        assert!(result["error"].is_null());
        assert!(normalize(&data, 2)["error"].is_string());
    }
    #[test]
    fn command_failures_are_per_device() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let script = directory.path().join("smartctl");
        assert!(read_disk(&script, "sda", Duration::from_millis(10))["error"].is_string());
        fs::write(&script, "#!/bin/sh\nexec sleep 5\n")?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        assert_eq!(
            read_disk(&script, "sda", Duration::from_millis(10))["error"],
            "SMART 读取超时"
        );
        fs::write(&script, "#!/bin/sh\nprintf invalid\n")?;
        assert!(read_disk(&script, "sda", Duration::from_secs(1))["error"].is_string());
        Ok(())
    }
}
