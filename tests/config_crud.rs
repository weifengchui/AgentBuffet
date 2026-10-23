//! 表单化配置 CRUD + 加密备份 API：group/agent/provider/gateway 端点与
//! export-enc/import-enc 闭环。全部走 loopback 守卫、校验失败不落盘范式。

use agentbuffet::config;
use agentbuffet::proxy::AppState;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use http::StatusCode;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("buffet-crud-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const CFG: &str = r#"gateway:
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
  - { id: hermes, default_group: g }
"#;

fn state_for(tag: &str) -> (Arc<AppState>, PathBuf) {
    let dir = temp_dir(tag);
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, CFG).unwrap();
    std::fs::write(dir.join("env"), "K1=secret-value\n").unwrap();
    let snap = config::load_str(CFG, "test.yaml").expect("config ok");
    let st = AppState {
        home: dir.clone(),
        env_file: dir.join("env"),
        ..AppState::with_paths(snap, cfg, dir.join("logs"))
    };
    (Arc::new(st), dir)
}

const LB: [u8; 4] = [127, 0, 0, 1];

async fn call(
    st: Arc<AppState>,
    peer: [u8; 4],
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let app = agentbuffet::build_admin_router(st);
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from((peer, 53))))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        code,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

fn read(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("config.yaml")).unwrap()
}

#[tokio::test]
async fn group_upsert_and_delete() {
    let (st, dir) = state_for("grp");
    let body = serde_json::json!({"name": "g2", "members": [
        {"provider": "p1", "model": "m-a", "priority": 1}]});
    let (code, _) = call(st.clone(), LB, "/_admin/group", body).await;
    assert_eq!(code, StatusCode::OK);
    let text = read(&dir);
    assert!(text.contains("g2"), "{text}");
    // 更新同名组：先在 provider 声明 m-b，再换成员
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/provider/edit",
        serde_json::json!({
        "id": "p1", "models": ["m-a", "m-b"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let body = serde_json::json!({"name": "g2", "members": [
        {"provider": "p1", "model": "m-b", "priority": 1}]});
    let (code, _) = call(st.clone(), LB, "/_admin/group", body).await;
    assert_eq!(code, StatusCode::OK);
    assert!(read(&dir).contains("m-b"));
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/group/delete",
        serde_json::json!({"name": "g2"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(!read(&dir).contains("g2"));
    // 不存在的组删除 → 422
    let (code, _) = call(
        st,
        LB,
        "/_admin/group/delete",
        serde_json::json!({"name": "g2"}),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn group_member_any_model_when_provider_models_empty() {
    // OpenRouter 类服务商 models 为空 = 不限制模型；成员可用任意 model。
    let (st, dir) = state_for("em");
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/provider/edit",
        serde_json::json!({"id": "p1", "models": []}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    let body = serde_json::json!({"name": "g3", "members": [
        {"provider": "p1", "model": "x-ai/grok-4", "priority": 1}]});
    let (code, j) = call(st.clone(), LB, "/_admin/group", body).await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert!(read(&dir).contains("x-ai/grok-4"));
    // models 非空时仍强制成员 model 必须在声明列表内：先删 g3 再恢复声明
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/group/delete",
        serde_json::json!({"name": "g3"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/provider/edit",
        serde_json::json!({"id": "p1", "models": ["m-a"]}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    let body = serde_json::json!({"name": "g4", "members": [
        {"provider": "p1", "model": "not-declared", "priority": 1}]});
    let (code, _) = call(st, LB, "/_admin/group", body).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn provider_edit_and_cascade_delete() {
    let (st, dir) = state_for("pv");
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/provider/edit",
        serde_json::json!({
        "id": "p1", "models": ["m-a", "m-z"], "quota_daily": 5000}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let text = read(&dir);
    assert!(
        text.contains("m-z") && text.contains("daily_tokens: 5000"),
        "{text}"
    );
    // 编辑不存在的 provider → 422
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/provider/edit",
        serde_json::json!({
        "id": "nope", "models": []}),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    // hermes 还引用 g（g 唯一成员是 p1），级联删空 g 会让 agent 悬空 → 校验 422 不落盘
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/provider/delete",
        serde_json::json!({"id": "p1"}),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(read(&dir).contains("p1"), "失败不得落盘");
    // 先删 agent 再删 provider：g 空组级联删除
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/agent/delete",
        serde_json::json!({"id": "hermes"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/provider/delete",
        serde_json::json!({"id": "p1"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    let text = read(&dir);
    assert!(
        !text.contains("p1") && !text.contains("- name: g"),
        "{text}"
    );
}

#[tokio::test]
async fn agent_upsert_validates_group() {
    let (st, dir) = state_for("ag");
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/agent",
        serde_json::json!({
        "id": "mytool", "display_name": "My Tool", "default_group": "nope"}),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/agent",
        serde_json::json!({
        "id": "mytool", "display_name": "My Tool", "default_group": "g"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let text = read(&dir);
    assert!(
        text.contains("mytool") && text.contains("My Tool"),
        "{text}"
    );
}

#[tokio::test]
async fn gateway_edit_addr_changed_flag() {
    let (st, dir) = state_for("gw");
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/gateway",
        serde_json::json!({
        "log_retain_days": 7}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["addr_changed"], false);
    assert!(read(&dir).contains("log_retain_days: 7"));
}

#[tokio::test]
async fn enc_export_import_roundtrip() {
    let (st, dir) = state_for("enc");
    // 非 loopback 拒绝
    let (code, _) = call(
        st.clone(),
        [10, 0, 0, 1],
        "/_admin/export-enc",
        serde_json::json!({"password": "pw"}),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/export-enc",
        serde_json::json!({"password": "pw123"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let blob = j["data_b64"].as_str().unwrap().to_string();
    assert!(j["filename"].as_str().unwrap().ends_with(".aback"));
    // 改坏配置后导入还原
    std::fs::write(dir.join("config.yaml"), "gateway:\n  addr: \"127.0.0.1:0\"\n  ui_addr: \"127.0.0.1:0\"\nproviders: []\nrouting_groups: []\nagents: []\n").unwrap();
    let (code, _) = call(
        st.clone(),
        LB,
        "/_admin/import-enc",
        serde_json::json!({"password": "wrong", "data_b64": blob}),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(read(&dir).contains("providers: []"), "错误密码不得写盘");
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/import-enc",
        serde_json::json!({"password": "pw123", "data_b64": blob}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert_eq!(j["env_keys"], 1);
    let text = read(&dir);
    assert!(text.contains("p1") && text.contains("m-a"), "{text}");
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(env.contains("K1=secret-value"), "{env}");
}

async fn call_get(st: Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = agentbuffet::build_admin_router(st);
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from((LB, 53))))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
        .await
        .unwrap();
    (
        code,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn provider_keys_add_remove() {
    std::env::set_var("K1", "secret-value");
    let (st, dir) = state_for("keys");
    // add：追加到逗号分隔值末尾
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/provider/keys",
        serde_json::json!({"id": "p1", "action": "add", "key": "second-key"}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["key_count"], 2);
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(env.contains("K1=secret-value,second-key"), "{env}");
    // remove index 0
    let (code, j) = call(
        st.clone(),
        LB,
        "/_admin/provider/keys",
        serde_json::json!({"id": "p1", "action": "remove", "index": 0}),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["key_count"], 1);
    assert!(std::fs::read_to_string(dir.join("env"))
        .unwrap()
        .contains("K1=second-key"));
    // 非法输入：含逗号 / 未知 provider / index 越界 / 未知 action
    for (body, want) in [
        (
            serde_json::json!({"id": "p1", "action": "add", "key": "a,b"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            serde_json::json!({"id": "nope", "action": "add", "key": "x"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"id": "p1", "action": "remove", "index": 9}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            serde_json::json!({"id": "p1", "action": "clear"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (code, _) = call(st.clone(), LB, "/_admin/provider/keys", body).await;
        assert_eq!(code, want);
    }
    // 非 loopback 一律 403
    let (code, _) = call(
        st,
        [10, 0, 0, 1],
        "/_admin/provider/keys",
        serde_json::json!({"id": "p1", "action": "add", "key": "x"}),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn agents_detect_finds_installed() {
    let (st, dir) = state_for("det");
    std::fs::create_dir_all(dir.join(".codex")).unwrap();
    std::fs::write(dir.join(".codex").join("config.toml"), "model = \"gpt\"\n").unwrap();
    let (code, j) = call_get(st.clone(), "/api/agents/detect").await;
    assert_eq!(code, StatusCode::OK);
    let arr = j.as_array().unwrap();
    assert_eq!(arr.len(), 4);
    let hm = arr.iter().find(|x| x["agent_id"] == "hermes").unwrap();
    assert_eq!(hm["adapter"], "hermes");
    let codex = arr.iter().find(|x| x["agent_id"] == "codex").unwrap();
    assert_eq!(codex["installed"], true);
    assert_eq!(codex["in_config"], false);
    let oc = arr.iter().find(|x| x["agent_id"] == "opencode").unwrap();
    assert_eq!(oc["installed"], false);
    let cc = arr.iter().find(|x| x["agent_id"] == "claude-code").unwrap();
    assert_eq!(cc["adapter"], "claude-code");
}
