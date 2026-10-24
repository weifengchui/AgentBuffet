<div align="center">

# 🍱 AgentBuffet

**A tiny, local AI model API gateway — one binary, zero runtime dependencies.**

Aggregate every model provider behind a single endpoint and give each agent its own
buffet line with automatic priority-based failover.

Idle CPU < 0.5% · RAM < 30 MB · single static binary
*(measured on a typical personal setup: ~0% idle CPU, ~8 MB RSS)*

[English](README.md) · [简体中文](README.zh-CN.md)

[![Release](https://img.shields.io/github/v/release/weifengchui/AgentBuffet?label=release&color=4f8cff)](https://github.com/weifengchui/AgentBuffet/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![CI](https://github.com/weifengchui/AgentBuffet/actions/workflows/ci.yml/badge.svg)](https://github.com/weifengchui/AgentBuffet/actions/workflows/ci.yml)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS%20%C2%B7%20Windows-lightgrey)](https://github.com/weifengchui/AgentBuffet/releases/latest)

[🌐 Product page](https://weifengchui.github.io/AgentBuffet/) · [⬇ Download](https://github.com/weifengchui/AgentBuffet/releases/latest)

</div>

---

## Why?

You juggle OpenCode, Codex, Hermes and a drawer full of provider API keys. Every
provider has its own quota, rate limits and outages — and every agent config has to
know all of that. **AgentBuffet sits in between**: agents talk to one local endpoint,
the gateway routes each request across providers with automatic failover, rotates API
keys, tracks usage and keeps every real key on this machine only.

No database. No sidecar processes. No C runtime. One binary, done.

## Highlights

| | |
|---|---|
| ⚡ **Featherweight** | Idle CPU < 0.5% & RAM < 30 MB under typical personal use (measured ~0% / ~8 MB), single static binary |
| 🔀 **Failover** | Priority-ordered routing chain, sliding-window circuit breaker, quota soft caps, passive 429 calibration, first-chunk error switching |
| 🔑 **Key rotation** | Multiple API keys per provider, round-robin; 401/403 auto-switches to the next key before failing over |
| 🖥️ **Form-driven console** | Bilingual (zh/EN) web UI: pick vendor → pick plan → paste key. Drag-to-reorder routing, free-model badges, hot reload — no hand-written YAML required |
| 🔌 **Agent attach** | Auto-detects installed OpenCode / Codex / Hermes and injects gateway config (backed up first, byte-exact restore on detach). Drift is detected and repaired automatically; multiple OpenCode instances supported |
| 🌊 **Zero-buffer SSE** | Streaming responses are forwarded chunk by chunk — no aggregation, no added latency |
| 🔒 **Encrypted backup** | Config + all keys exported as a password-encrypted `.aback` (Argon2id + AES-256-GCM) |
| 🚀 **Autostart** | systemd user unit (Linux), LaunchAgent (macOS), Startup folder (Windows) — pure file operations |

## Quick start

1. **Download** the archive for your platform from [Releases](https://github.com/weifengchui/AgentBuffet/releases/latest) and extract it — you get a single `AgentBuffet` binary.

   ```bash
   tar xzf agentbuffet-linux-x86_64.tar.gz   # Windows: just unzip
   ```

2. **Start the gateway:**

   ```bash
   export DEEPSEEK_API_KEY=sk-...            # or put keys in ~/.agentbuffet/env (0600)
   ./AgentBuffet serve
   ```

   The gateway listens on `http://127.0.0.1:3425` (traffic) and `http://127.0.0.1:3426` (console).
   A `.env` file in your home directory is loaded automatically on start and hot reload.

3. **Open the console** at `http://127.0.0.1:3426/`, add a provider (vendor → plan → paste key),
   create a routing group, then flip the switch next to a detected agent. Done — your agents
   are now eating from the buffet.

Any OpenAI-compatible client can also be pointed at the gateway manually:

```
OPENAI_BASE_URL=http://127.0.0.1:3425/<agent-id>/v1
OPENAI_API_KEY=agentbuffet
```

## How failover works

Each request walks its routing chain in priority order:

```
agent ──▶ /<agent-id>/v1/chat/completions
              │
              ▼
        [1] deepseek/deepseek-chat   429? → circuit breaker opens, cooldown
              │ fail
              ▼
        [2] openrouter/deepseek-r1   401/403? → rotate to next API key first
              │ fail
              ▼
        [3] moonshot/moonshot-v1-8k  timeout / first-chunk error → next hop
```

Every hop — success, retry, skip or breaker state — is recorded and shown per request
in the console (routing chain column).

## Configuration

The console writes everything for you. For the impatient, `~/.agentbuffet/config.yaml`:

<details>
<summary>Full example</summary>

```yaml
gateway:
  addr: "127.0.0.1:3425"
  ui_addr: "127.0.0.1:3426"
providers:
  - id: deepseek
    vendor: deepseek
    base_url: "https://api.deepseek.com/v1"
    api_key_env: DEEPSEEK_API_KEY
    models: [deepseek-chat, deepseek-reasoner]
    quota:
      daily_tokens: 1000000
  - id: moonshot
    vendor: moonshot
    base_url: "https://api.moonshot.cn/v1"
    api_key_env: MOONSHOT_API_KEY
    models: [moonshot-v1-8k]
routing_groups:
  - name: coding-flash
    strategy: order
    members:
      - { provider: deepseek, model: deepseek-chat, priority: 1 }
      - { provider: moonshot, model: moonshot-v1-8k, priority: 2 }
agents:
  - { id: opencode, default_group: coding-flash }
  - { id: hermes, default_group: coding-flash }
failover:
  max_retries: 3
  timeouts:
    connect_seconds: 5
    first_byte_seconds: 60
    stream_idle_seconds: 120
```

</details>

Environment variables: `AGENTBUFFET_CONFIG` overrides the config path, `AGENTBUFFET_HOME`
the data directory (usage logs, key env file).

## CLI

| Command | Description |
|---|---|
| `AgentBuffet [serve]` | Start the gateway (default subcommand) |
| `AgentBuffet reload` | Hot-reload config (invalid config is rejected; the running snapshot keeps serving) |
| `AgentBuffet agents detect\|status\|attach\|detach` | Agent attach manager |
| `AgentBuffet service install\|uninstall\|status` | Autostart: systemd / LaunchAgent / Startup folder, pure file ops |
| `AgentBuffet config export [--out PATH]` | Export current config (stdout by default) |
| `AgentBuffet config import PATH` | Import a full config (validated; old file backed up as `.bak`) |

## Security

- Admin API and console bind to loopback only; non-loopback requests get `403`.
- Real API keys are persisted only in `~/.agentbuffet/env` on this machine
  (owner-only `0600`) and loaded into the gateway's process environment. They never
  appear in `config.yaml`, logs, the UI or agent configs — those contain only
  variable names, placeholders and a last-4 mask. Password-encrypted `.aback`
  backups contain keys by design; nothing is ever sent off the machine.
- `base_url` pointing at private/loopback IPs requires an explicit `allow_private: true` (fail-closed).
- The attach writer only touches a whitelist of files, refuses to write on parse failure,
  and always backs up first.
- Nothing phones home: no telemetry, no external processes, model catalog fetches are plain HTTPS.

## Build from source

```bash
cargo build --release     # -> target/release/AgentBuffet
cargo test                # unit + integration (mock providers, no network needed)
```

Rust edition 2021. CI enforces `fmt --check`, `clippy -D warnings` and tests on
Linux / macOS / Windows; pushing a `v*` tag builds five platform artifacts and publishes a Release.

## Contributing

Issues and PRs are welcome. For features, opening an issue first to discuss the
scope is appreciated — this project deliberately stays a single dependency-light
binary, so large new runtime dependencies are unlikely to be accepted.
Please keep commits passing `cargo fmt`, `cargo clippy` and `cargo test`.

## License

[MIT](LICENSE) © 2026 weifengchui
