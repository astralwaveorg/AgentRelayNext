//! IPC 接线（WP3-A2b 后半段）：临时 Unix socket 上的限长帧会话。
//!
//! socket 位于实例目录内（`relay.sock`）：实例目录 0700（A2a）保证
//! 只有同 UID 进程能进入目录并连接，socket 文件绑定后核对属主 UID。
//! 已存在的 socket 路径一律拒绝——不抢占、不删除归属不明的 socket。
//! 每连接：有限读写等待（超时关闭）、LF 分帧会话（relay-protocol
//! [`relay_protocol::session`]）、hello v1 握手确认、唯一一条业务
//! 请求（只读 status 替身）回复后关闭；并发连接数有上限，超出的
//! 连接立即关闭。绑定失败不宣告 ready。
//!
//! 本包不接真实业务写入、CLI/MCP 或 worker；跨 UID 对端校验无测试
//! 环境，标未测。

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use relay_protocol::{RequestHandler, Session};

/// IPC 配置：有限读写等待、连接总期限与并发上限。
#[derive(Debug, Clone)]
pub struct IpcConfig {
    /// 单次读等待上限（毫秒）：空闲连接超时即关闭。
    pub read_timeout_ms: u64,
    /// 单次写等待上限（毫秒）。
    pub write_timeout_ms: u64,
    /// 连接总期限（毫秒）：从 accept 起算，持续低速发送也不能一直
    /// 占槽。
    pub total_deadline_ms: u64,
    /// 并发连接数上限：超出的连接立即关闭，不处理。
    pub max_connections: usize,
}

impl Default for IpcConfig {
    fn default() -> Self {
        IpcConfig {
            read_timeout_ms: 5_000,
            write_timeout_ms: 5_000,
            total_deadline_ms: 30_000,
            max_connections: 8,
        }
    }
}

/// 固定只读 status 替身：对任何请求返回同一份只读状态（不接真实
/// 业务写入）。
#[derive(Debug, Default)]
pub struct ReadOnlyStatusHandler;

impl RequestHandler for ReadOnlyStatusHandler {
    fn handle(
        &mut self,
        _method: &str,
        _params: Option<&serde_json::Value>,
        _id: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "schema_version": 1,
            "state": "ready",
            "readonly": true,
        }))
    }
}

/// 跨线程共享的处理器适配：Session 持有适配器，内部锁住共享 handler，
/// 并统计业务调用次数供零调用断言。骨架模式用只读 status 替身；
/// M1 业务模式用 M1 分派器（trait object 统一接入）。
struct DynHandler(Arc<Mutex<dyn RequestHandler + Send>>, Arc<AtomicUsize>);

impl RequestHandler for DynHandler {
    fn handle(
        &mut self,
        method: &str,
        params: Option<&serde_json::Value>,
        id: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0
            .lock()
            .expect("业务 handler 锁中毒")
            .handle(method, params, id)
    }
}

/// 闭包处理器适配：daemon 侧的分派闭包（M1）与只读替身统一接入。
pub struct FnHandler<F: Fn(&str, Option<&serde_json::Value>) -> Option<serde_json::Value> + Send>(
    pub F,
);

impl<F> RequestHandler for FnHandler<F>
where
    F: Fn(&str, Option<&serde_json::Value>) -> Option<serde_json::Value> + Send,
{
    fn handle(
        &mut self,
        method: &str,
        params: Option<&serde_json::Value>,
        _id: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        (self.0)(method, params)
    }
}

/// accept 循环：非阻塞轮询停止标志；每连接一个线程，并发有上限，
/// 超出上限的连接立即关闭（不读取、不处理）。
pub(crate) fn accept_loop(
    listener: UnixListener,
    stop: impl Fn() -> bool,
    config: IpcConfig,
    handler: Arc<Mutex<dyn RequestHandler + Send>>,
    business_calls: Arc<AtomicUsize>,
) {
    if let Err(_e) = listener.set_nonblocking(true) {
        return;
    }
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        if stop() {
            break;
        }
        match listener.accept() {
            Ok((read_half, _addr)) => {
                if active.load(Ordering::SeqCst) >= config.max_connections {
                    // 超并发上限：立即关闭，不处理。
                    drop(read_half);
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let write_half = match read_half.try_clone() {
                    Ok(s) => s,
                    Err(_) => {
                        active.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                };
                let active2 = Arc::clone(&active);
                let handler2 = Arc::clone(&handler);
                let calls2 = Arc::clone(&business_calls);
                let cfg = config.clone();
                std::thread::spawn(move || {
                    let mut read_half = read_half;
                    let mut write_half = write_half;
                    serve_connection(&mut read_half, &mut write_half, &handler2, &cfg, &calls2);
                    active2.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

/// 服务一个连接：有限读写等待；LF 分帧会话（hello 握手 → 唯一一条
/// 业务请求回复后关闭）；任何会话错误或超时都关闭连接。
pub(crate) fn serve_connection(
    read_half: &mut UnixStream,
    write_half: &mut UnixStream,
    handler: &Arc<Mutex<dyn RequestHandler + Send>>,
    config: &IpcConfig,
    business_calls: &Arc<AtomicUsize>,
) {
    // 分派任何输入到业务会话之前，先核对内核提供的对端有效 UID：
    // 与本进程不一致的连接直接关闭（零 handler 调用）。跨 UID 拒绝
    // 路径无第二 UID 测试环境，标未测；同 UID 由联测覆盖。
    let own = rustix::process::geteuid().as_raw();
    match relay_sys::socket_peer_euid(read_half) {
        Ok(uid) if uid == own => {}
        Ok(uid) => {
            eprintln!("relay daemon: 拒绝连接：对端 euid {uid} 与本进程 {own} 不一致");
            return;
        }
        Err(_) => return,
    }
    // BSD/macOS 上 accept 出的 socket 继承监听端 O_NONBLOCK；显式恢复
    // 阻塞模式，让读写等待由 SO_RCVTIMEO/SO_SNDTIMEO 控制。
    let _ = read_half.set_nonblocking(false);
    let _ = write_half.set_nonblocking(false);
    let _ = read_half.set_read_timeout(Some(Duration::from_millis(config.read_timeout_ms)));
    let _ = write_half.set_write_timeout(Some(Duration::from_millis(config.write_timeout_ms)));
    let deadline = std::time::Instant::now() + Duration::from_millis(config.total_deadline_ms);
    let mut session = Session::new(DynHandler(Arc::clone(handler), Arc::clone(business_calls)));
    let mut buf = [0u8; 4096];
    loop {
        // 连接总期限：持续低速发送不能一直占槽。
        if std::time::Instant::now() >= deadline {
            break;
        }
        match read_half.read(&mut buf) {
            Ok(0) => {
                // 对端关闭：缓冲残留按断帧处理（结果不影响关闭动作）。
                let _ = session.finish();
                break;
            }
            Ok(n) => match session.push(&buf[..n]) {
                Ok(out) => {
                    if write_half.write_all(&out).is_err() {
                        break;
                    }
                    // 唯一业务请求已回复：关闭连接。
                    if session.is_closed() {
                        break;
                    }
                }
                Err(_err) => {
                    // 握手拒绝、限长、断帧、协议错误：关闭，不发半帧。
                    break;
                }
            },
            // 读写等待超限（SO_RCVTIMEO 表现为 WouldBlock）或其它
            // 错误：关闭连接。
            Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                break
            }
            Err(_) => break,
        }
    }
}
