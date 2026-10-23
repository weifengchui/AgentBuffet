//! `buffetd import magpie`：从 Magpie（yetone/magpie）的 providers.json 导入
//! 供应商与路由组，生成 AgentBuffet 初始配置。
//!
//! 映射规则（fail-closed 结构校验 + 逐条目降级警告）：
//! - Provider：id/vendor 同名；base_url ← chat（OpenAI 兼容 base，已含 /v1）；
//!   api_key_env ← `BUFFET_<ID>_API_KEY`，真实 Key 只写入 env 文件（600），
//!   绝不进 config.yaml；off 的供应商跳过。
//! - Group：strategy 一律 order（Magpie 的 rotate/smart/usage/pace 降级为 order 并警告）；
//!   members "provider/model[:effort]" 按序映射 priority；嵌套 "group/<id>" 跳过并警告；
//!   组内 off 成员与未知 provider 跳过并警告。
//! - agents 无对应数据源，不生成，输出提示用户自行添加或用 attach。

use serde::Deserialize;

#[derive(Deserialize)]
struct MagpieFile {
    #[serde(default)]
    providers: Vec<MagpieProvider>,
    #[serde(default)]
    groups: Vec<MagpieGroup>,
}

#[derive(Deserialize)]
struct MagpieProvider {
    id: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    chat: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    off: bool,
}

#[derive(Deserialize)]
struct MagpieGroup {
    id: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    routing: String,
    #[serde(default)]
    off: Vec<String>,
}

/// 导入产物：可直接落盘的 config.yaml 文本、待写入 env 的键值、降级警告。
pub struct ImportOutcome {
    pub yaml: String,
    /// (环境变量名, 真实 Key)。只进 env 文件，不进 YAML。
    pub env_keys: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

/// 供应商 id → 环境变量名：大写，非字母数字转 `_`。
fn env_name(provider_id: &str) -> String {
    let mut s = String::from("BUFFET_");
    for c in provider_id.chars() {
        if c.is_ascii_alphanumeric() {
            s.extend(c.to_uppercase());
        } else {
            s.push('_');
        }
    }
    s.push_str("_API_KEY");
    s
}

/// YAML 双引号标量转义（反斜杠与引号）。
fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 解析 Magpie providers.json 文本，生成 AgentBuffet 配置。
/// 结构性错误（JSON 非法、id 缺失/重复）返回 Err；条目级问题进 warnings。
pub fn import_magpie(text: &str) -> Result<ImportOutcome, String> {
    let f: MagpieFile =
        serde_json::from_str(text).map_err(|e| format!("providers.json 解析失败: {e}"))?;

    let mut warnings = Vec::new();
    let mut env_keys = Vec::new();
    let mut imported: Vec<MagpieProvider> = Vec::new();
    let mut ids: Vec<String> = Vec::new();

    for p in f.providers {
        if p.id.is_empty() {
            return Err("providers.json 含空 id 的供应商".into());
        }
        if ids.iter().any(|x| x == &p.id) {
            return Err(format!("供应商 id 重复: {}", p.id));
        }
        ids.push(p.id.clone());
        if p.off {
            warnings.push(format!("供应商 {} 已停用（off），跳过", p.id));
            continue;
        }
        if p.chat.is_empty() {
            warnings.push(format!(
                "供应商 {} 无 OpenAI 兼容 chat base_url（订阅/Anthropic/Gemini 型），跳过",
                p.id
            ));
            continue;
        }
        if p.key.is_empty() {
            warnings.push(format!(
                "供应商 {} 无 API Key，config 已引用环境变量，请自行导出",
                p.id
            ));
        } else {
            env_keys.push((env_name(&p.id), p.key.clone()));
        }
        imported.push(p);
    }

    let mut yaml = String::from(
        "# 由 buffetd import magpie 生成\n\
         gateway:\n\
         \x20 addr: \"127.0.0.1:3425\"\n\
         \x20 ui_addr: \"127.0.0.1:3426\"\n\
         providers:\n",
    );
    if imported.is_empty() {
        return Err("没有可导入的供应商（均为 off / 无 chat base_url）".into());
    }
    for p in &imported {
        yaml.push_str(&format!("- id: {}\n", q(&p.id)));
        yaml.push_str(&format!("  vendor: {}\n", q(&p.id)));
        yaml.push_str(&format!("  base_url: {}\n", q(&p.chat)));
        yaml.push_str(&format!("  api_key_env: {}\n", env_name(&p.id)));
        if p.models.is_empty() {
            yaml.push_str("  models: []\n");
        } else {
            let ms: Vec<String> = p.models.iter().map(|m| q(m)).collect();
            yaml.push_str(&format!("  models: [{}]\n", ms.join(", ")));
        }
    }

    yaml.push_str("routing_groups:\n");
    let mut group_count = 0usize;
    let valid_ids: Vec<&String> = imported.iter().map(|p| &p.id).collect();
    for g in &f.groups {
        if g.id.is_empty() {
            return Err("providers.json 含空 id 的路由组".into());
        }
        if !g.routing.is_empty() && g.routing != "order" {
            warnings.push(format!(
                "路由组 {} strategy={} v1 仅支持 order，已降级",
                g.id, g.routing
            ));
        }
        let mut lines = String::new();
        let mut prio = 0u32;
        for m in &g.members {
            if g.off.iter().any(|o| o == m) {
                warnings.push(format!("路由组 {} 成员 {m} 已停用，跳过", g.id));
                continue;
            }
            if m.starts_with("group/") {
                warnings.push(format!(
                    "路由组 {} 含嵌套成员 {m}，v1 不支持嵌套组，跳过",
                    g.id
                ));
                continue;
            }
            // "provider/model[:effort]"
            let head = m.split(':').next().unwrap_or("");
            let (prov, model) = match head.split_once('/') {
                Some((p, mo)) if !p.is_empty() && !mo.is_empty() => (p, mo),
                _ => {
                    warnings.push(format!("路由组 {} 成员 {m} 格式无法解析，跳过", g.id));
                    continue;
                }
            };
            if !valid_ids.iter().any(|x| x.as_str() == prov) {
                warnings.push(format!("路由组 {} 成员 {m} 的供应商未导入，跳过", g.id));
                continue;
            }
            prio += 1;
            lines.push_str(&format!(
                "  - {{ provider: {}, model: {}, priority: {} }}\n",
                q(prov),
                q(model),
                prio
            ));
        }
        if lines.is_empty() {
            warnings.push(format!("路由组 {} 无有效成员，跳过", g.id));
            continue;
        }
        yaml.push_str(&format!(
            "- name: {}\n  strategy: order\n  members:\n",
            q(&g.id)
        ));
        yaml.push_str(&lines);
        group_count += 1;
    }
    if group_count == 0 {
        warnings.push(
            "未导入任何路由组；agents 需自行在 config.yaml 添加（default_group 指向已有组）".into(),
        );
    }

    Ok(ImportOutcome {
        yaml,
        env_keys,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_name_sanitizes_id() {
        assert_eq!(env_name("deepseek"), "BUFFET_DEEPSEEK_API_KEY");
        assert_eq!(env_name("moon-shot.cn"), "BUFFET_MOON_SHOT_CN_API_KEY");
    }
}
