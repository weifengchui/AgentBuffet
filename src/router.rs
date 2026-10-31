//! 路径式 agent 识别 + model→路由组解析（设计文档 v0.10 §5/§6）。

use crate::config::{Snapshot, RESERVED_SEGMENTS};
use std::fmt;

pub const UNIDENTIFIED: &str = "unidentified";

/// 一次请求解析出的转发计划。
pub struct Plan {
    pub agent_id: String,
    pub group: String,
    /// 上游相对路径，即去掉前导 v1/ 后的部分，如 "chat/completions"。
    pub upstream_path: String,
    /// 已按 priority 升序、过滤 disabled 后的候选成员。
    pub candidates: Vec<Candidate>,
}

pub struct Candidate {
    pub provider_id: String,
    pub model: String,
}

#[derive(Debug)]
pub enum RouteError {
    /// 400：model 未命中或参数非法，带可读提示。
    Bad(String),
    /// 404：路径结构不对。
    NotFound(String),
}

impl RouteError {
    pub fn status(&self) -> u16 {
        match self {
            RouteError::Bad(_) => 400,
            RouteError::NotFound(_) => 404,
        }
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouteError::Bad(m) | RouteError::NotFound(m) => write!(f, "{m}"),
        }
    }
}

/// 解析请求路径，拆出 agent_id 与去掉前导 v1/ 的上游路径。
///
/// 支持两种形态：
/// - `/<agent>/v1/chat/completions` → agent=<agent>, upstream="chat/completions"
/// - `/v1/chat/completions`         → agent=unidentified, upstream="chat/completions"
pub fn parse_path(path: &str) -> Result<(String, String), RouteError> {
    let trimmed = path.trim_start_matches('/');
    let mut segs = trimmed.split('/');
    let first = segs.next().unwrap_or("");
    if first.is_empty() {
        return Err(RouteError::NotFound("empty path".into()));
    }
    // 直接 /v1/... → unidentified，rest 为 v1 之后的完整路径
    if first == "v1" {
        let rest: Vec<&str> = segs.collect();
        return Ok((UNIDENTIFIED.to_string(), rest.join("/")));
    }
    // /<agent>/v1/...
    let agent = first;
    if RESERVED_SEGMENTS.contains(&agent) {
        return Err(RouteError::NotFound(format!("reserved segment: {agent}")));
    }
    let second = segs.next().unwrap_or("");
    if second != "v1" {
        return Err(RouteError::NotFound(format!(
            "路径需为 /{agent}/v1/... 或 /v1/..."
        )));
    }
    let rest: Vec<&str> = segs.collect();
    Ok((decode_segment(agent), rest.join("/")))
}

/// 路径段百分号解码。
///
/// 多环境实例的 id 形如 `hermes@wsl:Ubuntu`——`@` 与 `:` 在 RFC 3986 的 path
/// 段里是合法字符，但部分 HTTP 客户端仍会编码成 `%40` / `%3A`。这里做一次
/// 解码兜底，两种写法都能命中同一个 agent id。
fn decode_segment(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 依据 §6 规则解析 model→路由组，产出候选列表。
///
/// - model 以 `group/` 开头 → 路由组=该组，候选=组内全部成员（上游用各自 model）
/// - 否则 → 路由组=agent.default_group，候选=组内 model 匹配的成员
/// - 候选为空 → 400，列出可用 model 与组名
pub fn resolve(
    snap: &Snapshot,
    agent_id: &str,
    model: &str,
) -> Result<(String, Vec<Candidate>), RouteError> {
    if model.is_empty() {
        return Err(RouteError::Bad("请求缺少 model 字段".into()));
    }

    let (group_name, matched): (String, Vec<&crate::config::Member>) = if let Some(g) =
        model.strip_prefix("group/")
    {
        if !snap.groups.contains_key(g) {
            return Err(RouteError::Bad(format!(
                "路由组不存在: {g}；可用组: {}",
                group_list(snap)
            )));
        }
        let members = snap.group_members(g);
        (g.to_string(), members)
    } else {
        // 用 agent 的 default_group
        let group = match snap.agents.get(agent_id) {
            Some(a) => a.default_group.clone(),
            None => {
                // Hermes 配置档（`hermes@wsl:Ubuntu-24.04~profile:study`）沿用
                // 基础实例的路由组：只有用量统计按完整 id 分开，路由行为不变。
                match agent_id
                    .split_once("~profile:")
                    .and_then(|(base, _)| snap.agents.get(base))
                {
                    Some(a) => a.default_group.clone(),
                    // unidentified 或未知 agent：无 default_group，要求用 group/
                    None => {
                        return Err(RouteError::Bad(format!(
                            "未知 agent \"{agent_id}\" 无默认路由组，请用 group/<name> 指定；可用组: {}",
                            group_list(snap)
                        )));
                    }
                }
            }
        };
        let members: Vec<&crate::config::Member> = snap
            .group_members(&group)
            .into_iter()
            .filter(|m| m.model == model)
            .collect();
        if members.is_empty() {
            return Err(RouteError::Bad(format!(
                    "model \"{model}\" 不在路由组 \"{group}\" 内；该组可用 model: {}；或改用 group/<name>；可用组: {}",
                    model_list(snap, &group),
                    group_list(snap)
                )));
        }
        (group, members)
    };

    let mut candidates = Vec::new();
    for m in matched {
        if m.disabled {
            continue;
        }
        let p = match snap.providers.get(&m.provider) {
            Some(p) => p,
            None => continue,
        };
        if p.disabled {
            continue;
        }
        candidates.push(Candidate {
            provider_id: m.provider.clone(),
            model: m.model.clone(),
        });
    }
    if candidates.is_empty() {
        return Err(RouteError::Bad(format!(
            "路由组 \"{group_name}\" 内所有候选 provider 均已禁用"
        )));
    }
    Ok((group_name, candidates))
}

fn group_list(snap: &Snapshot) -> String {
    let mut v: Vec<&String> = snap.groups.keys().collect();
    v.sort();
    format!(
        "[{}]",
        v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    )
}

fn model_list(snap: &Snapshot, group: &str) -> String {
    let mut v: Vec<&str> = snap
        .group_members(group)
        .into_iter()
        .map(|m| m.model.as_str())
        .collect();
    v.sort_unstable();
    v.dedup();
    format!("[{}]", v.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_prefixed() {
        let (a, p) = parse_path("/hermes/v1/chat/completions").unwrap();
        assert_eq!(a, "hermes");
        assert_eq!(p, "chat/completions");
    }

    #[test]
    fn parse_direct_v1() {
        let (a, p) = parse_path("/v1/chat/completions").unwrap();
        assert_eq!(a, UNIDENTIFIED);
        assert_eq!(p, "chat/completions");
    }

    #[test]
    fn parse_rejects_reserved() {
        assert!(parse_path("/health/v1/x").is_err());
    }

    #[test]
    fn parse_requires_v1_second() {
        assert!(parse_path("/hermes/chat/completions").is_err());
    }

    /// 多环境实例 id（`hermes@wsl:Ubuntu`）走路径前缀时，未编码与百分号编码
    /// 两种写法都要解析到同一个 agent id。
    #[test]
    fn parse_scoped_agent_id_encoded_and_plain() {
        let (a, p) = parse_path("/hermes@wsl:Ubuntu/v1/chat/completions").unwrap();
        assert_eq!(a, "hermes@wsl:Ubuntu");
        assert_eq!(p, "chat/completions");

        let (a2, _) = parse_path("/hermes%40wsl%3AUbuntu/v1/chat/completions").unwrap();
        assert_eq!(a2, "hermes@wsl:Ubuntu");
    }

    const MINI_CONFIG: &str = r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
providers:
  - id: p1
    vendor: mock
    base_url: "http://127.0.0.1:1/v1"
    api_key_env: K1
    allow_private: true
    models: [m-a]
routing_groups:
  - name: g
    strategy: order
    members:
      - { provider: p1, model: m-a, priority: 1 }
agents:
  - { id: "hermes@wsl:Ubuntu-24.04", default_group: g }
"#;

    /// Hermes 配置档的复合 id 沿用基础实例的路由组（用量仍按完整 id 分开统计）。
    #[test]
    fn resolve_profile_id_inherits_base_agent_group() {
        let snap = crate::config::load_str(MINI_CONFIG, "test.yaml").unwrap();
        let (group, cands) = resolve(&snap, "hermes@wsl:Ubuntu-24.04~profile:study", "m-a").unwrap();
        assert_eq!(group, "g");
        assert_eq!(cands.len(), 1);

        // 未登记的普通 agent 仍按原规则报错。
        let err = match resolve(&snap, "unknown", "m-a") {
            Err(e) => e,
            Ok(_) => panic!("未登记 agent 应报错"),
        };
        assert!(err.to_string().contains("未知 agent"));
    }
}
