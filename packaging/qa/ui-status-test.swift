import Foundation

private func expect(_ condition: Bool, _ message: String) {
    if !condition {
        FileHandle.standardError.write(Data("ui-status-test failed: \(message)\n".utf8))
        exit(1)
    }
}

@main
struct UIStatusTest {
    static func main() {
        let now = Date(timeIntervalSince1970: 1_700_000_100)
        let ready = MenuStatusSnapshot(state: "ready", pending: 0, blocking: 0)
        let degraded = MenuStatusSnapshot(state: "degraded", pending: 1, blocking: 2)

        // 1. 断开 / 连接失败 -> 必须提示服务不可用，不能继续显示旧的正常快照。
        let inactive = MenuStatus.text(conn: .inactive, message: nil, snapshot: ready, lastUpdated: now, now: now)
        expect(inactive == "服务不可用（未连接）", "inactive should report unavailable: \(inactive)")
        expect(MenuStatus.symbolName(for: .inactive) == "bolt.horizontal.circle", "inactive icon mismatch")

        let failed = MenuStatus.text(conn: .failed, message: "连接超时", snapshot: ready, lastUpdated: now, now: now)
        expect(failed == "服务不可用：连接超时", "failed should include reason: \(failed)")
        expect(MenuStatus.symbolName(for: .failed) == "bolt.horizontal.circle", "failed icon mismatch")

        // 2. 初次连接且无快照 -> 连接中。
        let connecting = MenuStatus.text(conn: .connecting, message: nil, snapshot: nil, lastUpdated: nil, now: now)
        expect(connecting == "连接中…", "connecting without snapshot mismatch: \(connecting)")

        // 3. 快照超过 30 秒未更新 -> 显式提示过期。
        let staleDate = now.addingTimeInterval(-31)
        let stale = MenuStatus.text(conn: .ready, message: nil, snapshot: ready, lastUpdated: staleDate, now: now)
        expect(stale.hasPrefix("状态可能已过期：服务 ready"), "stale status missing prefix: \(stale)")

        // 4. 30 秒内正常快照 -> 服务 ready；degraded / maintenance / archive 语义准确。
        let freshDate = now.addingTimeInterval(-30)
        let ok = MenuStatus.text(conn: .ready, message: nil, snapshot: ready, lastUpdated: freshDate, now: now)
        expect(ok == "服务 ready", "fresh ready mismatch: \(ok)")
        expect(MenuStatus.symbolName(for: .ready) == "point.3.connected.trianglepath.dotted", "ready icon mismatch")

        let deg = MenuStatus.text(conn: .degraded, message: nil, snapshot: degraded, lastUpdated: now, now: now)
        expect(deg == "服务 degraded（派发门：待确认 1，阻断 2）", "degraded mismatch: \(deg)")
        expect(MenuStatus.symbolName(for: .degraded) == "exclamationmark.triangle", "degraded icon mismatch")

        let maint = MenuStatus.text(
            conn: .maintenance,
            message: nil,
            snapshot: MenuStatusSnapshot(state: "maintenance", pending: 0, blocking: 0),
            lastUpdated: now,
            now: now
        )
        expect(maint == "服务维护中（暂停写入与派发）", "maintenance mismatch: \(maint)")
        expect(MenuStatus.symbolName(for: .maintenance) == "exclamationmark.triangle", "maintenance icon mismatch")

        let archive = MenuStatus.text(
            conn: .archive,
            message: nil,
            snapshot: MenuStatusSnapshot(state: "archive", pending: 0, blocking: 0),
            lastUpdated: now,
            now: now
        )
        expect(archive == "只读档案（不启动任务）", "archive mismatch: \(archive)")
        expect(MenuStatus.symbolName(for: .archive) == "archivebox", "archive icon mismatch")

        print("ui-status-test: all assertions passed")
    }
}
