use super::*;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

fn db(path: &Path) -> Result<Connection, Box<dyn std::error::Error>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err("数据库必须是普通文件".into());
    }
    check_sidecars(path, false)?;
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

const ACTIVE: &str = "SELECT
    (SELECT count(*) FROM tasks WHERE state IN
        ('queued','preparing','launching','running','verifying','cancelling')) +
    (SELECT count(*) FROM dispatch_gate WHERE state='pending') +
    (SELECT count(*) FROM managed_groups WHERE state IN ('tracking','blocking'))";

fn idle(conn: &Connection) -> Result<i64, Box<dyn std::error::Error>> {
    let schema: i64 = conn.query_row(
        "SELECT value FROM meta WHERE key='schema_version'",
        [],
        |r| {
            let v: String = r.get(0)?;
            Ok(v.parse().unwrap_or(-1))
        },
    )?;
    let active: i64 = conn.query_row(ACTIVE, [], |r| r.get(0))?;
    if active != 0 {
        return Err("有排队、活动或退出未确认的任务，不能升级".into());
    }
    Ok(schema)
}

fn synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn snapshot(instance: &Path, target: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let (_lock, _) = relay_daemon::acquire_instance_lock(instance)?;
    let root = instance.parent().ok_or("实例无父目录")?;
    let artifacts = root.join("managed").join("artifacts");
    let source = db(&instance.join("relay.sqlite3"))?;
    let schema = idle(&source)?;
    if schema != relay_store::SCHEMA_VERSION && (schema, relay_store::SCHEMA_VERSION) != (8, 9) {
        return Err("该版本不支持备份此 schema".into());
    }
    fs::DirBuilder::new().mode(0o700).create(target)?;
    let snapshot = target.join("relay.sqlite3");
    let snapshot_name = snapshot.to_str().ok_or("备份路径不是 UTF-8")?;
    source.execute("VACUUM INTO ?1", [snapshot_name])?;
    let saved = db(&snapshot)?;
    let totals = counts(&source)?;
    if check_backup(&saved, schema)? != totals {
        return Err("SQLite 快照计数校验失败".into());
    }
    File::open(&snapshot)?.sync_all()?;

    let copy_root = target.join("artifacts");
    copy_artifacts(&saved, &artifacts, &copy_root, true, None)?;
    let info = json!({"format": 2, "schema": schema, "tasks": totals.0, "events": totals.1,
        "artifacts": totals.2, "created_at": now_text(), "database_sha256": digest(&snapshot)?});
    synced(&target.join("verified.json"), info.to_string().as_bytes())?;
    File::open(target)?.sync_all()?;
    if let Some(parent) = target.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(info)
}

fn prepare_migration(instance: &Path, stage: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let (_lock, _) = relay_daemon::acquire_instance_lock(instance)?;
    let live = instance.join("relay.sqlite3");
    let source = db(&live)?;
    let original = file_identity(&live)?;
    let totals = check_backup(&source, 8)?;
    if relay_store::SCHEMA_VERSION != 9 {
        return Err("此迁移只支持 v8 → v9".into());
    }
    let parent = stage.parent().ok_or("缺少暂存目录父路径")?;
    private_directory(parent)?;
    fs::DirBuilder::new().mode(0o700).create(stage)?;
    let candidate = stage.join("relay.sqlite3");
    let path = candidate.to_str().ok_or("暂存路径不是 UTF-8")?;
    source.execute("VACUUM INTO ?1", [path])?;
    drop(source);
    relay_store::migrate_v8_to_v9(&candidate, &stage.join("pre-v9.sqlite3"))?;
    if check_backup(&db(&candidate)?, 9)? != totals {
        return Err("迁移前后任务、事件或制品计数不符".into());
    }
    File::open(&candidate)?.sync_all()?;
    let binding = json!({"original":original,"candidate":file_identity(&candidate)?});
    synced(&stage.join("exchange.json"), binding.to_string().as_bytes())?;
    File::open(stage)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(json!({"from": 8, "to": 9, "tasks": totals.0, "events": totals.1, "artifacts": totals.2}))
}

fn file_identity(path: &Path) -> io::Result<[u64; 2]> {
    let m = regular(path)?.metadata()?;
    Ok([m.dev(), m.ino()])
}

fn private_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let m = fs::symlink_metadata(path)?;
    if path.canonicalize()? != path
        || !m.is_dir()
        || m.mode() & 0o7777 != 0o700
        || m.uid() != rustix::process::geteuid().as_raw()
    {
        return Err("迁移目录必须是当前用户的 0700 物理绝对路径".into());
    }
    Ok(())
}

fn check_sidecars(path: &Path, must_be_absent: bool) -> Result<(), Box<dyn std::error::Error>> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match fs::symlink_metadata(sidecar) {
            Ok(m) if !must_be_absent && m.is_file() => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return Err("数据库侧车仍存在或类型无效".into()),
        }
    }
    Ok(())
}

fn seal_for_exchange(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    regular(path)?;
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    if mode != "delete" {
        return Err("不能完成 WAL 检查点并退出 WAL 模式".into());
    }
    drop(conn);
    check_sidecars(path, true)?;
    regular(path)?.sync_all()?;
    Ok(())
}

fn switch_database(
    instance: &Path,
    stage: &Path,
    rollback: bool,
) -> Result<Value, Box<dyn std::error::Error>> {
    let (_lock, _) = relay_daemon::acquire_instance_lock(instance)?;
    let marker = instance.join("upgrade.maintenance");
    regular(&marker)?;
    private_directory(stage)?;
    let live = instance.join("relay.sqlite3");
    let other = stage.join("relay.sqlite3");
    let mut encoded = Vec::new();
    regular(&stage.join("exchange.json"))?
        .take(512)
        .read_to_end(&mut encoded)?;
    let binding: Value = serde_json::from_slice(&encoded)?;
    let original = binding.get("original").ok_or("缺少原库标识")?;
    let candidate = binding.get("candidate").ok_or("缺少迁移库标识")?;
    let live_identity = json!(file_identity(&live)?);
    let peer_identity = json!(file_identity(&other)?);
    let live_conn = db(&live)?;
    let live_schema = idle(&live_conn)?;
    if rollback && live_schema == 8 && original == &live_identity && candidate == &peer_identity {
        return Ok(json!({"schema":8,"changed":false}));
    }
    let (expected_live, expected_peer) = if rollback {
        (candidate, original)
    } else {
        (original, candidate)
    };
    if expected_live != &live_identity || expected_peer != &peer_identity {
        return Err("数据库身份与本次迁移记录不符，拒绝交换".into());
    }
    let peer_conn = db(&other)?;
    let peer_schema = idle(&peer_conn)?;
    let expected = if rollback { (9, 8) } else { (8, 9) };
    if (live_schema, peer_schema) != expected
        || check_backup(&live_conn, live_schema)? != check_backup(&peer_conn, peer_schema)?
    {
        return Err("交换前 schema 或历史计数不符，数据库未替换".into());
    }
    drop(live_conn);
    drop(peer_conn);
    seal_for_exchange(&live)?;
    seal_for_exchange(&other)?;
    relay_sys::exchange_paths(&live, &other)?;
    File::open(instance)?.sync_all()?;
    File::open(stage)?.sync_all()?;
    Ok(json!({"schema":peer_schema,"changed":true}))
}

fn regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "需要普通文件"));
    }
    Ok(file)
}

fn digest(path: &Path) -> io::Result<String> {
    let mut file = regular(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        write!(hex, "{byte:02x}").expect("write to String");
    }
    Ok(hex)
}

fn counts(conn: &Connection) -> rusqlite::Result<(i64, i64, i64)> {
    conn.query_row("SELECT (SELECT count(*) FROM tasks), (SELECT count(*) FROM events), (SELECT count(*) FROM artifacts)", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
}

fn check_backup(
    conn: &Connection,
    schema: i64,
) -> Result<(i64, i64, i64), Box<dyn std::error::Error>> {
    conn.pragma_update(None, "trusted_schema", "OFF")?;
    let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let foreign_keys = conn.prepare("PRAGMA foreign_key_check")?.exists([])?;
    if check != "ok" || foreign_keys || idle(conn)? != schema {
        return Err("SQLite 完整性、引用或 schema 校验失败".into());
    }
    Ok(counts(conn)?)
}

fn copy_artifacts(
    source: &Connection,
    artifacts: &Path,
    copy_root: &Path,
    check_registered_path: bool,
    relocate_to: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::DirBuilder::new().mode(0o700).create(copy_root)?;
    let mut query = source.prepare("SELECT task_id,attempt_no,artifact_dir,manifest_sha256,patch_sha256,receipt_sha256,handoff_sha256 FROM artifacts ORDER BY task_id,attempt_no")?;
    let mut rows = query.query([])?;
    let mut copied = 0;
    while let Some(row) = rows.next()? {
        let task: String = row.get(0)?;
        let attempt: u32 = row.get(1)?;
        if !task.starts_with("task_")
            || !task.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || attempt == 0
        {
            return Err("备份或恢复遇到无效制品标识".into());
        }
        let key = format!("{task}-{attempt}");
        let dir = artifacts.join(&key);
        let stored: String = row.get(2)?;
        if (check_registered_path && Path::new(&stored) != dir) || dir.canonicalize()? != dir {
            return Err("备份或恢复遇到未知或符号链接制品目录".into());
        }
        let dest = copy_root.join(&key);
        fs::DirBuilder::new().mode(0o700).create(&dest)?;
        for (idx, file, max) in [
            (3, "manifest.json", 64 * 1024),
            (4, "diff.patch", 512 * 1024),
            (5, "test_receipt.json", 64 * 1024),
            (6, "handoff.json", 16 * 1024),
        ] {
            let expected: String = row.get(idx)?;
            let input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(dir.join(file))?;
            if !input.metadata()?.is_file() {
                return Err("备份或恢复遇到非常规制品文件".into());
            }
            let mut bytes = Vec::new();
            input.take(max + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max || relay_git::sha256_hex(&bytes) != expected {
                return Err(format!("制品摘要或长度不符：{task}/{attempt}/{file}").into());
            }
            synced(&dest.join(file), &bytes)?;
        }
        File::open(&dest)?.sync_all()?;
        copied += 1;
    }
    drop(rows);
    drop(query);
    if let Some(target) = relocate_to {
        let path = target.to_str().ok_or("恢复路径不是 UTF-8")?;
        if source.execute(
            "UPDATE artifacts SET artifact_dir=?1 || '/' || task_id || '-' || attempt_no",
            [path],
        )? != copied
        {
            return Err("恢复制品位置登记失败".into());
        }
    }
    File::open(copy_root)?.sync_all()?;
    Ok(())
}

fn restore(backup: &Path, target: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let parent = target.parent().ok_or("恢复目标无父目录")?;
    if !backup.is_absolute()
        || backup.canonicalize()? != backup
        || parent.canonicalize()? != parent
        || target.file_name().is_none()
        || !target.is_absolute()
    {
        return Err("备份和恢复目标必须是物理绝对路径".into());
    }
    if fs::symlink_metadata(target).is_ok() {
        return Err("恢复目标已存在".into());
    }
    let mut marker = Vec::new();
    regular(&backup.join("verified.json"))?
        .take(8193)
        .read_to_end(&mut marker)?;
    if marker.len() > 8192 {
        return Err("备份清单过大".into());
    }
    let manifest: Value = serde_json::from_slice(&marker)?;
    let format = manifest["format"].as_u64().ok_or("备份格式缺失")?;
    let schema = relay_store::SCHEMA_VERSION;
    if !matches!(format, 1 | 2) || manifest["schema"].as_i64() != Some(schema) {
        return Err("备份格式或 schema 不兼容".into());
    }
    let stage = parent.join(format!(".relay-restore-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&stage)?;
    let result = (|| -> Result<Value, Box<dyn std::error::Error>> {
        let instance = stage.join("instance");
        let managed = stage.join("managed");
        for dir in [&instance, &managed] {
            fs::DirBuilder::new().mode(0o700).create(dir)?;
        }
        let local_db = instance.join("relay.sqlite3");
        let mut from = regular(&backup.join("relay.sqlite3"))?;
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&local_db)?;
        io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        drop(to);
        if format == 2 && manifest["database_sha256"].as_str() != Some(digest(&local_db)?.as_str())
        {
            return Err("备份数据库摘要不匹配".into());
        }
        let mut conn = Connection::open_with_flags(&local_db, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let totals = check_backup(&conn, schema)?;
        if [
            manifest["tasks"].as_i64(),
            manifest["events"].as_i64(),
            manifest["artifacts"].as_i64(),
        ] != [Some(totals.0), Some(totals.1), Some(totals.2)]
        {
            return Err("备份计数与清单不匹配".into());
        }
        // 不读取旧实例的绝对路径。事务内只重绑新实例的登记引用。
        let tx = conn.transaction()?;
        copy_artifacts(
            &tx,
            &backup.join("artifacts"),
            &managed.join("artifacts"),
            false,
            Some(&target.join("managed/artifacts")),
        )?;
        tx.commit()?;
        drop(conn);
        File::open(&local_db)?.sync_all()?;
        synced(&instance.join("restore.archive"), b"read-only archive\n")?;
        for dir in [&instance, &managed, &stage] {
            File::open(dir)?.sync_all()?;
        }
        relay_sys::rename_no_replace(&stage, target)?;
        File::open(parent)?.sync_all()?;
        Ok(
            json!({"instance_dir": target.join("instance"), "mode": "archive", "schema": schema,
            "database_sha256_verified": format == 2, "tasks": totals.0, "events": totals.1, "artifacts": totals.2}),
        )
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Rotate the two launchd log files with a 5 MiB x 5 policy. Run only while
/// the daemon is stopped: launchd keeps writing to the old descriptor, so a
/// live rename would not redirect future output into the new file.
fn rotate_logs(instance: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    let root = instance.parent().ok_or("实例无父目录")?;
    let (_lock, _) = relay_daemon::acquire_instance_lock(instance)?;
    let limit = 5 * 1024 * 1024u64;
    let mut rotated = Vec::new();
    for name in ["daemon.log", "daemon.err.log"] {
        let path = root.join(name);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() <= limit {
            continue;
        }
        for index in (1..=4).rev() {
            let from = root.join(format!("{name}.{index}"));
            let to = root.join(format!("{name}.{}", index + 1));
            if from.is_file() {
                let _ = fs::remove_file(&to);
                fs::rename(&from, &to)?;
            }
        }
        fs::rename(&path, root.join(format!("{name}.1")))?;
        rotated.push(json!({"file": name, "bytes": meta.len()}));
    }
    Ok(json!({"schema_version": 1, "limit_bytes": limit, "keep": 5, "rotated": rotated}))
}

/// Preview or remove old terminal artifacts/worktrees. `--apply` is required
/// to delete; the default is a read-only preview. Plans that stayed unused
/// for more than 24 hours are removed with the artifact cleanup. Audit
/// events/tasks are retained for the separate 90-day metadata policy.
fn prune(
    instance: &Path,
    days: u64,
    metadata_days: u64,
    apply: bool,
) -> Result<Value, Box<dyn std::error::Error>> {
    if !(1..=3650).contains(&days) || metadata_days > 3650 {
        return Err("--older-than-days 必须在 1..=3650，--metadata-days 在 0..=3650".into());
    }
    let (_lock, _) = relay_daemon::acquire_instance_lock(instance)?;
    let db_path = instance.join("relay.sqlite3");
    let mut conn = Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let cutoff = format!("-{days} days");
    let tasks: Vec<(String, u32)> = conn
        .prepare(
            "SELECT task_id, active_attempt FROM tasks
             WHERE state IN ('completed','rejected','cancelled','failed')
               AND julianday(updated_at) < julianday('now', ?1)
             ORDER BY task_id",
        )?
        .query_map([&cutoff], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))?
        .collect::<Result<_, _>>()?;
    let managed = instance.parent().ok_or("实例无父目录")?.join("managed");
    let artifacts_root = managed.join("artifacts");
    let worktrees_root = managed.join("worktrees");
    let mut entries = Vec::new();
    let mut bytes = 0u64;
    for (task, attempt) in &tasks {
        let worktree = worktrees_root.join(task);
        let worktree_meta = fs::symlink_metadata(&worktree).ok();
        if worktree.starts_with(&worktrees_root)
            && worktree_meta
                .as_ref()
                .is_some_and(|m| m.is_dir() && !m.is_symlink())
        {
            let size = dir_size(&worktree);
            bytes = bytes.saturating_add(size);
            entries.push(json!({"kind":"worktree","path":worktree,"bytes":size}));
        }
        let artifacts: Vec<(u32, String)> = conn
            .prepare("SELECT attempt_no, artifact_dir FROM artifacts WHERE task_id=?1")?
            .query_map([task], |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (artifact_attempt, dir) in artifacts {
            let path = PathBuf::from(&dir);
            // Only remove a real directory that canonicalizes under the
            // managed artifacts root; never follow/replace a symlink.
            let Ok(canon_root) = artifacts_root.canonicalize() else {
                continue;
            };
            let Ok(canon_path) = path.canonicalize() else {
                continue;
            };
            if canon_path == canon_root || !canon_path.starts_with(&canon_root) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&canon_path) else {
                continue;
            };
            if meta.is_symlink() || !meta.is_dir() {
                continue;
            }
            let size = dir_size(&canon_path);
            bytes = bytes.saturating_add(size);
            entries.push(
                json!({"kind":"artifact","task":task,"attempt":artifact_attempt,
                "path":canon_path,"bytes":size}),
            );
            if apply {
                fs::remove_dir_all(&canon_path)?;
                conn.execute(
                    "DELETE FROM artifacts WHERE task_id=?1 AND attempt_no=?2",
                    rusqlite::params![task, i64::from(artifact_attempt)],
                )?;
            }
        }
        let _ = attempt;
        if apply
            && worktree.starts_with(&worktrees_root)
            && worktree_meta
                .as_ref()
                .is_some_and(|m| m.is_dir() && !m.is_symlink())
        {
            fs::remove_dir_all(&worktree)?;
        }
    }
    // 09: audit metadata defaults to 90 days. Only remove a task when its
    // artifact rows and worktree are already gone; active/awaiting/uncertain
    // tasks are never candidates.
    let metadata_candidates: Vec<String> = if metadata_days > 0 {
        let cutoff = format!("-{metadata_days} days");
        let mut stmt = conn.prepare(
            "SELECT task_id FROM tasks
             WHERE state IN ('completed','rejected','cancelled','failed')
               AND julianday(updated_at) < julianday('now', ?1)
             ORDER BY task_id",
        )?;
        let rows = stmt
            .query_map([&cutoff], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        rows
    } else {
        Vec::new()
    };
    // Scratch directories no attempt references after it ends: review
    // copies, per-implementation Claude sandboxes and staged artifacts.
    // Prune holds the instance lock, so no task can be using them.
    let mut scratch_paths: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(&managed) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "review" || name.starts_with(".claude-implement-") {
                let path = entry.path();
                if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && !meta.is_symlink())
                {
                    scratch_paths.push(path);
                }
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&artifacts_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".staging-") {
                let path = entry.path();
                if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir() && !meta.is_symlink())
                {
                    scratch_paths.push(path);
                }
            }
        }
    }
    let mut scratch_dirs = 0u64;
    for path in scratch_paths {
        let size = dir_size(&path);
        bytes = bytes.saturating_add(size);
        entries.push(json!({"kind":"scratch","path":path,"bytes":size}));
        scratch_dirs += 1;
        if apply {
            fs::remove_dir_all(&path)?;
        }
    }
    let mut metadata_removed = 0u64;
    if apply && !metadata_candidates.is_empty() {
        let tx = conn.transaction()?;
        for task in &metadata_candidates {
            let artifacts: i64 = tx.query_row(
                "SELECT COUNT(*) FROM artifacts WHERE task_id=?1",
                [task.as_str()],
                |r| r.get(0),
            )?;
            let worktree = worktrees_root.join(task);
            if artifacts != 0 || worktree.exists() {
                continue;
            }
            for table in [
                "stage_runs",
                "events",
                "attempts",
                "requests",
                "artifacts",
                "gate_resolutions",
                "dispatch_gate",
                "managed_groups",
                "launch_fences",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE task_id=?1"),
                    [task.as_str()],
                )?;
            }
            tx.execute("DELETE FROM tasks WHERE task_id=?1", [task.as_str()])?;
            metadata_removed += 1;
        }
        tx.commit()?;
    }
    let plans = if apply {
        conn.execute(
            "DELETE FROM plans WHERE status='unused'
               AND julianday(expires_at) < julianday('now', '-1 day')",
            [],
        )?
    } else {
        0
    };
    Ok(json!({
        "schema_version": 1,
        "preview": !apply,
        "older_than_days": days,
        "metadata_days": metadata_days,
        "terminal_tasks": tasks.len(),
        "metadata_candidates": metadata_candidates.len(),
        "metadata_tasks_removed": metadata_removed,
        "entries": entries,
        "scratch_dirs": scratch_dirs,
        "estimated_bytes": bytes,
        "expired_plans_removed": if apply { plans } else { 0 },
    }))
}

pub(super) fn run(args: &[String]) -> Result<(), u8> {
    let result = (|| -> Result<Value, Box<dyn std::error::Error>> {
        if let [sub, backup, target] = args {
            if sub == "restore" {
                return restore(Path::new(backup), Path::new(target));
            }
        }
        let instance = flag(args, "--instance-dir")
            .map(PathBuf::from)
            .ok_or("缺少 --instance-dir")?;
        match args.first().map(String::as_str) {
            Some("check-idle") => {
                let state = client::call(&instance.join("relay.sock"), "ui.overview", json!({}))?;
                let versions = &state["service"]["versions"];
                if versions["product"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .is_none()
                    || versions["wire"].as_u64().is_none()
                    || versions["db_schema"].as_u64().is_none()
                {
                    return Err("运行中的服务未返回版本".into());
                }
                let schema = idle(&db(&instance.join("relay.sqlite3"))?)?;
                if versions["db_schema"].as_i64() != Some(schema) {
                    return Err("服务版本与磁盘 schema 不一致".into());
                }
                Ok(versions.clone())
            }
            Some("serve-archive") => {
                let dir = instance.parent().ok_or("实例无父目录")?;
                if !fs::symlink_metadata(instance.join("restore.archive"))?.is_file() {
                    return Err("只能打开已恢复的只读档案".into());
                }
                let config = relay_daemon::service::ServiceConfig::from_json(&json!({
                    "repo_root": dir.join("unused-repo"), "managed_root": dir.join("managed"), "rules": []
                }))?;
                let mut daemon = relay_daemon::start_m1(
                    &instance, &now_text(), relay_daemon::ipc::IpcConfig::default(), config,
                )?;
                daemon.serve_forever();
                Ok(json!({"mode":"archive"}))
            }
            Some(action @ ("prepare-v8-v9" | "activate-v9" | "rollback-v8")) => {
                let target = args.get(1).map(PathBuf::from).filter(|p| p.is_absolute())
                    .ok_or("缺少绝对迁移暂存路径")?;
                if action == "prepare-v8-v9" {
                    prepare_migration(&instance, &target)
                } else {
                    switch_database(&instance, &target, action == "rollback-v8")
                }
            }
            Some("rotate-logs") => rotate_logs(&instance),
            Some("prune") => {
                let days = flag(args, "--older-than-days")
                    .unwrap_or_else(|| "30".to_owned())
                    .parse::<u64>()
                    .map_err(|_| "无效的 --older-than-days")?;
                let metadata_days = flag(args, "--metadata-days")
                    .unwrap_or_else(|| "90".to_owned())
                    .parse::<u64>()
                    .map_err(|_| "无效的 --metadata-days")?;
                prune(
                    &instance,
                    days,
                    metadata_days,
                    args.iter().any(|a| a == "--apply"),
                )
            }
            Some("snapshot") => {
                let target = args
                    .get(1)
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .ok_or("缺少绝对备份路径")?;
                snapshot(&instance, &target)
            }
            _ => Err(
                "用法：relayctl maintenance check-idle|rotate-logs|prune|snapshot|prepare-v8-v9|activate-v9|rollback-v8|serve-archive … --instance-dir <目录>；或 restore <备份> <新根目录>"
                    .into(),
            ),
        }
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            Ok(())
        }
        Err(e) => {
            eprintln!("维护检查失败：{e}");
            Err(1)
        }
    }
}
