use anyhow::{Result, ensure};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tier_v2::{Store, config::Config};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        ensure!(
            Instant::now() < deadline,
            "background service did not make progress"
        );
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn service_skips_busy_files_reloads_policy_and_stops_cleanly() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let replica = directory.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let config = directory.path().join("config.toml");
    let settings = |gc| {
        format!(
            "primary = '{}'\n[replicas]\nbackup = '{}'\n[[policies]]\npath = ''\ncopies = 1\ngc = {gc}\n[background]\nscan_interval_secs = 1\naudit_interval_secs = 2\n",
            primary.display(),
            replica.display()
        )
    };
    fs::write(&config, settings(false))?;
    let store = Store::initialize(Config::load(&config)?)?;
    {
        let mut writer = store.write(Path::new("file"))?;
        writer.write_all(b"durable content")?;
        writer.sync_all()?;
    }
    let reader = store.read(Path::new("file"))?;
    let log_path = directory.path().join("service.log");
    let mut service = Service(
        Command::new(env!("CARGO_BIN_EXE_tier-v2"))
            .arg(&config)
            .arg("run")
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log_path)?)
            .spawn()?,
    );
    wait_until(|| fs::read_to_string(&log_path).is_ok_and(|log| log.contains("replicate")))?;
    assert!(!replica.join("file").exists());
    drop(reader);
    wait_until(|| replica.join("file").exists())?;
    assert_eq!(fs::read(replica.join("file"))?, b"durable content");
    // Atomic configuration replacement, as a deployment tool would perform.
    let replacement = directory.path().join("next.toml");
    fs::write(&replacement, settings(true))?;
    fs::rename(replacement, &config)?;
    wait_until(|| !primary.join("file").exists())?;
    // SAFETY: child id is a live child process started above; SIGTERM is valid.
    ensure!(
        unsafe { libc::kill(service.0.id() as libc::pid_t, libc::SIGTERM) } == 0,
        "failed to stop service"
    );
    wait_until(|| service.0.try_wait().is_ok_and(|status| status.is_some()))?;
    ensure!(service.0.wait()?.success(), "service did not exit cleanly");
    Ok(())
}
