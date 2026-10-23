# AgentBuffet

极低功耗的本地 AI 模型 API 网关：一个 `buffetd` 二进制，把多家模型供应商聚合成统一入口，为每个 Agent 提供按优先级自动故障转移的"自助餐台"。空闲 CPU < 0.5%、常驻内存 < 30MB。

## 特性

- **单二进制、零运行时依赖**：不依赖外部进程、数据库、GUI 框架；静态聚合，装完即用。
- **路径式 Agent 识别**：`http://127.0.0.1:3425/<agent-id>/v1/...`，无需为每个 Agent 单独开端口。
- **故障转移**：组内按 priority 依次尝试；熔断（滑动窗口）、额度软上限、429 被动校准、首 chunk 错误切换。
- **零缓冲 SSE 透传**：流式响应逐 chunk 转发，不聚合、不等待。
- **接入器**：一条命令把 OpenCode / Codex 配置注入网关（写入前备份，卸载逐字节还原；真实 Key 永不落 Agent 配置）。
- **Web 控制台**：AgentLink 风格深色界面，五视图（概览 / 服务商 / 路由 / 接入器 / 设置），中英双语，仅监听 loopback。日常配置全部表单化：选厂商 → 选套餐 → 贴 Key 即添加服务商；路由组、Agent、网关参数均可在界面增删改，热重载即时生效，无需手写 YAML（YAML 编辑器保留为高级折叠项）。

## 快速开始

1. 从 [Releases](../../releases) 下载对应平台的压缩包，解压得到 `buffetd`。

2. 写配置 `~/.agentbuffet/config.yaml`：

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
  - name: cheap
    strategy: order
    members:
      - { provider: moonshot, model: moonshot-v1-8k, priority: 1 }
agents:
  - { id: opencode, default_group: coding-flash }
  - { id: codex, default_group: coding-flash }
  - { id: hermes, default_group: cheap }
failover:
  max_retries: 3
  timeouts:
    connect_seconds: 5
    first_byte_seconds: 60
    stream_idle_seconds: 120
```

3. 注入供应商 Key 并启动（Key 只存在于网关进程环境变量）：

```bash
export DEEPSEEK_API_KEY=sk-...
export MOONSHOT_API_KEY=sk-...
buffetd serve
```

4. 打开 `http://127.0.0.1:3426/` 查看 UI，对检测到的 Agent 拨一下开关即完成接入。

## 接入 Agent

```bash
buffetd agents detect          # 扫描已安装的 Agent 与接入状态
buffetd agents attach opencode # 注入配置（自动备份，幂等）
buffetd agents attach codex --dry-run
buffetd agents detach opencode # 还原（备份优先，逐字节）
buffetd agents status          # 漂移检查
```

未收录的 Agent（任意 OpenAI-compatible 客户端）用通用适配器：`attach` 会打印环境变量模板，设 `OPENAI_BASE_URL=http://127.0.0.1:3425/<agent-id>/v1`、`OPENAI_API_KEY=agentbuffet` 即可。

## CLI 一览

| 命令 | 说明 |
| --- | --- |
| `buffetd [serve]` | 启动网关（默认子命令） |
| `buffetd reload` | 通知运行中的网关热重载配置（校验失败拒载，旧快照继续服务） |
| `buffetd agents detect\|status\|attach\|detach` | 接入器 |
| `buffetd service install\|uninstall` | 写/删 systemd 用户级自启单元（Linux） |

环境变量：`AGENTBUFFET_CONFIG` 覆盖配置文件路径，`AGENTBUFFET_HOME` 覆盖数据目录（用量日志、Key 环境文件）。

## 安全边界

- 管理面（`/_admin`、UI）仅监听 loopback；非 loopback 访问一律 403。
- 真实 API Key 只存在于网关进程环境，不写入日志、UI、配置文件；Agent 配置里只有占位符。
- 加密备份（`.aback`）：设置视图可将配置 + 全部 Key 打包，经 Argon2id + AES-256-GCM 密码加密后导出；导入自动校验并热重载，密码遗忘则无法恢复。
- provider `base_url` 指向私有/回环 IP 必须显式 `allow_private: true`（fail-closed）。
- 接入器只写白名单文件，解析失败拒绝写入，绝不"尽力而为"。

## 构建

```bash
cargo build --release          # 产物 target/release/buffetd
cargo test                     # 单元 + 集成（含 mock provider，无外网依赖）
```

CI 覆盖 Linux / macOS / Windows 三平台的 `fmt --check`、`clippy -D warnings`、`test`；打 `v*` tag 自动构建六平台产物并发布 Release。
