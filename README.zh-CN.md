<div align="center">

# 🍱 AgentBuffet

**极低功耗的本地 AI 模型 API 网关 —— 单二进制，零运行时依赖。**

把多家模型供应商聚合成一个统一入口，为每个 Agent 提供按优先级自动故障转移的"自助餐台"。

空闲 CPU < 0.5% · 常驻内存 < 30 MB · 单个静态二进制

[English](README.md) · [简体中文](README.zh-CN.md)

[![Release](https://img.shields.io/github/v/release/weifengchui/AgentBuffet?label=release&color=4f8cff)](https://github.com/weifengchui/AgentBuffet/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![CI](https://github.com/weifengchui/AgentBuffet/actions/workflows/ci.yml/badge.svg)](https://github.com/weifengchui/AgentBuffet/actions/workflows/ci.yml)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS%20%C2%B7%20Windows-lightgrey)](https://github.com/weifengchui/AgentBuffet/releases/latest)

[🌐 产品主页](https://weifengchui.github.io/AgentBuffet/) · [⬇ 下载发布包](https://github.com/weifengchui/AgentBuffet/releases/latest)

</div>

---

## 为什么需要它？

你同时使用 OpenCode、Codex、Hermes 等多个助手，抽屉里躺着一堆供应商 API Key。
每家都有自己的额度、限流和故障时刻，而每个 Agent 配置都得知道这一切。
**AgentBuffet 夹在中间**：所有 Agent 只对一个本地地址说话，网关负责跨供应商路由、
自动故障转移、Key 轮转、用量记账——真实密钥永远只留在本机。

没有数据库，没有旁路进程，没有 C 运行库。一个二进制，装完即用。

## 核心亮点

| | |
|---|---|
| ⚡ **极轻** | 空闲 CPU < 0.5%，常驻内存 < 30 MB，单个静态二进制 |
| 🔀 **自动故障转移** | 按优先级走路由链；滑动窗口熔断、额度软上限、429 被动校准、首 chunk 错误即时切换 |
| 🔑 **多 Key 轮转** | 同一服务商可挂多个 API Key，请求轮转分摊；401/403 先换 Key 重试，全部失败才转移服务商 |
| 🖥️ **表单化控制台** | 中英双语 Web UI：选厂商 → 选套餐 → 贴 Key 即添加服务商；拖拽排序、免费模型标识、热重载即时生效，无需手写 YAML |
| 🔌 **接入器一键托管** | 自动检测本机 OpenCode / Codex / Hermes 并注入网关配置（写前备份，卸载逐字节还原）；配置漂移自动检测、全自动修复；支持 OpenCode 多实例独立路由 |
| 🌊 **零缓冲 SSE** | 流式响应逐 chunk 透传，不聚合、不增加延迟 |
| 🔒 **加密备份** | 配置 + 全部 Key 经 Argon2id + AES-256-GCM 密码加密导出为 `.aback` |
| 🚀 **开机自启** | Linux systemd 用户单元 / macOS LaunchAgent / Windows 启动目录，均为纯文件系统操作 |

## 快速开始

1. 从 [Releases](https://github.com/weifengchui/AgentBuffet/releases/latest) 下载对应平台的压缩包并解压，得到一个 `AgentBuffet` 可执行文件。

   ```bash
   tar xzf agentbuffet-linux-x86_64.tar.gz   # Windows 直接解压 zip
   ```

2. 启动网关：

   ```bash
   export DEEPSEEK_API_KEY=sk-...            # 或写入 ~/.agentbuffet/env（0600）由网关自动载入
   ./AgentBuffet serve
   ```

   流量地址 `http://127.0.0.1:3425`，控制台 `http://127.0.0.1:3426`。
   启动与热重载时会自动载入用户主目录下的 `.env` 文件。

3. 打开 `http://127.0.0.1:3426/`：添加服务商（选厂商 → 选套餐 → 贴 Key）、
   建路由组，然后对检测到的 Agent 拨一下开关。完成——你的 Agent 正式开饭。

任意 OpenAI 兼容客户端也可手动指过来：

```
OPENAI_BASE_URL=http://127.0.0.1:3425/<agent-id>/v1
OPENAI_API_KEY=agentbuffet
```

## 故障转移是怎么跑的

每个请求沿路由链按优先级逐跳尝试：

```
agent ──▶ /<agent-id>/v1/chat/completions
              │
              ▼
        [1] deepseek/deepseek-chat   429？→ 熔断打开，进入冷却
              │ 失败
              ▼
        [2] openrouter/deepseek-r1   401/403？→ 先轮转下一个 API Key
              │ 失败
              ▼
        [3] moonshot/moonshot-v1-8k  超时 / 首 chunk 错误 → 继续转移
```

每一跳的成功、重试、跳过与熔断状态都会记录，并在控制台的「路由链」列逐请求展示。

## 配置

控制台会替你写好一切。急性子可以直接看 `~/.agentbuffet/config.yaml`：

<details>
<summary>完整示例</summary>

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

环境变量：`AGENTBUFFET_CONFIG` 覆盖配置文件路径，`AGENTBUFFET_HOME` 覆盖数据目录（用量日志、Key 环境文件）。

## CLI 一览

| 命令 | 说明 |
|---|---|
| `AgentBuffet [serve]` | 启动网关（默认子命令） |
| `AgentBuffet reload` | 热重载配置（校验失败拒载，旧快照继续服务） |
| `AgentBuffet agents detect\|status\|attach\|detach` | 接入器 |
| `AgentBuffet service install\|uninstall\|status` | 开机自启：systemd / LaunchAgent / 启动目录，纯文件操作 |
| `AgentBuffet config export [--out PATH]` | 导出当前配置（默认 stdout） |
| `AgentBuffet config import PATH` | 导入整份配置（校验失败不落盘，原文件自动备份 `.bak`） |

## 安全边界

- 管理面（`/_admin`、UI）仅监听 loopback；非回环访问一律 `403`。
- 真实 API Key 仅持久化在本机 `~/.agentbuffet/env` 文件（权限 0600，仅属主可读写），由网关载入进程环境；`config.yaml`、日志、UI 与 Agent 配置中只有变量名、占位符与末四位掩码。密码加密的 `.aback` 备份按设计包含密钥；任何密钥都不出本机。
- `base_url` 指向私有/回环 IP 必须显式 `allow_private: true`（fail-closed）。
- 接入器只写白名单文件，解析失败拒绝写入，绝不"尽力而为"，写前必备份。
- 零上报：无遥测、不派生外部进程，模型目录拉取只是普通 HTTPS 请求。

## 从源码构建

```bash
cargo build --release     # 产物 target/release/AgentBuffet
cargo test                # 单元 + 集成测试（内置 mock provider，无需外网）
```

Rust edition 2021。CI 在 Linux / macOS / Windows 三平台强制执行 `fmt --check`、
`clippy -D warnings` 与测试；推送 `v*` tag 自动构建五平台产物并发布 Release。

## 参与贡献

欢迎 Issue 与 PR。新功能建议先开 Issue 对齐范围——本项目刻意保持单二进制、
轻依赖的定位，引入大型运行时依赖的改动大概率不会合入。
提交前请保证 `cargo fmt`、`cargo clippy`、`cargo test` 全部通过。

## 许可证

[MIT](LICENSE) © 2026 weifengchui
