//! A12: prune removes review/implementation scratch dirs and abandoned
//! artifact staging areas, while leaving the instance usable.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use relay_store::Store;

fn temp_root(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("relay-prune-{name}-{}-{nanos}", std::process::id()));
    let instance = root.join("instance");
    fs::create_dir_all(&instance).unwrap();
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::write(path.join("data.txt"), "x").unwrap();
}

#[test]
fn prune_removes_scratch_directories() {
    let root = temp_root("scratch");
    let instance = root.join("instance");
    let managed = root.join("managed");
    Store::open(&instance.join("relay.sqlite3")).unwrap();
    dir(&managed.join("review"));
    dir(&managed.join(".claude-implement-task_1-1"));
    dir(&managed.join("artifacts/.staging-123"));
    dir(&managed.join("artifacts/kept"));

    let output = Command::new(env!("CARGO_BIN_EXE_relayctl"))
        .args([
            "maintenance",
            "prune",
            "--instance-dir",
            instance.to_str().unwrap(),
            "--older-than-days",
            "30",
            "--metadata-days",
            "90",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!managed.join("review").exists());
    assert!(!managed.join(".claude-implement-task_1-1").exists());
    assert!(!managed.join("artifacts/.staging-123").exists());
    assert!(managed.join("artifacts/kept").exists());
    // Instance still opens.
    Store::open(&instance.join("relay.sqlite3")).unwrap();
    fs::remove_dir_all(&root).ok();
}
