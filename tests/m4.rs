//! M4 验收：接入器（备份/幂等/同名拒绝/逐字节还原/漂移/通用模板）、
//! 管理面 JSON 端点形状、attach/detach 往返、改路由写盘+热重载、连通性测试、非 loopback 403。

use agentbuffet::{
    build_admin_router, config, failover::now_epoch_secs, onboard, proxy::AppState, usage::Event,
};
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::routing::get;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower::ServiceExt;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    // 隔离：scope 枚举不再包含 WSL / Windows 侧的真实用户目录，测试既不会
    // 改写用户真实配置，也不会读到它们。
    onboard::enable_isolation_for_tests();
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("buffet-m4-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn target(agent: &str, addr: &str) -> onboard::Target {
    onboard::Target {
        agent_id: agent.into(),
        default_group: "g".into(),
        groups: vec!["g".into()],
        models: vec!["m1".into(), "m2".into()],
        gateway_addr: addr.into(),
        adapter: String::new(),
    }
}

fn m4_config(base1: &str, base2: &str) -> String {
    format!(
        r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
providers:
  - {{ id: p1, vendor: mock, base_url: "{base1}", api_key_env: BUFFET_M4_KEY, models: [m1], quota: {{ daily_tokens: 1000 }}, allow_private: true }}
  - {{ id: p2, vendor: mock, base_url: "{base2}", api_key_env: BUFFET_M4_KEY2, models: [m2], allow_private: true }}
routing_groups:
  - name: g
    strategy: order
    members:
      - {{ provider: p1, model: m1, priority: 1 }}
      - {{ provider: p2, model: m2, priority: 2 }}
agents:
  - {{ id: opencode, default_group: g }}
  - {{ id: hermes, default_group: g }}
  - {{ id: claude-code, default_group: g }}
"#
    )
}

/// 构造带配置文件与隔离 home 的 AppState（不监听端口，用 oneshot 调用）。
fn state_for(tag: &str, home: PathBuf) -> (Arc<AppState>, PathBuf) {
    let dir = temp_dir(tag);
    let cfg = dir.join("config.yaml");
    let yaml = m4_config("http://127.0.0.1:1/v1", "http://127.0.0.1:2/v1");
    std::fs::write(&cfg, &yaml).unwrap();
    let snap = config::load_str(&yaml, "test.yaml").expect("config ok");
    let mut st = AppState::with_paths(snap, cfg, dir.join("logs"));
    st.home = home;
    (Arc::new(st), dir)
}

const LB: [u8; 4] = [127, 0, 0, 1];

async fn call(
    st: Arc<AppState>,
    peer: [u8; 4],
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let app = build_admin_router(st);
    let b = Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from((peer, 53))));
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::String(
        String::from_utf8_lossy(&bytes).into_owned(),
    ));
    (code, json)
}

// ---------------------------------------------------------------- 接入器直调

#[test]
fn opencode_attach_idempotent_and_detach_byte_identical() {
    let home = temp_dir("oc-attach");
    let p = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let orig =
        "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"theme\": \"gruvbox\"\n}\n";
    std::fs::write(&p, orig).unwrap();

    let t = target("opencode", "127.0.0.1:3425");
    let r = onboard::apply(&home, &t).unwrap();
    assert_eq!(r.files.len(), 1);
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("http://127.0.0.1:3425/opencode/v1"));
    assert!(text.contains("\"apiKey\": \"agentbuffet\""));
    assert!(text.contains("\"m1\""), "真实模型必须注入");
    assert!(text.contains("\"theme\""));
    // 备份 = 原始逐字节
    let bp = format!("{}{}", p.display(), onboard::BACKUP_SUFFIX);
    assert_eq!(std::fs::read(&bp).unwrap(), orig.as_bytes());

    // 幂等：二次 attach 不叠加、内容不变、备份不被覆盖
    let after1 = std::fs::read_to_string(&p).unwrap();
    let r2 = onboard::apply(&home, &t).unwrap();
    assert!(r2.note.contains("无需变更"), "{}", r2.note);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), after1);
    assert_eq!(after1.matches("@ai-sdk/openai-compatible").count(), 1); // 注入块未叠加
    assert_eq!(std::fs::read(&bp).unwrap(), orig.as_bytes());

    // detach：逐字节还原
    let r3 = onboard::revert(&home, "opencode", "").unwrap();
    assert!(r3.note.contains("逐字节"), "{}", r3.note);
    assert_eq!(std::fs::read(&p).unwrap(), orig.as_bytes());
}

#[test]
fn opencode_rejects_user_defined_provider() {
    let home = temp_dir("oc-reject");
    let p = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let orig = format!(
        "{}",
        serde_json::json!({
            "provider": {"agentbuffet": {
                "npm": "@ai-sdk/openai",
                "options": {"baseURL": "https://api.user.test/v1", "apiKey": "sk-user"}
            }}
        })
    );
    std::fs::write(&p, &orig).unwrap();
    let err = onboard::apply(&home, &target("opencode", "127.0.0.1:3425")).unwrap_err();
    assert!(err.contains("拒绝写入"), "{err}");
    assert_eq!(std::fs::read(&p).unwrap(), orig.as_bytes());
}

#[test]
fn opencode_detach_without_backup_removes_only_provider() {
    let home = temp_dir("oc-nobak");
    let p = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "{\n  \"theme\": \"gruvbox\"\n}\n").unwrap();
    onboard::apply(&home, &target("opencode", "127.0.0.1:3425")).unwrap();
    std::fs::remove_file(format!("{}{}", p.display(), onboard::BACKUP_SUFFIX)).unwrap();

    let r = onboard::revert(&home, "opencode", "").unwrap();
    assert!(r.note.contains("仅删除注入的 provider"), "{}", r.note);
    let text = std::fs::read_to_string(&p).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    // provider 注入块被删；其余字段（含 model 指针）按设计保持现状
    assert!(doc
        .get("provider")
        .and_then(|x| x.get("agentbuffet"))
        .is_none());
    assert_eq!(doc["theme"], "gruvbox");
}

#[test]
fn codex_attach_layout_and_detach_byte_identical() {
    let home = temp_dir("cx-attach");
    let p = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let orig = "history = { save = true }\n\n[features]\nexperimental = true\n";
    std::fs::write(&p, orig).unwrap();

    let t = target("codex", "127.0.0.1:3425");
    let r = onboard::apply(&home, &t).unwrap();
    assert!(r.note.contains("已注入"), "{}", r.note);
    let text = std::fs::read_to_string(&p).unwrap();
    let parsed: toml::Value = text.parse().expect("注入后 TOML 仍可解析");
    assert_eq!(
        parsed["model_providers"]["agentbuffet"]["base_url"].as_str(),
        Some("http://127.0.0.1:3425/codex/v1")
    );
    assert_eq!(parsed["model_provider"].as_str(), Some("agentbuffet"));
    assert_eq!(parsed["model"].as_str(), Some("group/g"));
    // 顶层键必须先于所有 [table]（TOML 语法要求），provider 表块在文末
    let i_model = text.find("model_provider = ").unwrap();
    let i_feat = text.find("[features]").unwrap();
    let i_tbl = text.find("# >>> agentbuffet:begin").unwrap();
    assert!(i_model < i_feat && i_feat < i_tbl);

    let r2 = onboard::revert(&home, "codex", "").unwrap();
    assert!(r2.note.contains("逐字节"), "{}", r2.note);
    assert_eq!(std::fs::read(&p).unwrap(), orig.as_bytes());
}

/// 用户自定义了顶层 model/model_provider 时，AgentBuffet 仍然原地接管
/// （最高优先级）——不再像以前那样跳过并让用户手动改。
#[test]
fn codex_user_top_model_is_taken_over() {
    let home = temp_dir("cx-usertop");
    let p = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let orig = "model = \"gpt-5\"\nmodel_provider = \"openai\"\n\n[features]\nx = 1\n";
    std::fs::write(&p, orig).unwrap();

    let r = onboard::apply(&home, &target("codex", "127.0.0.1:3425")).unwrap();
    assert!(r.note.contains("已接管"), "{}", r.note);
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        !text.contains("agentbuffet:defaults"),
        "接管后不应再有 defaults 块（会与顶层键构成 TOML 重复键）"
    );
    assert!(text.contains("[model_providers.agentbuffet]"));
    let parsed: toml::Value = text.parse().unwrap();
    assert_eq!(parsed["model_provider"].as_str(), Some("agentbuffet"));
    assert_eq!(parsed["model"].as_str(), Some("group/g"));

    let r2 = onboard::revert(&home, "codex", "").unwrap();
    assert!(r2.note.contains("逐字节"), "{}", r2.note);
    assert_eq!(std::fs::read(&p).unwrap(), orig.as_bytes());
}

/// 最高优先级：外部把顶层键改回别的 provider 后，应判为漂移并能抢回。
#[test]
fn codex_takeover_is_reasserted_after_external_edit() {
    let home = temp_dir("cx-priority");
    let p = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "model_provider = \"openai\"\nmodel = \"gpt-5\"\n").unwrap();

    let t = target("codex", "127.0.0.1:3425");
    onboard::apply(&home, &t).unwrap();

    // 模拟外部工具改回原来的 provider
    let text = std::fs::read_to_string(&p).unwrap();
    std::fs::write(
        &p,
        text.replace(
            "model_provider = \"agentbuffet\"",
            "model_provider = \"openai\"",
        ),
    )
    .unwrap();

    // 判定为漂移（顶层 provider 不再指向 AgentBuffet）
    let s = onboard::detect(&home, &t);
    assert!(s.attached, "注入块仍在，应算已接入");
    assert!(s.drifted, "顶层键被改回，应判为漂移");

    // 巡检/重新接入把它抢回来
    onboard::apply(&home, &t).unwrap();
    let parsed: toml::Value = std::fs::read_to_string(&p).unwrap().parse().unwrap();
    assert_eq!(parsed["model_provider"].as_str(), Some("agentbuffet"));
    assert_eq!(parsed["model"].as_str(), Some("group/g"));
}

/// 接管顶层键时保留行尾注释与缩进，不破坏用户原有书写风格。
#[test]
fn codex_takeover_preserves_comment_and_indent() {
    let home = temp_dir("cx-comment");
    let p = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "model_provider = \"openai\" # 用户的注释\n").unwrap();

    onboard::apply(&home, &target("codex", "127.0.0.1:3425")).unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        text.contains("model_provider = \"agentbuffet\" # 用户的注释"),
        "行尾注释应保留: {text}"
    );
}

/// 巡检只覆盖**已登记**的 agent：未登记的实例不会被自动改写。
///
/// 这既是权限边界（不碰用户没要求接入的环境），也是一道安全保证——巡检绝不会
/// 因为扫到别的环境就把用户真实配置改掉（测试里若越界，会把测试端口写进用户
/// 真实的 WSL 配置，表现为"接入后一直漂移"）。
#[tokio::test]
async fn repair_drifted_skips_unregistered_instances() {
    let home = temp_dir("repair-unreg");
    let p = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // 已接入，但 base_url 指向别的网关（旧方案/被改旧）→ 漂移
    std::fs::write(
        &p,
        "model_provider = \"agentbuffet\"\nmodel = \"group/g\"\n\n\
         # >>> agentbuffet:begin\n[model_providers.agentbuffet]\nname = \"AgentBuffet\"\n\
         base_url = \"http://127.0.0.1:9999/codex/v1\"\nwire_api = \"chat\"\n# <<< agentbuffet:end\n",
    )
    .unwrap();

    // m4_config 的 agents 里没有 codex —— 未登记实例
    let (st, _d) = state_for("repair-unreg", home.clone());
    assert!(!st.snap.read().await.agents.contains_key("codex"));

    let fixed = agentbuffet::repair_drifted(&st).await;
    assert!(
        !fixed.contains(&"codex".to_string()),
        "未登记实例不应被巡检自动修复: {fixed:?}"
    );

    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        text.contains("127.0.0.1:9999"),
        "未登记实例的配置不应被改写: {text}"
    );
}

/// 已登记的实例即使被外部改回，巡检也会把它抢回来（最高优先级）。
#[tokio::test]
async fn repair_drifted_reasserts_registered_instance() {
    let home = temp_dir("repair-reg");
    let p = home.join(".codex").join("config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // 已接入，但 base_url 指向别的网关（旧方案/被改旧）→ 漂移
    std::fs::write(
        &p,
        "model_provider = \"agentbuffet\"\nmodel = \"group/g\"\n\n\
         # >>> agentbuffet:begin\n[model_providers.agentbuffet]\nname = \"AgentBuffet\"\n\
         base_url = \"http://127.0.0.1:9999/codex/v1\"\nwire_api = \"chat\"\n# <<< agentbuffet:end\n",
    )
    .unwrap();

    // 配置里登记 codex（m4_config 没有 → 手工补一条）
    let (st, dir) = state_for("repair-reg", home.clone());
    let cfg = dir.join("config.yaml");
    let mut yaml = std::fs::read_to_string(&cfg).unwrap();
    yaml.push_str("  - { id: codex, default_group: g }\n");
    std::fs::write(&cfg, &yaml).unwrap();
    let snap = config::load_str(&yaml, "test.yaml").expect("config ok");
    *st.snap.write().await = Arc::new(snap);

    let fixed = agentbuffet::repair_drifted(&st).await;
    assert!(
        fixed.contains(&"codex".to_string()),
        "已登记实例应被修复: {fixed:?}"
    );

    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        !text.contains("127.0.0.1:9999"),
        "base_url 应改回当前网关: {text}"
    );
}

/// 通用适配器（Generic）→ 422/400：协议不支持的 claude-code 必须拒绝。
#[test]
fn generic_attach_prints_template_without_files() {
    let home = temp_dir("generic");
    let r = onboard::apply(&home, &target("my-ide", "127.0.0.1:3425")).unwrap();
    let tpl = r.env_template.unwrap();
    assert!(tpl.contains("export OPENAI_BASE_URL=\"http://127.0.0.1:3425/my-ide/v1\""));
    assert!(tpl.contains("export OPENAI_API_KEY=\"agentbuffet\""));
    assert!(r.files.is_empty());
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    let r2 = onboard::revert(&home, "my-ide", "").unwrap();
    assert!(r2.note.contains("无需还原"), "{}", r2.note);
}

#[test]
fn drift_after_gateway_addr_change() {
    let home = temp_dir("drift");
    let p = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "{}\n").unwrap();
    onboard::apply(&home, &target("opencode", "127.0.0.1:3425")).unwrap();

    let s = onboard::detect(&home, &target("opencode", "127.0.0.1:9999"));
    assert!(s.attached && s.drifted, "{s:?}");

    // 重新 attach 修复漂移
    onboard::apply(&home, &target("opencode", "127.0.0.1:9999")).unwrap();
    let s2 = onboard::detect(&home, &target("opencode", "127.0.0.1:9999"));
    assert!(s2.attached && !s2.drifted, "{s2:?}");
}

// ---------------------------------------------------------------- 多环境隔离

/// 同名 Agent 在不同环境（scope）里是**独立实例**：配置各写各的，互不覆盖。
///
/// 这是 Magpie 的核心语义（`codex@wsl:Ubuntu` vs `codex`），本项目沿用。
#[test]
fn same_agent_in_two_scopes_writes_separate_configs() {
    let home = temp_dir("scope-split");
    // 本机侧：装 opencode
    let oc = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(oc.parent().unwrap()).unwrap();
    std::fs::write(&oc, "{}\n").unwrap();

    // 本机实例与"另一个环境"的实例，base_url 里的路径前缀必须不同
    let t_native = target("opencode", "127.0.0.1:3425");

    let r1 = onboard::apply(&home, &t_native).unwrap();
    assert!(r1.files[0].path.contains("opencode.json"), "{:?}", r1.files);

    // 适配器按 id 推断时必须忽略 @scope 后缀：两者都应是 opencode 适配器
    assert_eq!(
        onboard::Adapter::for_agent("opencode@wsl:Ubuntu"),
        onboard::Adapter::OpenCode
    );
    assert_eq!(
        onboard::Adapter::for_agent("opencode"),
        onboard::Adapter::OpenCode
    );

    // 基础类型剥离后一致，scope 不同 → id 不同
    assert_eq!(
        agentbuffet::scope::kind_of("opencode@wsl:Ubuntu"),
        "opencode"
    );
    assert_ne!(
        agentbuffet::scope::scope_of("opencode@wsl:Ubuntu"),
        agentbuffet::scope::scope_of("opencode")
    );
}

/// 环境不存在时（发行版已停止/卸载），写操作必须报错，**绝不能落到本机配置上**。
#[test]
fn unknown_scope_attach_is_refused() {
    let home = temp_dir("scope-missing");
    let oc = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(oc.parent().unwrap()).unwrap();
    let orig = "{}\n";
    std::fs::write(&oc, orig).unwrap();

    // wsl:NoSuchDistro 不在环境列表里 → 拒绝，且本机文件不能被改
    let t = target("opencode@wsl:NoSuchDistro", "127.0.0.1:3425");
    let err = onboard::apply(&home, &t).unwrap_err();
    assert!(err.contains("运行环境") || err.contains("wsl"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&oc).unwrap(),
        orig,
        "本机配置不能被改动"
    );
}

/// detect_all：只列装了 Agent 的环境实例，且每个实例带自己的 scope 信息。
#[test]
fn detect_all_reports_scope_per_instance() {
    let home = temp_dir("scope-detect");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex").join("config.toml"), "model = \"gpt\"\n").unwrap();

    let snap = config::load_str(
        &m4_config("http://127.0.0.1:1/v1", "http://127.0.0.1:2/v1"),
        "test.yaml",
    )
    .expect("config ok");
    let list = onboard::detect_all(&home, &snap);
    let codex = list
        .iter()
        .find(|a| a.agent_id == "codex")
        .expect("应检出本机的 codex");
    assert_eq!(codex.kind, "codex");
    assert_eq!(codex.scope_kind, "native");
    assert_eq!(codex.scope_id, "native");
    assert!(codex.installed);
    assert!(codex.config_path.as_deref().unwrap().contains(".codex"));
}

// ---------------------------------------------------------------- 管理面 HTTP

#[tokio::test]
async fn ui_index_served_and_non_loopback_forbidden() {
    let (st, _d) = state_for("ui", temp_dir("ui-home"));
    let (code, j) = call(st.clone(), LB, "GET", "/", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(j.as_str().unwrap().contains("AgentBuffet"));

    for (m, u, b) in [
        ("GET", "/", None),
        ("GET", "/api/overview", None),
        ("GET", "/api/providers", None),
        ("POST", "/_admin/reload", None),
        (
            "POST",
            "/api/agents/attach",
            Some(serde_json::json!({"agent_id": "hermes"})),
        ),
    ] {
        let (code, _) = call(st.clone(), [8, 8, 8, 8], m, u, b).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{m} {u}");
    }
}

#[tokio::test]
async fn api_json_shapes() {
    let (st, _d) = state_for("shape", temp_dir("shape-home"));
    let base = now_epoch_secs();
    if let Some(u) = &st.usage {
        for i in 0..25u64 {
            u.record(&Event {
                ts: base + i,
                agent: "hermes".into(),
                group: "g".into(),
                provider: "p1".into(),
                model: "m1".into(),
                status: 200,
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_est: None,
                duration_ms: 50,
                retries: 0,
                fail_chain: vec!["p1:ok".into()],
            });
        }
    }
    st.states.add_tokens("p1", 100, base);
    st.states.note_failure("p1", "500", base);

    let (code, j) = call(st.clone(), LB, "GET", "/api/overview", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["totals"]["requests"], 25);
    assert_eq!(j["date"].as_str().unwrap().len(), 10);
    let recent = j["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 20);
    assert_eq!(recent[0]["ts"], base + 5); // 取尾 20 且保持时间序
    assert_eq!(recent[19]["ts"], base + 24);

    let (code, j) = call(st.clone(), LB, "GET", "/api/providers", None).await;
    assert_eq!(code, StatusCode::OK);
    let arr = j.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let p1 = &arr[0];
    assert_eq!(p1["id"], "p1");
    assert_eq!(p1["status"], "ok");
    assert_eq!(p1["tokens_used"], 100);
    assert_eq!(p1["effective_cap"], 800); // 1000 × 0.8
    assert_eq!(p1["last_fail_reason"], "500");

    let (code, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(code, StatusCode::OK);
    let g = &j["groups"][0];
    assert_eq!(g["name"], "g");
    assert_eq!(g["members"][0]["provider"], "p1");
    assert_eq!(g["members"][0]["disabled"], false);

    let (code, j) = call(st.clone(), LB, "GET", "/api/agents", None).await;
    assert_eq!(code, StatusCode::OK);
    let arr = j.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    let hermes = arr.iter().find(|a| a["agent_id"] == "hermes").unwrap();
    assert_eq!(hermes["adapter"], "hermes");
    assert_eq!(hermes["attached"], false);
}

#[tokio::test]
async fn attach_detach_roundtrip_via_http() {
    let home = temp_dir("http-oc-home");
    let p = home.join(".config").join("opencode").join("opencode.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let orig = "{\n  \"theme\": \"gruvbox\"\n}\n";
    std::fs::write(&p, orig).unwrap();
    let (st, _d) = state_for("http-oc", home.clone());

    // 未登记的 agent（多环境实例常见）→ 200，并自动补登记进网关配置，
    // 让网关侧的 /<agent-id>/v1 前缀能对上。
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/api/agents/attach",
        Some(serde_json::json!({"agent_id": "nope"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    let (code, j) = call(st.clone(), LB, "GET", "/api/agents", None).await;
    assert_eq!(code, StatusCode::OK);
    let arr = j.as_array().unwrap();
    assert!(
        arr.iter().any(|a| a["agent_id"] == "nope"),
        "未登记的实例应被自动补登记: {arr:?}"
    );

    // dry-run 不落盘
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/api/agents/attach",
        Some(serde_json::json!({"agent_id": "opencode", "dry_run": true})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(j["note"].as_str().unwrap().contains("dry-run"));
    assert_eq!(std::fs::read(&p).unwrap(), orig.as_bytes());

    // claude-code → 422 拒绝
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/api/agents/attach",
        Some(serde_json::json!({"agent_id": "claude-code"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(j["error"].as_str().unwrap().contains("暂不支持"));

    // 实际 attach → 注入
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/api/agents/attach",
        Some(serde_json::json!({"agent_id": "opencode"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("agentbuffet"));

    // detach → 逐字节还原
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/api/agents/detach",
        Some(serde_json::json!({"agent_id": "opencode"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(j["note"].as_str().unwrap().contains("逐字节"));
    assert_eq!(std::fs::read(&p).unwrap(), orig.as_bytes());
}

#[tokio::test]
async fn routing_edit_writes_config_and_reloads() {
    let (st, dir) = state_for("route", temp_dir("route-home"));
    let cfg = dir.join("config.yaml");
    let orig = std::fs::read(&cfg).unwrap();

    // 停用 p1 → 写盘 + reload 生效
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({"provider": "p1", "disabled": true})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert_eq!(j["status"], "reloaded");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("disabled: true"), "{text}");
    assert!(dir.join("config.yaml.bak").exists());
    // 备份 = 原始逐字节
    assert_eq!(std::fs::read(dir.join("config.yaml.bak")).unwrap(), orig);

    let (_, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(j["groups"][0]["members"][0]["disabled"], true);
    let (_, j) = call(st.clone(), LB, "GET", "/api/providers", None).await;
    assert_eq!(j.as_array().unwrap()[0]["status"], "disabled");

    // 坏引用 → 422 且不写盘
    let before = std::fs::read(&cfg).unwrap();
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({
            "group": "g",
            "members": [{"provider": "ghost", "model": "m1", "priority": 1}]
        })),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(std::fs::read(&cfg).unwrap(), before);

    // 调序：p2 提到 priority 1
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({
            "group": "g",
            "members": [
                {"provider": "p1", "model": "m1", "priority": 2},
                {"provider": "p2", "model": "m2", "priority": 1}
            ]
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(j["groups"][0]["members"][0]["provider"], "p2");
}

#[tokio::test]
async fn routing_group_order_persists_and_views() {
    let home = temp_dir("gorder-home");
    let dir = temp_dir("gorder");
    let cfg = dir.join("config.yaml");
    // 追加一个名为 a2 的组：按配置顺序 g 在前，按字母序 a2 应在前，用于区分两种行为
    let yaml = m4_config("http://127.0.0.1:1/v1", "http://127.0.0.1:2/v1").replace(
        "\nagents:",
        "\n  - name: a2\n    strategy: order\n    members:\n      - { provider: p2, model: m2, priority: 1 }\nagents:",
    );
    std::fs::write(&cfg, &yaml).unwrap();
    let snap = config::load_str(&yaml, "test.yaml").expect("config ok");
    let mut st = AppState::with_paths(snap, cfg.clone(), dir.join("logs"));
    st.home = home;
    let st = Arc::new(st);

    // 默认按配置顺序返回
    let (_, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(j["groups"][0]["name"], "g");
    assert_eq!(j["groups"][1]["name"], "a2");

    // 坏组名 → 422 且不写盘
    let before = std::fs::read(&cfg).unwrap();
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({"order": ["ghost"]})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(std::fs::read(&cfg).unwrap(), before);

    // 重排：a2 提到第一
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({"order": ["a2", "g"]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert_eq!(j["status"], "reloaded");
    let (_, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(j["groups"][0]["name"], "a2");
    assert_eq!(j["groups"][1]["name"], "g");
    // 部分列表：未列出的组保持原有相对顺序排尾部
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({"order": ["g"]})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (_, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(j["groups"][0]["name"], "g");
    assert_eq!(j["groups"][1]["name"], "a2");
}

#[tokio::test]
async fn routing_edit_refreshes_drifted_agent_config() {
    let (st, _dir) = state_for("refresh", temp_dir("refresh-home"));
    // 预置 OpenCode 配置文件（apply 只改写已存在的配置）
    let oc = st.home.join(".config/opencode");
    std::fs::create_dir_all(&oc).unwrap();
    std::fs::write(
        oc.join("opencode.json"),
        "{\"$schema\": \"https://opencode.ai/config.json\"}",
    )
    .unwrap();
    // 以旧模型清单（仅 m1）接入 opencode → 相对当前配置（m1+m2）处于漂移
    let old = onboard::Target {
        agent_id: "opencode".into(),
        default_group: "g".into(),
        groups: vec!["g".into()],
        models: vec!["m1".into()],
        gateway_addr: "127.0.0.1:9".into(),
        adapter: String::new(),
    };
    onboard::apply(&st.home, &old).expect("attach ok");

    // 编辑路由组成员 → 热重载应自动重写漂移的接入器配置
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({
            "group": "g",
            "members": [
                {"provider": "p1", "model": "m1", "priority": 1},
                {"provider": "p2", "model": "m2", "priority": 2}
            ]
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    let refreshed = j["agents_refreshed"].as_array().unwrap();
    assert!(refreshed.iter().any(|x| x == "opencode"), "{j}");

    // 配置文件中已重新枚举 m2
    let p = st.home.join(".config/opencode/opencode.json");
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("m2"), "{text}");

    // 再次编辑（无漂移）→ 不重复重写
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({
            "group": "g",
            "members": [
                {"provider": "p1", "model": "m1", "priority": 1},
                {"provider": "p2", "model": "m2", "priority": 2}
            ]
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert!(j["agents_refreshed"].as_array().unwrap().is_empty(), "{j}");
}

#[tokio::test]
async fn member_disable_persists_and_shows_in_view() {
    let (st, dir) = state_for("mdis", temp_dir("mdis-home"));
    let cfg = dir.join("config.yaml");
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing",
        Some(serde_json::json!({
            "group": "g",
            "members": [
                {"provider": "p1", "model": "m1", "priority": 1, "disabled": true},
                {"provider": "p2", "model": "m2", "priority": 2}
            ]
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("disabled: true"), "{text}");
    let (_, j) = call(st.clone(), LB, "GET", "/api/routing", None).await;
    assert_eq!(j["groups"][0]["members"][0]["disabled"], true);
    assert_eq!(j["groups"][0]["members"][1]["disabled"], false);
}

#[tokio::test]
async fn multi_instance_opencode_uses_own_config_file() {
    let (st, dir) = state_for("mins", temp_dir("mins-home"));
    let oc = st.home.join(".config/opencode");
    std::fs::create_dir_all(&oc).unwrap();
    std::fs::write(oc.join("opencode.json"), "{}").unwrap();
    // 新建第二个实例 agent：opencode-work，显式复用 OpenCode 适配器
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/agent",
        Some(
            serde_json::json!({"id": "opencode-work", "default_group": "g", "adapter": "opencode"}),
        ),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/api/agents/attach",
        Some(serde_json::json!({"agent_id": "opencode-work"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    // 独立文件生成，且 base_url 带自身 agent 路径段
    let own = oc.join("opencode-work.json");
    let text = std::fs::read_to_string(&own).unwrap();
    assert!(text.contains("/opencode-work/v1"), "{text}");
    // 主实例文件未被覆盖
    assert!(!std::fs::read_to_string(oc.join("opencode.json"))
        .unwrap()
        .contains("buffet"));
    let _ = dir;
}

#[tokio::test]
async fn routing_test_endpoint_hits_upstream_models() {
    // mock 上游：GET /v1/models → 200
    let app = axum::Router::new().route(
        "/v1/models",
        get(|| async { (StatusCode::OK, "{\"object\":\"list\",\"data\":[]}") }),
    );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let dir = temp_dir("rtest");
    let cfg = dir.join("config.yaml");
    let yaml = m4_config(&format!("http://{addr}/v1"), &format!("http://{addr}/v1"));
    std::fs::write(&cfg, &yaml).unwrap();
    let snap = config::load_str(&yaml, "test.yaml").unwrap();
    let st = Arc::new(AppState::with_paths(snap, cfg, dir.join("logs")));

    // 环境变量缺失 → ok:false，且不回显 Key
    std::env::remove_var("BUFFET_M4_KEY");
    std::env::remove_var("BUFFET_M4_KEY2");
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing/test",
        Some(serde_json::json!({"provider": "p1"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["ok"], false);
    assert!(j["error"].as_str().unwrap().contains("环境变量"));

    std::env::set_var("BUFFET_M4_KEY", "sk-secret-123");
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing/test",
        Some(serde_json::json!({"provider": "p1"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["ok"], true, "{j}");
    assert_eq!(j["status"], 200);
    assert!(j["latency_ms"].is_number());

    // 未知 provider → 400
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/routing/test",
        Some(serde_json::json!({"provider": "ghost"})),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[test]
fn usage_recent_takes_tail_in_order() {
    let dir = temp_dir("recent");
    let u = agentbuffet::usage::Usage::new(dir.join("logs"), Path::new("/nonexistent-prices.yaml"));
    let base = now_epoch_secs();
    for i in 0..25u64 {
        u.record(&Event {
            ts: base + i,
            agent: "a".into(),
            group: "g".into(),
            provider: "p".into(),
            model: "m".into(),
            status: 200,
            prompt_tokens: 1,
            completion_tokens: 1,
            cost_est: None,
            duration_ms: 1,
            retries: 0,
            fail_chain: vec![],
        });
    }
    let r = u.recent(20);
    assert_eq!(r.len(), 20);
    assert_eq!(r[0].ts, base + 5);
    assert_eq!(r[19].ts, base + 24);
    assert!(u.recent(100).len() == 25);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hermes_attach_detect_revert() {
    let home = temp_dir("hm-attach");
    let p = home.join(".hermes").join("config.yaml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let orig = "model:\n  default: group/cheap-first\n  provider: magpie\n  base_url: https://nowcoding.ai/v1\n  api_key: sk-real-secret\nfallback_providers: []\ntoolsets:\n  - hermes-cli\n";
    std::fs::write(&p, orig).unwrap();

    let t = target("hermes", "127.0.0.1:3425");
    let s = onboard::detect(&home, &t);
    assert!(s.installed, "detect installed");
    assert!(!s.attached, "detect not attached");

    let r = onboard::apply(&home, &t).unwrap();
    assert_eq!(r.files.len(), 1);
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(
        text.contains("  base_url: http://127.0.0.1:3425/hermes/v1"),
        "{text}"
    );
    assert!(text.contains("  provider: agentbuffet"));
    assert!(text.contains("  api_key: agentbuffet"));
    assert!(text.contains("  default: agentbuffet/group/g"));
    // model 块之外的行逐字节保留
    assert!(text.contains("fallback_providers: []"));
    assert!(text.contains("  - hermes-cli"));
    assert!(!text.contains("sk-real-secret"));

    let s2 = onboard::detect(&home, &t);
    assert!(s2.attached && !s2.drifted, "attached & no drift");

    // 网关地址变化 → 漂移
    let s3 = onboard::detect(&home, &target("hermes", "127.0.0.1:9999"));
    assert!(s3.attached && s3.drifted, "drift detected");

    // 幂等：二次 attach 无变更、备份不被覆盖
    let bp = format!("{}{}", p.display(), onboard::BACKUP_SUFFIX);
    let r2 = onboard::apply(&home, &t).unwrap();
    assert!(r2.note.contains("无需变更"), "{}", r2.note);
    assert_eq!(std::fs::read(&bp).unwrap(), orig.as_bytes());

    // revert：逐字节还原
    let r3 = onboard::revert(&home, "hermes", "").unwrap();
    assert!(r3.note.contains("逐字节"), "{}", r3.note);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), orig);
}

#[test]
fn hermes_attach_inserts_missing_keys() {
    let home = temp_dir("hm-insert");
    let p = home.join(".hermes").join("config.yaml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "model:\n  default: x\nagent:\n  max_turns: 90\n").unwrap();
    let t = target("hermes", "127.0.0.1:3425");
    onboard::apply(&home, &t).unwrap();
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("  provider: agentbuffet"), "{text}");
    assert!(text.contains("  base_url: http://127.0.0.1:3425/hermes/v1"));
    assert!(text.contains("  api_key: agentbuffet"));
    assert!(text.contains("agent:\n  max_turns: 90"));
}
