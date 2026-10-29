//! M4 接入器（设计文档 §7）：detect / plan / apply / revert + 漂移检测。
//! 硬约束：
//! - 只写适配器白名单内的文件路径；文件解析失败 → 拒绝写入，绝不"尽力而为"。
//! - 写入前备份 `<file>.before-agentbuffet`（已存在则不覆盖）；revert 优先从备份恢复（逐字节），
//!   备份丢失时精确删除注入块。
//! - 幂等：重复 attach 只更新注入块，不叠加；同名 provider 被用户自定义过 → 拒绝。
//! - 永不写入真实 API Key：apiKey 一律填占位符，真实 Key 只存在于网关进程环境变量。
//! - 运行期只读做漂移检测，永不主动写——改写只发生在显式 CLI/UI 操作（本模块由调用方触发）。
//! - 多环境隔离（参照 Magpie）：agent id 带 `@scope` 后缀即表示某个运行环境里的实例
//!   （`hermes` = 本机，`hermes@wsl:Ubuntu` = WSL 发行版），配置各写各的，互不覆盖。

use crate::scope;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const PROVIDER_NAME: &str = "agentbuffet";
pub const KEY_PLACEHOLDER: &str = "agentbuffet";
pub const BACKUP_SUFFIX: &str = ".before-agentbuffet";

/// Codex 注入块标记（文本手术定位用）。
const CX_TABLE_BEGIN: &str = "# >>> agentbuffet:begin";
const CX_TABLE_END: &str = "# <<< agentbuffet:end";
const CX_DEF_BEGIN: &str = "# >>> agentbuffet:defaults:begin";
const CX_DEF_END: &str = "# <<< agentbuffet:defaults:end";

/// 用户主目录：Windows 优先 USERPROFILE，Unix 优先 HOME；两者皆缺失返回 None
/// （调用方据此拒绝把配置写进程序/exe 目录，避免升级覆盖）。
pub fn home_dir_opt() -> Option<PathBuf> {
    let (first, second) = if cfg!(windows) {
        ("USERPROFILE", "HOME")
    } else {
        ("HOME", "USERPROFILE")
    };
    std::env::var_os(first)
        .or_else(|| std::env::var_os(second))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 隔离实例的主目录覆盖环境变量。
///
/// **测试与验证实例必须设置它**：`AGENTBUFFET_CONFIG` 只隔离网关自己的配置，
/// 不隔离 Agent 工具的本地配置（`~/.codex/config.toml`、`~/.hermes/config.yaml`、
/// WSL 里的对应文件）。不设的话，隔离实例的 attach 会直接改写用户真实配置——
/// 端口号也会跟着写错，表现为"接入后一直漂移"。
pub const ENV_AGENT_HOME: &str = "AGENTBUFFET_AGENT_HOME";

/// 进程内隔离模式的主目录（测试用）。
///
/// 用 `OnceLock` 而不是环境变量：Rust 测试默认多线程并行，环境变量的
/// set/读跨线程没有同步保证——曾因此让测试在隔离开关"还没生效"的窗口里
/// 枚举到真实 WSL scope，并把测试端口写进了用户真实的配置。
static ISOLATED_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// 当前是否处于隔离模式（接入器主目录被覆盖）。
pub fn agent_home_isolated() -> bool {
    ISOLATED_HOME.get().is_some() || std::env::var_os(ENV_AGENT_HOME).is_some_and(|v| !v.is_empty())
}

/// 进程级一次性开启隔离模式（测试专用）。
///
/// 开启后：scope 枚举不再包含 WSL / Windows 侧的真实用户目录，`home_dir()` 指向
/// 一个临时目录。测试进程因此既不会改写用户真实配置，也不会读到它们——
///
/// 没有这层隔离时，测试里对 `hermes@wsl:Ubuntu` 调 `apply` 会**直接写入用户
/// 真实的 WSL 配置**（scope 枚举读的是注册表，与传入的 home 无关）。
///
/// 进程内全局生效（`OnceLock`，无跨线程竞态）；测试可并行调用。
pub fn enable_isolation_for_tests() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join("agentbuffet-isolated-home");
        let _ = std::fs::create_dir_all(&dir);
        let _ = ISOLATED_HOME.set(dir);
    });
}

/// 用户主目录（适配器配置文件所在）。
pub fn home_dir() -> PathBuf {
    if let Some(d) = ISOLATED_HOME.get() {
        return d.clone();
    }
    if let Some(v) = std::env::var_os(ENV_AGENT_HOME) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home_dir_opt().unwrap_or_else(|| PathBuf::from("."))
}

/// 注入目标：由网关配置快照构造。
#[derive(Debug, Clone)]
pub struct Target {
    pub agent_id: String,
    /// 默认模型指向的组（agent.default_group 或 --group 覆盖）。
    pub default_group: String,
    /// 全部路由组名（已排序），注册为 `group/<name>` 虚拟模型。
    pub groups: Vec<String>,
    /// 所接入组内可路由的真实模型 ID（已排序去重），注入为可直接选择的模型。
    pub models: Vec<String>,
    /// 网关监听地址，如 `127.0.0.1:3425`。
    pub gateway_addr: String,
    /// 显式适配器提示（opencode/codex/hermes/claude-code/generic）；空则按 agent_id 推断。
    pub adapter: String,
}

impl Target {
    /// 注入到 Agent 配置里的网关地址。
    ///
    /// 路径前缀承载 agent id（`/<agent-id>/v1`），因此 WSL 里的实例与本机实例
    /// 在网关侧天然分开统计；WSL 里访问网关的主机地址按其网络模式修正。
    pub fn base_url(&self) -> String {
        let addr = scope::gateway_addr_for(&self.agent_id, &self.gateway_addr, &home_dir());
        format!("http://{}/{}/v1", addr, self.agent_id)
    }
    pub fn model_pointer(&self) -> String {
        format!("{PROVIDER_NAME}/group/{}", self.default_group)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    OpenCode,
    Codex,
    Hermes,
    Generic,
    ClaudeCode,
}

impl Adapter {
    pub fn name(self) -> &'static str {
        match self {
            Adapter::OpenCode => "opencode",
            Adapter::Codex => "codex",
            Adapter::Hermes => "hermes",
            Adapter::Generic => "generic",
            Adapter::ClaudeCode => "claude-code",
        }
    }

    /// agent id → 适配器。已知名字用专属适配器，其余走通用（只打印模板）。
    /// 优先按显式提示解析适配器，支持多实例（如 opencode-work 复用 OpenCode 适配器）。
    /// id 里的 `@scope` 后缀（多环境实例）不影响适配器选择。
    pub fn resolve(agent_id: &str, hint: &str) -> Adapter {
        match hint {
            "opencode" => Adapter::OpenCode,
            "codex" => Adapter::Codex,
            "hermes" => Adapter::Hermes,
            "claude-code" => Adapter::ClaudeCode,
            "generic" => Adapter::Generic,
            _ => Adapter::for_agent(agent_id),
        }
    }

    pub fn for_agent(agent_id: &str) -> Adapter {
        match scope::kind_of(agent_id) {
            "opencode" => Adapter::OpenCode,
            "codex" => Adapter::Codex,
            "hermes" => Adapter::Hermes,
            "claude-code" => Adapter::ClaudeCode,
            _ => Adapter::Generic,
        }
    }

    /// 适配器白名单文件路径（Generic 无文件）。
    pub fn config_path(self, home: &Path, agent_id: &str) -> Option<PathBuf> {
        match self {
            Adapter::OpenCode => {
                // 多实例：每个实例一个配置文件；环境（scope）不进文件名——
                // 不同环境的主目录本就不同，同名文件不会撞车。
                let fname = {
                    let base = scope::kind_of(agent_id);
                    if base == "opencode" {
                        "opencode.json".to_string()
                    } else {
                        let safe: String = base
                            .chars()
                            .map(|c| {
                                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                                    c
                                } else {
                                    '-'
                                }
                            })
                            .collect();
                        format!("{safe}.json")
                    }
                };
                Some(home.join(".config").join("opencode").join(fname))
            }
            Adapter::Codex => Some(home.join(".codex").join("config.toml")),
            Adapter::Hermes => Some(home.join(".hermes").join("config.yaml")),
            Adapter::ClaudeCode => Some(home.join(".claude.json")),
            Adapter::Generic => None,
        }
    }
}

/// 解析该 Agent 实际所在的主目录。
///
/// - id 带 `@scope` 后缀（多环境实例，如 `hermes@wsl:Ubuntu`）→ 该环境的主目录，
///   与本机实例各写各的，互不覆盖。
/// - 本机实例 → 本机主目录。**不再回落到 WSL**：回落会把两套环境混成一套，
///   这正是多环境隔离要消灭的问题（WSL 里的实例请用 `hermes@wsl:<发行版>` 接入）。
pub fn resolve_home(home: &Path, agent_id: &str, adapter: &str) -> PathBuf {
    let _ = adapter;
    match scope::scope_of(agent_id) {
        scope::NATIVE => home.to_path_buf(),
        sid => scope::home_for_scope(sid, home).unwrap_or_else(|| home.to_path_buf()),
    }
}

/// 同上，但环境不存在（发行版已停止/卸载）时报错——写操作绝不能落到别的环境去。
fn scope_home(home: &Path, agent_id: &str) -> Result<PathBuf, String> {
    let sid = scope::scope_of(agent_id);
    if sid == scope::NATIVE {
        return Ok(home.to_path_buf());
    }
    // 隔离模式下根本没有"别的环境"：非 native scope 一律拒绝写入。
    // 这是硬保险——即使 scope 枚举被缓存或绕过，也不会碰到用户真实配置。
    if agent_home_isolated() {
        return Err(format!(
            "隔离模式（{ENV_AGENT_HOME}）下不处理 {sid} 实例，已拒绝写入"
        ));
    }
    scope::home_for_scope(sid, home).ok_or_else(|| {
        format!("未找到运行环境 {sid}（WSL 发行版可能已停止或卸载），请先启动该发行版后重试")
    })
}

/// 已知可检测的本机 Agent（接入器视图快速接入）。
pub const KNOWN_AGENTS: &[&str] = &["opencode", "codex", "hermes", "claude-code"];

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub agent_id: String,
    pub adapter: &'static str,
    /// 所在运行环境 id（`native` / `wsl:Ubuntu`）。
    pub scope_id: String,
    /// 运行环境展示名（本机 (Windows) / WSL · Ubuntu）。
    pub scope_label: String,
    /// 运行环境类型：`native` / `wsl`。
    pub scope_kind: String,
    pub config_path: Option<String>,
    pub installed: bool,
    pub attached: bool,
    pub drifted: bool,
    pub note: String,
}

/// 检测到的一个 Agent 实例（某个环境里的某个工具）。
#[derive(Debug, Clone, Serialize)]
pub struct DetectedAgent {
    /// 实例 id：本机 `hermes`，WSL `hermes@wsl:Ubuntu`。
    pub agent_id: String,
    /// 工具类型：`hermes` / `opencode` / `codex` / `claude-code`。
    pub kind: String,
    pub adapter: &'static str,
    pub scope_id: String,
    pub scope_label: String,
    pub scope_kind: String,
    pub config_path: Option<String>,
    pub installed: bool,
    pub attached: bool,
    pub drifted: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub agent_id: String,
    pub adapter: &'static str,
    /// 将写入/已写入的文件与 diff 预览（plan 不落盘，apply 落盘）。
    pub files: Vec<FileChange>,
    /// 通用适配器：环境变量模板（不写文件）。
    pub env_template: Option<String>,
    pub note: String,
}

const APPLIED_NOTE: &str =
    "已注入 agentbuffet provider（Key 为占位符）；请重启该 Agent 并新建会话生效";
const DRY_NOTE: &str = "dry-run：以下为将写入的变更预览，未落盘";

// ---------------------------------------------------------------- 公共入口

/// 检测单个 agent 的适配器状态（只读）。
pub fn detect(home: &Path, target: &Target) -> AgentStatus {
    let a = Adapter::resolve(&target.agent_id, &target.adapter);
    let scope_id = scope::scope_of(&target.agent_id).to_string();
    let scopes = scope::enumerate_cached(home);
    let sc = scope::scope_by_id(&scopes, &scope_id);
    let scope_label = sc.map(|s| s.label()).unwrap_or_else(|| scope_id.clone());
    let scope_kind = sc.map(|s| s.kind.as_str()).unwrap_or("native").to_string();
    let home = &resolve_home(home, &target.agent_id, &target.adapter);
    let path = a.config_path(home, &target.agent_id);
    let cp = path.as_ref().map(|p| p.display().to_string());
    let mut s = AgentStatus {
        agent_id: target.agent_id.clone(),
        adapter: a.name(),
        scope_id,
        scope_label: scope_label.clone(),
        scope_kind,
        config_path: cp,
        installed: false,
        attached: false,
        drifted: false,
        note: String::new(),
    };
    if scope::scope_of(&target.agent_id) != scope::NATIVE && sc.is_none() {
        s.note = "未找到该运行环境（WSL 发行版可能已停止或卸载）".into();
        return s;
    }
    match a {
        Adapter::Generic => {
            s.note = "通用 OpenAI-compatible：attach 仅打印环境变量模板，不写文件".into();
        }
        Adapter::ClaudeCode => {
            s.installed = path.map(|p| p.exists()).unwrap_or(false);
            s.note = "Anthropic 协议，v1 暂不支持（计划 v1.1 协议适配器接入）".into();
        }
        Adapter::OpenCode => {
            let Some(p) = path else { return s };
            let Ok(text) = std::fs::read_to_string(&p) else {
                s.note = "未检测到 OpenCode 配置文件".into();
                return s;
            };
            let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
                s.note = "配置文件存在但 JSON 解析失败".into();
                return s;
            };
            s.installed = true;
            s.attached = oc_has_ours(&doc, target);
            s.drifted = s.attached && oc_drifted(&doc, target);
            if s.drifted {
                s.note = "已接入但配置与当前网关不一致（漂移），重新 attach 可修复".into();
            }
        }
        Adapter::Codex => {
            let Some(p) = path else { return s };
            let Ok(text) = std::fs::read_to_string(&p) else {
                s.note = "未检测到 Codex 配置文件".into();
                return s;
            };
            let Ok(parsed) = text.parse::<toml::Value>() else {
                s.note = "配置文件存在但 TOML 解析失败".into();
                return s;
            };
            s.installed = true;
            s.attached = cx_has_ours(&text, &parsed, target);
            s.drifted = s.attached && cx_drifted(&text, &parsed, target);
            if s.drifted {
                s.note = "已接入但注入块与当前网关不一致（漂移），重新 attach 可修复".into();
            }
        }
        Adapter::Hermes => {
            let Some(p) = path else { return s };
            let mut locs: Vec<String> = Vec::new();
            if let Some(dir) = p.parent() {
                if dir.is_dir() {
                    locs.push(format!("{}：{}", scope_label, dir.display()));
                }
            }
            s.installed = !locs.is_empty();
            if let Ok(text) = std::fs::read_to_string(&p) {
                s.attached = hm_has_ours(&text);
                s.drifted = s.attached && hm_drifted(&text, target);
            }
            s.note = if s.drifted {
                "已接入但 base_url 与当前网关不一致（漂移），重新 attach 可修复".into()
            } else {
                locs.join("；")
            };
        }
    }
    s
}

/// 扫描全部环境（本机 + 各 WSL 发行版）下已安装的 Agent 实例。
///
/// 每个实例独立成条：`hermes`（本机）与 `hermes@wsl:Ubuntu`（WSL）是两个条目，
/// 各自的配置文件、接入状态、漂移状态互不相干。
pub fn detect_all(home: &Path, snap: &crate::config::Snapshot) -> Vec<DetectedAgent> {
    let scopes = scope::enumerate_cached(home);
    let gw = snap.gateway_addr.to_string();
    let mut gnames: Vec<&String> = snap.groups.keys().collect();
    gnames.sort();
    let groups: Vec<String> = gnames.iter().map(|s| (*s).clone()).collect();
    let mut out = Vec::new();
    for sc in &scopes {
        for kind in KNOWN_AGENTS {
            let agent_id = scope::agent_id_for(kind, &sc.id);
            // 默认组必须与巡检修复、CLI/UI 接入用的是同一套规则，否则会出现
            // "检测说漂移、巡检说没漂移"的自相矛盾。
            let default_group = snap.default_group_for(&agent_id);
            let target = Target {
                agent_id: agent_id.clone(),
                default_group: default_group.clone(),
                groups: groups.clone(),
                models: crate::routable_models(snap, &default_group),
                gateway_addr: gw.clone(),
                adapter: (*kind).to_string(),
            };
            let st = detect(home, &target);
            if !st.installed {
                continue;
            }
            out.push(DetectedAgent {
                agent_id: st.agent_id,
                kind: (*kind).to_string(),
                adapter: st.adapter,
                scope_id: st.scope_id,
                scope_label: st.scope_label,
                scope_kind: st.scope_kind,
                config_path: st.config_path,
                installed: st.installed,
                attached: st.attached,
                drifted: st.drifted,
                note: st.note,
            });
        }
    }
    out
}

/// dry-run：输出将修改的文件与 diff 预览，不落盘（§7 plan）。
pub fn plan(home: &Path, target: &Target) -> Result<ApplyResult, String> {
    let home = &scope_home(home, &target.agent_id)?;
    let a = Adapter::resolve(&target.agent_id, &target.adapter);
    match a {
        Adapter::Generic => Ok(generic_result(
            target,
            "dry-run 与实际 attach 相同：不落盘，仅打印模板",
        )),
        Adapter::ClaudeCode => {
            Err("Claude Code 使用 Anthropic 协议，v1 暂不支持接入（计划 v1.1）".into())
        }
        Adapter::OpenCode => {
            let (path, text) = oc_read(home, target)?;
            let doc = oc_parse(&text, &path)?;
            let new_text = oc_render(&doc, target)?;
            Ok(oc_result(target, &path, &text, &new_text, DRY_NOTE))
        }
        Adapter::Codex => {
            let (path, text) = cx_read(home, target)?;
            let parsed = cx_parse(&text, &path)?;
            let (new_text, note) = cx_render(&text, &parsed, target)?;
            Ok(cx_result(target, &path, &text, &new_text, DRY_NOTE, note))
        }
        Adapter::Hermes => {
            let (path, text) = hm_read(home, target)?;
            let new_text = hm_render(&text, target)?;
            Ok(hm_result(target, &path, &text, &new_text, DRY_NOTE))
        }
    }
}

/// 实际写入：备份 → 注入（幂等）。
pub fn apply(home: &Path, target: &Target) -> Result<ApplyResult, String> {
    let home = &scope_home(home, &target.agent_id)?;
    let a = Adapter::resolve(&target.agent_id, &target.adapter);
    match a {
        Adapter::Generic => Ok(generic_result(target, APPLIED_NOTE)),
        Adapter::ClaudeCode => {
            Err("Claude Code 使用 Anthropic 协议，v1 暂不支持接入（计划 v1.1）".into())
        }
        Adapter::OpenCode => {
            let (path, text) = oc_read(home, target)?;
            let doc = oc_parse(&text, &path)?;
            let new_text = oc_render(&doc, target)?;
            if new_text == text {
                return Ok(oc_result(
                    target,
                    &path,
                    &text,
                    &new_text,
                    "已接入且配置一致，无需变更",
                ));
            }
            backup(&path)?;
            std::fs::write(&path, &new_text)
                .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
            Ok(oc_result(target, &path, &text, &new_text, APPLIED_NOTE))
        }
        Adapter::Codex => {
            let (path, text) = cx_read(home, target)?;
            let parsed = cx_parse(&text, &path)?;
            let (new_text, note) = cx_render(&text, &parsed, target)?;
            if new_text == text {
                return Ok(cx_result(
                    target,
                    &path,
                    &text,
                    &new_text,
                    "已接入且配置一致，无需变更",
                    note,
                ));
            }
            backup(&path)?;
            std::fs::write(&path, &new_text)
                .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
            Ok(cx_result(
                target,
                &path,
                &text,
                &new_text,
                APPLIED_NOTE,
                note,
            ))
        }
        Adapter::Hermes => {
            let (path, text) = hm_read(home, target)?;
            let new_text = hm_render(&text, target)?;
            if new_text == text {
                return Ok(hm_result(
                    target,
                    &path,
                    &text,
                    &new_text,
                    "已接入且配置一致，无需变更",
                ));
            }
            backup(&path)?;
            std::fs::write(&path, &new_text)
                .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
            Ok(hm_result(target, &path, &text, &new_text, APPLIED_NOTE))
        }
    }
}

/// 还原：优先从备份恢复（逐字节），备份丢失时精确删除注入块。
pub fn revert(home: &Path, agent_id: &str, hint: &str) -> Result<ApplyResult, String> {
    let a = Adapter::resolve(agent_id, hint);
    // 多环境实例：还原的是该环境里的那份配置（WSL 实例还原 WSL 里的文件）
    let home = scope_home(home, agent_id)?;
    let home = home.as_path();
    match a {
        Adapter::Generic => Ok(ApplyResult {
            agent_id: agent_id.into(),
            adapter: a.name(),
            files: Vec::new(),
            env_template: None,
            note: "通用适配器未写过文件，无需还原".into(),
        }),
        Adapter::ClaudeCode => Err("Claude Code v1 从未被接入，无需还原".into()),
        Adapter::Hermes => {
            let path = Adapter::Hermes
                .config_path(home, agent_id)
                .expect("hermes whitelist path");
            if !path.exists() {
                return Err(format!("配置文件 {} 不存在", path.display()));
            }
            let bp = backup_path(&path);
            if !bp.exists() {
                return Err(
                    "备份文件缺失，无法安全还原 Hermes 配置；请在 Hermes 中手动将 model 块改回原服务商"
                        .into(),
                );
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
            let orig = String::from_utf8_lossy(
                &std::fs::read(&bp).map_err(|e| format!("读取备份失败: {e}"))?,
            )
            .into_owned();
            std::fs::write(&path, &orig).map_err(|e| format!("写入失败: {e}"))?;
            Ok(ApplyResult {
                agent_id: agent_id.into(),
                adapter: Adapter::Hermes.name(),
                files: vec![FileChange {
                    path: path.display().to_string(),
                    preview: line_preview(&text, &orig),
                }],
                env_template: None,
                note: "已从备份逐字节还原；请重启该 Agent 并新建会话生效".into(),
            })
        }
        Adapter::OpenCode | Adapter::Codex => {
            let path = a
                .config_path(home, agent_id)
                .expect("adapter whitelist path");
            if !path.exists() {
                return Err(format!("配置文件 {} 不存在", path.display()));
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
            let bp = backup_path(&path);
            let (new_text, note) = if bp.exists() {
                let orig = std::fs::read(&bp).map_err(|e| format!("读取备份失败: {e}"))?;
                let orig = String::from_utf8_lossy(&orig).into_owned();
                (orig, "已从备份逐字节还原".to_string())
            } else {
                match a {
                    Adapter::OpenCode => {
                        let mut doc = oc_parse(&text, &path)?;
                        if !remove_oc_provider(&mut doc)? {
                            return Ok(ApplyResult {
                                agent_id: agent_id.into(),
                                adapter: a.name(),
                                files: Vec::new(),
                                env_template: None,
                                note: "未发现 agentbuffet 注入块，无需还原".into(),
                            });
                        }
                        let nt =
                            serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())? + "\n";
                        (
                            nt,
                            "备份丢失：仅删除注入的 provider，其余字段保持现状".into(),
                        )
                    }
                    Adapter::Codex => {
                        let nt = remove_region(
                            &remove_region(&text, CX_DEF_BEGIN, CX_DEF_END),
                            CX_TABLE_BEGIN,
                            CX_TABLE_END,
                        );
                        if nt == text {
                            return Ok(ApplyResult {
                                agent_id: agent_id.into(),
                                adapter: a.name(),
                                files: Vec::new(),
                                env_template: None,
                                note: "未发现 agentbuffet 注入块，无需还原".into(),
                            });
                        }
                        (nt, "备份丢失：已精确删除注入块".into())
                    }
                    _ => unreachable!(),
                }
            };
            std::fs::write(&path, &new_text).map_err(|e| format!("写入失败: {e}"))?;
            Ok(ApplyResult {
                agent_id: agent_id.into(),
                adapter: a.name(),
                files: vec![FileChange {
                    path: path.display().to_string(),
                    preview: line_preview(&text, &new_text),
                }],
                env_template: None,
                note: format!("{note}；请重启该 Agent 并新建会话生效"),
            })
        }
    }
}

/// 全部 agent 的批量状态（CLI status / UI Agents 视图）。
pub fn status_all(home: &Path, targets: &[Target]) -> Vec<AgentStatus> {
    targets.iter().map(|t| detect(home, t)).collect()
}

// ---------------------------------------------------------------- 旧版 id 迁移

/// 主目录字符串归一（跨版本比较用）：统一分隔符与大小写、折叠 `\\wsl$\\` 到
/// `\\wsl.localhost\\`、去掉末尾分隔符。Windows 路径大小写不敏感，统一转小写。
fn norm_home(s: &str) -> String {
    let mut t: String = s
        .trim()
        .chars()
        .map(|c| if c == '/' { '\\' } else { c })
        .collect();
    while t.len() > 1 && t.ends_with('\\') {
        t.pop();
    }
    t.make_ascii_lowercase();
    t.replace("\\\\wsl$\\", "\\\\wsl.localhost\\")
}

/// 旧版 dash 风格 id（`hermes-wsl-ubuntu-24-04-yxf`）→ 基础名。
/// env 短名取 `wsl` / `win` / `local`；基础名自身可含 `-`（如 claude-code），
/// 所以按**最后**出现的环境标记切。
fn legacy_base(id: &str) -> Option<&str> {
    for m in ["-wsl-", "-win-", "-local-"] {
        if let Some((base, _)) = id.rsplit_once(m) {
            if !base.is_empty() {
                return Some(base);
            }
        }
    }
    None
}

/// 把配置里旧版（v0.2.16）遗留的 agent 条目迁移成新的 `<kind>@<scope>` 编码。
///
/// 旧版把 WSL / Windows 侧实例登记成 `hermes-wsl-ubuntu-24-04-yxf` 这类 dash id，
/// 并用 `home` 字段记录真实主目录。新版 scope 解析只认 `@` 后缀——旧 id 会被当成
/// **本机实例**，于是 WSL 的记录显示、读写全落到 Windows 侧：这正是
/// "接入 Windows 的 Hermes 却把 WSL 那份也算已接入、删了还提示已接入"的根因。
///
/// 迁移规则（按 `home` 字段匹配当前枚举到的环境）：
/// - 没有 `home` 字段的条目不动（裸名即本机实例，新旧编码一致）；
/// - `home` 匹配不到任何环境（发行版没启动）→ 原样保留，绝不瞎猜；
/// - 匹配到 native → 只清掉 `home` 字段；
/// - 新 id 与既有条目冲突 → 删除旧条目（以新编码那条为准）；
/// - 其余 → 改写成 `<base>@<scope id>`，base 优先取 `adapter` 字段。
///
/// 有变更才回写，且写回前整份重校验，校验不过不落盘。返回 (旧 id, 新 id) 列表，
/// 新 id 等于旧 id 表示只清了字段。注入文件里的旧 base_url 路径会随后被
/// 漂移巡检自动改写，无需在此处理。
pub fn migrate_legacy(path: &std::path::Path, scopes: &[scope::Scope]) -> Vec<(String, String)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let Ok(mut doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return Vec::new();
    };
    let mut renames: Vec<(String, String)> = Vec::new();
    let mut changed = false;
    {
        let Some(root) = doc.as_mapping_mut() else {
            return renames;
        };
        let Some(agents) = root
            .get_mut(serde_yaml::Value::String("agents".into()))
            .and_then(|v| v.as_sequence_mut())
        else {
            return renames;
        };
        let existing: Vec<String> = agents
            .iter()
            .filter_map(|a| a.get("id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let mut drop: Vec<usize> = Vec::new();
        for (i, entry) in agents.iter_mut().enumerate() {
            let Some(id) = entry.get("id").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            if id.contains('@') {
                continue; // 已是新编码
            }
            let Some(home_raw) = entry.get("home").and_then(|v| v.as_str()) else {
                continue; // 无 home：裸名本机实例，新旧编码一致，不动
            };
            let want = norm_home(home_raw);
            let Some(sc) = scopes
                .iter()
                .find(|s| norm_home(&s.home.display().to_string()) == want)
            else {
                continue; // 环境匹配不上（发行版没启动），保守不动
            };
            let base = entry
                .get("adapter")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .or_else(|| legacy_base(&id).map(String::from))
                .unwrap_or_else(|| id.clone());
            let new_id = scope::agent_id_for(&base, &sc.id);
            if new_id == id {
                // native 实例：只清 home 字段
                if let Some(m) = entry.as_mapping_mut() {
                    m.remove(serde_yaml::Value::String("home".into()));
                    renames.push((id.clone(), new_id));
                    changed = true;
                }
                continue;
            }
            if existing.iter().any(|e| *e == new_id) {
                drop.push(i); // 已有新编码条目，旧的是重复登记
                renames.push((id, new_id));
                changed = true;
                continue;
            }
            if let Some(m) = entry.as_mapping_mut() {
                m.insert(
                    serde_yaml::Value::String("id".into()),
                    serde_yaml::Value::String(new_id.clone()),
                );
                m.remove(serde_yaml::Value::String("home".into()));
                renames.push((id, new_id));
                changed = true;
            }
        }
        for i in drop.iter().rev() {
            agents.remove(*i);
        }
    }
    if !changed {
        return renames;
    }
    let out = match serde_yaml::to_string(&doc) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let filename = path.display().to_string();
    if crate::config::load_str(&out, &filename).is_err() {
        return Vec::new(); // 校验不过绝不落盘
    }
    if std::fs::write(path, out).is_err() {
        return Vec::new();
    }
    renames
}

/// 按当前枚举到的环境执行迁移（启动 / 热重载入口用）。
pub fn migrate_legacy_at(path: &std::path::Path, native_home: &Path) -> Vec<(String, String)> {
    migrate_legacy(path, &scope::enumerate_cached(native_home))
}

// ---------------------------------------------------------------- 备份

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), BACKUP_SUFFIX))
}

/// 备份原文件；已存在则不覆盖（§7 apply）。
fn backup(path: &Path) -> Result<(), String> {
    let bp = backup_path(path);
    if bp.exists() {
        return Ok(());
    }
    std::fs::copy(path, &bp).map_err(|e| format!("备份 {} 失败: {e}", bp.display()))?;
    Ok(())
}

// ---------------------------------------------------------------- 通用适配器

fn env_template(target: &Target) -> String {
    format!(
        "export OPENAI_BASE_URL=\"{}\"\nexport OPENAI_API_KEY=\"{KEY_PLACEHOLDER}\"",
        target.base_url()
    )
}

fn generic_result(target: &Target, note: &str) -> ApplyResult {
    ApplyResult {
        agent_id: target.agent_id.clone(),
        adapter: Adapter::Generic.name(),
        files: Vec::new(),
        env_template: Some(env_template(target)),
        note: format!("{note}；真实 Key 只存在于网关进程，占位符即可"),
    }
}

// ---------------------------------------------------------------- OpenCode（JSON）

fn oc_read(home: &Path, target: &Target) -> Result<(PathBuf, String), String> {
    let path = Adapter::OpenCode
        .config_path(home, &target.agent_id)
        .expect("opencode whitelist path");
    // 多实例：独立文件缺失时，从主配置播种，避免要求用户手建文件；
    // 但"主实例"（opencode）本身缺失 → 说明该环境没装 OpenCode，拒绝凭空造文件。
    let base = scope::kind_of(&target.agent_id);
    if !path.exists() {
        let main = home.join(".config").join("opencode").join("opencode.json");
        if base == "opencode" {
            return Err(format!(
                "未检测到 OpenCode 配置文件 {}，请先在该环境安装 OpenCode",
                path.display()
            ));
        }
        let Ok(seed) = std::fs::read_to_string(&main) else {
            return Err(format!(
                "未检测到 OpenCode 配置文件 {}，请先在该环境安装 OpenCode",
                path.display()
            ));
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, seed).map_err(|e| format!("创建 {} 失败: {e}", path.display()))?;
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    Ok((path, text))
}

fn oc_parse(text: &str, path: &Path) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| format!("解析 {} 失败: {e}；拒绝写入", path.display()))
}

fn oc_expected_provider(target: &Target) -> serde_json::Value {
    let mut models = serde_json::Map::new();
    for g in &target.groups {
        models.insert(format!("group/{g}"), serde_json::json!({ "name": g }));
    }
    // OpenCode 本地会拒绝未声明的 model ID，必须把组内真实可路由模型一并枚举。
    for m in &target.models {
        models
            .entry(m.clone())
            .or_insert_with(|| serde_json::json!({ "name": m }));
    }
    serde_json::json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "AgentBuffet",
        "options": {
            "baseURL": target.base_url(),
            "apiKey": KEY_PLACEHOLDER
        },
        "models": models
    })
}

/// 注入块是否属于我们：provider.agentbuffet 具备我们的特征（npm + 占位 Key）。
/// 与网关地址无关——地址变了只算"漂移"，不算"未接入"。
fn oc_has_ours(doc: &serde_json::Value, _target: &Target) -> bool {
    let Some(cur) = doc.pointer(&format!("/provider/{PROVIDER_NAME}")) else {
        return false;
    };
    cur.get("npm").and_then(|v| v.as_str()) == Some("@ai-sdk/openai-compatible")
        && cur.pointer("/options/apiKey").and_then(|v| v.as_str()) == Some(KEY_PLACEHOLDER)
}

fn oc_drifted(doc: &serde_json::Value, target: &Target) -> bool {
    let expect_provider = oc_expected_provider(target);
    let cur = doc.pointer(&format!("/provider/{PROVIDER_NAME}"));
    cur != Some(&expect_provider)
        || doc.get("model").and_then(|v| v.as_str()) != Some(target.model_pointer().as_str())
}

/// 渲染注入后的完整 JSON 文本（幂等：同名 provider 整体替换）。
fn oc_render(doc: &serde_json::Value, target: &Target) -> Result<String, String> {
    let mut doc = doc.clone();
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| "配置文件不是 JSON 对象，拒绝写入".to_string())?;
    let providers = obj
        .entry("provider")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let pv = providers
        .as_object_mut()
        .ok_or_else(|| "provider 字段不是对象，拒绝写入".to_string())?;
    let expected = oc_expected_provider(target);
    if let Some(cur) = pv.get(PROVIDER_NAME) {
        let ours = cur.get("npm").and_then(|v| v.as_str()) == Some("@ai-sdk/openai-compatible")
            && cur.pointer("/options/apiKey").and_then(|v| v.as_str()) == Some(KEY_PLACEHOLDER);
        if !ours {
            return Err(format!(
                "provider \"{PROVIDER_NAME}\" 已被用户自定义（baseURL 非本网关），拒绝写入；请改名或删除后重试"
            ));
        }
    }
    pv.insert(PROVIDER_NAME.into(), expected);
    obj.insert("model".into(), serde_json::json!(target.model_pointer()));
    serde_json::to_string_pretty(&doc)
        .map_err(|e| e.to_string())
        .map(|s| s + "\n")
}

/// 备份丢失时删除注入 provider；仅当结构特征属于我们（npm + 占位 Key）才删，
/// 用户自定义的同名 provider 一律不动。
fn remove_oc_provider(doc: &mut serde_json::Value) -> Result<bool, String> {
    let Some(obj) = doc.as_object_mut() else {
        return Err("配置文件不是 JSON 对象".into());
    };
    let Some(pv) = obj.get_mut("provider").and_then(|v| v.as_object_mut()) else {
        return Ok(false);
    };
    let ours = pv
        .get(PROVIDER_NAME)
        .map(|cur| {
            cur.get("npm").and_then(|v| v.as_str()) == Some("@ai-sdk/openai-compatible")
                && cur.pointer("/options/apiKey").and_then(|v| v.as_str()) == Some(KEY_PLACEHOLDER)
        })
        .unwrap_or(false);
    if !ours {
        return Ok(false);
    }
    Ok(pv.remove(PROVIDER_NAME).is_some())
}

fn oc_result(target: &Target, path: &Path, old: &str, new: &str, note: &str) -> ApplyResult {
    let base = scope::kind_of(&target.agent_id);
    ApplyResult {
        agent_id: target.agent_id.clone(),
        adapter: Adapter::OpenCode.name(),
        files: vec![FileChange {
            path: path.display().to_string(),
            preview: line_preview(old, new),
        }],
        env_template: None,
        note: if base == "opencode" {
            note.into()
        } else {
            format!(
                "{note}；多实例启动：OPENCODE_CONFIG={} opencode",
                path.display()
            )
        },
    }
}

// ---------------------------------------------------------------- Codex（TOML）

fn cx_read(home: &Path, target: &Target) -> Result<(PathBuf, String), String> {
    let path = Adapter::Codex
        .config_path(home, &target.agent_id)
        .expect("codex whitelist path");
    if !path.exists() {
        return Err(format!(
            "未检测到 Codex 配置文件 {}，请先安装 Codex CLI",
            path.display()
        ));
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    Ok((path, text))
}

fn cx_parse(text: &str, path: &Path) -> Result<toml::Value, String> {
    text.parse::<toml::Value>()
        .map_err(|e| format!("解析 {} 失败: {e}；拒绝写入", path.display()))
}

fn cx_table_block(target: &Target) -> String {
    format!(
        "{CX_TABLE_BEGIN}\n[model_providers.{PROVIDER_NAME}]\nname = \"AgentBuffet\"\nbase_url = \"{}\"\nwire_api = \"chat\"\n{CX_TABLE_END}\n",
        target.base_url()
    )
}

/// 顶层 `model` 键期望值：网关暴露的组虚拟模型 ID（不带 provider 前缀）。
fn cx_want_model(target: &Target) -> String {
    format!("group/{}", target.default_group)
}

/// 顶层键的行号。TOML 规定顶层键必须位于所有表头之前，故"顶层"= 首个 `[table]` 之前的行。
fn cx_top_key_idx(lines: &[&str], key: &str) -> Option<usize> {
    let first_table = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    lines[..first_table].iter().position(|l| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// 顶层键的字符串值。
fn cx_top_value<'a>(parsed: &'a toml::Value, key: &str) -> Option<&'a str> {
    parsed.get(key).and_then(|v| v.as_str())
}

/// 原地改写一个顶层键的值，保留缩进与行尾注释。键不存在时原样返回。
///
/// 这是"AgentBuffet 最高优先级"的落点：用户自设的 `model_provider` / `model`
/// 会被直接改写成 AgentBuffet 的值，而不是像以前那样跳过并让用户手动改。
fn cx_set_top_key(text: &str, key: &str, value: &str) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let Some(i) = cx_top_key_idx(&lines, key) else {
        return (text.to_string(), false);
    };
    let line = lines[i];
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let Some(eq) = line.find('=') else {
        return (text.to_string(), false);
    };
    let lhs = line[indent_len..eq + 1].trim_end(); // "model_provider ="
                                                   // 行尾注释：仅当原值是简单引号字符串时才安全剥离（字符串内部可能含 '#'）。
    let after = line[eq + 1..].trim_start();
    let comment = match after.chars().next() {
        Some(q @ ('"' | '\'')) => after[1..]
            .find(q)
            .and_then(|end| {
                let tail = after[2 + end..].trim();
                tail.starts_with('#').then(|| format!(" {tail}"))
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let mut out: Vec<String> = lines.iter().map(|s| (*s).to_string()).collect();
    out[i] = format!("{indent}{lhs} \"{value}\"{comment}");
    let mut res = out.join("\n");
    if text.ends_with('\n') {
        res.push('\n');
    }
    (res, true)
}

/// 精确移除 defaults 区域（含标记行与其后紧跟的换行），不动文件里其它空行。
fn cx_strip_def_region(text: &str) -> String {
    let (Some(b), Some(e)) = (text.find(CX_DEF_BEGIN), text.find(CX_DEF_END)) else {
        return text.to_string();
    };
    if e < b {
        return text.to_string();
    }
    let mut end = e + CX_DEF_END.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..b]);
    out.push_str(&text[end..]);
    out
}

/// 注入块属于我们：存在我们的表头标记且 provider 表在。
/// 与网关地址无关——地址变了只算"漂移"，不算"未接入"。
fn cx_has_ours(text: &str, parsed: &toml::Value, _target: &Target) -> bool {
    text.contains(CX_TABLE_BEGIN)
        && parsed
            .get("model_providers")
            .and_then(|m| m.get(PROVIDER_NAME))
            .is_some()
}

/// Codex 漂移判定：provider 表块 + 顶层 model_provider/model 都必须指向 AgentBuffet。
///
/// 顶层键是 Codex 真正生效的入口（provider 表只是定义）。只要这两个键被外部改回
/// 别的 provider，就判漂移——巡检会把它抢回来。
fn cx_drifted(text: &str, parsed: &toml::Value, target: &Target) -> bool {
    // 1) provider 表块必须与我们期望的一致
    let want_table = cx_table_block(target);
    if extract_region(text, CX_TABLE_BEGIN, CX_TABLE_END).as_deref() != Some(want_table.trim_end())
    {
        return true;
    }
    // 2) 顶层 model_provider / model 必须指向 AgentBuffet
    cx_top_value(parsed, "model_provider") != Some(PROVIDER_NAME)
        || cx_top_value(parsed, "model") != Some(cx_want_model(target).as_str())
}

/// 渲染注入后的完整文本。返回 (新文本, 附加提示)。
fn cx_render(
    text: &str,
    parsed: &toml::Value,
    target: &Target,
) -> Result<(String, String), String> {
    let has_table = parsed
        .get("model_providers")
        .and_then(|m| m.get(PROVIDER_NAME))
        .is_some();
    let ours = text.contains(CX_TABLE_BEGIN);
    if has_table && !ours {
        return Err(format!(
            "[model_providers.{PROVIDER_NAME}] 已存在但非 AgentBuffet 注入（缺少标记），拒绝写入；请手动删除后重试"
        ));
    }

    // 1) provider 表块：有标记则原位替换，否则追加文末。
    let table = cx_table_block(target);
    let (body, table_applied) = replace_region(text, CX_TABLE_BEGIN, CX_TABLE_END, &table);
    let mut out = body;
    if !table_applied {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&table);
    }

    // 2) 顶层 model_provider / model：AgentBuffet 最高优先级，无条件接管。
    //
    //    以前这里是"检测到用户自定义就跳过"，结果是"已接入但没生效"——Codex 仍在
    //    用用户原来的 provider。现在改为原地改写，原值留在 .before-agentbuffet
    //    备份里，detach 可完整还原。
    let want_model = cx_want_model(target);
    let old_provider = cx_top_value(parsed, "model_provider").map(|s| s.to_string());
    let old_model = cx_top_value(parsed, "model").map(|s| s.to_string());

    // 旧的 defaults 块与顶层键内容重复，会构成 TOML 重复键——接管后必须移除。
    out = cx_strip_def_region(&out);

    let (t1, took_provider) = cx_set_top_key(&out, "model_provider", PROVIDER_NAME);
    let (t2, took_model) = cx_set_top_key(&t1, "model", &want_model);
    out = t2;

    // 缺失的顶层键补在首个表头之前（TOML 顶层键必须先于所有 [table]）。
    let mut missing = String::new();
    if !took_provider {
        missing.push_str(&format!("model_provider = \"{PROVIDER_NAME}\"\n"));
    }
    if !took_model {
        missing.push_str(&format!("model = \"{want_model}\"\n"));
    }
    if !missing.is_empty() {
        out = insert_before_first_table(&out, &missing);
    }

    let mut note = String::new();
    if old_provider.as_deref() != Some(PROVIDER_NAME)
        || old_model.as_deref() != Some(want_model.as_str())
    {
        note = format!(
            "已接管顶层 model_provider/model（原 {} / {}）；AgentBuffet 为最高优先级，外部改回后巡检会自动抢回，detach 可从备份还原",
            old_provider.as_deref().unwrap_or("(无)"),
            old_model.as_deref().unwrap_or("(无)")
        );
    }
    Ok((out, note))
}

/// 把块插到首个表头行之前（顶层键必须位于所有 [table] 之前）；无表头则追加文末。
fn insert_before_first_table(text: &str, block: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let idx = lines.iter().position(|l| l.trim_start().starts_with('['));
    let mut out = String::new();
    match idx {
        Some(i) => {
            for l in &lines[..i] {
                out.push_str(l);
                out.push('\n');
            }
            out.push_str(block);
            // 块自带行尾换行，不再补空行：与 replace_region 的复写输出逐字节一致，
            // 保证重复 apply 幂等（补空行会被第二次复写吞掉，导致每次注入都判定有变更）。
            for l in &lines[i..] {
                out.push_str(l);
                out.push('\n');
            }
        }
        None => {
            out.push_str(text);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
        }
    }
    out
}

/// 替换 begin..end（含标记行）区域；找到返回 (新文本, true)。
fn replace_region(text: &str, begin: &str, end: &str, block: &str) -> (String, bool) {
    match (text.find(begin), text.find(end)) {
        (Some(b), Some(e)) if e >= b => {
            let tail_start = e + end.len();
            // 连同标记行尾的换行一起替换
            let mut out = String::new();
            out.push_str(&text[..b]);
            out.push_str(block.trim_end());
            out.push('\n');
            let rest = &text[tail_start..];
            out.push_str(rest.trim_start_matches('\n'));
            (out, true)
        }
        _ => (text.to_string(), false),
    }
}

/// 删除 begin..end（含标记行）区域。
fn remove_region(text: &str, begin: &str, end: &str) -> String {
    let (t, found) = replace_region(text, begin, end, "");
    if !found {
        return text.to_string();
    }
    // 清理删除后可能留下的连续空行
    let mut collapsed = String::new();
    let mut blank_run = 0usize;
    for line in t.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed
}

/// 提取 begin..end（含标记行）的原文。
fn extract_region(text: &str, begin: &str, end: &str) -> Option<String> {
    let b = text.find(begin)?;
    let e = text.find(end)?;
    if e < b {
        return None;
    }
    Some(text[b..e + end.len()].to_string())
}

fn cx_result(
    target: &Target,
    path: &Path,
    old: &str,
    new: &str,
    note: &str,
    extra_note: String,
) -> ApplyResult {
    let note = if extra_note.is_empty() {
        note.into()
    } else {
        format!("{note}；{extra_note}")
    };
    ApplyResult {
        agent_id: target.agent_id.clone(),
        adapter: Adapter::Codex.name(),
        files: vec![FileChange {
            path: path.display().to_string(),
            preview: line_preview(old, new),
        }],
        env_template: None,
        note,
    }
}

// ---------------------------------------------------------------- diff 预览

/// 极简行级 diff：公共前缀/后缀之外的中段整体替换（预览用途）。
fn line_preview(old: &str, new: &str) -> String {
    if old == new {
        return "(无变更)".into();
    }
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let mut p = 0usize;
    while p < o.len() && p < n.len() && o[p] == n[p] {
        p += 1;
    }
    let mut s = 0usize;
    while s < o.len() - p && s < n.len() - p && o[o.len() - 1 - s] == n[n.len() - 1 - s] {
        s += 1;
    }
    let mut out = String::new();
    for l in &o[p..o.len() - s] {
        out.push_str("- ");
        out.push_str(l);
        out.push('\n');
    }
    for l in &n[p..n.len() - s] {
        out.push_str("+ ");
        out.push_str(l);
        out.push('\n');
    }
    if out.len() > 4000 {
        out.truncate(4000);
        out.push_str("\n…(预览截断)");
    }
    out
}

// ---------------------------------------------------------------- Hermes（YAML）

fn hm_read(home: &Path, target: &Target) -> Result<(PathBuf, String), String> {
    let path = Adapter::Hermes
        .config_path(home, &target.agent_id)
        .expect("hermes whitelist path");
    if !path.exists() {
        return Err(format!(
            "未检测到 Hermes 配置文件 {}，请先在本机安装 Hermes",
            path.display()
        ));
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    Ok((path, text))
}

/// 顶层 `model:` 块范围 (start, end)，不含 `model:` 行本身。
fn hm_model_block(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| l.trim_end() == "model:")?;
    let mut end = start + 1;
    while end < lines.len() {
        let l = lines[end];
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            end += 1;
            continue;
        }
        if l.starts_with(' ') || l.starts_with('\t') {
            end += 1;
        } else {
            break;
        }
    }
    Some((start, end))
}

fn hm_get(lines: &[&str], key: &str) -> Option<String> {
    let (s, e) = hm_model_block(lines)?;
    let prefix = format!("{key}:");
    lines[s + 1..e].iter().find_map(|l| {
        l.trim()
            .strip_prefix(&prefix)
            .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
    })
}

fn hm_has_ours(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    hm_get(&lines, "provider").as_deref() == Some(PROVIDER_NAME)
}

fn hm_drifted(text: &str, target: &Target) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    hm_get(&lines, "base_url").as_deref() != Some(target.base_url().as_str())
}

/// 行级手术：只替换/插入 model 块内 provider/base_url/api_key/default 四键，
/// 其余行逐字节保留（不重排、不丢注释）。
fn hm_render(text: &str, target: &Target) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let (s, e) =
        hm_model_block(&lines).ok_or_else(|| "配置缺少顶层 model: 块，拒绝写入".to_string())?;
    let want: [(&str, String); 4] = [
        ("provider", PROVIDER_NAME.to_string()),
        ("base_url", target.base_url()),
        ("api_key", KEY_PLACEHOLDER.to_string()),
        ("default", target.model_pointer()),
    ];
    let indent = lines[s + 1..e]
        .iter()
        .find(|l| !l.trim().is_empty() && (l.starts_with(' ') || l.starts_with('\t')))
        .map(|l| l[..l.len() - l.trim_start().len()].to_string())
        .unwrap_or_else(|| "  ".to_string());
    let has_key = |k: &str| {
        lines[s + 1..e]
            .iter()
            .any(|l| l.trim().starts_with(&format!("{k}:")))
    };
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);
    for (i, line) in lines.iter().enumerate() {
        if i == s {
            out.push(line.to_string());
            for (k, v) in want.iter().filter(|(k, _)| !has_key(k)) {
                out.push(format!("{indent}{k}: {v}"));
            }
            continue;
        }
        if i > s && i < e {
            if let Some((k, v)) = want
                .iter()
                .find(|(k, _)| line.trim().starts_with(&format!("{k}:")))
            {
                out.push(format!("{indent}{k}: {v}"));
                continue;
            }
        }
        out.push(line.to_string());
    }
    let mut res = out.join("\n");
    if text.ends_with('\n') {
        res.push('\n');
    }
    Ok(res)
}

fn hm_result(target: &Target, path: &Path, old: &str, new: &str, note: &str) -> ApplyResult {
    ApplyResult {
        agent_id: target.agent_id.clone(),
        adapter: Adapter::Hermes.name(),
        files: vec![FileChange {
            path: path.display().to_string(),
            preview: line_preview(old, new),
        }],
        env_template: None,
        note: note.into(),
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_preview_basic() {
        let d = line_preview("a\nb\nc\n", "a\nX\nc\n");
        assert!(d.contains("- b"));
        assert!(d.contains("+ X"));
        assert_eq!(line_preview("same\n", "same\n"), "(无变更)");
    }

    #[test]
    fn resolve_home_falls_back_to_base() {
        // 临时主目录里没有 Hermes 配置，且 Linux 下无 WSL 候选 → 原样返回
        let dir = std::env::temp_dir().join("ab-resolve-home-test");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(resolve_home(&dir, "hermes", ""), dir);
    }

    #[test]
    fn adapter_routing() {
        assert_eq!(Adapter::for_agent("opencode"), Adapter::OpenCode);
        assert_eq!(Adapter::for_agent("codex"), Adapter::Codex);
        assert_eq!(Adapter::for_agent("claude-code"), Adapter::ClaudeCode);
        assert_eq!(Adapter::for_agent("hermes"), Adapter::Hermes);
        assert_eq!(Adapter::for_agent("my-ide"), Adapter::Generic);
    }

    #[test]
    fn cx_replace_and_remove_region_roundtrip() {
        let text = "top = 1\n\n[foo]\nx = 2\n";
        let block = format!(
            "{CX_TABLE_BEGIN}\n[model_providers.agentbuffet]\nbase_url = \"u\"\n{CX_TABLE_END}\n"
        );
        let (t2, applied) = replace_region(text, CX_TABLE_BEGIN, CX_TABLE_END, &block);
        assert!(!applied);
        assert_eq!(t2, text);
        let with = text.to_string() + "\n" + &block;
        let (t3, applied) = replace_region(&with, CX_TABLE_BEGIN, CX_TABLE_END, &block);
        assert!(applied);
        assert!(t3.contains("[model_providers.agentbuffet]"));
        let removed = remove_region(&t3, CX_TABLE_BEGIN, CX_TABLE_END);
        assert!(!removed.contains("agentbuffet"));
        assert!(removed.contains("[foo]"));
    }

    #[test]
    fn insert_defaults_before_first_table() {
        let text = "a = 1\n\n[t]\nb = 2\n";
        let out = insert_before_first_table(text, "model = \"x\"\n");
        let idx_model = out.find("model").unwrap();
        let idx_table = out.find("[t]").unwrap();
        assert!(idx_model < idx_table);
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "buffet-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn hermes_target(agent_id: &str) -> Target {
        Target {
            agent_id: agent_id.to_string(),
            default_group: "g".to_string(),
            groups: vec!["g".to_string()],
            models: vec![],
            gateway_addr: "127.0.0.1:3425".to_string(),
            adapter: "hermes".to_string(),
        }
    }

    /// native 实例：装在伪造 home 里的 Hermes 必须被识别，且 scope 字段为
    /// native、配置路径落在该 home 下。
    #[test]
    fn detect_native_instance_from_fake_home() {
        let home = tmp_home("det-native");
        let cfg = home.join(".hermes").join("config.yaml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "base_url: http://127.0.0.1:3425/hermes/v1\n").unwrap();
        let s = detect(&home, &hermes_target("hermes"));
        assert!(s.installed, "伪造 home 里的 hermes 应被识别");
        assert_eq!(s.scope_id, crate::scope::NATIVE);
        assert_eq!(s.scope_kind, "native");
        let p = s.config_path.unwrap();
        assert!(
            p.ends_with(".hermes/config.yaml") || p.ends_with(".hermes\\config.yaml"),
            "{p}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 环境已消失（发行版停止/卸载）：detect 必须给出明确 note 且 installed=false，
    /// 绝不能拿本机同名配置冒充实例。
    #[test]
    fn detect_unknown_scope_reports_missing_env() {
        let home = tmp_home("det-ghost");
        let cfg = home.join(".hermes").join("config.yaml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "base_url: http://x/v1\n").unwrap();
        let s = detect(&home, &hermes_target("hermes@wsl:GhostDistro42"));
        assert!(!s.installed);
        assert!(s.note.contains("未找到该运行环境"), "{:?}", s.note);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 未知 scope 的 home 解析必须回落到 native home（与 attach 拒绝写操作
    /// 互补：读路径不炸，写路径在 apply 处把关）。
    #[test]
    fn resolve_home_unknown_scope_falls_back_to_native() {
        let home = tmp_home("resolve-ghost");
        assert_eq!(
            resolve_home(&home, "hermes@wsl:GhostDistro42", "hermes"),
            home
        );
        assert_eq!(resolve_home(&home, "hermes", "hermes"), home);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `\\wsl$\` 与 `\\wsl.localhost\` 是同一入口的两种写法；大小写与末尾
    /// 分隔符也不应影响匹配。
    #[test]
    fn norm_home_unifies_unc_prefix_and_case() {
        assert_eq!(
            norm_home("\\\\wsl$\\Ubuntu-24.04\\home\\yxf"),
            norm_home("\\\\wsl.localhost\\ubuntu-24.04\\home\\yxf\\")
        );
        assert_eq!(norm_home("/tmp/a/b"), norm_home("\\tmp\\a\\b"));
    }

    fn fake_scopes(native: &Path, wsl: &Path, win: &Path) -> Vec<scope::Scope> {
        vec![
            scope::native_scope(native),
            scope::Scope {
                id: "wsl:Ubuntu-24.04".into(),
                kind: scope::Kind::Wsl,
                home: wsl.to_path_buf(),
                distro: Some("Ubuntu-24.04".into()),
                user: None,
                root: None,
                gw_host: None,
                note: String::new(),
            },
            scope::Scope {
                id: "win:yxf".into(),
                kind: scope::Kind::Windows,
                home: win.to_path_buf(),
                distro: None,
                user: Some("yxf".into()),
                root: None,
                gw_host: None,
                note: String::new(),
            },
        ]
    }

    const MIG_HEAD: &str = r#"gateway:
  addr: "127.0.0.1:3641"
  ui_addr: "127.0.0.1:3642"
providers:
  - { id: p1, vendor: mock, base_url: "http://127.0.0.1:1/v1", api_key_env: K1, models: [m1], allow_private: true }
routing_groups:
  - name: bbb
    strategy: order
    members: [{ provider: p1, model: m1, priority: 1 }]
agents:
"#;

    /// 旧版 dash id（v0.2.16 遗留）按 home 字段迁移成 `<kind>@<scope>` 编码：
    /// WSL 实例不再被当成"本机实例"（跨环境串台根因）；匹配不到的环境原样保留。
    #[test]
    fn migrate_legacy_dash_ids_to_scope_encoding() {
        let dir = std::env::temp_dir().join(format!("buffet-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let native = dir.join("native");
        let win = dir.join("Users").join("yxf");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::create_dir_all(&win).unwrap();
        let scopes = fake_scopes(
            &native,
            &PathBuf::from("\\\\wsl.localhost\\Ubuntu-24.04\\home\\yxf"),
            &win,
        );
        let cfg = dir.join("config.yaml");
        std::fs::write(
            &cfg,
            &format!(
                r#"{MIG_HEAD}  - {{ id: hermes-wsl-ubuntu-24-04-yxf, default_group: bbb, adapter: hermes, home: \\wsl$\Ubuntu-24.04\home\yxf }}
  - {{ id: claude-code-win-yxf, default_group: bbb, home: {} }}
  - {{ id: codex-wsl-Ghost, default_group: bbb, adapter: codex, home: \\wsl.localhost\Ghost\home\x }}
  - {{ id: opencode, default_group: bbb, home: {} }}
"#,
                win.display(),
                native.display()
            ),
        )
        .unwrap();
        let renames = migrate_legacy(&cfg, &scopes);
        assert_eq!(renames.len(), 3, "3 条被迁移/清理: {renames:?}");
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("hermes@wsl:Ubuntu-24.04"), "{text}");
        assert!(text.contains("claude-code@win:yxf"), "{text}");
        assert!(text.contains("codex-wsl-Ghost"), "匹配不到环境必须原样保留");
        assert!(text.contains("id: opencode"), "{text}");
        // 只有 ghost 那条还留着 home 字段
        assert_eq!(text.matches("home:").count(), 1, "{text}");
        // 迁移结果必须能通过整份校验
        crate::config::load_str(&text, "mig.yaml").expect("迁移后配置应通过校验");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新旧两条指向同一实例：以新编码为准，旧 dash 条目直接删除（去重）。
    #[test]
    fn migrate_legacy_drops_duplicate_of_new_entry() {
        let dir = std::env::temp_dir().join(format!("buffet-mig2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let native = dir.join("native");
        std::fs::create_dir_all(&native).unwrap();
        let scopes = fake_scopes(
            &native,
            &PathBuf::from("\\\\wsl.localhost\\Ubuntu-24.04\\home\\yxf"),
            &dir.join("Users").join("yxf"),
        );
        let cfg = dir.join("config.yaml");
        std::fs::write(
            &cfg,
            &format!(
                r#"{MIG_HEAD}  - {{ id: hermes@wsl:Ubuntu-24.04, default_group: bbb, adapter: hermes }}
  - {{ id: hermes-wsl-ubuntu-24-04-yxf, default_group: bbb, adapter: hermes, home: \\wsl$\Ubuntu-24.04\home\yxf }}
"#
            ),
        )
        .unwrap();
        let renames = migrate_legacy(&cfg, &scopes);
        assert_eq!(renames.len(), 1);
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            !text.contains("hermes-wsl-ubuntu"),
            "旧条目应被删除: {text}"
        );
        assert_eq!(text.matches("hermes@wsl:Ubuntu-24.04").count(), 1, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
