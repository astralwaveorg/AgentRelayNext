//! WP3-A2a 启动骨架契约测试：显式临时实例；二次实例拒绝；恢复先于
//! ready；失败不宣告 ready。

use std::fs;
use std::path::{Path, PathBuf};

use relay_daemon::DaemonError;
use relay_store::{PlanInput, Store, SubmitInput, TransitionInput};
use std::os::unix::fs::PermissionsExt;

const T0: &str = "2026-11-22T01:00:00Z";
const T15: &str = "2026-11-22T01:15:00Z";
const T60: &str = "2026-11-22T02:00:00Z";

fn remove_dir(dir: &Path) {
    fs::remove_dir_all(dir).ok();
}

fn temp_dir(_name: &str) -> PathBuf {
    // 短目录名：实例目录内的 IPC socket 路径受 macOS sun_path（104
    // 字节）限制，长 TMPDIR 下长名会绑定失败。
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rd{}{}{}",
        std::process::id(),
        seq,
        nanos % 1_000_000
    ));
    fs::create_dir_all(&dir).unwrap();
    // 02：实例目录 0700。
    #[cfg(unix)]
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// 用 relay-store 造一个 preparing 任务，供恢复测试。
fn stage_preparing_task(instance_dir: &Path, plan_id: &str, request_id: &str) -> String {
    let store = Store::open(&instance_dir.join("relay.sqlite3")).unwrap();
    store
        .put_plan(PlanInput {
            plan_id,
            project_id: "proj_f6a2",
            goal: "验证恢复",
            brief: "验证恢复简报",
            base_oid: "0123456789abcdef0123456789abcdef01234567",
            created_at: T0,
            expires_at: T60,
            write_scope_json: "[]",
            rule_ids_json: "[]",
            profile_id: "",
        })
        .unwrap();
    let outcome = store
        .submit(SubmitInput {
            request_id,
            plan_id,
            now: T0,
        })
        .unwrap();
    store
        .transition(TransitionInput {
            task_id: &outcome.task_id,
            expected_active_attempt: 1,
            expected_revision: 1,
            event: relay_core::Event::Claim,
            now: T15,
        })
        .unwrap();
    outcome.task_id
}

#[test]
fn first_instance_starts_ready_and_second_is_rejected() {
    let dir = temp_dir("a2a-second");
    let first = relay_daemon::start(&dir, T0).unwrap();
    assert!(first.recovered().is_empty());

    // 第二实例：锁争用 → 不就绪。
    let err = match relay_daemon::start(&dir, T0) {
        Err(err) => err,
        Ok(_) => panic!("第二实例应被拒绝"),
    };
    assert!(matches!(err, DaemonError::LockContended { .. }), "{err:?}");

    drop(first);
    remove_dir(&dir);
}

#[test]
fn recovery_runs_before_ready_and_keeps_interrupted() {
    let dir = temp_dir("a2a-recover");
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    let task_id = stage_preparing_task(&instance, "plan_r1", "req_r1");

    // 启动即恢复：preparing → interrupted，先于 ready 返回。
    let daemon = relay_daemon::start(&instance, T60).unwrap();
    let recovered = daemon.recovered();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].task_id, task_id);
    assert_eq!(recovered[0].state, relay_core::State::Interrupted);
    // 恢复不重新派发：store 内任务保持 interrupted。
    let task = daemon.store().get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.state, relay_core::State::Interrupted);
    assert_eq!(task.revision, 3);

    remove_dir(&dir);
}

#[test]
fn store_failure_keeps_daemon_not_ready() {
    let dir = temp_dir("a2a-corrupt");
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    // 伪造成非 SQLite 文件的 relay.sqlite3：打开即失败。
    fs::write(instance.join("relay.sqlite3"), "not a database").unwrap();

    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(matches!(err, DaemonError::Store(_)), "{err:?}");
    // 失败不宣告 ready：实例目录内不得有就绪产物。
    assert!(matches!(
        relay_daemon::start(&instance, T0),
        Err(DaemonError::Store(_))
    ));

    remove_dir(&dir);
}

#[test]
fn invalid_instance_dir_is_rejected() {
    let dir = temp_dir("a2a-invalid");
    // 路径是文件而非目录。
    let file_path = dir.join("instance");
    fs::write(&file_path, "x").unwrap();
    let err = relay_daemon::start(&file_path, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::InstanceDirInvalid { .. }),
        "{err:?}"
    );
    // 目录权限超出 0700 约束。
    let bad_mode = dir.join("bad-mode");
    fs::create_dir_all(&bad_mode).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&bad_mode, fs::Permissions::from_mode(0o755)).unwrap();
    let err = relay_daemon::start(&bad_mode, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::InstanceDirInvalid { ref reason, .. } if reason.contains("0700")),
        "{err:?}"
    );

    remove_dir(&dir);
}

#[test]
fn sticky_bit_instance_dir_is_rejected_before_any_writes() {
    let dir = temp_dir("a2a-sticky");
    // 01700（粘滞位 + 0700）也在触碰锁与 SQLite 之前拒绝。
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o1700)).unwrap();
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::InstanceDirInvalid { ref reason, .. } if reason.contains("0700")),
        "{err:?}"
    );
    // 零写入：拒绝路径不创建锁文件或 SQLite 文件。
    let entries: Vec<_> = fs::read_dir(&instance).unwrap().collect();
    assert!(entries.is_empty(), "拒绝后不得有写入: {entries:?}");
    // setgid 变体（02700）同样拒绝。
    let instance2 = dir.join("instance2");
    fs::create_dir_all(&instance2).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&instance2, fs::Permissions::from_mode(0o2700)).unwrap();
    let err = relay_daemon::start(&instance2, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::InstanceDirInvalid { ref reason, .. } if reason.contains("0700")),
        "{err:?}"
    );
    let entries: Vec<_> = fs::read_dir(&instance2).unwrap().collect();
    assert!(entries.is_empty(), "拒绝后不得有写入: {entries:?}");

    remove_dir(&dir);
}

#[test]
fn symlinked_store_paths_and_lock_are_rejected() {
    let dir = temp_dir("a2a-fix-symlink");
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    let outside = dir.join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    use std::os::unix::fs::symlink;

    // SQLite 主库路径是符号链接 → 拒绝。
    symlink(dir.join("target.db"), instance.join("relay.sqlite3")).unwrap();
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::SymlinkRejected { .. }),
        "{err:?}"
    );
    fs::remove_file(instance.join("relay.sqlite3")).unwrap();

    // 锁文件路径是符号链接 → 拒绝（先清掉首次 start 遗留的普通锁文件）。
    fs::remove_file(instance.join("daemon.lock")).unwrap();
    symlink(dir.join("target.lock"), instance.join("daemon.lock")).unwrap();
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::SymlinkRejected { .. }),
        "{err:?}"
    );
    fs::remove_file(instance.join("daemon.lock")).unwrap();

    // 拒绝后实例目录内无残留文件。
    let entries: Vec<_> = fs::read_dir(&instance).unwrap().collect();
    assert!(entries.is_empty(), "拒绝后不得有残留: {entries:?}");

    remove_dir(&dir);
}

#[test]
fn lock_releases_after_daemon_drop_allowing_restart() {
    let dir = temp_dir("a2a-restart");
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    {
        let daemon = relay_daemon::start(&instance, T0).unwrap();
        drop(daemon);
    }
    // 锁随 Drop 释放：可立即再次启动。
    let second = relay_daemon::start(&instance, T0).unwrap();
    assert!(second.recovered().is_empty());

    remove_dir(&dir);
}
