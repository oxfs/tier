use anyhow::Result;
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;
use tier::{core::Engine, recovery};

#[test]
fn import_requires_confirmation_for_damaged_media() -> Result<()> {
    let tmp = tempdir()?;
    let disk = tmp.path().join("disk");
    fs::create_dir(&disk)?;
    let engine = Engine::open(&tmp.path().join("source.db"))?;
    engine.add_target("disk", &disk)?;
    engine.set_policy("/",&serde_json::from_value(serde_json::json!({"front":["disk"],"front_count":1,"long":["disk"],"long_count":1,"delay_secs":0}))?)?;
    engine.put("/photo", b"correct")?;
    let snapshot = engine.snapshot()?;
    let object = disk.join(snapshot["copies"][0]["object"].as_str().unwrap());
    fs::write(&object, b"corrupt")?;
    let destination = tmp.path().join("recovered.db");
    let maps = BTreeMap::new();
    assert!(recovery::import(&snapshot, &destination, &maps, false).is_err());
    assert!(!destination.exists());
    let report = recovery::import(&snapshot, &destination, &maps, true)?;
    assert_eq!(report["missing"], serde_json::json!(["photo"]));
    assert_eq!(report["damaged"], serde_json::json!([object]));
    assert!(Engine::open(&destination)?.get("/photo").is_err());
    Ok(())
}
#[test]
fn trash_subtree_is_verified_without_restoring_deleted_paths() -> Result<()> {
    let tmp = tempdir()?;
    let disk = tmp.path().join("disk");
    fs::create_dir(&disk)?;
    let db = Engine::open(&tmp.path().join("db"))?;
    db.add_target("local", &disk)?;
    db.set_policy("/",&serde_json::from_value(serde_json::json!({"front":["local"],"front_count":1,"long":["local"],"long_count":1,"delay_secs":0}))?)?;
    db.mkdir("/old")?;
    db.put("/old/photo", b"abc")?;
    db.remove("/old")?;
    db.put("/live", b"live")?;
    let mut snapshot = db.snapshot()?;
    let label = serde_json::json!({"entry":1,"path":"old/photo"});
    let out = tmp.path().join("extract");
    let report = recovery::inspect(&snapshot, std::slice::from_ref(&disk), Some(&out))?;
    assert_eq!(report["trash"]["verified"], serde_json::json!([label]));
    assert!(!out.join("old").exists());
    assert_eq!(fs::read(out.join("live"))?, b"live");
    let id = snapshot["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "photo")
        .unwrap()["id"]
        .clone();
    let copy = snapshot["copies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node"] == id)
        .unwrap()
        .clone();
    fs::write(disk.join(copy["object"].as_str().unwrap()), b"bad")?;
    let dest = tmp.path().join("restore");
    assert!(recovery::import(&snapshot, &dest, &BTreeMap::new(), false).is_err());
    assert!(!dest.exists());
    let report = recovery::preflight(&snapshot, std::slice::from_ref(&disk))?;
    assert_eq!(report["trash"]["missing"], serde_json::json!([label]));
    assert_eq!(report["trash"]["damaged"].as_array().unwrap().len(), 1);

    // Simulate an archived trash file with no registered local copy.
    snapshot["copies"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["node"] != id);
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let url = format!("http://{}", server.server_addr());
    snapshot["cloud_targets"] = serde_json::json!([{"name":"remote","url":url}]);
    snapshot["cloud_copies"] = serde_json::json!([{"node":id,"hash":copy["hash"],"target":"remote","key":uuid::Uuid::new_v4().to_string()}]);
    let worker = std::thread::spawn(move || {
        for bytes in [b"abc", b"bad"] {
            server
                .recv()
                .unwrap()
                .respond(tiny_http::Response::from_data(bytes.to_vec()))
                .unwrap();
        }
    });
    let report = recovery::preflight(&snapshot, std::slice::from_ref(&disk))?;
    assert_eq!(report["trash"]["missing"], serde_json::json!([]));
    assert_eq!(report["trash"]["verified_remote"][0]["path"], label);
    let report = recovery::preflight(&snapshot, &[disk])?;
    assert_eq!(report["trash"]["missing"], serde_json::json!([label]));
    assert_eq!(
        report["trash"]["remote_errors"].as_array().unwrap().len(),
        1
    );
    worker.join().unwrap();
    Ok(())
}
