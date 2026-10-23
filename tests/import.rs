//! `buffetd import magpie` 导入映射测试：产物必须过 config::load_str，
//! Key 只进 env_keys 不进 YAML，降级/跳过条目逐一警告。

use agentbuffet::import::import_magpie;

const MAGPIE_SAMPLE: &str = r#"{
  "providers": [
    { "id": "deepseek", "name": "DeepSeek", "key": "sk-ds-123",
      "chat": "https://api.deepseek.com/v1",
      "models": ["deepseek-chat", "deepseek-reasoner"] },
    { "id": "moonshot", "name": "Moonshot", "key": "sk-ms-456",
      "chat": "https://api.moonshot.cn/v1", "models": ["moonshot-v1-8k"] },
    { "id": "old-off", "key": "sk-x", "chat": "https://x.example/v1",
      "models": ["m"], "off": true },
    { "id": "sub-type", "key": "", "models": ["m"], "anthropic": "https://a.example" }
  ],
  "groups": [
    { "id": "coding-flash", "routing": "order",
      "members": ["deepseek/deepseek-chat", "moonshot/moonshot-v1-8k"] },
    { "id": "smart-pool", "routing": "smart",
      "members": ["deepseek/deepseek-chat:high", "group/coding-flash", "ghost/m", "sub-type/m"] },
    { "id": "empty-ish", "members": ["group/coding-flash"] }
  ]
}"#;

#[test]
fn import_generates_valid_config() {
    let r = import_magpie(MAGPIE_SAMPLE).expect("import ok");
    let snap = agentbuffet::config::load_str(&r.yaml, "imported.yaml").expect("产物必须过校验");
    assert_eq!(snap.providers.len(), 2);
    assert_eq!(snap.groups.len(), 2);

    let ds = snap.providers.get("deepseek").unwrap();
    assert_eq!(ds.base_url, "https://api.deepseek.com/v1");
    assert_eq!(ds.api_key_env, "BUFFET_DEEPSEEK_API_KEY");
    assert_eq!(ds.models, vec!["deepseek-chat", "deepseek-reasoner"]);

    // 嵌套组成员按序映射 priority
    let g = snap.groups.get("coding-flash").unwrap();
    assert_eq!(g.strategy, "order");
    assert_eq!(g.members.len(), 2);
    assert_eq!(g.members[0].priority, 1);
    assert_eq!(g.members[1].provider, "moonshot");
}

#[test]
fn keys_only_in_env_never_in_yaml() {
    let r = import_magpie(MAGPIE_SAMPLE).expect("import ok");
    assert!(r.env_keys.contains(&(
        "BUFFET_DEEPSEEK_API_KEY".to_string(),
        "sk-ds-123".to_string()
    )));
    assert!(r.env_keys.contains(&(
        "BUFFET_MOONSHOT_API_KEY".to_string(),
        "sk-ms-456".to_string()
    )));
    assert!(!r.yaml.contains("sk-ds-123"));
    assert!(!r.yaml.contains("sk-ms-456"));
}

#[test]
fn skips_and_downgrades_are_warned() {
    let r = import_magpie(MAGPIE_SAMPLE).expect("import ok");
    let joined = r.warnings.join("\n");
    assert!(joined.contains("old-off"), "{joined}"); // off 供应商
    assert!(joined.contains("sub-type"), "{joined}"); // 无 chat base
    assert!(joined.contains("smart-pool"), "{joined}"); // smart → order 降级
    assert!(joined.contains("嵌套"), "{joined}"); // group/ 成员
    assert!(joined.contains("ghost/m"), "{joined}"); // 未导入供应商
    assert!(joined.contains("sub-type/m"), "{joined}"); // 未导入供应商（无 chat）
    assert!(joined.contains("empty-ish"), "{joined}"); // 无有效成员
                                                       // effort 后缀剥离：deepseek-chat:high 正常导入
    let snap = agentbuffet::config::load_str(&r.yaml, "x").unwrap();
    let g = snap.groups.get("smart-pool").unwrap();
    assert_eq!(g.members.len(), 1);
    assert_eq!(g.members[0].model, "deepseek-chat");
}

#[test]
fn deterministic_output() {
    let a = import_magpie(MAGPIE_SAMPLE).unwrap();
    let b = import_magpie(MAGPIE_SAMPLE).unwrap();
    assert_eq!(a.yaml, b.yaml);
}

#[test]
fn rejects_bad_input() {
    assert!(import_magpie("not json").is_err());
    assert!(import_magpie(r#"{"providers":[{"id":"a","chat":"https://x/v1","key":"k"},{"id":"a","chat":"https://y/v1"}]}"#).is_err());
    // 全部供应商不可用 → 明确报错而非落空配置
    assert!(import_magpie(r#"{"providers":[{"id":"a","models":["m"]}],"groups":[]}"#).is_err());
}
