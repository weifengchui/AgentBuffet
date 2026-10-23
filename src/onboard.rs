//! M4 接入器（设计文档 §7）：detect / plan / apply / revert + 漂移检测。
//! 硬约束：
//! - 只写适配器白名单内的文件路径；文件解析失败 → 拒绝写入，绝不"尽力而为"。
//! - 写入前备份 `<file>.before-agentbuffet`（已存在则不覆盖）；revert 优先从备份恢复（逐字节），
//!   备份丢失时精确删除注入块。
//! - 幂等：重复 attach 只更新注入块，不叠加；同名 provider 被用户自定义过 → 拒绝。
//! - 永不写入真实 API Key：apiKey 一律填占位符，真实 Key 只存在于网关进程环境变量。
//! - 运行期只读做漂移检测，永不主动写——改写只发生在显式 CLI/UI 操作（本模块由调用方触发）。

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

/// 用户主目录（适配器配置文件所在）。
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
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
}

impl Target {
    pub fn base_url(&self) -> String {
        format!("http://{}/{}/v1", self.gateway_addr, self.agent_id)
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
    pub fn for_agent(agent_id: &str) -> Adapter {
        match agent_id {
            "opencode" => Adapter::OpenCode,
            "codex" => Adapter::Codex,
            "hermes" => Adapter::Hermes,
            "claude-code" => Adapter::ClaudeCode,
            _ => Adapter::Generic,
        }
    }

    /// 适配器白名单文件路径（Generic 无文件）。
    pub fn config_path(self, home: &Path) -> Option<PathBuf> {
        match self {
            Adapter::OpenCode => Some(home.join(".config").join("opencode").join("opencode.json")),
            Adapter::Codex => Some(home.join(".codex").join("config.toml")),
            Adapter::Hermes => Some(home.join(".hermes").join("config.yaml")),
            Adapter::ClaudeCode => Some(home.join(".claude.json")),
            Adapter::Generic => None,
        }
    }
}

/// 已知可检测的本机 Agent（接入器视图快速接入）。
pub const KNOWN_AGENTS: &[&str] = &["opencode", "codex", "hermes", "claude-code"];

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub agent_id: String,
    pub adapter: &'static str,
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
    let a = Adapter::for_agent(&target.agent_id);
    let path = a.config_path(home);
    let cp = path.as_ref().map(|p| p.display().to_string());
    let mut s = AgentStatus {
        agent_id: target.agent_id.clone(),
        adapter: a.name(),
        config_path: cp,
        installed: false,
        attached: false,
        drifted: false,
        note: String::new(),
    };
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
                    locs.push(format!("本机：{}", dir.display()));
                }
            }
            for w in hm_windows_homes() {
                locs.push(format!("Windows 侧：{w}"));
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

/// dry-run：输出将修改的文件与 diff 预览，不落盘（§7 plan）。
pub fn plan(home: &Path, target: &Target) -> Result<ApplyResult, String> {
    let a = Adapter::for_agent(&target.agent_id);
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
    let a = Adapter::for_agent(&target.agent_id);
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
pub fn revert(home: &Path, agent_id: &str) -> Result<ApplyResult, String> {
    let a = Adapter::for_agent(agent_id);
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
                .config_path(home)
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
            let path = a.config_path(home).expect("adapter whitelist path");
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

fn oc_read(home: &Path, _target: &Target) -> Result<(PathBuf, String), String> {
    let path = Adapter::OpenCode
        .config_path(home)
        .expect("opencode whitelist path");
    if !path.exists() {
        return Err(format!(
            "未检测到 OpenCode 配置文件 {}，请先安装 OpenCode",
            path.display()
        ));
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
    ApplyResult {
        agent_id: target.agent_id.clone(),
        adapter: Adapter::OpenCode.name(),
        files: vec![FileChange {
            path: path.display().to_string(),
            preview: line_preview(old, new),
        }],
        env_template: None,
        note: note.into(),
    }
}

// ---------------------------------------------------------------- Codex（TOML）

fn cx_read(home: &Path, _target: &Target) -> Result<(PathBuf, String), String> {
    let path = Adapter::Codex
        .config_path(home)
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

fn cx_defaults_block(target: &Target) -> String {
    format!(
        "{CX_DEF_BEGIN}\nmodel_provider = \"{PROVIDER_NAME}\"\nmodel = \"group/{}\"\n{CX_DEF_END}\n",
        target.default_group
    )
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

fn cx_drifted(text: &str, parsed: &toml::Value, target: &Target) -> bool {
    // 注入块内容与期望不一致即漂移；defaults 仅在"应当存在"时参与比较。
    let want_table = cx_table_block(target);
    if extract_region(text, CX_TABLE_BEGIN, CX_TABLE_END).as_deref() != Some(want_table.trim_end())
    {
        return true;
    }
    let user_top = parsed.get("model_provider").is_some() || parsed.get("model").is_some();
    let has_def = text.contains(CX_DEF_BEGIN);
    if has_def {
        let want_def = cx_defaults_block(target);
        if extract_region(text, CX_DEF_BEGIN, CX_DEF_END).as_deref() != Some(want_def.trim_end()) {
            return true;
        }
    } else if !user_top {
        return true; // 本应有 defaults 注入块却缺失
    }
    false
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
    let mut note = String::new();

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

    // 2) 顶层 defaults：仅当用户未自定义顶层 model/model_provider 时注入。
    let user_top = parsed.get("model_provider").is_some() || parsed.get("model").is_some();
    let def = cx_defaults_block(target);
    let (body2, def_applied) = replace_region(&out, CX_DEF_BEGIN, CX_DEF_END, &def);
    out = body2;
    if !def_applied {
        if user_top {
            note = "检测到用户自定义的顶层 model/model_provider，未覆盖；请手动设置 model_provider = \"agentbuffet\"".to_string();
        } else {
            out = insert_before_first_table(&out, &def);
        }
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

/// WSL 内探测 Windows 侧 Hermes 主目录（纯文件系统扫描，不起外部进程）。
fn hm_windows_homes() -> Vec<String> {
    let mut v = Vec::new();
    let users = Path::new("/mnt/c/Users");
    if let Ok(rd) = std::fs::read_dir(users) {
        for entry in rd.flatten() {
            let p = entry.path().join(".hermes");
            if p.is_dir() {
                v.push(p.display().to_string());
            }
        }
    }
    v
}

fn hm_read(home: &Path, _target: &Target) -> Result<(PathBuf, String), String> {
    let path = Adapter::Hermes
        .config_path(home)
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
}
