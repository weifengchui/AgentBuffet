//! 主流厂商目录：UI 快捷添加用（厂商 + 套餐 → base_url + 默认模型）。
//! 数据为公开 API 端点，Key 由用户输入，只进 env 文件与进程环境变量。

#[derive(serde::Serialize)]
pub struct Plan {
    pub id: String,
    pub label_zh: String,
    pub label_en: String,
    pub base_url: String,
    pub models: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct Vendor {
    pub id: String,
    pub name_zh: String,
    pub name_en: String,
    pub plans: Vec<Plan>,
}

fn v(id: &str, zh: &str, en: &str, plans: Vec<Plan>) -> Vendor {
    Vendor {
        id: id.into(),
        name_zh: zh.into(),
        name_en: en.into(),
        plans,
    }
}
fn p(id: &str, zh: &str, en: &str, url: &str, models: &[&str]) -> Plan {
    Plan {
        id: id.into(),
        label_zh: zh.into(),
        label_en: en.into(),
        base_url: url.into(),
        models: models.iter().map(|s| s.to_string()).collect(),
    }
}

/// 目录（顺序即 UI 展示顺序）。
pub fn catalog() -> Vec<Vendor> {
    vec![
        v(
            "deepseek",
            "DeepSeek 官方",
            "DeepSeek",
            vec![p(
                "api",
                "按量 API",
                "API",
                "https://api.deepseek.com/v1",
                &["deepseek-chat", "deepseek-reasoner"],
            )],
        ),
        v(
            "moonshot",
            "月之暗面 Kimi",
            "Moonshot Kimi",
            vec![p(
                "api",
                "按量 API",
                "API",
                "https://api.moonshot.cn/v1",
                &["moonshot-v1-8k"],
            )],
        ),
        v(
            "zhipu",
            "智谱 GLM",
            "Z.AI GLM",
            vec![p(
                "api",
                "按量 API",
                "API",
                "https://open.bigmodel.cn/api/paas/v4",
                &["glm-4.6"],
            )],
        ),
        v(
            "bailian",
            "阿里云百炼",
            "Alibaba Bailian",
            vec![
                p(
                    "token-plan",
                    "Token Plan 套餐",
                    "Token Plan",
                    "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
                    &[],
                ),
                p(
                    "standard",
                    "标准按量 API",
                    "Standard API",
                    "https://dashscope.aliyuncs.com/compatible-mode/v1",
                    &["qwen-plus"],
                ),
            ],
        ),
        v(
            "opencode",
            "OpenCode",
            "OpenCode",
            vec![
                p(
                    "zen",
                    "Zen 免费池",
                    "Zen free pool",
                    "https://opencode.ai/zen/v1",
                    &[
                        "minimax-m3.1-flash-free",
                        "deepseek-v4.1-flash-free",
                        "glm-5.3-flash-free",
                    ],
                ),
                p(
                    "go",
                    "Go 套餐",
                    "Go plan",
                    "https://opencode.ai/zen/go/v1",
                    &["minimax-m3.1-flash-free"],
                ),
            ],
        ),
        v(
            "openrouter",
            "OpenRouter",
            "OpenRouter",
            vec![p(
                "api",
                "聚合 API",
                "Aggregator API",
                "https://openrouter.ai/api/v1",
                &[],
            )],
        ),
        v(
            "siliconflow",
            "硅基流动",
            "SiliconFlow",
            vec![p(
                "api",
                "按量 API",
                "API",
                "https://api.siliconflow.cn/v1",
                &[],
            )],
        ),
    ]
}

/// 「自定义」伪厂商：不在目录里，由 UI 追加，端点与模型名全部由用户填写。
pub const CUSTOM: &str = "custom";

/// 自定义服务商 id 只接受字母、数字、`-`、`_`（要能拼出合法的环境变量名）。
pub fn valid_custom_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// provider id：单套餐厂商用厂商名，多套餐用 `厂商-套餐`。
pub fn provider_id(vendor_id: &str, plan_id: &str) -> String {
    let plans = catalog()
        .into_iter()
        .find(|x| x.id == vendor_id)
        .map(|x| x.plans.len())
        .unwrap_or(0);
    if plans == 1 {
        vendor_id.to_string()
    } else {
        format!("{vendor_id}-{plan_id}")
    }
}

/// 查找 (vendor, plan) → (provider_id, plan)。
pub fn lookup(vendor_id: &str, plan_id: &str) -> Option<(String, Plan)> {
    let vendor = catalog().into_iter().find(|x| x.id == vendor_id)?;
    // 单套餐厂商允许省略 plan_id（UI 自动选中，手工调用也可留空）。
    let plan = if plan_id.is_empty() && vendor.plans.len() == 1 {
        vendor.plans.into_iter().next()?
    } else {
        vendor.plans.into_iter().find(|x| x.id == plan_id)?
    };
    Some((provider_id(vendor_id, &plan.id), plan))
}

/// provider id → 环境变量名：大写，非字母数字转 `_`。
pub fn env_name(id: &str) -> String {
    let mut s = String::from("BUFFET_");
    for c in id.chars() {
        if c.is_ascii_alphanumeric() {
            s.extend(c.to_uppercase());
        } else {
            s.push('_');
        }
    }
    s.push_str("_API_KEY");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_env_names() {
        assert_eq!(provider_id("deepseek", "api"), "deepseek");
        assert_eq!(provider_id("opencode", "zen"), "opencode-zen");
        assert_eq!(provider_id("bailian", "token-plan"), "bailian-token-plan");
        assert_eq!(
            env_name("bailian-token-plan"),
            "BUFFET_BAILIAN_TOKEN_PLAN_API_KEY"
        );
    }

    #[test]
    fn lookup_hits_base_url() {
        let (id, plan) = lookup("opencode", "go").expect("exists");
        assert_eq!(id, "opencode-go");
        assert_eq!(plan.base_url, "https://opencode.ai/zen/go/v1");
        assert!(lookup("nope", "api").is_none());
        assert!(lookup("deepseek", "nope").is_none());
        // 单套餐厂商：空 plan 自动命中唯一套餐
        let (id, _) = lookup("openrouter", "").expect("sole plan");
        assert_eq!(id, "openrouter");
        assert!(lookup("opencode", "").is_none());
    }
}
