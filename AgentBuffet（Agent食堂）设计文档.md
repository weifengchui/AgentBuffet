> **AgentBuffet（Agent食堂）—— 极低功耗 AI 模型 API 网关**
> 版本：v0.10 设计稿 ｜ 日期：2026-10-23
> 本文档可直接交给 AI 编程助手实现。
>
> **v0.10 变更**（相对 v0.9）：
> 1. 新增 §7 Agent 接入（Onboarding）：参考 Magpie 的无缝体验，网关可一键改写本机 Agent 配置完成对接，可逆、幂等、带备份；原"不做主动改写 Agent 配置"的非目标相应修订。
> 2. 明确请求 model 与路由组成员的匹配规则（§6），`group/<name>` 虚拟模型可直接指定路由组。
> 3. 流式错误判定补充"首 chunk peek"；超时拆分为 connect / first_byte / stream_idle 三段（§6）。
> 4. SSRF 禁令增加 `allow_private` 显式豁免，兼容本地推理与 mock provider（§5、§9）。
> 5. 崩溃重启后从当日 JSONL 恢复今日聚合（§8）。
> 6. 明确热重载触发方式：管理端点 + CLI，跨平台一致（§5）。
> 7. 价格表未命中 model 时费用记 null 并标注"价格未知"（§8）。
> 8. 路径保留字、启动自检语义（结构错误拒启 / 连通失败仅告警）明确（§5、§10）。

#### 1. 概述

AgentBuffet 是一个运行在本机的轻量 API 转发网关。它接管多个 Agent 的模型请求，按路由组分发给多个 API 供应商；当当前供应商额度耗尽或报错时，自动切换到下一家。它只做转发、路由、记账和 Agent 接入，不做模型内容处理、不做插件、不做桌面客户端。

核心价值排序：**低功耗 > 切换可靠 > 看得清谁用了谁 > 易用**。

无缝体验的定义：用户装好 buffetd、写好配置、启动，然后在 UI 里对检测到的 Agent 拨一个开关——该 Agent 的后续请求就走网关，不要求用户手工改任何 Agent 配置文件。

#### 2. 目标与非目标

**要做**
- 本机回环监听，单二进制，无外部依赖，跨平台。
- OpenAI-compatible 协议转发（流式零缓冲）。
- 多供应商 + 路由组 + 顺序故障转移 + 熔断。
- 本地额度软上限与被动校准。
- 请求级记账，界面上能看清 **哪个 Agent → 哪个路由组 → 哪个供应商**。
- YAML 配置 + 校验 + 快照式热重载。
- 内置静态 Web UI，浏览器访问，零框架依赖。
- **Agent 接入器**：自动检测本机已安装的 Agent，一键改写其配置指向网关（备份、幂等、可逆、dry-run）。

**明确不做（防止范围膨胀）**
- 桌面客户端、系统托盘、WebView、菜单栏。
- 插件系统、插件市场、热加载沙箱。
- fs watcher 监听 Agent 配置或网关配置文件。
- 无人值守的自动改写：接入必须显式触发（CLI 或 UI 开关），网关永不自行修改 Agent 配置。
- Anthropic / Gemini 原生协议（v1.1+）。
- MCP、联网搜索、工具调用增强、内容审查改写。
- 平台登录态 / Cookie 接入（合规与维护成本过高）。
- 局域网共享、Tunnel 穿透、多租户、团队计费。
- 数据库、实时聚合、余额预测、配速曲线。
- 任何 Key 或统计数据上传远端。

#### 3. 技术选型

| 项 | 选择 | 理由 |
|---|---|---|
| 语言 | **Rust**（备选 Go） | 无 GC，长驻空闲 CPU 接近 0；tokio 异步栈成熟 |
| Web 框架 | Axum + Tokio | SSE 流式转发自然，生态稳 |
| HTTP 客户端 | reqwest（连接池复用） | 避免每请求新建连接 |
| 配置 | serde_yaml，`~/.agentbuffet/config.yaml` | 可读性好 |
| 价格表 | 独立 `prices.yaml` | 价格会变，与配置解耦 |
| 存储 | JSONL 按天滚动 + 内存今日聚合 | 零数据库，查询 O(1) |
| UI | 内嵌静态资源（rust-embed），原生 HTML+JS | 首次加载 < 100KB |
| 接入器 | 内置适配器（代码，非插件） | 每个 Agent 的配置格式是私有 API，必须硬编码 + schema 测试 |
| 部署 | 单二进制 + systemd/launchd 自启 | 崩溃自动拉起 |

**硬指标**：空闲 CPU < 0.5%（单核基准）、常驻内存 < 30MB、单次转发增加延迟 < 5ms、UI 刷新不触发文件扫描、流式首字节延迟增加 < 10ms。

#### 4. 架构

```
Agent (base_url → http://127.0.0.1:3425/<agent-id>/v1，由接入器自动写入)
        │
        ▼
┌──────────────────── buffetd ────────────────────┐
│ 1. Listener  (loopback only, :3425)             │
│ 2. Path router  (/hermes/v1 → agent_id)         │
│ 3. Config snapshot (Arc-swap, validated)        │
│ 4. Protocol adapter (OpenAI-compatible only)    │
│ 5. Router  (model→group 解析 → candidates)       │
│ 6. Failover engine (quota guard + breaker)      │
│ 7. Streaming proxy (zero-buffer SSE passthrough)│
│ 8. Usage recorder (in-mem today counter + JSONL)│
│ 9. Web UI / stats API (:3426, read-only)        │
│10. Admin API (:3426/_admin: reload / onboard)   │
│11. Health & self-check (:3425/health)           │
└─────────────────────────────────────────────────┘
        │
        ▼
   Providers (DeepSeek / Qwen / GLM / MiniMax / OpenRouter / ...)
```

**关键约束**：流式响应必须零缓冲透传。禁止把完整响应读入内存再转发——这是 Magpie 类网关吃 CPU 的典型原因。唯一例外：§6 定义的首 chunk peek（至多缓冲一个 chunk，用于错误判定）。

#### 5. 配置

```yaml
# ~/.agentbuffet/config.yaml
gateway:
  addr: "127.0.0.1:3425"
  ui_addr: "127.0.0.1:3426"
  allow_loopback_only: true
  log_retain_days: 30
  conservative_quota_ratio: 0.8        # 额度保守系数
  expose_groups_as_models: true        # 路由组注册为 group/<name> 虚拟模型

prices:
  file: "./prices.yaml"                # model → input/output 单价；未命中记 null

providers:
  - id: ds-1
    vendor: deepseek
    base_url: "https://api.deepseek.com/v1"
    api_key_env: "DEEPSEEK_API_KEY_1"
    models: ["deepseek-chat"]
    quota: { daily_tokens: 1000000 }   # 本地软上限
    allow_private: false               # true 时豁免 SSRF 禁令（本地推理/mock）

  - id: ds-2
    vendor: deepseek
    base_url: "https://api.deepseek.com/v1"
    api_key_env: "DEEPSEEK_API_KEY_2"
    models: ["deepseek-chat"]

  - id: qwen-1
    vendor: qwen
    base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1"
    api_key_env: "QWEN_API_KEY"
    models: ["qwen-plus", "qwen-max"]

routing_groups:
  - name: coding-flash
    strategy: order                      # v1 仅 order
    members:
      - { provider: ds-1, model: deepseek-chat, priority: 1 }
      - { provider: ds-2, model: deepseek-chat, priority: 2 }
      - { provider: qwen-1, model: qwen-plus, priority: 3 }

agents:
  - { id: hermes,      display_name: Hermes,      default_group: coding-flash }
  - { id: opencode,    display_name: OpenCode,    default_group: coding-flash }
  - { id: claude-code, display_name: Claude Code, default_group: coding-flash }

failover:
  retry_on_status: [429, 500, 502, 503, 504]
  retry_on_timeout: true
  max_retries: 3
  timeouts:
    connect_seconds: 5                   # 建连
    first_byte_seconds: 60               # 上游首字节（含流式）
    stream_idle_seconds: 120             # 流中静默；长回答合法，不设总时长上限
  circuit_breaker: { window_seconds: 60, threshold: 3, open_seconds: 120 }
```

**路径保留字**：`health`、`_admin`、`v1` 不得作为 agent id，配置校验时拒绝。

**Agent 侧手工接入**（不用接入器时）只需改一处：`base_url = http://127.0.0.1:3425/hermes/v1`。不需要改任何 header。

**热重载触发**：`POST http://127.0.0.1:3426/_admin/reload`（仅 loopback）或 CLI `buffetd reload`。不引入 fs watcher，Windows 无 SIGHUP，端点方案跨平台一致。校验失败返回 422 并带文件名与行号，旧快照继续服务，进行中请求不受影响。

**启动自检语义**：YAML 结构/引用校验失败 → 拒绝启动（报错带文件名与行号）；provider 连通性或鉴权失败 → 仅告警并把该 provider 标记为 degraded，不拒启。

#### 6. 路由与故障转移（核心状态机）

**model → 路由组解析规则**（v0.10 新增，必须实现）：
1. 请求 model 以 `group/` 开头 → 路由组 = 该组，候选 = 组内全部成员按 priority 排序，上游调用使用各成员自己的 model。
2. 否则 → 路由组 = 该 agent 的 `default_group`，候选 = 组内 `model == 请求 model` 的成员按 priority 排序。
3. 候选为空 → 400，错误信息列出该组可用 model 及所有组名。
4. `model_map`（跨平台 model 名不一致的映射层）为 v1.1 项。

```
request in
  ├─ extract agent_id from path prefix → unknown → 记为 "unidentified"
  ├─ resolve routing_group（见上方解析规则；未命中 → 400，列出可用组）
  ├─ build candidates ordered by priority
  ├─ filter: disabled / quota exhausted / circuit open / degraded
  │
  └─ for each candidate:
       ├─ non-streaming: 可安全重试整条请求
       ├─ streaming:
       │    ├─ peek 首个上游 chunk：非 2xx，或 2xx 但首事件为 SSE error 载荷
       │    │   且尚未向客户端写出任何字节 → 切下一个 provider
       │    ├─ 已开始向客户端写 body → 立即断开，返回 X-Buffet-Failover-Chain，
       │    │   由 Agent 自行重试
       │    └─ peek 到的首 chunk 判定成功后原样写出，之后严格零缓冲
       ├─ 401/403 → 标记 provider 需人工修 key（degraded），继续
       ├─ 429 → 用 retry-after 被动校准：有 retry-after 则隔离至重置点，
       │        否则按保守系数收紧当日软上限，继续
       └─ no candidate left → 返回最后一次错误
```

**三条必须遵守的规则**
1. **绝不写到一半换供应商**。这会产生半截输出污染上下文，比失败更糟。
2. **额度是估算**。UI 全程标注"估算值"，并以 429/401 等真实信号为准。
3. **熔断优先于重试**。窗口内连续失败 N 次即隔离，避免把额度浪费在已宕机的节点上。

#### 7. Agent 接入（Onboarding）—— v0.10 新增

参考 Magpie 的无缝体验，但用**路径前缀**承载 agent id（Magpie 用单 `/v1` + header 约定），零 header、天然满足"看得清谁用了谁"。用户无感知，因为 URL 由接入器自动写入。

**CLI**

```
buffetd agents detect                 # 扫描已知适配器，列出已安装 Agent 及状态
buffetd agents attach <agent-id> [--group coding-flash] [--dry-run]
buffetd agents detach <agent-id>
buffetd agents status                 # 网关侧记录 vs Agent 实际配置的漂移检查
```

**适配器契约**（内置代码，每个已知 Agent 一个）：

| 步骤 | 行为 |
|---|---|
| detect | 配置文件存在且可解析（JSON/TOML），返回 installed / attached / drifted |
| plan | dry-run 输出将修改的文件与 diff 预览，不落盘 |
| apply | 先备份 `<file>.before-agentbuffet`（已存在则不覆盖）；注入名为 `agentbuffet` 的 provider，`base_url = http://127.0.0.1:3425/<agent-id>/v1`；按 `expose_groups_as_models` 注册全部路由组为 `group/<name>` 虚拟模型；将 Agent 默认模型指向其 `default_group` |
| revert | 从备份恢复，或精确删除注入块（备份丢失时） |

**安全护栏**
- 只写适配器白名单内的文件路径；文件解析失败 → 拒绝写入并报错，绝不"尽力而为"。
- 幂等：重复 attach 只更新注入块，不叠加；同名 provider 若被用户自定义过 → 拒绝并提示改名。
- 永不写入任何 API Key：注入的 apiKey 字段填占位符，真实 Key 只存在于网关进程的环境变量。
- attach/detach 后提示"重启该 Agent 并新建会话生效"。
- 网关运行期只读 Agent 配置用于漂移检测，**永不主动写**——改写只发生在显式 CLI/UI 操作。

**v1 适配器范围**
- OpenCode 系（JSON provider 注入，本机已验证格式）。
- Codex CLI（config.toml base_url 注入）。
- 通用 OpenAI-compatible：不写文件，打印 `OPENAI_BASE_URL` / `OPENAI_API_KEY` 环境变量模板。
- Claude Code：Anthropic 协议，待 v1.1 协议适配器落地后接入；v1 仅在 detect 中提示"暂不支持"。

**UI**：Agents 视图（第四视图）列出检测到的 Agent、接入开关、dry-run diff 预览，背后调用与 CLI 相同的 `_admin` 端点。

#### 8. 用量与看板

**记录格式**（JSONL，`~/.agentbuffet/logs/usage-YYYY-MM-DD.jsonl`）

```json
{"ts":1729660800,"agent":"hermes","group":"coding-flash",
 "provider":"ds-1","model":"deepseek-chat","status":200,
 "prompt_tokens":1204,"completion_tokens":388,"cost_est":0.00042,
 "duration_ms":1842,"retries":1,"fail_chain":["ds-1:429","ds-2"]}
```

- `cost_est`：prices.yaml 未命中该 model 时记 `null`，UI 显示"价格未知"，**不得按 0 计**。
- 单价口径：`prices.yaml` 按**每 1M tokens** 记各模型 input/output 单价；日期文件名按 UTC。

**内存聚合**：今日 counters 按 `(agent, group, provider)` 元组累加请求数/tokens/费用/失败数，每次请求只是整数加法，UI 读取无文件 IO。

**崩溃恢复**：启动时顺序读当日 JSONL 重建 counters（一次性，不影响空闲指标）；若日志文件缺失则从 0 开始并在 UI 标注。

**UI 四视图**
1. **Overview**：今日请求数 / tokens / 估算费用；最近 20 条流水（Agent → 组 → 供应商 → 状态 → 耗时）。
2. **Providers**：供应商、所属组、今日 tokens、额度估算进度条、状态（正常/熔断/额度耗尽/degraded）、最近失败原因。
3. **Routing**：组成员拖拽调优先级、启用/禁用开关、一键连通性测试。
4. **Agents**：检测到的本机 Agent、接入开关、漂移状态（§7）。

要求：无前端框架、增量拉取或手动刷新、禁止服务端高频轮询。

#### 9. 安全边界

- 仅监听 loopback；`allow_loopback_only=false` 时必须显式开启并警告。
- **SSRF 防护**：provider `base_url` 解析后禁止指向私有/回环 IP 段，**每次建连时解析**（防 DNS rebinding）；`allow_private: true` 显式豁免（本地 Ollama/vLLM、CI mock provider 用）。
- API Key 只从环境变量读取，禁止出现在日志、错误信息、UI、fail chain 详情、Agent 配置文件中。
- 所有外发请求必须有超时与 context 取消传播；客户端断开必须立即取消上游请求。
- `_admin` 端点仅接受 loopback，且只暴露 reload 与 onboarding 两类操作。
- 不做内容审查、不缓存请求体（避免敏感数据落盘）。

#### 10. MVP 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 ✅ | 网关内核 + 路径式 Agent 识别 + model→组解析 + OpenAI 兼容转发（含流式零缓冲与首 chunk peek） | curl 通、SSE 不断流、`group/` 模型可路由、CPU 空闲 <1% |
| M2 ✅ | 供应商管理 + order 故障转移 + 熔断 + 额度软上限与 429 被动校准 | mock server（allow_private）模拟 429/超时/流中断自动切换 |
| M3 ✅ | JSONL 记录 + 内存今日聚合（含启动恢复）+ 价格表 + 快照式热重载 | 改配置不重启、重启后今日数据不丢、UI 刷新不扫盘 |
| M4 ✅ | Web UI 四视图 + `/health` + 自启脚本 + **接入器（detect/attach/detach，OpenCode/Codex/通用三适配器）** | 开关一拨 Agent 即走网关；detach 后配置逐字节还原 |
| M5 ✅ | 性能打磨 + 三平台打包 + CI（含 mock provider 与适配器 schema 测试） | 空闲 CPU<0.5%，内存<30MB |
| v1.1 | Anthropic Messages 适配（含 Claude Code 接入）、smart 调度、model_map、订阅 token 接入 | — |

#### 11. 开发约束（抄进 issue / PR 模板）

- 禁止引入 GUI 框架、WebView、插件系统、数据库 ORM、前端框架。
- 流式转发零缓冲；禁止完整响应入内存；唯一例外是首 chunk peek。
- 配置校验失败必须拒绝启动，报错带文件名与行号；重载失败不得影响进行中请求。
- 接入器只写适配器白名单文件；写入必备份、幂等、可逆；解析失败拒绝写入。
- 网关运行期永不主动改写 Agent 配置，改写只由显式 CLI/UI 操作触发。
- Key 不进日志、不进 Agent 配置；错误信息脱敏。
- 新增字段先写校验与默认值，保持向后兼容。
- 字段名以本文档为准，不与上游 Magpie 混用。
- 必须提供 mock provider 用于 failover 单测与 CI。

#### 12. 已知风险

- 额度估算不准（刻意取舍，靠保守系数 + 被动校准缓解）。
- 部分供应商的 model 名在各平台不一致，需要 `model_map` 映射层（v1.1）。
- 网关本身是单点：需 `/health`、崩溃自启、启动自检（连通性失败即告警）。
- **Agent 配置格式是上游私有 API**：改版可能破坏适配器 → schema 测试 + 解析失败拒写 + 备份可逆 + 漂移检测兜底。
- 路径前缀要求 Agent 支持自定义 base_url（OpenAI-compatible Agent 普遍支持；不支持者只能走通用环境变量模板）。
- 名称检索性差：仓库/包名统一用 `agent-buffet` / `buffetd`，README 首屏写明中文名。
