import Foundation

enum ServiceConnection: String, Equatable {
    case inactive, connecting, ready, degraded, failed, maintenance, archive
}

struct MenuStatusSnapshot: Equatable {
    let state: String
    let pending: Int
    let blocking: Int
}

/// 菜单栏与服务状态文案的纯函数投影：无 UIKit/AppKit/SwiftUI 依赖，
/// 可由 swiftc 独立编译做自动化断言。
enum MenuStatus {
    static let staleThresholdSeconds: TimeInterval = 30

    static func serviceLabel(_ raw: String, pending: Int, blocking: Int) -> String {
        switch raw {
        case "ready": return "服务 ready"
        case "degraded": return "服务 degraded（派发门：待确认 \(pending)，阻断 \(blocking)）"
        case "maintenance": return "服务维护中（暂停写入与派发）"
        case "archive": return "只读档案（不启动任务）"
        default: return "服务 \(raw)"
        }
    }

    /// 菜单文字与图标使用同一数据源：连接失败或数据过期时不得继续显示“正常”。
    static func text(
        conn: ServiceConnection,
        message: String?,
        snapshot: MenuStatusSnapshot?,
        lastUpdated: Date?,
        now: Date = Date()
    ) -> String {
        if conn == .inactive { return "服务不可用（未连接）" }
        if conn == .failed { return "服务不可用" + (message.map { "：\($0)" } ?? "") }
        guard let snapshot else { return "连接中…" }
        let base = serviceLabel(snapshot.state, pending: snapshot.pending, blocking: snapshot.blocking)
        if let lastUpdated, now.timeIntervalSince(lastUpdated) > staleThresholdSeconds {
            let stamp = lastUpdated.formatted(date: .omitted, time: .shortened)
            return "状态可能已过期：\(base)（最后更新 \(stamp)）"
        }
        return base
    }

    static func symbolName(for conn: ServiceConnection) -> String {
        switch conn {
        case .degraded, .maintenance: return "exclamationmark.triangle"
        case .inactive, .failed: return "bolt.horizontal.circle"
        case .connecting: return "arrow.triangle.2.circlepath"
        case .archive: return "archivebox"
        case .ready: return "point.3.connected.trianglepath.dotted"
        }
    }
}
