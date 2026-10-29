//! 配置模型与校验：结构错误一律拒绝启动，报错带文件名。
//! 校验口径以设计文档 v0.10 §5 为准。

use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

pub const RESERVED_SEGMENTS: &[&str] = &["health", "_admin", "v1"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    pub addr: String,
    pub ui_addr: String,
    #[serde(default = "d_true")]
    pub allow_loopback_only: bool,
    #[serde(default = "d_retain")]
    pub log_retain_days: u32,
    #[serde(default = "d_ratio")]
    pub conservative_quota_ratio: f64,
    #[serde(default = "d_true")]
    pub expose_groups_as_models: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prices {
    #[serde(default = "d_prices")]
    pub file: PathBuf,
}

impl Default for Prices {
    fn default() -> Self {
        Self { file: d_prices() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quota {
    pub daily_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub vendor: String,
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// 免费模型 ID 列表（拉取目录时按服务商定价字段或命名启发式判定，供 UI 精确标记）。
    #[serde(default)]
    pub free_models: Vec<String>,
    #[serde(default)]
    pub quota: Option<Quota>,
    #[serde(default)]
    pub allow_private: bool,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub provider: String,
    pub model: String,
    pub priority: u32,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingGroup {
    pub name: String,
    #[serde(default = "d_order")]
    pub strategy: String,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    pub default_group: String,
    /// 显式适配器（如 opencode），支持同一工具多实例接入；空则按 id 推断。
    #[serde(default)]
    pub adapter: String,
    /// 旧版（v0.2.16）接入时写入的实例主目录。新版按 id 里的 `@scope` 定位环境，
    /// 该字段只用于启动时把旧 dash id（如 `hermes-wsl-ubuntu-24-04-yxf`）迁移成
    /// 新编码；迁移完成后会被清除。保留解析是为了旧配置不至于直接校验失败。
    #[serde(default)]
    pub home: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    #[serde(default = "d_connect")]
    pub connect_seconds: u64,
    #[serde(default = "d_fb")]
    pub first_byte_seconds: u64,
    #[serde(default = "d_idle")]
    pub stream_idle_seconds: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_seconds: d_connect(),
            first_byte_seconds: d_fb(),
            stream_idle_seconds: d_idle(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreaker {
    #[serde(default = "d_window")]
    pub window_seconds: u64,
    #[serde(default = "d_threshold")]
    pub threshold: u32,
    #[serde(default = "d_open")]
    pub open_seconds: u64,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            window_seconds: d_window(),
            threshold: d_threshold(),
            open_seconds: d_open(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failover {
    #[serde(default = "d_retry_status")]
    pub retry_on_status: Vec<u16>,
    #[serde(default = "d_true")]
    pub retry_on_timeout: bool,
    #[serde(default = "d_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub circuit_breaker: CircuitBreaker,
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            retry_on_status: d_retry_status(),
            retry_on_timeout: true,
            max_retries: d_retries(),
            timeouts: Timeouts::default(),
            circuit_breaker: CircuitBreaker::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub gateway: Gateway,
    #[serde(default)]
    pub prices: Prices,
    pub providers: Vec<Provider>,
    pub routing_groups: Vec<RoutingGroup>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub failover: Failover,
}

fn d_true() -> bool {
    true
}
fn d_retain() -> u32 {
    30
}
fn d_ratio() -> f64 {
    0.8
}
fn d_prices() -> PathBuf {
    default_prices_path()
}

/// 单价文件默认路径：`~/.agentbuffet/prices.yaml`（Windows 为
/// `C:\Users\<用户名>\.agentbuffet\prices.yaml`），与 exe 目录解耦，升级不丢。
pub fn default_prices_path() -> PathBuf {
    crate::onboard::home_dir_opt()
        .map(|h| h.join(".agentbuffet").join("prices.yaml"))
        .unwrap_or_else(|| PathBuf::from("./prices.yaml"))
}
fn d_order() -> String {
    "order".into()
}
fn d_connect() -> u64 {
    5
}
fn d_fb() -> u64 {
    60
}
fn d_idle() -> u64 {
    120
}
fn d_window() -> u64 {
    60
}
fn d_threshold() -> u32 {
    3
}
fn d_open() -> u64 {
    120
}
fn d_retry_status() -> Vec<u16> {
    vec![429, 500, 502, 503, 504]
}
fn d_retries() -> u32 {
    3
}

/// 校验通过后的配置快照，供 Arc-swap 热重载。
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub raw: RawConfig,
    pub gateway_addr: SocketAddr,
    pub ui_addr: SocketAddr,
    pub providers: HashMap<String, Provider>,
    pub groups: HashMap<String, RoutingGroup>,
    pub agents: HashMap<String, Agent>,
}

impl Snapshot {
    /// 某个 agent 实例的默认组：已登记的用它自己的；未登记的用第一个路由组
    /// （按名排序）。
    ///
    /// 检测与巡检必须用同一套规则，否则会出现"检测说漂移、巡检说没漂移"这种
    /// 自相矛盾——多环境实例（hermes@wsl:Ubuntu）尤其容易踩到。
    pub fn default_group_for(&self, agent_id: &str) -> String {
        if let Some(a) = self.agents.get(agent_id) {
            return a.default_group.clone();
        }
        let mut names: Vec<&String> = self.groups.keys().collect();
        names.sort();
        names.first().map(|s| (*s).clone()).unwrap_or_default()
    }

    /// 组内启用成员按 priority 升序（路由候选）。
    pub fn active_members(&self, group: &str) -> Vec<&Member> {
        self.group_members(group)
            .into_iter()
            .filter(|m| !m.disabled)
            .collect()
    }

    /// 组内成员按 priority 升序。
    pub fn group_members(&self, group: &str) -> Vec<&Member> {
        let mut m: Vec<&Member> = match self.groups.get(group) {
            Some(g) => g.members.iter().collect(),
            None => return Vec::new(),
        };
        m.sort_by_key(|x| x.priority);
        m
    }
}

/// 解析 + 校验。返回全部错误（带文件名），供拒启或 422。
pub fn load_str(text: &str, filename: &str) -> Result<Snapshot, Vec<String>> {
    let raw: RawConfig = match serde_yaml::from_str(text) {
        Ok(c) => c,
        Err(e) => return Err(vec![format!("{filename}: {e}")]),
    };
    let mut errs: Vec<String> = Vec::new();

    let gateway_addr: SocketAddr = match raw.gateway.addr.parse() {
        Ok(a) => a,
        Err(_) => {
            errs.push(format!(
                "{filename}: gateway.addr 无法解析为 ip:port: {}",
                raw.gateway.addr
            ));
            "127.0.0.1:3425".parse().unwrap()
        }
    };
    let ui_addr: SocketAddr = match raw.gateway.ui_addr.parse() {
        Ok(a) => a,
        Err(_) => {
            errs.push(format!(
                "{filename}: gateway.ui_addr 无法解析为 ip:port: {}",
                raw.gateway.ui_addr
            ));
            "127.0.0.1:3426".parse().unwrap()
        }
    };

    if raw.gateway.allow_loopback_only
        && !(gateway_addr.ip().is_loopback() && ui_addr.ip().is_loopback())
    {
        errs.push(format!(
            "{filename}: allow_loopback_only=true 时监听地址必须为 loopback"
        ));
    }

    // providers
    let mut providers: HashMap<String, Provider> = HashMap::new();
    for p in &raw.providers {
        if providers.insert(p.id.clone(), p.clone()).is_some() {
            errs.push(format!("{filename}: provider id 重复: {}", p.id));
        }
        match reqwest::Url::parse(&p.base_url) {
            Ok(u) => {
                if u.scheme() != "http" && u.scheme() != "https" {
                    errs.push(format!(
                        "{filename}: provider {} base_url 协议必须为 http(s)",
                        p.id
                    ));
                }
                if let Some(host) = u.host_str() {
                    if let Ok(ip) = host.parse::<IpAddr>() {
                        if is_private_ip(ip) && !p.allow_private {
                            errs.push(format!(
                                "{filename}: provider {} base_url 指向私有/回环 IP，需显式 allow_private: true",
                                p.id
                            ));
                        }
                    }
                }
            }
            Err(e) => errs.push(format!("{filename}: provider {} base_url 非法: {e}", p.id)),
        }
        for m in &p.models {
            if m.is_empty() {
                errs.push(format!("{filename}: provider {} 含空 model 名", p.id));
            }
        }
    }

    // routing groups
    let mut groups: HashMap<String, RoutingGroup> = HashMap::new();
    for g in &raw.routing_groups {
        if groups.insert(g.name.clone(), g.clone()).is_some() {
            errs.push(format!("{filename}: routing_group 名字重复: {}", g.name));
        }
        if g.strategy != "order" {
            errs.push(format!(
                "{filename}: 路由组 {} strategy={} v1 仅支持 order",
                g.name, g.strategy
            ));
        }
        if g.members.is_empty() {
            errs.push(format!("{filename}: 路由组 {} 无成员", g.name));
        }
        let mut prio = std::collections::HashSet::new();
        for m in &g.members {
            match providers.get(&m.provider) {
                None => errs.push(format!(
                    "{filename}: 路由组 {} 引用不存在的 provider: {}",
                    g.name, m.provider
                )),
                Some(p) => {
                    // models 为空 = 不限制模型（如 OpenRouter 这类目录庞大的服务商），
                    // 允许成员引用任意 model，由 UI 自由输入。
                    if !p.models.is_empty() && !p.models.contains(&m.model) {
                        errs.push(format!(
                            "{filename}: 路由组 {} 成员 {}/{} 的 model 不在该 provider 声明列表中",
                            g.name, m.provider, m.model
                        ));
                    }
                }
            }
            if m.model.is_empty() {
                errs.push(format!("{filename}: 路由组 {} 成员含空 model", g.name));
            }
            if !prio.insert(m.priority) {
                errs.push(format!(
                    "{filename}: 路由组 {} priority={} 重复",
                    g.name, m.priority
                ));
            }
        }
    }

    // agents
    let mut agents: HashMap<String, Agent> = HashMap::new();
    for a in &raw.agents {
        if RESERVED_SEGMENTS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(a.id.as_str()))
        {
            errs.push(format!("{filename}: agent id \"{}\" 为保留字", a.id));
        }
        if a.id.is_empty()
            || !a.id.chars().all(|c| {
                // 多环境实例形如 hermes@wsl:Ubuntu：@ 与 : 用于标记运行环境
                c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | ':')
            })
        {
            errs.push(format!(
                "{filename}: agent id {} 只允许字母/数字/-/_/./@/:",
                a.id
            ));
        }
        if agents.insert(a.id.clone(), a.clone()).is_some() {
            errs.push(format!("{filename}: agent id 重复: {}", a.id));
        }
        if !groups.contains_key(&a.default_group) {
            errs.push(format!(
                "{filename}: agent {} default_group 不存在: {}",
                a.id, a.default_group
            ));
        }
    }

    if !errs.is_empty() {
        return Err(errs);
    }

    Ok(Snapshot {
        raw,
        gateway_addr,
        ui_addr,
        providers,
        groups,
        agents,
    })
}

pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xC0) == 64) // CGNAT 100.64/10
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            v6.is_loopback() || (o[0] & 0xFE) == 0xFC // ULA fc00::/7
        }
    }
}

/// 启动自检：Key 缺失等只告警不拒启（§5）。
pub fn startup_warnings(snap: &Snapshot) -> Vec<String> {
    let mut w = Vec::new();
    for p in snap.providers.values() {
        if std::env::var(&p.api_key_env).is_err() {
            w.push(format!(
                "provider {} 的环境变量 {} 未设置，请求该 provider 时将报 degraded",
                p.id, p.api_key_env
            ));
        }
    }
    w
}

/// 在配置文件里新增/更新一条 agent 记录（CLI `agents attach` 用）。
///
/// 多环境实例的 id 形如 `hermes@wsl:Ubuntu`、`hermes@win:yxf`，与本机的
/// `hermes` 各占一条，配置与用量分开统计。
///
/// 硬约束：写回前重新校验整份配置，校验不过就**不落盘**（绝不写坏配置）。
pub fn upsert_agent(
    path: &std::path::Path,
    id: &str,
    default_group: &str,
    adapter: &str,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取配置 {} 失败: {e}", path.display()))?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("解析配置 {} 失败: {e}", path.display()))?;
    {
        let root = doc
            .as_mapping_mut()
            .ok_or_else(|| "配置根节点不是映射".to_string())?;
        let agents = root
            .entry(serde_yaml::Value::String("agents".into()))
            .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
        let seq = agents
            .as_sequence_mut()
            .ok_or_else(|| "agents 不是序列".to_string())?;
        let mut m = serde_yaml::Mapping::new();
        m.insert(
            serde_yaml::Value::String("id".into()),
            serde_yaml::Value::String(id.to_string()),
        );
        m.insert(
            serde_yaml::Value::String("default_group".into()),
            serde_yaml::Value::String(default_group.to_string()),
        );
        if !adapter.trim().is_empty() {
            m.insert(
                serde_yaml::Value::String("adapter".into()),
                serde_yaml::Value::String(adapter.trim().to_string()),
            );
        }
        match seq.iter_mut().find(|a| {
            a.get("id")
                .and_then(|v| v.as_str())
                .map(|s| s == id)
                .unwrap_or(false)
        }) {
            Some(a) => *a = serde_yaml::Value::Mapping(m),
            None => seq.push(serde_yaml::Value::Mapping(m)),
        }
    }
    let out = serde_yaml::to_string(&doc).map_err(|e| format!("序列化配置失败: {e}"))?;
    let filename = path.display().to_string();
    if let Err(errs) = load_str(&out, &filename) {
        return Err(format!("新配置未通过校验，已放弃写入：{}", errs.join("；")));
    }
    std::fs::write(path, out).map_err(|e| format!("写入配置 {} 失败: {e}", path.display()))?;
    Ok(())
}

/// 从配置文件里删除一条 agent 记录（detach 用）。没有该记录返回 false，不报错。
///
/// 硬约束同 upsert_agent：写回前重新校验整份配置，校验不过就**不落盘**。
pub fn remove_agent(path: &std::path::Path, id: &str) -> Result<bool, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取配置 {} 失败: {e}", path.display()))?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("解析配置 {} 失败: {e}", path.display()))?;
    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| "配置根节点不是映射".to_string())?;
    let Some(agents) = root.get_mut(serde_yaml::Value::String("agents".into())) else {
        return Ok(false);
    };
    let Some(seq) = agents.as_sequence_mut() else {
        return Ok(false);
    };
    let before = seq.len();
    seq.retain(|a| {
        a.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s != id)
            .unwrap_or(true)
    });
    if seq.len() == before {
        return Ok(false);
    }
    let out = serde_yaml::to_string(&doc).map_err(|e| format!("序列化配置失败: {e}"))?;
    let filename = path.display().to_string();
    if let Err(errs) = load_str(&out, &filename) {
        return Err(format!("新配置未通过校验，已放弃写入：{}", errs.join("；")));
    }
    std::fs::write(path, out).map_err(|e| format!("写入配置 {} 失败: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
gateway:
  addr: "127.0.0.1:3425"
  ui_addr: "127.0.0.1:3426"
providers:
  - { id: p1, vendor: mock, base_url: "http://127.0.0.1:1/v1", api_key_env: K1, models: [m1], allow_private: true }
routing_groups:
  - name: bbb
    strategy: order
    members: [{ provider: p1, model: m1, priority: 1 }]
  - name: aaa
    strategy: order
    members: [{ provider: p1, model: m1, priority: 1 }]
agents:
  - { id: codex, default_group: bbb }
"#;

    /// 旧版遗留的 `home` 字段必须能解析（否则整份配置直接校验失败），
    /// 且仅供迁移逻辑读取。
    #[test]
    fn agent_accepts_legacy_home_field() {
        let yaml = YAML.replace(
            "  - { id: codex, default_group: bbb }",
            "  - { id: codex, default_group: bbb, home: C:\\Users\\yxf }",
        );
        let snap = load_str(&yaml, "t.yaml").expect("home 字段应被接受");
        assert_eq!(snap.agents["codex"].home, "C:\\Users\\yxf");
    }

    /// detach 删登记：删掉目标条目、保留其余；无该条目返回 false 不改文件。
    #[test]
    fn remove_agent_deletes_entry() {
        let dir = std::env::temp_dir().join(format!("buffet-rm-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.yaml");
        std::fs::write(&p, YAML).unwrap();
        assert!(remove_agent(&p, "codex").unwrap());
        let text = std::fs::read_to_string(&p).unwrap();
        let snap = load_str(&text, "t.yaml").expect("删后仍应通过校验");
        assert!(snap.agents.is_empty(), "{:?}", snap.agents.keys());
        assert!(!remove_agent(&p, "codex").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已登记的实例用它自己的默认组；未登记的用第一个组（按名排序）。
    ///
    /// 检测与巡检必须用同一套规则，否则会出现"检测说漂移、巡检说没漂移"。
    #[test]
    fn default_group_for_is_consistent() {
        let snap = load_str(YAML, "t.yaml").expect("config ok");
        assert_eq!(snap.default_group_for("codex"), "bbb");
        // 未登记的多环境实例 → 第一个组（"aaa" < "bbb"）
        assert_eq!(snap.default_group_for("hermes@wsl:Ubuntu"), "aaa");
        assert_eq!(snap.default_group_for("hermes@wsl:Ubuntu"), "aaa");
    }
}
