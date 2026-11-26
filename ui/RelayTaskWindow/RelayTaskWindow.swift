// Relay 任务窗口（07），默认连接本机试用实例。
//
// 显示服务状态（inactive/connecting/ready/degraded，独立于任务
// 状态）、最近任务的状态，以及待验收任务的结果（评审结论、两个摘要、
// 差异分页）。数据来自 daemon 的本机 RPC `ui.overview` 与
// `relay_artifact`；前台每秒拉取一次，后台不轮询。关闭窗口不取消任务、
// 不停止后台服务。验收使用 relay_review，绑定任务 revision 与两个摘要。
//
// 无界面模式：`RelayTaskWindow --socket <path> --dump` 用同一套取数与
// 文案输出文本摘要后退出（0 = 服务 ready/degraded；1 = 不可达）。

import AppKit
import Darwin
import SwiftUI
import UniformTypeIdentifiers

// MARK: - 有界子进程（超时、取消、输出上限）

struct ProcessResult {
    let status: Int32
    let output: Data
    let timedOut: Bool
}

private final class OutputBox {
    private let lock = NSLock()
    private let limit: Int
    private var buffer = Data()
    init(limit: Int) { self.limit = limit }
    func append(_ chunk: Data) {
        guard !chunk.isEmpty else { return }
        lock.lock(); defer { lock.unlock() }
        if buffer.count < limit { buffer.append(chunk.prefix(limit - buffer.count)) }
    }
    var data: Data { lock.lock(); defer { lock.unlock() }; return buffer }
}

enum ProcessRunner {
    /// 默认 120 秒超时；stdout/stderr 合并后最多保留 64 KiB。超时先 TERM，
    /// 宽限后仍存活再 SIGKILL；不无限 readDataToEndOfFile。
    static func run(
        executable: URL,
        arguments: [String],
        environment: [String: String]? = nil,
        timeout: TimeInterval = 120,
        outputLimit: Int = 64 * 1024
    ) throws -> ProcessResult {
        let child = Process()
        child.executableURL = executable
        child.arguments = arguments
        if let environment { child.environment = environment }
        let pipe = Pipe()
        child.standardOutput = pipe
        child.standardError = pipe
        try child.run()
        let box = OutputBox(limit: outputLimit)
        let handle = pipe.fileHandleForReading
        handle.readabilityHandler = { stream in box.append(stream.availableData) }
        let deadline = Date().addingTimeInterval(timeout)
        while child.isRunning && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.05)
        }
        var timedOut = false
        if child.isRunning {
            timedOut = true
            child.terminate()
            Thread.sleep(forTimeInterval: 0.5)
            if child.isRunning { kill(child.processIdentifier, SIGKILL) }
        }
        child.waitUntilExit()
        handle.readabilityHandler = nil
        box.append(handle.availableData)
        return ProcessResult(status: child.terminationStatus, output: box.data, timedOut: timedOut)
    }
}

// MARK: - 本机 RPC（每连接 hello + 至多一条业务请求）

enum RelayError: Error, CustomStringConvertible {
    case inactive, pathTooLong, system(String), connect(String), io(String), badProtocol(String)
    case business(code: String, message: String, action: String)
    var description: String {
        switch self {
        case .inactive: return "后台服务未运行（socket 不存在）"
        case .pathTooLong: return "socket 路径过长"
        case .system(let what): return "系统调用失败：\(what)"
        case .connect(let why): return "无法连接后台服务：\(why)"
        case .io(let why): return "读写失败：\(why)"
        case .badProtocol(let why): return "协议错误：\(why)"
        case .business(let code, let message, _): return "\(message)（\(code)）"
        }
    }
}

enum Relay {
    private static func writeAll(_ fd: Int32, _ text: String) throws {
        let bytes = Array(text.utf8)
        var offset = 0
        while offset < bytes.count {
            let n = bytes[offset...].withUnsafeBufferPointer { buf in
                write(fd, buf.baseAddress, buf.count)
            }
            if n <= 0 { throw RelayError.io(String(cString: strerror(errno))) }
            offset += n
        }
    }

    private static func readLine(_ fd: Int32) throws -> String {
        var out: [UInt8] = []
        var byte: UInt8 = 0
        while true {
            let n = read(fd, &byte, 1)
            if n < 0 { throw RelayError.io(String(cString: strerror(errno))) }
            if n == 0 { break }
            if byte == 0x0A { break }
            out.append(byte)
            if out.count > 1_048_576 { throw RelayError.io("响应超过 1 MiB") }
        }
        return String(decoding: out, as: UTF8.self)
    }

    static func rpc(socket path: String, method: String, params: String) throws -> [String: Any] {
        guard FileManager.default.fileExists(atPath: path) else { throw RelayError.inactive }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw RelayError.system("socket") }
        defer { close(fd) }
        var noSignal: Int32 = 1
        guard setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSignal, socklen_t(MemoryLayout<Int32>.size)) == 0 else {
            throw RelayError.system("SO_NOSIGPIPE")
        }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8CString)
        guard bytes.count <= MemoryLayout.size(ofValue: addr.sun_path) else { throw RelayError.pathTooLong }
        withUnsafeMutableBytes(of: &addr.sun_path) { dst in
            for (index, byte) in bytes.enumerated() { dst[index] = UInt8(bitPattern: byte) }
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard rc == 0 else { throw RelayError.connect(String(cString: strerror(errno))) }
        var tv = timeval(tv_sec: 5, tv_usec: 0)
        for option in [SO_RCVTIMEO, SO_SNDTIMEO] {
            guard setsockopt(fd, SOL_SOCKET, option, &tv, socklen_t(MemoryLayout<timeval>.size)) == 0 else {
                throw RelayError.system("socket timeout")
            }
        }
        try writeAll(fd, #"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1}}"# + "\n")
        let confirm = try readLine(fd)
        guard let confirmData = confirm.data(using: .utf8),
              let confirmObject = try? JSONSerialization.jsonObject(with: confirmData) as? [String: Any],
              let confirmResult = confirmObject["result"] as? [String: Any],
              confirmResult["version"] as? Int == 1 else {
            throw RelayError.badProtocol("握手确认缺失或不符")
        }
        try writeAll(fd, #"{"jsonrpc":"2.0","id":2,"method":"\#(method)","params":\#(params)}"# + "\n")
        let reply = try readLine(fd)
        guard let data = reply.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw RelayError.badProtocol("响应不是 JSON")
        }
        if let error = object["error"] as? [String: Any] {
            throw RelayError.business(
                code: error["code"] as? String ?? "UNKNOWN",
                message: error["message"] as? String ?? "后台返回业务错误",
                action: error["action"] as? String ?? ""
            )
        }
        guard let result = object["result"] as? [String: Any] else {
            throw RelayError.badProtocol("响应缺少 result")
        }
        if let error = result["error"] as? [String: Any] {
            throw RelayError.business(
                code: error["code"] as? String ?? "UNKNOWN",
                message: error["message"] as? String ?? "后台返回业务错误",
                action: error["action"] as? String ?? ""
            )
        }
        return result
    }
}

// MARK: - 视图模型（与 --dump 共用）

struct StageInfo: Identifiable {
    let id: String, round: Int, stage: String, state: String
    let started: String, ended: String?
}

struct OverviewTask: Identifiable {
    let id: String, project: String, state: String, goal: String, updated: String
    let revision: Int, attempt: Int, reason: String?
    let review: [String: Any]?
    let stages: [StageInfo]
}

struct Overview {
    let state: String, pending: Int, blocking: Int, versions: [String: String]
    let tasks: [OverviewTask]

    init(_ result: [String: Any]) {
        let service = result["service"] as? [String: Any] ?? [:]
        let gate = service["dispatch_gate"] as? [String: Any] ?? [:]
        state = service["state"] as? String ?? "degraded"
        pending = gate["pending"] as? Int ?? 0
        blocking = gate["blocking"] as? Int ?? 0
        versions = (service["versions"] as? [String: Any] ?? [:]).compactMapValues {
            if let value = $0 as? String { return value }
            if let value = $0 as? Int { return String(value) }
            return nil
        }
        tasks = (result["tasks"] as? [[String: Any]] ?? []).map { item in
            let stages = (item["stages"] as? [[String: Any]] ?? []).enumerated().map { index, row in
                StageInfo(
                    id: "\(index)", round: row["round"] as? Int ?? 0,
                    stage: row["stage"] as? String ?? "?",
                    state: row["state"] as? String ?? "?",
                    started: row["started_at"] as? String ?? "",
                    ended: row["ended_at"] as? String)
            }
            return OverviewTask(
                id: item["id"] as? String ?? "?", project: item["project_id"] as? String ?? "?",
                state: item["state"] as? String ?? "?", goal: item["goal"] as? String ?? "",
                updated: item["updated_at"] as? String ?? "", revision: item["revision"] as? Int ?? 0,
                attempt: item["active_attempt"] as? Int ?? 0, reason: item["reason"] as? String,
                review: item["review"] as? [String: Any], stages: stages)
        }
    }
}

enum Wording {
    /// 07：任务汇总状态的可读名称。
    static func state(_ raw: String) -> String {
        switch raw {
        case "queued": return "排队"
        case "preparing", "launching", "running", "verifying": return "运行中（\(raw)）"
        case "awaiting_review": return "待验收"
        case "blocked", "interrupted": return "需处理（\(raw)）"
        case "failed": return "失败"
        case "cancelled": return "已取消"
        case "rejected": return "验收被拒"
        case "completed": return "已完成"
        default: return raw
        }
    }

    static func stage(_ raw: String) -> String {
        switch raw {
        case "implement": return "实现"
        case "verify": return "验证"
        case "review": return "评审"
        default: return raw
        }
    }

    static func failureReason(_ raw: String) -> String {
        let review = raw.hasPrefix("评审：")
        let code = review ? String(raw.dropFirst("评审：".count)) : raw
        let message: String
        switch code {
        case "GO_QUOTA_EXHAUSTED": message = "OpenCode Go 套餐额度已用完。请到账户页核对额度。"
        case "GO_BILLING_UNVERIFIED", "GO_BILLING_NOT_INCLUDED", "GO_USAGE_PROTOCOL":
            message = "无法确认 OpenCode Go 的套餐计费。任务已停止，请核对账户。"
        case "UPSTREAM_AUTH": message = "供应商拒绝了账户认证。请核对本次启用的账户。"
        case "UPSTREAM_RATE_LIMIT": message = "供应商限流。任务已停止，没有切换模型。"
        case "UPSTREAM_TRANSPORT": message = "连接供应商失败。请核对网络后再试。"
        case "MODEL_MISMATCH": message = "供应商返回的模型与任务获准使用的模型不一致。"
        default: return raw
        }
        return "\(review ? "评审：" : "")\(message)（\(code)）"
    }

    static func service(_ raw: String, pending: Int, blocking: Int) -> String {
        MenuStatus.serviceLabel(raw, pending: pending, blocking: blocking)
    }

    static func short(_ digest: String?) -> String {
        guard let digest, digest.count >= 12 else { return digest ?? "—" }
        return String(digest.prefix(12)) + "…"
    }

    static func clock(_ rfc3339: String) -> String {
        // 2026-11-23T12:34:56Z → 12:34:56Z（UTC，原样标注）
        if let range = rfc3339.range(of: "T") { return String(rfc3339[range.upperBound...]) }
        return rfc3339
    }

    /// 文本摘要（窗口与 --dump 共用同一份文案）。
    static func summary(_ overview: Overview, detail: String?) -> String {
        var lines = [service(overview.state, pending: overview.pending, blocking: overview.blocking)]
        lines.append("版本：产品 \(overview.versions["product"] ?? "?")，业务 \(overview.versions["business"] ?? "?")，wire \(overview.versions["wire"] ?? "?")，DB schema \(overview.versions["db_schema"] ?? "?")")
        if overview.tasks.isEmpty {
            lines.append("任务：暂无")
        } else {
            lines.append("任务（\(overview.tasks.count)）：")
            for task in overview.tasks {
                lines.append("  \(task.id)  \(state(task.state))  \(task.goal)  更新 \(clock(task.updated))")
                if task.state == "awaiting_review" {
                    if let review = task.review {
                        if let error = review["error"] as? String {
                            lines.append("    待验收结果：不可用（\(error)）")
                        } else {
                            lines.append("    评审 \(review["verdict"] as? String ?? "?")，返工 \(review["reworks"] as? Int ?? 0) 次，补丁 \(short(review["candidate_diff_sha256"] as? String))，收据 \(short(review["test_receipt_sha256"] as? String))")
                        }
                    }
                    lines.append("    可在窗口验收；验收只登记交付结果，不会改变主分支")
                }
            }
        }
        if let detail { lines.append(detail) }
        return lines.joined(separator: "\n")
    }
}

func stateTint(_ raw: String) -> Color {
    switch raw {
    case "completed": return .green
    case "awaiting_review": return .blue
    case "failed", "interrupted", "blocked": return .orange
    case "cancelled", "rejected": return .secondary
    default: return .accentColor
    }
}

func stateIcon(_ raw: String) -> String {
    switch raw {
    case "completed": return "checkmark.circle.fill"
    case "awaiting_review": return "tray.and.arrow.down.fill"
    case "failed", "interrupted", "blocked": return "exclamationmark.triangle.fill"
    case "cancelled", "rejected": return "xmark.circle"
    default: return "circle.dotted"
    }
}

struct StatusBadge: View {
    let state: String
    var body: some View {
        HStack(spacing: 4) {
            Circle().fill(stateTint(state)).frame(width: 6, height: 6)
            Text(Wording.state(state)).font(.caption.weight(.medium))
        }
        .padding(.horizontal, 8).padding(.vertical, 3)
        .background(stateTint(state).opacity(0.12), in: Capsule())
        .accessibilityLabel(Wording.state(state))
    }
}

/// Capture what the user saw before they open the confirmation dialog.
struct ReviewDecision {
    let taskID: String
    let attempt: Int
    let revision: Int
    let diff: String
    let receipt: String

    init?(_ task: OverviewTask) {
        guard task.state == "awaiting_review", task.attempt > 0, task.revision > 0,
              let review = task.review, review["error"] == nil,
              let diff = review["candidate_diff_sha256"] as? String,
              let receipt = review["test_receipt_sha256"] as? String else { return nil }
        taskID = task.id
        attempt = task.attempt
        revision = task.revision
        self.diff = diff
        self.receipt = receipt
    }
}

enum ReviewVerdict: String {
    case accept, reject
    var resultingState: String { self == .accept ? "completed" : "rejected" }
}

@MainActor
final class Model: ObservableObject {
    typealias Conn = ServiceConnection
    @Published var conn: Conn = .inactive
    @Published var overview: Overview?
    @Published var message: String?
    @Published var lastUpdated: Date?
    @Published var selected: String?
    @Published var diff: String?
    @Published var diffNext: Int?
    @Published var diffError: String?
    @Published var receipt: String?
    @Published var handoff: String?
    @Published var artifactError: String?
    @Published var actionMessage: String?
    @Published var busy = false
    @Published var showComposer = false
    @Published var showRegistration = false
    @Published var acceptanceTarget: ReviewDecision?
    @Published var previewStatus: String?
    @Published var enablingPreview = false
    @Published private(set) var socket: String
    @Published fileprivate var projects: [ProjectEntry] = []
    @Published fileprivate var activeProject: ProjectEntry?
    @Published var projectListError: String?
    private var projectService: ProjectService?
    var canSwitchProject: Bool { relaySocket == defaultSocket }
    var localPreview: Bool { socket == defaultSocket }
    private var loop: Task<Void, Never>?
    private var initializingSelection = true
    private var refreshing = false

    init(socket: String) { self.socket = socket }

    func start() {
        guard loop == nil else { return }
        loop = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refresh()
                // 窗口可见时 1 秒，关闭窗口后 5 秒低频刷新（不再完全冻结）。
                let open = await MainActor.run { NSApp.windows.contains { $0.isVisible } }
                let seconds = open ? 1 : 5
                try? await Task.sleep(nanoseconds: UInt64(seconds) * 1_000_000_000)
            }
        }
        Task { await checkPreview() }
    }

    /// 菜单文字与图标使用同一数据源：连接失败/过期时不得继续显示“正常”。
    var menuStatus: String {
        let snapshot = overview.map {
            MenuStatusSnapshot(state: $0.state, pending: $0.pending, blocking: $0.blocking)
        }
        return MenuStatus.text(conn: conn, message: message, snapshot: snapshot, lastUpdated: lastUpdated)
    }

    func checkPreview() async {
        guard localPreview, let relayctl = Bundle.main.resourceURL?.appendingPathComponent("relay/relayctl") else { return }
        do {
            let result = try await Task.detached(priority: .utility) {
                try ProcessRunner.run(executable: relayctl, arguments: ["local-preview", "status"], timeout: 15)
            }.value
            if let object = try? JSONSerialization.jsonObject(with: result.output) as? [String: Any],
               let status = object["status"] as? String {
                previewStatus = status
            }
        } catch { previewStatus = "unavailable" }
    }

    func enablePreview() async {
        guard localPreview, !enablingPreview else { return }
        enablingPreview = true
        actionMessage = nil
        defer { enablingPreview = false }
        guard let relayctl = Bundle.main.resourceURL?.appendingPathComponent("relay/relayctl") else { return }
        do {
            let result = try await Task.detached(priority: .userInitiated) {
                try ProcessRunner.run(executable: relayctl, arguments: ["local-preview", "enable"], timeout: 60)
            }.value
            let object = try? JSONSerialization.jsonObject(with: result.output) as? [String: Any]
            if result.status == 0, object?["status"] as? String == "running" {
                previewStatus = "running"
                actionMessage = "本地预览服务已启用"
                await refresh()
            } else {
                actionMessage = "启用失败：" + String(decoding: result.output.prefix(512), as: UTF8.self)
            }
        } catch { actionMessage = "启用失败：\(error)" }
    }

    func stop() { loop?.cancel(); loop = nil }

    func refresh() async {
        guard !refreshing else { return }
        refreshing = true
        defer { refreshing = false }
        if overview == nil && conn == .inactive && message == nil { conn = .connecting }
        let socket = self.socket
        do {
            let result = try await Task.detached(priority: .userInitiated) { try Relay.rpc(socket: socket, method: "ui.overview", params: "{}") }.value
            guard socket == self.socket else { return }
            let overview = Overview(result)
            self.overview = overview
            if let current = selected, !overview.tasks.contains(where: { $0.id == current }) {
                selected = overview.tasks.first?.id
                initializingSelection = false
            } else if initializingSelection && selected == nil, let first = overview.tasks.first {
                selected = first.id
                initializingSelection = false
            }
            conn = Conn(rawValue: overview.state) ?? .degraded
            message = nil
            lastUpdated = Date()
        } catch {
            guard socket == self.socket else { return }
            // 07: a transient read failure must not erase the last visible
            // state. Keep the snapshot and show how old it is; only a
            // never-connected client has no overview to preserve.
            if case RelayError.inactive = error { conn = .inactive } else { conn = .failed }
            message = "\(error)"
        }
    }

    func loadProjects() async {
        guard canSwitchProject else { return }
        do {
            let (entries, unavailable) = try await Task.detached {
                let result = try projectSetup(["list"], input: Data())
                guard let rows = result["projects"] as? [[String: String]] else {
                    throw RelayError.badProtocol("项目清单格式错误")
                }
                return (rows.compactMap { row -> ProjectEntry? in
                    guard let id = row["id"], let name = row["name"] else { return nil }
                    return ProjectEntry(id: id, name: name)
                }, result["unavailable"] as? Int ?? 0)
            }.value
            projects = entries
            projectListError = unavailable == 0 ? nil : "有 \(unavailable) 项登记资料无法读取"
        } catch { projectListError = "\(error)" }
    }

    fileprivate func openProject(_ entry: ProjectEntry?) async {
        let sameProject = entry?.id == activeProject?.id
        let serviceUsable = projectService != nil && (conn == .ready || conn == .degraded)
        guard canSwitchProject, !busy, !sameProject || !serviceUsable else { return }
        busy = true
        actionMessage = nil
        defer { busy = false }
        do {
            let connection = try await Task.detached { try entry.map(startProjectService) }.value
            socket = connection?.socket ?? defaultSocket
            projectService = connection
            activeProject = entry
            overview = nil
            selected = nil
            initializingSelection = true
            acceptanceTarget = nil
            diff = nil
            diffNext = nil
            diffError = nil
            receipt = nil
            handoff = nil
            artifactError = nil
            message = nil
            lastUpdated = nil
            conn = .connecting
            await refresh()
        } catch { actionMessage = "\(error)" }
    }

    func submitDemo() async {
        guard localPreview, !busy else { return }
        busy = true
        actionMessage = nil
        defer { busy = false }
        do {
            let id = try await Task.detached(priority: .userInitiated) {
                guard let resources = Bundle.main.resourceURL else { throw RelayError.system("应用资源缺失") }
                let result = try ProcessRunner.run(
                    executable: resources.appendingPathComponent("relay/relayctl"),
                    arguments: ["local-preview", "submit"], timeout: 120)
                guard result.status == 0, !result.timedOut,
                      let object = try? JSONSerialization.jsonObject(with: result.output) as? [String: Any],
                      let task = object["task"] as? [String: Any], let id = task["id"] as? String else {
                    throw RelayError.badProtocol(String(decoding: result.output.prefix(1024), as: UTF8.self))
                }
                return id
            }.value
            selected = id
            actionMessage = "已提交离线演示"
            await refresh()
        } catch { actionMessage = "\(error)" }
    }

    func runPairedDemo() async {
        guard localPreview, !busy else { return }
        busy = true
        actionMessage = "正在运行双工具样本：Claude Code 实现 → 本地验证 → OpenCode 评审"
        defer { busy = false }
        do {
            try await Task.detached(priority: .userInitiated) {
                guard let resources = Bundle.main.resourceURL else { throw RelayError.system("应用资源缺失") }
                let relay = resources.appendingPathComponent("relay")
                let home = FileManager.default.homeDirectoryForCurrentUser.path
                let fm = FileManager.default
                guard let claude = ["\(home)/.local/bin/claude", "/opt/homebrew/bin/claude", "/usr/local/bin/claude"]
                    .first(where: { fm.isExecutableFile(atPath: $0) }) else {
                    throw RelayError.system("找不到 Claude Code。请先安装 Claude Code，再运行样本。")
                }
                guard let opencode = ["/opt/homebrew/bin/opencode", "\(home)/.opencode/bin/opencode", "/usr/local/bin/opencode"]
                    .first(where: { fm.isExecutableFile(atPath: $0) }) else {
                    throw RelayError.system("找不到 OpenCode。请先安装 OpenCode，再运行样本。")
                }
                let result = try ProcessRunner.run(
                    executable: URL(fileURLWithPath: "/usr/bin/python3"),
                    arguments: [
                        "-I", relay.appendingPathComponent("paired-demo/claude-review-e2e.py").path,
                        relay.appendingPathComponent("relayctl").path, claude, "--pair-opencode", opencode,
                        "--catalog", relay.appendingPathComponent("mock/catalog").path,
                    ],
                    environment: ["PATH": "/usr/bin:/bin", "HOME": home],
                    timeout: 180)
                guard result.status == 0, !result.timedOut,
                      let object = try? JSONSerialization.jsonObject(with: result.output) as? [String: Any],
                      object["mode"] as? String == "two-clis",
                      object["state"] as? String == "awaiting_review",
                      object["stages"] as? [[String]] == [["implement", "succeeded"], ["verify", "succeeded"], ["review", "succeeded"]],
                      object["artifacts"] as? Int == 1 else {
                    throw RelayError.badProtocol("双工具样本未通过：\(String(decoding: result.output.suffix(1024), as: UTF8.self))")
                }
            }.value
            actionMessage = "双工具样本通过：Claude Code 实现、验证和 OpenCode 评审均成功。已生成 1 份待人工验收制品；本次样本在临时实例运行。"
        } catch { actionMessage = "\(error)" }
    }

    func decide(_ target: ReviewDecision, verdict: ReviewVerdict) async {
        guard !busy else { return }
        busy = true
        actionMessage = nil
        defer { busy = false }
        let socket = self.socket
        do {
            let data = try JSONSerialization.data(withJSONObject: [
                "task_id": target.taskID, "attempt_no": target.attempt, "expected_revision": target.revision,
                "request_id": UUID().uuidString.lowercased(), "verdict": verdict.rawValue,
                "diff_sha256": target.diff, "test_sha256": target.receipt,
            ])
            let params = String(decoding: data, as: UTF8.self)
            try await Task.detached(priority: .userInitiated) {
                let result = try Relay.rpc(socket: socket, method: "relay_review", params: params)
                guard let task = result["task"] as? [String: Any],
                      task["id"] as? String == target.taskID,
                      task["state"] as? String == verdict.resultingState else {
                    throw RelayError.badProtocol("后台未确认验收")
                }
            }.value
            actionMessage = verdict == .accept ? "任务已验收，主分支未改动" : "交付已拒绝，主分支未改动"
        } catch let RelayError.business(code, message, _)
            where ["STALE_REVISION", "WRONG_STATE", "ARTIFACT_INVALID", "ARTIFACT_UNAVAILABLE"].contains(code) {
            actionMessage = "交付状态或制品已变化（\(code)）：\(message)。请检查当前版本后再决定。"
        } catch { actionMessage = "\(error)" }
        await refresh()
    }

    func loadDiff(task: OverviewTask, offset: Int) async {
        let socket = self.socket
        let params = #"{"task_id":"\#(task.id)","attempt_no":\#(task.attempt),"kind":"diff","offset":\#(offset),"limit":4096}"#
        diffError = nil
        do {
            let result = try await Task.detached(priority: .userInitiated) { try Relay.rpc(socket: socket, method: "relay_artifact", params: params) }.value
            guard socket == self.socket, selected == task.id else { return }
            diff = (offset == 0 ? "" : (diff ?? "")) + (result["content"] as? String ?? "")
            diffNext = result["next_offset"] as? Int
        } catch {
            if socket == self.socket, selected == task.id {
                diffNext = nil
                diffError = "差异不可读：\(error)"
            }
        }
    }

    func loadReceipt(task: OverviewTask) async {
        await loadTextArtifact(task: task, kind: "test_receipt") { text in self.receipt = text }
    }

    func loadHandoff(task: OverviewTask) async {
        await loadTextArtifact(task: task, kind: "handoff") { text in self.handoff = text }
    }

    private func loadTextArtifact(task: OverviewTask, kind: String,
                                  apply: @escaping (String) -> Void) async {
        let socket = self.socket
        artifactError = nil
        var text = ""
        var offset = 0
        do {
            while true {
                let params = #"{"task_id":"\#(task.id)","attempt_no":\#(task.attempt),"kind":"\#(kind)","offset":\#(offset),"limit":32768}"#
                let result = try await Task.detached(priority: .userInitiated) { try Relay.rpc(socket: socket, method: "relay_artifact", params: params) }.value
                guard socket == self.socket, selected == task.id else { return }
                text += result["content"] as? String ?? ""
                guard let next = result["next_offset"] as? Int, next > offset, text.utf8.count < 256 * 1024 else {
                    break
                }
                offset = next
            }
            guard socket == self.socket, selected == task.id else { return }
            apply(text)
        } catch {
            if socket == self.socket, selected == task.id { artifactError = "\(error)" }
        }
    }
}

// MARK: - 界面

private struct RegisteredProject: Sendable {
    let id: String, profile: String, root: String, workflow: String
    let scopes: [String], rules: [String]
    let dispatchEnabled: Bool
    let route: Data?
    let routeTitle: String?
    let implementTool: String?
    let implementModel: String?
    let reviewTool: String?
    let reviewModel: String?

    static func routeSnapshot(_ result: [String: Any]) throws -> Data? {
        guard let value = result["external_route"], !(value is NSNull) else { return nil }
        guard let route = value as? [String: Any],
              let implement = route["implement"] as? [String: Any],
              implement["tool"] as? String != nil, implement["model"] as? String != nil,
              let review = route["review"] as? [String: Any],
              review["tool"] as? String != nil, review["model"] as? String != nil else {
            throw RelayError.badProtocol("无法读取真实路线")
        }
        return try JSONSerialization.data(withJSONObject: route, options: .sortedKeys)
    }

    init(_ result: [String: Any]) throws {
        try checkProjectReply(result)
        guard let id = result["project_id"] as? String,
              let profile = result["profile_id"] as? String,
              let root = result["repo_root"] as? String,
              let workflow = result["workflow"] as? String,
              let scopes = result["write_scope"] as? [String], !scopes.isEmpty,
              let rules = result["verification_rule_ids"] as? [String] else {
            throw RelayError.badProtocol("项目登记信息不完整")
        }
        self.id = id
        self.profile = profile
        self.root = root
        self.workflow = workflow
        self.scopes = scopes
        self.rules = rules
        self.dispatchEnabled = result["dispatch_enabled"] as? Bool ?? false
        self.route = try Self.routeSnapshot(result)
        if let incoming = result["external_route"] as? [String: Any] {
            let implement = incoming["implement"] as? [String: Any]
            let review = incoming["review"] as? [String: Any]
            implementTool = implement?["tool"] as? String
            implementModel = implement?["model"] as? String
            reviewTool = review?["tool"] as? String
            reviewModel = review?["model"] as? String
            routeTitle = [implement, review].compactMap { row in
                guard let row, let tool = row["tool"] as? String, let model = row["model"] as? String else { return nil }
                return "\(tool)（\(model)）"
            }.joined(separator: " → ")
        } else {
            implementTool = nil
            implementModel = nil
            reviewTool = nil
            reviewModel = nil
            routeTitle = nil
        }
    }
}

private func checkProjectReply(_ result: [String: Any]) throws {
    if let error = result["error"] as? [String: Any] {
        throw RelayError.badProtocol(error["message"] as? String ?? "后台没有确认操作")
    }
}

private struct FrozenSubmission: Sendable {
    let id: String, request: String, expires: String, base: String, goal: String, brief: String
    let paths: [String], rules: [String], warnings: [String]

    init(result: [String: Any], project: RegisteredProject, goal: String, brief: String, paths: [String], rules: [String]) throws {
        try checkProjectReply(result)
        guard let id = result["plan_id"] as? String, let expires = result["expires_at"] as? String,
              let resolved = result["resolved"] as? [String: Any],
              resolved["project_id"] as? String == project.id,
              resolved["profile_id"] as? String == project.profile,
              resolved["write_scope"] as? [String] == paths,
              resolved["rules"] as? [String] == rules,
              let base = resolved["base_oid"] as? String,
              try RegisteredProject.routeSnapshot(resolved) == project.route else {
            throw RelayError.badProtocol("计划返回的项目、文件范围或验证规则与预览不一致")
        }
        self.id = id
        self.request = UUID().uuidString.lowercased()
        self.base = base
        self.expires = expires
        self.goal = goal
        self.brief = brief
        self.paths = paths
        self.rules = rules
        self.warnings = result["warnings"] as? [String] ?? []
    }
}

private func projectRPC(socket: String, method: String, params: [String: Any]) throws -> [String: Any] {
    let bytes = try JSONSerialization.data(withJSONObject: params)
    return try Relay.rpc(socket: socket, method: method, params: String(decoding: bytes, as: UTF8.self))
}

@MainActor
private final class ComposerState: ObservableObject {
    @Published var project: RegisteredProject?
    @Published var frozen: FrozenSubmission?
    @Published var goal = ""
    @Published var brief = ""
    @Published var paths = ""
    @Published var rules = Set<String>()
    @Published var work = false
    @Published var attemptedSubmission = false
    @Published var failure: String?

    var pathList: [String] {
        paths.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }
}

@MainActor
private struct TaskComposer: View {
    let socket: String
    let onSubmitted: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @StateObject private var draft = ComposerState()

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(draft.frozen == nil ? "新建项目任务" : "核对并提交").font(.title2.bold())
            if let project = draft.project {
                Text(project.id).font(.headline)
                Text(project.root).font(.caption.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
                if let routeTitle = project.routeTitle {
                    Label(routeTitle, systemImage: "network").foregroundStyle(.secondary)
                } else {
                    Text("当前实例未启用真实模型。").foregroundStyle(.secondary)
                }
                if let frozen = draft.frozen {
                    ScrollView {
                        LabeledContent("目标", value: frozen.goal)
                        LabeledContent("任务说明", value: frozen.brief)
                        LabeledContent("Git 基线", value: frozen.base)
                        LabeledContent("有效期", value: frozen.expires)
                        LabeledContent("写入范围", value: frozen.paths.joined(separator: "，"))
                        LabeledContent("验证规则", value: frozen.rules.joined(separator: "，"))
                        if !frozen.warnings.isEmpty {
                            LabeledContent("提醒", value: frozen.warnings.joined(separator: "；"))
                                .foregroundStyle(.orange)
                        }
                        if project.route != nil {
                            let implement = [project.implementTool, project.implementModel]
                                .compactMap { $0 }.joined(separator: " / ")
                            let review = [project.reviewTool, project.reviewModel]
                                .compactMap { $0 }.joined(separator: " / ")
                            Text("提交后，\(implement.isEmpty ? "实现者" : implement) 会收到目标、任务说明和上列文件的基线文本；\(review.isEmpty ? "评审者" : review) 只收到目标、补丁、验证通过状态和两个摘要。费用类别和原生额度仍以账户页为准；实现方可能使用套餐 Credits。")
                                .foregroundStyle(.secondary)
                        }
                        Text("提交后改动留在受管工作副本，不会写入主 checkout。")
                            .foregroundStyle(.secondary)
                    }
                } else {
                    Form {
                        TextField("目标", text: $draft.goal)
                        TextField("任务说明", text: $draft.brief, axis: .vertical).lineLimit(2...4)
                        TextField("写入范围（每行一项）", text: $draft.paths, axis: .vertical).lineLimit(2...5)
                        Text("登记范围：\(project.scopes.joined(separator: "，"))")
                            .font(.caption).foregroundStyle(.secondary)
                        Section("验证规则") {
                            if project.rules.isEmpty { Text("项目未配置验证规则。").foregroundStyle(.secondary) }
                            ForEach(project.rules, id: \.self) { rule in
                                Toggle(rule, isOn: Binding(
                                    get: { draft.rules.contains(rule) },
                                    set: { enabled in
                                        if enabled { draft.rules.insert(rule) } else { draft.rules.remove(rule) }
                                    }
                                ))
                            }
                        }
                    }
                    .disabled(draft.work)
                }
            } else if draft.work {
                ProgressView("正在读取项目登记…")
            } else if draft.failure != nil {
                ContentUnavailableView("项目登记不可用", systemImage: "folder.badge.questionmark",
                    description: Text("请启动已登记项目的本机服务，再打开对应任务窗口。演示实例可继续运行离线样本。"))
            }
            if let failure = draft.failure {
                Text(failure).font(.callout).foregroundStyle(.red).textSelection(.enabled)
            }
            Spacer(minLength: 0)
            HStack {
                Button("关闭", role: .cancel) { dismiss() }.disabled(draft.work)
                Spacer()
                if let frozen = draft.frozen {
                    if !draft.attemptedSubmission {
                        Button("返回编辑") { draft.frozen = nil; draft.failure = nil }.disabled(draft.work)
                    }
                    Button(draft.attemptedSubmission ? "重送同一请求" : (draft.project?.route == nil ? "确认提交" : "同意发送并提交")) { Task { await submit(frozen) } }
                        .disabled(draft.work)
                } else if let project = draft.project {
                    Button("生成计划预览") { Task { await preview(project) } }
                        .disabled(draft.work || !project.dispatchEnabled || draft.goal.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                            || draft.brief.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                            || draft.pathList.isEmpty || draft.rules.isEmpty || draft.goal.utf8.count > 1024
                            || draft.brief.utf8.count > 16384 || draft.pathList.count > 64 || draft.rules.count > 8)
                }
            }
        }
        .padding(22)
        .frame(width: 565, height: 485)
        .interactiveDismissDisabled(draft.work)
        .task { await load() }
    }

    @MainActor private func load() async {
        draft.work = true
        defer { draft.work = false }
        let socket = self.socket
        do {
            draft.project = try await Task.detached {
                try RegisteredProject(projectRPC(socket: socket, method: "ui.project", params: [:]))
            }.value
        } catch { draft.failure = "\(error)" }
    }

    @MainActor private func preview(_ project: RegisteredProject) async {
        guard !draft.work else { return }
        draft.work = true
        defer { draft.work = false }
        draft.failure = nil
        let socket = self.socket, goal = draft.goal, brief = draft.brief, paths = draft.pathList
        let selectedRules = project.rules.filter(draft.rules.contains)
        do {
            draft.frozen = try await Task.detached {
                let reply = try projectRPC(socket: socket, method: "relay_plan", params: [
                    "project_id": project.id, "profile_id":project.profile, "goal":goal, "brief":brief,
                    "write_scope":paths,"verification_rule_ids":selectedRules
                ])
                return try FrozenSubmission(result: reply, project: project, goal: goal, brief: brief, paths: paths, rules: selectedRules)
            }.value
        } catch { draft.failure = "\(error)" }
    }

    @MainActor private func submit(_ submission: FrozenSubmission) async {
        guard !draft.work else { return }
        draft.work = true
        draft.attemptedSubmission = true
        defer { draft.work = false }
        draft.failure = nil
        let socket = self.socket
        do {
            let id = try await Task.detached {
                let reply = try projectRPC(socket: socket, method:"relay_submit",
                    params:["plan_id":submission.id,"request_id":submission.request])
                try checkProjectReply(reply)
                guard let task = reply["task"] as? [String: Any], let id = task["id"] as? String, !id.isEmpty else {
                    throw RelayError.badProtocol("后台未确认提交")
                }
                return id
            }.value
            dismiss()
            onSubmitted(id)
        } catch {
            draft.failure = "\(error)；如果无法确定是否已排队，可重送同一请求，或先查看任务列表。"
        }
    }
}


private struct RegistrationPreview: Sendable {
    let input: Data
    let approval: String
    let project: String, repo: String, base: String
    let scope: [String], ruleDisplay: String
}

@MainActor
private final class RegistrationState: ObservableObject {
    @Published var pickFolder = false
    @Published var folder: URL?
    @Published var name = ""
    @Published var scope = ""
    @Published var executable = ""
    @Published var arguments = ""
    @Published var seconds = "30"
    @Published var busy = false
    @Published var preview: RegistrationPreview?
    @Published var error: String?
    @Published var saved: String?

    static func lines(_ text: String) -> [String] {
        text.split(whereSeparator: \.isNewline).map(String.init)
    }
}

fileprivate struct ProjectEntry: Identifiable, Sendable {
    let id: String
    let name: String
}

/// Holding the write end keeps the helper running. Closing it lets the helper
/// drop its socket and lock normally, including when the app exits.
private final class ProjectService: @unchecked Sendable {
    let socket: String
    private let keepAlive: FileHandle
    private let child: Process

    init(socket: String, keepAlive: FileHandle, child: Process) {
        self.socket = socket
        self.keepAlive = keepAlive
        self.child = child
    }

    deinit { try? keepAlive.close() }
}

private func startProjectService(_ entry: ProjectEntry) throws -> ProjectService {
    guard let bin = Bundle.main.resourceURL?.appendingPathComponent("relay/relayctl") else {
        throw RelayError.system("应用资源缺失")
    }
    let command = Process()
    command.executableURL = bin
    command.arguments = ["project-setup", "serve", "--project-id", entry.id]
    command.environment = ["PATH": "/usr/bin:/bin", "HOME": FileManager.default.homeDirectoryForCurrentUser.path]
    let source = Pipe(), sink = Pipe()
    command.standardInput = source
    command.standardOutput = sink
    command.standardError = sink
    try command.run()
    let keepAlive = source.fileHandleForWriting
    var ready = false
    defer {
        if !ready { try? keepAlive.close() }
    }
    var line = Data()
    let deadline = ProcessInfo.processInfo.systemUptime + 12
    let reader = sink.fileHandleForReading
    var complete = false
    while line.count < 4096 {
        let remaining = deadline - ProcessInfo.processInfo.systemUptime
        guard remaining > 0 else { break }
        var descriptor = pollfd(fd: reader.fileDescriptor, events: Int16(POLLIN), revents: 0)
        let available = poll(&descriptor, 1, Int32((remaining * 1000).rounded(.up)))
        if available < 0, errno == EINTR { continue }
        guard available > 0, let byte = try reader.read(upToCount: 1), !byte.isEmpty else { break }
        if byte.first == 10 { complete = true; break }
        line.append(byte)
    }
    if line.isEmpty && !complete {
        throw RelayError.system("项目服务未返回启动结果")
    }
    guard complete, let response = try? JSONSerialization.jsonObject(with: line) as? [String: String],
        response["project_id"] == entry.id, let socket = response["socket"] else {
        throw RelayError.badProtocol(String(decoding: line.prefix(1024), as: UTF8.self))
    }
    let registered = try projectRPC(socket: socket, method: "ui.project", params: [:])
    try checkProjectReply(registered)
    guard registered["project_id"] as? String == entry.id, registered["dispatch_enabled"] as? Bool == false else {
        throw RelayError.badProtocol("服务返回的项目或执行状态不符")
    }
    ready = true
    return ProjectService(socket: socket, keepAlive: keepAlive, child: command)
}

private func projectSetup(_ args: [String], input: Data) throws -> [String: Any] {
    guard let bin = Bundle.main.resourceURL?.appendingPathComponent("relay/relayctl") else {
        throw RelayError.system("应用资源缺失")
    }
    let command = Process()
    command.executableURL = bin
    command.arguments = ["project-setup"] + args
    command.environment = ["PATH":"/usr/bin:/bin", "HOME":FileManager.default.homeDirectoryForCurrentUser.path]
    let source = Pipe(), sink = Pipe()
    command.standardInput = source
    command.standardOutput = sink
    command.standardError = sink
    try command.run()
    do { try source.fileHandleForWriting.write(contentsOf: input) }
    catch {
        try? source.fileHandleForWriting.close()
        command.terminate()
        throw error
    }
    try source.fileHandleForWriting.close()
    let bytes = sink.fileHandleForReading.readDataToEndOfFile()
    command.waitUntilExit()
    guard command.terminationStatus == 0,
          let record = try? JSONSerialization.jsonObject(with: bytes) as? [String: Any] else {
        throw RelayError.badProtocol(String(decoding: bytes.prefix(1024), as: UTF8.self))
    }
    return record
}

@MainActor
private struct ProjectRegistrationView: View {
    @Environment(\.dismiss) private var dismiss
    @StateObject private var draft = RegistrationState()
    let onSaved: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("登记本机项目").font(.title2.bold())
            if let path = draft.saved {
                Label("项目登记已保存", systemImage: "checkmark.circle.fill").font(.headline)
                Text(path).font(.caption.monospaced()).textSelection(.enabled)
                Text("关闭后可从「项目」菜单打开。此操作未启动任务或发送账户请求。").foregroundStyle(.secondary)
            } else if let preview = draft.preview {
                Form {
                    LabeledContent("项目 ID", value: preview.project)
                    LabeledContent("仓库", value: preview.repo)
                    LabeledContent("当前 Git 基线", value: preview.base)
                    LabeledContent("写入范围", value: preview.scope.joined(separator: "，"))
                    LabeledContent("验证命令", value: preview.ruleDisplay)
                    Text("确认后只保存本机登记资料。不会执行验证命令，也不会向模型发送文件。").foregroundStyle(.secondary)
                }
            } else {
                Form {
                    Button(draft.folder == nil ? "选择 Git 仓库…" : "更换 Git 仓库…") { draft.pickFolder = true }
                    if let folder = draft.folder { Text(folder.path).font(.caption).textSelection(.enabled) }
                    TextField("项目名称", text: $draft.name)
                    TextField("可写文件或目录（每行一项）", text: $draft.scope, axis: .vertical).lineLimit(2...4)
                    TextField("验证程序的绝对路径", text: $draft.executable)
                    TextField("验证参数（每行一个）", text: $draft.arguments, axis: .vertical).lineLimit(1...3)
                    TextField("超时（秒）", text: $draft.seconds)
                    Text("验证工作目录为受管副本。参数直接传给程序，不经过 shell。").font(.caption).foregroundStyle(.secondary)
                }.disabled(draft.busy)
            }
            if let error = draft.error { Text(error).foregroundStyle(.red).textSelection(.enabled) }
            Spacer(minLength: 0)
            HStack {
                Button("关闭", role: .cancel) { dismiss() }.disabled(draft.busy)
                Spacer()
                if draft.busy { ProgressView().controlSize(.small) }
                if draft.saved == nil {
                    if let preview = draft.preview {
                        Button("返回编辑") { draft.preview = nil; draft.error = nil }.disabled(draft.busy)
                        Button("确认登记") { Task { await save(preview) } }.disabled(draft.busy)
                    } else {
                        Button("预览登记") { Task { await preview() } }
                            .disabled(draft.busy || draft.folder == nil || draft.name.isEmpty
                                || draft.scope.isEmpty || !draft.executable.hasPrefix("/")
                                || !(1...600).contains(Int(draft.seconds) ?? 0))
                    }
                }
            }
        }
        .padding(22)
        .frame(width: 560, height: 515)
        .interactiveDismissDisabled(draft.busy)
        .fileImporter(isPresented: $draft.pickFolder, allowedContentTypes: [.folder]) { selection in
            switch selection {
            case .success(let folder):
                draft.folder = folder
                if draft.name.isEmpty { draft.name = folder.lastPathComponent }
                draft.error = nil
            case .failure(let error): draft.error = "\(error)"
            }
        }
    }

    private func preview() async {
        guard !draft.busy, let folder = draft.folder, let seconds = Int(draft.seconds), (1...600).contains(seconds) else { return }
        draft.error = nil
        draft.busy = true
        defer { draft.busy = false }
        do {
            let name = draft.name
            let scope = RegistrationState.lines(draft.scope)
            let exe = draft.executable
            let args = RegistrationState.lines(draft.arguments)
            let input = try JSONSerialization.data(withJSONObject: [
                "name":name,"repo_root":folder.path,"write_scope":scope,
                "rule":["id":"check","executable":exe,"argv":args,"timeout_ms":seconds * 1000]
            ])
            let result = try await Task.detached {
                let reply = try projectSetup(["preview"], input: input)
                guard let approval = reply["approval"] as? String,
                      let record = reply["registration"] as? [String: Any],
                      let config = record["project_binding"] as? [String: Any],
                      let project = config["project_id"] as? String,
                      let root = record["repo_root"] as? String,
                      let checkedScopes = config["write_scope"] as? [String], checkedScopes == scope,
                      let head = record["base_at_registration"] as? String else {
                    throw RelayError.badProtocol("登记预览缺少核对字段")
                }
                let argv = ([exe] + args).map { $0.debugDescription }.joined(separator: " ")
                return RegistrationPreview(input: input, approval: approval, project: project,
                    repo: root, base: head, scope: checkedScopes, ruleDisplay: "\(argv) · \(seconds) 秒")
            }.value
            draft.preview = result
        } catch { draft.error = "\(error)" }
    }

    private func save(_ preview: RegistrationPreview) async {
        guard !draft.busy else { return }
        draft.busy = true
        draft.error = nil
        defer { draft.busy = false }
        do {
            draft.saved = try await Task.detached {
                let record = try projectSetup(["save", "--approve", preview.approval], input: preview.input)
                guard let path = record["path"] as? String else { throw RelayError.badProtocol("缺少登记路径") }
                return path
            }.value
            onSaved()
        } catch { draft.error = "\(error)" }
    }
}

struct ServiceBar: View {
    @ObservedObject var model: Model
    var color: Color {
        switch model.conn {
        case .ready: return .green
        case .degraded, .maintenance: return .orange
        case .connecting: return .yellow
        case .inactive, .failed: return .red
        case .archive: return .secondary
        }
    }
    var label: String {
        switch model.conn {
        case .inactive: return "后台服务未运行"
        case .failed: return "后台服务连接失败"
        case .connecting: return "连接中…"
        case .maintenance: return "服务维护中（暂停写入与派发）"
        case .archive: return "只读档案（不启动任务）"
        case .ready, .degraded:
            return model.overview.map {
                Wording.service($0.state, pending: $0.pending, blocking: $0.blocking)
            } ?? "服务状态未知"
        }
    }
    var body: some View {
        HStack(spacing: 10) {
            Circle().fill(color).frame(width: 10, height: 10)
                .shadow(color: color.opacity(0.55), radius: 4)
            Text(label).font(.headline)
            if let message = model.message { Text(message).foregroundStyle(.secondary).lineLimit(1) }
            Spacer()
            if let product = model.overview?.versions["product"] {
                Text("v\(product)").font(.caption.monospaced()).foregroundStyle(.secondary)
            }
            if let updated = model.lastUpdated {
                Text("上次更新 \(updated.formatted(date: .omitted, time: .standard))").foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 10).padding(.vertical, 8)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .accessibilityElement(children: .combine)
        .accessibilityLabel("服务状态：\(label)")
    }
}

struct ReviewCard: View {
    let task: OverviewTask
    var body: some View {
        GroupBox("待验收结果") {
            VStack(alignment: .leading, spacing: 6) {
                if let review = task.review, review["error"] == nil {
                    Text("评审 \(review["verdict"] as? String ?? "?")，返工 \(review["reworks"] as? Int ?? 0) 次")
                        .accessibilityLabel("评审结论 \(review["verdict"] as? String ?? "未知")，返工 \(review["reworks"] as? Int ?? 0) 次")
                    let implement = review["implement"] as? [String: Any] ?? [:]
                    let reviewer = review["reviewer"] as? [String: Any] ?? [:]
                    Text("实现者 \(implement["identity"] as? String ?? "?") / \(implement["model"] as? String ?? "?")；评审者 \(reviewer["identity"] as? String ?? "?") / \(reviewer["model"] as? String ?? "?")")
                        .foregroundStyle(.secondary)
                        .accessibilityLabel("实现者 \(implement["identity"] as? String ?? "未知")，模型 \(implement["model"] as? String ?? "未知")；评审者 \(reviewer["identity"] as? String ?? "未知")，模型 \(reviewer["model"] as? String ?? "未知")")
                    Text("补丁摘要 \(Wording.short(review["candidate_diff_sha256"] as? String))")
                    Text("收据摘要 \(Wording.short(review["test_receipt_sha256"] as? String))")
                    let findings = review["findings"] as? [[String: Any]] ?? []
                    if !findings.isEmpty {
                        Text("findings（\(findings.count)）").font(.subheadline.bold())
                            .accessibilityLabel("发现 \(findings.count) 条问题")
                        ForEach(Array(findings.enumerated()), id: \.offset) { _, finding in
                            Text("  \(finding["severity"] as? String ?? "") \(finding["path"] as? String ?? "")：\(finding["evidence"] as? String ?? "")")
                                .font(.caption)
                                .accessibilityLabel("问题：严重程度 \(finding["severity"] as? String ?? "未知")，路径 \(finding["path"] as? String ?? "未知")，证据 \(finding["evidence"] as? String ?? "")")
                        }
                    }
                } else {
                    Text("待验收结果不可用：\((task.review?["error"] as? String) ?? "未登记制品")")
                        .foregroundStyle(.red)
                        .accessibilityLabel("待验收结果不可用")
                }
                Text("验收只登记交付结果，不会合并或应用补丁。")
                    .font(.caption).foregroundStyle(.secondary)
                    .accessibilityLabel("验收只登记交付结果，不会合并或应用补丁")
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .textSelection(.enabled)
        }
    }
}

struct DetailView: View {
    @ObservedObject var model: Model
    let task: OverviewTask
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(task.goal).font(.title2.bold()).lineLimit(2)
                    Spacer()
                    StatusBadge(state: task.state)
                }
                HStack(spacing: 8) {
                    Label(task.project, systemImage: "folder")
                    Label("revision \(task.revision)", systemImage: "number")
                    Label("attempt \(task.attempt)", systemImage: "repeat")
                    Label(Wording.clock(task.updated), systemImage: "clock")
                }
                .font(.caption).foregroundStyle(.secondary)
                .labelStyle(.titleAndIcon)
                if let reason = task.reason { Text("原因：\(Wording.failureReason(reason))").foregroundStyle(.secondary) }
                if !task.stages.isEmpty {
                    GroupBox("阶段时间线") {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(task.stages) { stage in
                                HStack(spacing: 8) {
                                    Text("\(stage.round)").font(.caption.monospaced()).foregroundStyle(.secondary)
                                    Text(Wording.stage(stage.stage)).font(.callout.weight(.medium))
                                    Text(stage.state).font(.caption.monospaced()).foregroundStyle(.secondary)
                                    Spacer()
                                    Text(Wording.clock(stage.started)).font(.caption2.monospaced()).foregroundStyle(.secondary)
                                }
                                .accessibilityElement(children: .combine)
                                .accessibilityLabel("第 \(stage.round) 轮 \(Wording.stage(stage.stage))，\(stage.state)")
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                if task.state == "awaiting_review" {
                    ReviewCard(task: task)
                    if let target = ReviewDecision(task) {
                        Button("验收交付…") { model.acceptanceTarget = target }
                            .disabled(model.busy || model.conn == .inactive || model.conn == .failed)
                            .accessibilityLabel("验收交付，打开确认对话框")
                    }
                }
                GroupBox("交付差异（分页，不运行、不渲染）") {
                    VStack(alignment: .leading, spacing: 6) {
                        if let diff = model.diff {
                            Text(diff).font(Font.system(size: 11, design: .monospaced)).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                        }
                        if let error = model.diffError { Text(error).foregroundStyle(.red) }
                        HStack {
                            Button("加载差异") { Task { await model.loadDiff(task: task, offset: 0) } }
                                .accessibilityLabel("加载交付差异")
                            if model.diffNext != nil {
                                Button("下一页") { Task { await model.loadDiff(task: task, offset: model.diffNext ?? 0) } }
                                    .accessibilityLabel("加载下一页差异")
                            }
                        }
                        .disabled(model.conn == .inactive || model.conn == .failed)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                GroupBox("测试收据与交付说明（只读）") {
                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Button("加载测试收据") { Task { await model.loadReceipt(task: task) } }
                                .accessibilityLabel("加载测试收据")
                            Button("加载交付说明") { Task { await model.loadHandoff(task: task) } }
                                .accessibilityLabel("加载交付说明")
                        }
                        if let receipt = model.receipt {
                            Text(receipt).font(Font.system(size: 11, design: .monospaced)).textSelection(.enabled)
                        }
                        if let handoff = model.handoff {
                            Text(handoff).font(Font.system(size: 11, design: .monospaced)).textSelection(.enabled)
                        }
                        if let error = model.artifactError { Text(error).foregroundStyle(.red) }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .disabled(model.conn == .inactive || model.conn == .failed)
                }
            }
            .padding()
        }
        .confirmationDialog("确认这份交付的处理决定", isPresented: Binding(
            get: { model.acceptanceTarget?.taskID == task.id },
            set: { if !$0 { model.acceptanceTarget = nil } }
        ), titleVisibility: .visible, presenting: model.acceptanceTarget) { frozen in
            Button("确认验收") { Task { await model.decide(frozen, verdict: .accept) } }
            Button("拒绝交付", role: .destructive) { Task { await model.decide(frozen, verdict: .reject) } }
            Button("返回", role: .cancel) {}
        } message: { frozen in
            Text("尝试 \(frozen.attempt) · revision \(frozen.revision)\n补丁 \(Wording.short(frozen.diff)) · 收据 \(Wording.short(frozen.receipt))\n两项决定都不会写入主分支。")
        }
    }
}

struct ContentView: View {
    @Environment(\.scenePhase) private var scenePhase
    @ObservedObject var model: Model
    var body: some View {
        VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 12) {
                HStack(spacing: 12) {
                    ZStack {
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .fill(LinearGradient(colors: [.cyan, .blue, .indigo],
                                                 startPoint: .topLeading, endPoint: .bottomTrailing))
                            .frame(width: 44, height: 44)
                            .shadow(color: .blue.opacity(0.25), radius: 8, y: 3)
                        Image(systemName: "point.3.connected.trianglepath.dotted")
                            .font(.title3.weight(.semibold)).foregroundStyle(.white)
                    }
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 8) {
                            Text("Agent Relay").font(.title2.bold())
                            Text("本机预览").font(.caption.weight(.medium))
                                .padding(.horizontal, 9).padding(.vertical, 4)
                                .background(.tint.opacity(0.12), in: Capsule())
                        }
                        Text("One agent writes · another reviews · you get the evidence")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if model.canSwitchProject {
                        Menu {
                            Button("离线演示") { Task { await model.openProject(nil) } }
                            ForEach(model.projects) { entry in
                                Button(entry.name) { Task { await model.openProject(entry) } }
                            }
                            if let error = model.projectListError { Text(error) }
                            Divider()
                            Button("刷新项目清单") { Task { await model.loadProjects() } }
                        } label: {
                            Label(model.activeProject?.name ?? "项目", systemImage: "folder")
                        }
                        .menuStyle(.borderlessButton)
                        .disabled(model.busy)
                        .accessibilityLabel("切换项目")
                    }
                    Button("登记项目…", systemImage: "folder.badge.plus") { model.showRegistration = true }
                        .buttonStyle(.bordered)
                        .disabled(model.showComposer)
                    Button("新建任务…", systemImage: "plus") { model.showComposer = true }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.overview == nil || model.activeProject != nil || model.busy)
                }
                if model.activeProject != nil {
                    Label("项目仅查看 · 执行路线尚未启用", systemImage: "pause.circle")
                        .font(.callout).foregroundStyle(.secondary)
                }
                ServiceBar(model: model)
                if model.localPreview, model.previewStatus == "not_configured" {
                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: "bolt.horizontal.circle")
                            .font(.title3).foregroundStyle(.tint)
                        VStack(alignment: .leading, spacing: 4) {
                            Text("本地预览尚未启用").font(.callout.bold())
                            Text("启用会创建 ~/.agent-relay-next 并注册当前用户的 LaunchAgent；不会安装系统守护进程。")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Button(model.enablingPreview ? "正在启用…" : "启用本地预览", systemImage: "power") {
                            Task { await model.enablePreview() }
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.enablingPreview)
                        .accessibilityLabel("启用本地预览")
                    }
                    .padding(12)
                    .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel("本地预览尚未启用")
                }
            }
            .padding(14)
            .relaySurface()
            .padding([.top, .horizontal], 14)
            if model.localPreview || model.actionMessage != nil {
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        if model.localPreview {
                            Button("运行离线演示", systemImage: "play.fill") { Task { await model.submitDemo() } }
                                .disabled(model.busy || model.overview == nil)
                            Button("运行双工具样本") { Task { await model.runPairedDemo() } }
                                .disabled(model.busy)
                            Text("两项都不调用真实模型").foregroundStyle(.secondary)
                        }
                        Spacer()
                    }
                    if let message = model.actionMessage { Text(message).textSelection(.enabled) }
                }.padding(12)
            }
            Divider()
            HSplitView {
                List(model.overview?.tasks ?? [], selection: $model.selected) { task in
                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: stateIcon(task.state))
                            .font(.system(size: 15, weight: .semibold))
                            .foregroundStyle(stateTint(task.state))
                            .frame(width: 20, height: 20)
                        VStack(alignment: .leading, spacing: 6) {
                            Text(task.goal).font(.subheadline.weight(.semibold)).lineLimit(2)
                            HStack(spacing: 8) {
                                StatusBadge(state: task.state)
                                Text(Wording.clock(task.updated))
                                    .font(.caption2).foregroundStyle(.secondary)
                                if !task.stages.isEmpty {
                                    Text("\(task.stages.filter { $0.state == "succeeded" }.count)/\(task.stages.count) 阶段")
                                        .font(.caption2).foregroundStyle(.secondary)
                                }
                            }
                        }
                        Spacer(minLength: 0)
                    }
                    .padding(.vertical, 6)
                    .tag(task.id)
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel("\(task.goal)，\(Wording.state(task.state))")
                }
                .listStyle(.sidebar)
                .frame(minWidth: 250, idealWidth: 296, maxWidth: 340)
                if let id = model.selected,
                   let task = model.overview?.tasks.first(where: { $0.id == id }) {
                    DetailView(model: model, task: task).id(id)
                } else {
                    switch model.conn {
                    case .inactive, .failed:
                        Text(model.message ?? "请求失败").frame(maxWidth: .infinity, maxHeight: .infinity).foregroundStyle(.secondary)
                    default:
                        ContentUnavailableView(
                            model.overview?.tasks.isEmpty == true ? "暂无任务" : "选择一个任务",
                            systemImage: "tray",
                            description: Text("任务进度、评审和交付文件会显示在这里。")
                        ).frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
            }
        }
        .frame(minWidth: 760, minHeight: 480)
        .sheet(isPresented: $model.showRegistration) {
            ProjectRegistrationView { Task { await model.loadProjects() } }
        }
        .sheet(isPresented: $model.showComposer) {
            TaskComposer(socket: model.socket) { id in
                model.selected = id
                model.actionMessage = "任务已提交"
                Task { await model.refresh() }
            }
        }
        .onAppear { model.start() }
        .task { await model.loadProjects() }
        .onChange(of: scenePhase) { _, phase in
            // 前台每秒拉取；后台/折叠不持有高频轮询。
            if phase == .active { model.start() } else { model.stop() }
        }
        .onChange(of: model.selected) { _, _ in
            model.acceptanceTarget = nil
            model.diff = nil
            model.diffNext = nil
            model.diffError = nil
            model.receipt = nil
            model.handoff = nil
            model.artifactError = nil
        }
        .onDisappear { model.stop() }
    }
}

private extension View {
    @ViewBuilder func relaySurface() -> some View {
        if #available(macOS 26.0, *) {
            self.glassEffect(.regular, in: .rect(cornerRadius: 18))
        } else {
            self.background(.regularMaterial, in: RoundedRectangle(cornerRadius: 18))
        }
    }
}

struct TaskWindowCommands: View {
    @Environment(\.openWindow) private var openWindow
    @ObservedObject var model: Model
    var body: some View {
        Button("打开任务窗口") {
            openWindow(id: "tasks")
            NSApp.activate(ignoringOtherApps: true)
        }.keyboardShortcut("o", modifiers: [.command, .shift])
        Button("登记本机项目…") {
            openWindow(id: "tasks")
            NSApp.activate(ignoringOtherApps: true)
            model.showRegistration = true
        }.disabled(model.showComposer)
        Button("刷新服务状态") { Task { await model.refresh() } }
            .keyboardShortcut("r", modifiers: [.command, .shift])
    }
}

struct RelayMenu: View {
    @ObservedObject var model: Model
    @Environment(\.openWindow) private var openWindow
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                ZStack {
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .fill(LinearGradient(colors: [.cyan, .blue, .indigo],
                                             startPoint: .topLeading, endPoint: .bottomTrailing))
                        .frame(width: 34, height: 34)
                    Image(systemName: "point.3.connected.trianglepath.dotted")
                        .foregroundStyle(.white).font(.system(size: 15, weight: .semibold))
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text("Agent Relay").font(.headline)
                    Text(model.menuStatus)
                        .font(.caption).foregroundStyle(.secondary)
                        .accessibilityLabel("服务状态：\(model.menuStatus)")
                }
            }
            if let overview = model.overview, overview.pending + overview.blocking > 0 {
                Label("\(overview.pending + overview.blocking) 项需要处理", systemImage: "exclamationmark.triangle.fill")
                    .font(.callout).foregroundStyle(.orange)
            }
            let tasks = Array((model.overview?.tasks ?? []).prefix(3))
            if !tasks.isEmpty {
                Divider()
                Text("最近任务").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                ForEach(tasks) { task in
                    Button {
                        model.selected = task.id
                        openWindow(id: "tasks")
                        NSApp.activate(ignoringOtherApps: true)
                    } label: {
                        HStack(spacing: 8) {
                            Circle().fill(stateTint(task.state)).frame(width: 7, height: 7)
                            Text(task.goal).lineLimit(1)
                            Spacer()
                            Text(Wording.state(task.state)).font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("\(task.goal)，\(Wording.state(task.state))")
                }
            }
            Divider()
            TaskWindowCommands(model: model)
            Divider()
            Button("退出界面") { NSApp.terminate(nil) }
        }
        .padding(14)
        .frame(width: 330)
    }
}

/// 由 EntryPoint 在启动界面前设置（App 需要无参 init）。
let defaultSocket = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent(".agent-relay-next/instance/relay.sock").path
var relaySocket: String = ""

struct RelayApp: App {
    @StateObject private var model = Model(socket: relaySocket)
    var body: some Scene {
        Window("Relay 任务", id: "tasks") { ContentView(model: model) }
            .defaultSize(width: 950, height: 600)
            .commands {
                CommandGroup(replacing: .newItem) {}
                CommandMenu("任务") {
                    TaskWindowCommands(model: model)
                }
            }
        MenuBarExtra {
            RelayMenu(model: model)
        } label: {
            Label("Agent Relay", systemImage: MenuStatus.symbolName(for: model.conn))
        }
        .menuBarExtraStyle(.window)
    }
}

// MARK: - 入口

@main
struct EntryPoint {
    static func main() {
        var socket = ProcessInfo.processInfo.environment["RELAY_SOCKET"] ?? defaultSocket
        var dump = false
        var args = Array(CommandLine.arguments.dropFirst())
        while !args.isEmpty {
            switch args.removeFirst() {
            case "--socket": socket = args.isEmpty ? socket : args.removeFirst()
            case "--dump": dump = true
            default: break
            }
        }
        guard !socket.isEmpty else {
            FileHandle.standardError.write(Data("用法：RelayTaskWindow --socket <relay.sock> [--dump]\n".utf8))
            exit(2)
        }
        if dump {
            do {
                let result = try Relay.rpc(socket: socket, method: "ui.overview", params: "{}")
                print(Wording.summary(Overview(result), detail: nil))
                exit(0)
            } catch {
                print("\(error)")
                exit(1)
            }
        }
        relaySocket = socket
        RelayApp.main()
    }
}
