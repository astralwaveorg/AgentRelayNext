//! WP3-A2b IPC 接线联测：临时 Unix socket 上的真实握手、status 往返、
//! 读写等待超时、并发上限与已有路径拒绝。socket 路径用短名（macOS
//! sun_path 104 字节限制）。跨 UID 对端校验无测试环境，标未测。

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use relay_daemon::ipc::IpcConfig;
use relay_daemon::DaemonError;

const T0: &str = "2026-11-22T01:00:00Z";

fn short_temp_dir() -> PathBuf {
    // 并行测试共用同进程：计数器保证目录名严格唯一（同毫秒取模会碰撞）。
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // 短名：TMPDIR(44) + 名(~15) + /i/relay.sock(13) < 104。
    let dir = std::env::temp_dir().join(format!(
        "rip{}{}{}",
        std::process::id(),
        seq,
        nanos % 1_000_000
    ));
    let instance = dir.join("i");
    std::fs::create_dir_all(&instance).unwrap();
    std::fs::set_permissions(&instance, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn cleanup(dir: &Path) {
    std::fs::remove_dir_all(dir).ok();
}

fn send_json(stream: &mut UnixStream, value: &serde_json::Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).unwrap();
}

/// 读取一帧（以 LF 结尾）；连接关闭且无数据时返回 None。
fn read_frame(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return if buf.is_empty() { None } else { Some(buf) };
            }
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    return Some(buf);
                }
            }
            Err(_) => {
                return if buf.is_empty() { None } else { Some(buf) };
            }
        }
    }
}

fn hello() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"version": 1},
    })
}

fn status(id: i64) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "relay_status", "params": {"task_id": "task_x"},
    })
}

#[test]
fn handshake_and_status_round_trip_over_real_socket() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    let mut daemon = relay_daemon::start_with_config(
        &instance,
        T0,
        IpcConfig {
            read_timeout_ms: 2_000,
            write_timeout_ms: 2_000,
            total_deadline_ms: 10_000,
            max_connections: 4,
        },
    )
    .unwrap();
    daemon.serve_background();
    let path = daemon.socket_path().to_owned();

    let mut client = UnixStream::connect(&path).unwrap();
    send_json(&mut client, &hello());
    // 握手确认：同 ID 的 {"version":1}。
    let frame = read_frame(&mut client).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    assert_eq!(value["id"], 1, "{value}");
    assert_eq!(value["result"]["version"], 1, "{value}");

    // status 往返（只读替身）。
    send_json(&mut client, &status(7));
    let frame = read_frame(&mut client).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    assert_eq!(value["id"], 7, "{value}");
    assert_eq!(value["result"]["state"], "ready", "{value}");
    assert_eq!(value["result"]["readonly"], true, "{value}");

    // 回复后关闭：再读得到 EOF。
    assert!(
        read_frame(&mut client).is_none(),
        "服务器应在回复后关闭连接"
    );

    drop(daemon);
    assert!(!path.exists(), "daemon Drop 应删除自己绑定的 socket");
    cleanup(&dir);
}

#[test]
fn existing_socket_path_is_rejected_and_not_replaced() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    let socket_path = instance.join("relay.sock");
    use std::os::unix::fs::symlink;

    // 符号链接占位 → 拒绝。
    symlink(dir.join("elsewhere.sock"), &socket_path).unwrap();
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::SymlinkRejected { .. }),
        "{err:?}"
    );
    std::fs::remove_file(&socket_path).unwrap();

    // 归属不明的已有 socket：拒绝启动，不抢占、不删除。
    let holder = UnixListener::bind(&socket_path).unwrap();
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(
        matches!(err, DaemonError::SocketPathOccupied { .. }),
        "{err:?}"
    );
    assert!(socket_path.exists(), "已有 socket 不得被删除");
    // 已有 socket 仍归持有者：连接仍到达持有者。
    let client_thread = std::thread::spawn(move || UnixStream::connect(&socket_path).unwrap());
    let _conn = holder.accept().unwrap();
    let _client = client_thread.join().unwrap();

    cleanup(&dir);
}

#[test]
fn read_timeout_closes_idle_connection() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    let mut daemon = relay_daemon::start_with_config(
        &instance,
        T0,
        IpcConfig {
            read_timeout_ms: 150,
            write_timeout_ms: 1_000,
            total_deadline_ms: 10_000,
            max_connections: 4,
        },
    )
    .unwrap();
    daemon.serve_background();
    let path = daemon.socket_path().to_owned();

    let mut client = UnixStream::connect(&path).unwrap();
    // 不发送任何数据：读等待超限后服务器关闭连接。
    let mut byte = [0u8; 1];
    let result = client.read(&mut byte);
    match result {
        Ok(0) => {}
        Ok(_) => panic!("空闲连接应被关闭，而非收到数据"),
        Err(e) => assert!(
            e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::UnexpectedEof
                || e.kind() == std::io::ErrorKind::TimedOut,
            "意外错误: {e}"
        ),
    }

    drop(daemon);
    cleanup(&dir);
}

#[test]
fn concurrency_cap_closes_overflow_connections() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    let mut daemon = relay_daemon::start_with_config(
        &instance,
        T0,
        IpcConfig {
            read_timeout_ms: 1_000,
            write_timeout_ms: 1_000,
            total_deadline_ms: 10_000,
            max_connections: 1,
        },
    )
    .unwrap();
    daemon.serve_background();
    let path = daemon.socket_path().to_owned();

    // 第一个连接完成握手（确保其处理线程已占用并发名额）。
    let mut first = UnixStream::connect(&path).unwrap();
    send_json(&mut first, &hello());
    let frame = read_frame(&mut first).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    assert_eq!(value["result"]["version"], 1);

    // 第二个连接超出并发上限：立即被关闭。
    let mut second = UnixStream::connect(&path).unwrap();
    let mut byte = [0u8; 1];
    match second.read(&mut byte) {
        Ok(0) => {}
        Ok(_) => panic!("超限连接应立即关闭，而非收到数据"),
        Err(e) => assert!(
            e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::UnexpectedEof,
            "意外错误: {e}"
        ),
    }

    drop(daemon);
    cleanup(&dir);
}

/// 已独立复现的回归（17：f22c0b1）：daemon 退出曾无条件删除 socket
/// 路径，导致后来绑定在同一路径上的第二个监听器被删掉。退出必须只
/// 处理能确认归属（dev/ino 相同）的端点；归属不明则保留。
#[test]
fn exit_keeps_later_bound_socket_at_same_path() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    let daemon = relay_daemon::start(&instance, T0).unwrap();
    let path = daemon.socket_path().to_owned();

    // 第二个监听器重新绑定同一路径（接管者：先摘除再绑定）。
    std::fs::remove_file(&path).unwrap();
    let second = UnixListener::bind(&path).unwrap();

    // 退出第一个 daemon：不得删除后来绑定的 socket。
    drop(daemon);
    assert!(path.exists(), "后来绑定的 socket 必须保留");
    // 第二个监听器仍可正常 accept。
    let client = UnixStream::connect(&path).unwrap();
    let (_srv, _addr) = second.accept().unwrap();
    drop(client);

    cleanup(&dir);
}

/// 已复现回归（Codex）：新临时实例中，同 UID 的其他程序绑定 relay.sock
/// 后退出，Relay 启动曾接管并删除这个从未登记的 socket。必须拒绝启动
/// 并保留原路径；只有能证明是本实例先前创建的端点才允许回收。
#[test]
fn foreign_stale_socket_is_never_recycled() {
    // 短目录名（socket 路径受 macOS sun_path 限制）。
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rip{}{}{}",
        std::process::id(),
        seq,
        nanos % 1_000_000
    ));
    let instance = dir.join("instance");
    fs::create_dir_all(&instance).unwrap();
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&instance, fs::Permissions::from_mode(0o700)).unwrap();
    let socket_path = instance.join("relay.sock");
    // 同 UID 的其他程序绑定后退出：留下无监听者的 socket 文件，无归属
    // 记录（不是 Relay 创建的端点）。
    let foreign = UnixListener::bind(&socket_path).unwrap();
    drop(foreign);
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    match &err {
        DaemonError::SocketPathOccupied { reason, .. } => {
            assert!(reason.contains("归属"), "报可操作的原因: {reason}");
        }
        other => panic!("应拒绝启动: {other:?}"),
    }
    // 原路径保留，未被删除或替换。
    assert!(socket_path.exists(), "外来 socket 必须保留");
    // 路径上仍是无监听者的死 socket（未被 Relay 替换）。
    assert!(
        std::os::unix::net::UnixStream::connect(&socket_path).is_err(),
        "保留的端点不应被 Relay 占用"
    );
    cleanup(&dir);
}

/// 分派前对端 UID 门与拒绝路径零业务调用（同 UID 正例；跨 UID 拒绝
/// 无第二 UID 环境，标未测）。
#[test]
fn rejected_paths_make_zero_business_calls() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    let mut daemon = relay_daemon::start_with_config(
        &instance,
        T0,
        IpcConfig {
            read_timeout_ms: 1_000,
            write_timeout_ms: 1_000,
            max_connections: 4,
            total_deadline_ms: 300,
        },
    )
    .unwrap();
    daemon.serve_background();
    let path = daemon.socket_path().to_owned();

    // 畸形握手：连接关闭，业务 handler 零调用。
    let mut client = UnixStream::connect(&path).unwrap();
    client.write_all(b"{broken\n").unwrap();
    let mut byte = [0u8; 1];
    let _ = client.read(&mut byte); // 关闭信号
    drop(client);
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert_eq!(daemon.business_calls(), 0, "畸形握手不得调用业务 handler");

    // 持续低速发送（不构成完整帧）：总期限到后关闭，零业务调用。
    let mut slow = UnixStream::connect(&path).unwrap();
    for _ in 0..12 {
        let _ = slow.write_all(b"x");
        let _ = slow.flush();
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    // 总期限 300ms 早于低速流水结束：连接应已关闭。
    let mut byte = [0u8; 1];
    match slow.read(&mut byte) {
        Ok(0) => {}
        Ok(_) => panic!("总期限超限的连接不得继续收到数据"),
        Err(e) => assert!(
            e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::UnexpectedEof,
            "意外错误: {e}"
        ),
    }
    assert_eq!(daemon.business_calls(), 0, "总期限关闭不得调用业务 handler");

    drop(daemon);
    cleanup(&dir);
}

#[test]
fn store_failure_leaves_no_socket_path() {
    let dir = short_temp_dir();
    let instance = dir.join("i");
    // 伪造成非 SQLite 文件：store 打开失败，绑定不得发生。
    std::fs::write(instance.join("relay.sqlite3"), "not a database").unwrap();
    let err = relay_daemon::start(&instance, T0).unwrap_err();
    assert!(matches!(err, DaemonError::Store(_)), "{err:?}");
    assert!(
        !instance.join("relay.sock").exists(),
        "store 失败时不得绑定 socket（恢复先于绑定）"
    );
    cleanup(&dir);
}
