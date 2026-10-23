//! M5 验收：适配器注入产物的 schema 校验（设计文档 §7/§11）。
//! OpenCode：注入后 JSON 必须满足 @ai-sdk/openai-compatible 契约——
//! provider.agentbuffet 必含 npm/options.baseURL(http(s))/options.apiKey 占位/models group/*，
//! model 指针格式为 agentbuffet/group/<g>。
//! Codex：注入后 TOML 必须可解析，model_providers.agentbuffet 含
//! name/base_url/wire_api="chat"，顶层 model_provider/model 合法。
//! 示例 config.yaml 必须过 config::load_str（README 示例与测试同源）。

use agentbuffet::{config, onboard};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("buffet-schema-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn target(agent: &str) -> onboard::Target {
    onboard::Target {
        agent_id: agent.into(),
        default_group: "coding-flash".into(),
        groups: vec!["cheap".into(), "coding-flash".into()],
        models: vec!["deepseek-chat".into(), "gpt-4o-mini".into()],
        gateway_addr: "127.0.0.1:3425".into(),
        adapter: String::new(),
    }
}

fn write_file(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// ---------------------------------------------------------------- OpenCode

#[test]
fn opencode_injected_json_schema() {
    let home = temp_dir("oc");
    write_file(
        &home.join(".config/opencode/opencode.json"),
        "{\n  \"theme\": \"gruvbox\"\n}\n",
    );
    let t = target("opencode");
    let r = onboard::apply(&home, &t).expect("apply ok");
    assert!(r.note.contains("已注入"), "note: {}", r.note);

    let text = std::fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).expect("注入后仍是合法 JSON");

    // 用户原有字段保留
    assert_eq!(doc["theme"], serde_json::json!("gruvbox"));

    let p = &doc["provider"]["agentbuffet"];
    assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(p["name"], "AgentBuffet");

    let base = p["options"]["baseURL"].as_str().expect("baseURL");
    assert!(
        base.starts_with("http://") || base.starts_with("https://"),
        "baseURL: {base}"
    );
    assert_eq!(base, "http://127.0.0.1:3425/opencode/v1");

    // 永不写入真实 Key：只允许占位符
    assert_eq!(p["options"]["apiKey"], onboard::KEY_PLACEHOLDER);

    let models = p["models"].as_object().expect("models 对象");
    assert!(
        models.contains_key("group/coding-flash"),
        "models: {models:?}"
    );
    assert!(models.contains_key("group/cheap"), "models: {models:?}");
    // 真实模型必须枚举：OpenCode 本地校验会拒绝未声明的 model ID
    assert!(models.contains_key("deepseek-chat"), "models: {models:?}");
    assert!(models.contains_key("gpt-4o-mini"), "models: {models:?}");

    // model 指针格式：provider/group/<组名>
    assert_eq!(doc["model"], "agentbuffet/group/coding-flash");

    // 幂等：重复 apply 文本逐字节一致，不叠加
    let r2 = onboard::apply(&home, &t).expect("second apply ok");
    assert!(r2.note.contains("无需变更"), "note: {}", r2.note);
    let text2 = std::fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap();
    assert_eq!(text, text2);
}

#[test]
fn opencode_drifted_when_models_added() {
    // 组内新增真实模型后，旧注入文件应检出漂移，重新 attach 后新模型可选。
    let home = temp_dir("oc-drift");
    write_file(
        &home.join(".config/opencode/opencode.json"),
        "{\n  \"theme\": \"catppuccin\"\n}\n",
    );
    let t = target("opencode");
    onboard::apply(&home, &t).expect("apply ok");
    let mut models = t.models.clone();
    models.push("new-model".into());
    models.sort();
    let t2 = onboard::Target {
        models,
        ..t.clone()
    };
    let st = onboard::detect(&home, &t2);
    assert!(st.attached && st.drifted, "status: {st:?}");
    onboard::apply(&home, &t2).expect("re-apply ok");
    let text = std::fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let models = doc["provider"]["agentbuffet"]["models"]
        .as_object()
        .unwrap();
    assert!(models.contains_key("new-model"), "models: {models:?}");
    assert!(models.contains_key("deepseek-chat"), "models: {models:?}");
}

#[test]
fn opencode_schema_after_user_edited_other_fields() {
    // 用户改过其他字段后重新 attach，注入块 schema 仍成立且漂移可检出。
    let home = temp_dir("oc2");
    write_file(
        &home.join(".config/opencode/opencode.json"),
        "{\n  \"theme\": \"tokyo-night\"\n}\n",
    );
    let t = target("opencode");
    onboard::apply(&home, &t).expect("apply ok");

    // 路由组集合变了（baseURL 仍属本网关）→ 漂移
    let t2 = onboard::Target {
        groups: vec!["cheap".into(), "coding-flash".into(), "long-ctx".into()],
        ..t.clone()
    };
    let st = onboard::detect(&home, &t2);
    assert!(st.attached && st.drifted, "status: {st:?}");
    onboard::apply(&home, &t2).expect("re-apply ok");

    let text = std::fs::read_to_string(home.join(".config/opencode/opencode.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let models = doc["provider"]["agentbuffet"]["models"]
        .as_object()
        .unwrap();
    assert!(models.contains_key("group/long-ctx"), "models: {models:?}");
    assert_eq!(
        doc["provider"]["agentbuffet"]["options"]["apiKey"],
        onboard::KEY_PLACEHOLDER
    );
}

// ---------------------------------------------------------------- Codex

#[test]
fn codex_injected_toml_schema() {
    let home = temp_dir("cx");
    write_file(
        &home.join(".codex/config.toml"),
        "# 用户原有配置\nmodel = \"gpt-4o\"\n\n[shell]\nenabled = false\n",
    );
    let t = target("codex");
    let r = onboard::apply(&home, &t).expect("apply ok");
    // 用户自定义了顶层 model：不覆盖，只注入 provider 表块并提示
    assert!(r.note.contains("未覆盖"), "note: {}", r.note);

    let text = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    let v: toml::Value = text.parse().expect("注入后仍是合法 TOML");

    let p = v
        .get("model_providers")
        .and_then(|m| m.get("agentbuffet"))
        .expect("model_providers.agentbuffet 存在");
    assert_eq!(p.get("name").and_then(|x| x.as_str()), Some("AgentBuffet"));
    assert_eq!(
        p.get("base_url").and_then(|x| x.as_str()),
        Some("http://127.0.0.1:3425/codex/v1")
    );
    assert_eq!(p.get("wire_api").and_then(|x| x.as_str()), Some("chat"));

    // 用户顶层 model 未被改写
    assert_eq!(v.get("model").and_then(|x| x.as_str()), Some("gpt-4o"));
    // 用户原有表块保留
    assert!(v.get("shell").is_some(), "shell 表块应保留");
}

#[test]
fn codex_injected_toml_defaults_when_user_has_none() {
    let home = temp_dir("cx2");
    write_file(
        &home.join(".codex/config.toml"),
        "[shell]\nenabled = true\n",
    );
    let t = target("codex");
    let r = onboard::apply(&home, &t).expect("apply ok");
    assert!(r.note.contains("已注入"), "note: {}", r.note);

    let text = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    let v: toml::Value = text.parse().expect("注入后仍是合法 TOML");

    // 顶层 defaults 合法：model_provider 指向 agentbuffet，model 为 group/<g>
    assert_eq!(
        v.get("model_provider").and_then(|x| x.as_str()),
        Some("agentbuffet")
    );
    assert_eq!(
        v.get("model").and_then(|x| x.as_str()),
        Some("group/coding-flash")
    );

    let p = v
        .get("model_providers")
        .and_then(|m| m.get("agentbuffet"))
        .expect("provider 表块存在");
    assert_eq!(p.get("wire_api").and_then(|x| x.as_str()), Some("chat"));
    assert_eq!(
        p.get("base_url").and_then(|x| x.as_str()),
        Some("http://127.0.0.1:3425/codex/v1")
    );

    // 幂等：重复 apply 不叠加
    let r2 = onboard::apply(&home, &t).expect("second apply ok");
    assert!(r2.note.contains("无需变更"), "note: {}", r2.note);
    let text2 = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert_eq!(text, text2);
}

// ---------------------------------------------------------------- Generic

#[test]
fn generic_env_template_shape() {
    let home = temp_dir("gen");
    let t = target("my-ide");
    let r = onboard::apply(&home, &t).expect("apply ok");
    assert!(r.files.is_empty(), "generic 不写文件");
    let tpl = r.env_template.expect("模板存在");
    assert!(
        tpl.contains("export OPENAI_BASE_URL=\"http://127.0.0.1:3425/my-ide/v1\""),
        "{tpl}"
    );
    assert!(
        tpl.contains("export OPENAI_API_KEY=\"agentbuffet\""),
        "{tpl}"
    );
}

// ---------------------------------------------------------------- 示例配置

/// README/文档示例配置，必须过 load_str（schema 测试与文档同源）。
pub const EXAMPLE_CONFIG: &str = r#"
gateway:
  addr: "127.0.0.1:3425"
  ui_addr: "127.0.0.1:3426"
providers:
  - id: deepseek
    vendor: deepseek
    base_url: "https://api.deepseek.com/v1"
    api_key_env: DEEPSEEK_API_KEY
    models: [deepseek-chat, deepseek-reasoner]
    quota:
      daily_tokens: 1000000
  - id: moonshot
    vendor: moonshot
    base_url: "https://api.moonshot.cn/v1"
    api_key_env: MOONSHOT_API_KEY
    models: [moonshot-v1-8k]
routing_groups:
  - name: coding-flash
    strategy: order
    members:
      - { provider: deepseek, model: deepseek-chat, priority: 1 }
      - { provider: moonshot, model: moonshot-v1-8k, priority: 2 }
  - name: cheap
    strategy: order
    members:
      - { provider: moonshot, model: moonshot-v1-8k, priority: 1 }
agents:
  - { id: opencode, default_group: coding-flash }
  - { id: codex, default_group: coding-flash }
  - { id: hermes, default_group: cheap }
failover:
  max_retries: 3
  timeouts:
    connect_seconds: 5
    first_byte_seconds: 60
    stream_idle_seconds: 120
"#;

#[test]
fn example_config_passes_load_str() {
    let snap = config::load_str(EXAMPLE_CONFIG, "example.yaml").expect("示例配置应通过校验");
    assert_eq!(snap.providers.len(), 2);
    assert_eq!(snap.groups.len(), 2);
    assert_eq!(snap.agents.len(), 3);
    assert_eq!(snap.raw.failover.max_retries, 3);
}
