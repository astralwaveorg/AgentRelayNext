# Agent Relay

One coding agent writes. Another reviews. You get the evidence.

[English](README.md) · [简体中文](README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-2ea44f.svg)](LICENSE)
[![Platform: macOS 14+](https://img.shields.io/badge/platform-macOS%2014%2B%20Apple%20Silicon-111827.svg)](#requirements)
[![Built with Rust + SwiftUI](https://img.shields.io/badge/built%20with-Rust%20%2B%20SwiftUI-f97316.svg)](#architecture)
[![CI](https://github.com/astralwaveorg/AgentRelayNext/actions/workflows/ci.yml/badge.svg)](https://github.com/astralwaveorg/AgentRelayNext/actions/workflows/ci.yml)
[![Release](https://img.shields.io/badge/release-v0.1.27--preview-2a7fff.svg)](https://github.com/astralwaveorg/AgentRelayNext/releases/latest)

<img src="docs/assets/hero.svg" alt="Agent Relay local handoff between two coding agents on macOS" width="100%">

Agent Relay is a local macOS task relay for two independent AI coding agents. It exposes an MCP stdio bridge, a CLI, and a native SwiftUI window.

A single task runs in four local steps:

1. A writer agent edits files inside a detached Git worktree. Your main checkout stays untouched.
2. Relay runs your approved local test command with fixed arguments, no shell, and a cleared environment. It saves a hashed test receipt.
3. A second tool with a different model reviews only the patch and the test receipt. It has read-only input and returns a digest-bound report.
4. You open the SwiftUI window, CLI, or MCP client to inspect the patch, test receipt, and review findings. You accept or reject the result. Relay never merges, pushes, or deploys on its own.

## Key properties

- Independent review. The writer and reviewer must use different tools and different normalized models. Provider prefixes and aliases count as the same model.
- Bound evidence. Relay hashes `diff.patch`, `test_receipt.json`, and `review.json` with SHA-256 and binds them to one candidate. Any change after review blocks delivery.
- Untouched main checkout. All edits happen in a managed worktree. Your working tree, index, and `HEAD` do not change.
- Local headless daemon. A Rust daemon (`relayctl`) serves a local Unix socket and an MCP stdio bridge (`relayctl mcp`). A native SwiftUI app shows tasks and menu bar status.
- Fail-closed runtime. Relay uses persistent launch fences, dispatch gates, bounded process-group cleanup (`TERM` then `KILL`), hardened `/usr/bin/git` calls, `0700`/`0600` file permissions, and no network listener.

<img src="docs/assets/window.svg" alt="Synthetic Agent Relay task window with stage timeline, review card, and patch" width="100%">

*Synthetic UI preview. No real project, account, or task data.*

## How it works

<img src="docs/assets/workflow.svg" alt="Implement, verify, review, and human acceptance workflow" width="100%">

```text
Host / Codex / Shell ── MCP stdio / CLI / SwiftUI ──> relay daemon (local Unix socket)
                                                             │
              ┌──────────────────────────────────────────────┘
              ▼
   1. Implement (isolated Git worktree, write-scope allowlist)
              │
              ▼
   2. Verify    (fixed local command, no shell -> test_receipt.json + SHA-256)
              │
              ▼
   3. Review    (independent model, read-only diff + receipt -> digest-bound verdict)
              │
              ▼
   4. Accept    (you inspect evidence and accept / reject)
```

## Quick start

You can run the full offline loop in about three minutes without API keys.

### 1. Install the preview DMG

Download `RelayTaskWindow-<version>.dmg` and `SHA256SUMS` from [GitHub Releases](https://github.com/astralwaveorg/AgentRelayNext/releases/latest). Verify the checksum, then drag `RelayTaskWindow.app` into `Applications`:

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing
```

> **Unsigned developer preview.** This DMG has no Apple Developer ID signature or notarization. On first launch, Control-click `RelayTaskWindow.app` and choose **Open**, or allow it in **System Settings → Privacy & Security**. Do not disable Gatekeeper globally.

### 2. Enable the local service

Open **RelayTaskWindow** and click **Enable local preview** (启用本地预览), or run these commands in a terminal:

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
"$relayctl" local-preview enable
"$relayctl" local-preview status
```

`enable` creates `~/.agent-relay-next/`, installs the log-rotating wrapper, registers a per-user LaunchAgent, and runs `doctor` and `mcp-verify`. Run `local-preview disable` to stop and unregister the service while keeping your data.

### 3. Run the offline demo

Click **Run offline demo** in the task window. Relay runs a synthetic task through implementation, local verification, independent review, one automatic rework, and manual acceptance. It uses no accounts and makes no network calls.

### 4. Connect an MCP client (optional)

Add the local stdio bridge to any MCP host such as Codex or Claude Desktop:

```json
{
  "mcpServers": {
    "agent-relay": {
      "command": "/Users/<you>/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl",
      "args": ["mcp", "--socket", "/Users/<you>/.agent-relay-next/instance/relay.sock"]
    }
  }
}
```

Read [Getting Started and Operations](docs/getting-started.md) for upgrades, rollbacks, cleanup, and diagnostics. Read [Real Routes and Project Registration](docs/real-routes.md) to run on a local Git repository with real models.

## Architecture

```text
Host / Codex ── MCP stdio ──> relayctl mcp ── local Unix RPC ──┐
Shell ─────────────────────── relayctl ───────────────────────┤
SwiftUI window ───────────── local Unix RPC ──────────────────┤
                                                             ▼
                                                        relay daemon
                                                             │
                    ┌────────────────────────────────────────┼──────────────────────┐
                    ▼                                        ▼                      ▼
                 SQLite                               managed worktree        verification
           plans / tasks / events                  detached Git checkout    fixed argv, no shell
           stage_runs / artifacts                  bounded diff + receipt   process-group guard
```

| Component | Responsibility |
| --- | --- |
| `relay-core` | Pure task state machine, recovery classification, retry eligibility |
| `relay-protocol` | 256 KiB bounded JSON-RPC 2.0 framing, UTF-8 and duplicate-key checks, DTOs |
| `relay-store` | SQLite schema v9 (WAL), idempotency, CAS transitions, launch fences, `stage_runs` |
| `relay-git` | Write-scope rules, hardened `/usr/bin/git` layer, worktrees, diffs, receipts, atomic artifacts |
| `relay-daemon` | Single-writer orchestrator, process-group cleanup, Unix IPC, MCP stdio bridge, CLI |
| `relay-sys` | Minimal macOS and Unix primitives (`renamex_np`, `getpeereid`, `setpgid`/`killpg`, `statvfs`) |
| `ui/RelayTaskWindow` | Native SwiftUI task window, diff/receipt/handoff viewer, acceptance dialog, menu bar panel |

See [Architecture and Security](docs/architecture.md) for boundaries and invariants.

## Build from source

### Requirements

- macOS 14 or newer on Apple Silicon
- Git, the Rust toolchain pinned in CI, and Xcode Command Line Tools
- Optional for live routes: user-installed Claude Code and OpenCode CLIs (Relay never downloads or bundles vendor CLIs)

```sh
git clone https://github.com/astralwaveorg/AgentRelayNext.git
cd AgentRelayNext
./scripts/check full
packaging/package.sh "$PWD/dist"
```

## Current status and roadmap

Agent Relay `0.1.x` is an open-source developer preview. The local safety and audit core is in place, while the live route currently targets one fixed dual-account setup (`Claude Code + MiniMax` to write, `OpenCode Go + DeepSeek` to review, single-shot structured edits, manual acceptance).

Planned milestones for `0.2` through `0.4`:

- Open BYOK and multi-CLI adapters (`v0.2`). Remove provider-specific coupling and support OpenAI/Anthropic-compatible APIs, local Ollama, and any independent pair of Claude Code, Codex, OpenCode, Kimi Code, and WorkBuddy.
- Multi-turn worktree coding and automatic rework (`v0.3`). Allow writer agents to use file and test tools inside the isolated worktree with bounded rework loops, while keeping deterministic verification and read-only cross-review outside.
- One-click delivery and cost ledger (`v0.4`). Apply accepted patches to a new local branch, export Markdown PR evidence cards, and track multi-model cost savings.

Read the full engineering plan in [Roadmap](docs/roadmap.md).

## Documentation

| Guide | What it covers |
| --- | --- |
| [Getting Started and Operations](docs/getting-started.md) | Install, `local-preview enable/status/disable`, offline demo, upgrade, rollback, pruning, logs |
| [Real Routes and Project Registration](docs/real-routes.md) | Register local Git repos, live route preview and launch, model catalog, retry and fallback, billing and privacy |
| [Architecture and Security](docs/architecture.md) | Crate boundaries, state machine, review chain, local security invariants |
| [Release and Verification](docs/release.md) | Unsigned DMG build, SBOM, `verify-assets.py`, native smoke, GitHub Release workflow |
| [Roadmap](docs/roadmap.md) | Market analysis, current gaps, and `v0.1.28` to `v1.0` engineering plan |
| [Evidence Bundles](docs/evidence/README.md) | Sanitized records of real dual-account synthetic acceptance runs |

Also see [CHANGELOG.md](CHANGELOG.md), [CONTRIBUTING.md](CONTRIBUTING.md), [SECURITY.md](SECURITY.md), and [SUPPORT.md](SUPPORT.md).

## License

[MIT](LICENSE) © 2026 Agent Relay contributors.
