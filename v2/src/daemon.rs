use anyhow::Result;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tier_v2::{Store, config::Config};

pub fn run(path: &Path) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    let mut last_audit = None;
    while !stop.load(Ordering::Relaxed) {
        let mut interval = Duration::from_secs(60);
        let result = (|| -> Result<()> {
            // A changed policy takes effect on the next pass, without restart.
            let config = Config::load(path)?;
            interval = Duration::from_secs(config.background.scan_interval_secs);
            let audit_interval = Duration::from_secs(config.background.audit_interval_secs);
            let store = Store::new(config)?;
            let report = store.scan_pending()?;
            let caught_up = report.busy == 0 && report.errors.is_empty();
            log("replicate", &serde_json::to_value(report)?);
            if caught_up
                && last_audit.is_none_or(|instant: Instant| instant.elapsed() >= audit_interval)
            {
                let report = store.scan()?;
                log("audit", &serde_json::to_value(report)?);
                last_audit = Some(Instant::now());
            }
            Ok(())
        })();
        if let Err(error) = result {
            log(
                "error",
                &serde_json::json!({"message": format!("{error:#}")}),
            );
        }
        let deadline = Instant::now() + interval;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

fn log(event: &str, details: &serde_json::Value) {
    eprintln!(
        "{}",
        serde_json::json!({
            "time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            "event": event,
            "details": details,
        })
    );
}
