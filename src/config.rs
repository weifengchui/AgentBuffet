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
    PathBuf::from("./prices.yaml")
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
        if RESERVED_SEGMENTS.contains(&a.id.as_str()) {
            errs.push(format!("{filename}: agent id \"{}\" 为保留字", a.id));
        }
        if a.id.is_empty()
            || !a
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            errs.push(format!(
                "{filename}: agent id {} 只允许小写字母/数字/-/_",
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
