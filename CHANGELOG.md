# Changelog

所有用户可见变化记录在此。产品版本以 `crates/relay-daemon/Cargo.toml` 为准；里程碑
（M1/M2/M2.5/M3）不是版本号。

## 0.1.27 — 2026-11-25

### Added

- Provider retry on 429 / transient transport failures: up to 10 retries with a
  30 s interval (`RELAY_RETRY_MAX`, `RELAY_RETRY_INTERVAL_MS`, bounded). When the
  budget is exhausted the task fails explicitly with "重试上限内仍未成功：无法进行
  下去，任务失败"; no artifact is produced.
- Review fallback model: `RELAY_REVIEW_FALLBACK_MODEL` switches to another model
  in the same OpenCode Go account when the quota is exhausted. The approval binds
  the fallback model and usage records accept either the primary or fallback id.

### Changed

- The live route worker timeout is raised to 7 minutes so the bounded retry
  budget fits inside one worker run.

## 0.1.26 — 2026-11-25

### Added

- `relayctl local-preview enable|status|disable`: the packaged app can create
  the per-user instance, install the stable wrapper, register the LaunchAgent,
  and verify doctor + MCP without source or manual plist edits. `disable` only
  touches Relay-owned plists and preserves data.
- First-run onboarding in the task window offers the explicit enable action.
- Release pipeline: `verify-assets.py` writes and re-verifies `SHA256SUMS` from
  an explicit asset list; `smoke-native.sh` runs prepare/daemon/doctor/MCP/
  offline demo/digests/DMG mount in a temp HOME; stable asset names without
  build timestamps; tag-only releases with `overwrite_files: false` and
  `fail_on_unmatched_files: true`.

### Changed

- Worker process groups are reclaimed after leader exit (TERM → SIGKILL with
  bounded probes) instead of leaving descendants behind a permanent gate.
- Diff collection is bounded: 8 MiB/file, 32 MiB total, 16 MiB patch, 32 MiB
  git output, 30 s deadline; FIFO/socket/device are rejected without content
  reads; added files keep their executable bit.
- Git commands use `/usr/bin/git`, disable system/global config and attributes,
  force fsmonitor/hooks/attributes off, and clear `GIT_CONFIG_*` injections.
  Baseline rejects local execution surfaces and active `filter=` attributes.
- install/upgrade/rollback share `.relay-backup.*` with `metadata.json`; the
  rewritten `rollback.sh` validates app/snapshot digests, snapshots the current
  state first, restores same-schema snapshots, and refuses unsupported
  downgrades (9→8 uses `rollback-v8`).
- Catalog recommendations keep the established status but report
  `production_ready: false`, `preview_only: true`, `max_rework: 0`; real routes
  are no longer marked as production.
- UI demo subprocesses use a bounded runner (timeout, 64 KiB output cap,
  TERM→KILL); the menu text and icon share one status source with a 30 s stale
  hint; review cards expose implementer/reviewer/findings to VoiceOver.

## 0.1.25 — 2026-11-25

### Added

- `RELAY_MINIMAX_ENDPOINT` selects the pinned MiniMax implement endpoint: the
  international default (`api.minimax.io`) or the mainland-China endpoint
  (`api.minimax.cn`). Any other value fails closed, and the approval binds the
  actual endpoint.
- `RELAY_GO_MODEL` selects the OpenCode Go review model (default
  `deepseek-v4.1-flash`). The approval binds the actual model, and a value equal
  to the implement model is rejected.
- Live route docs provision the two dedicated keys through the
  `MINIMAX_API_KEY` / `OC_API_KEY` shell environment variables instead of
  keychain lookups.

### Fixed

- Claude Code 2.1.326 `system/thinking_tokens` progress frames emitted while
  extended thinking streams are accepted instead of failing the transcript as
  an unknown frame.

### Verified

- One end-to-end real dual-account synthetic sample passed on this candidate:
  implement via MiniMax `MiniMax-M3.1-Flash` (mainland-China endpoint,
  billing attribution `unverified`), verify, and review via OpenCode Go
  `deepseek-v4.1-flash` with `balance_charge_usd=0` and provider source `go`.

## 0.1.24 — 2026-11-24

### Fixed

- Baseline checks now read only repository-local Git config, so a global Git LFS
  filter no longer blocks every registered project.
- Daemon tests use short `/private/tmp` paths for SUN_LEN-limited sockets while
  keeping the setgid/sticky-bit ownership tests on the default temp directory.
- GitHub Actions pins Rust 1.99.1 with rustfmt/clippy, builds on macOS 26 arm64,
  and publishes the unsigned DMG through the tag workflow.

## 0.1.23 — 2026-11-24

### Added

- 90-day audit metadata cleanup in `maintenance prune` (`--metadata-days`, default 90).
- Startup wrapper for automatic 5 MiB x 5 log rotation on service start/restart.
- Versioned built-in model catalog and deterministic role recommendation
  (`relayctl catalog list|recommend`), with alias independence checks.
- More modern task-window header, status badges, stage timeline and menu-bar panel.
- Unsigned DMG build/verify scripts and complete GitHub release notes.

### Fixed

- Upgrade migrates the LaunchAgent to the stable log-rotating wrapper while keeping
  rollback compatibility.

## 0.1.22 — 2026-11-24

### Added

- MIT license and crate metadata.
- Unsigned local DMG build/verify scripts for GitHub/self-hosted distribution.
- Modern task-window header, status badges, stage timeline, richer menu-bar panel,
  and clearer sidebar/detail cards.

### Changed

- GitHub release instructions explicitly require manual first-launch approval because
  the project has no Apple Developer ID/notarization.
## 0.1.21 — 2026-11-24

### License

- Project license selected as MIT; crate metadata updated.

### Added

- Non-terminal task queue cap of 20, with idempotent replay still allowed.
- 2 GiB managed artifact/worktree quota sampled before new claims; running work may finish.
- `relayctl maintenance prune` preview/apply for expired terminal worktrees/artifacts and
  expired unused plans; task/event audit rows are retained.
- `relayctl maintenance rotate-logs` with 5 MiB x 5 retention for stopped instances.
- StageRun timeline in task detail; accessibility labels for diff, receipt, handoff and
  acceptance controls.

### Fixed

- Task-window detail now reads all artifact pages up to 256 KiB instead of truncating at
  the default slice.
- Explicitly unlock the instance flock before dropping the daemon handle; reduces
  same-process restart flakiness.
- Registry/run-time docs and candidate evidence updated for 0.1.21.

## 0.1.20 — 2026-11-24

### Fixed

- Retry now requires the new plan to keep the original write scope, verification rule
  set and execution profile, not only project/goal/base/brief.
- MCP business failures now return a structured `error` object with `isError: true`
  instead of text-only errors.
- The task window keeps the last known overview across transient daemon failures,
  auto-selects a valid task, and distinguishes inactive/failed/maintenance/archive.
- Task detail can load the test receipt and handoff in addition to the diff.
- Live-route workflow summaries no longer label real provider runs as offline adapters.
- Managed edit input is capped below the live route's 64 KiB stage-input limit so a
  preview cannot succeed and fail only after the worker starts.
- Default disk guard stops new claims below 512 MiB free on the managed volume.

### Added

- Read-only `Store::open_read_only` for diagnostics; `relay_status` now exposes
  outer revision/attempt and a stable `next_action`.
- Review notes are persisted as `review_note` audit events.
- README, architecture, contributing, security, support, changelog and open-source
  readiness documentation.

### Changed

- README, architecture, contributing, security, support and open-source readiness
  documentation expanded.
- Crate descriptions now describe product responsibilities rather than internal
  work-package names.
- Preview plans expire after 15 minutes (03 default) instead of one hour.

## 0.1.19 — 2026-11-24

- Installed as a local preview; added retry scope/rule/profile enforcement, structured
  MCP business errors, task-window transient-state retention, receipt/handoff paging,
  live-route summary labeling and an upgrade bootout/snapshot race fix.
- Upgraded in place from 0.1.18 with a verified 2-task/28-event/2-artifact snapshot.

## 0.1.18 — 2026-11-24

- Local preview installed at `~/Applications/RelayTaskWindow.app`.
- Fixed SIGTERM/SIGINT foreground cleanup for active offline process groups.
- Real-route preview/serve, StageRun persistence, project registration and task window
  acceptance are present.
- Full candidate check: 225 tests passed, 0 failed.

## 0.1.7 — 2026-11-24

- App icon, macOS 26 liquid-glass status card (material fallback), menu bar entry and
  foreground “任务” menu.

## 0.1.0 — 2026-11-23

- First local preview: offline demo, two local adapters, implementation → verification
  → independent review, one automatic rework, manual acceptance.
