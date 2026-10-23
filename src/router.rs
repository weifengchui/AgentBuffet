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
    Ok((agent.to_string(), rest.join("/")))
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
                // unidentified 或未知 agent：无 default_group，要求用 group/
                return Err(RouteError::Bad(format!(
                    "未知 agent \"{agent_id}\" 无默认路由组，请用 group/<name> 指定；可用组: {}",
                    group_list(snap)
                )));
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
}
