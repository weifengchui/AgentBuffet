//! 厂商快捷添加：GET /api/vendor-catalog 目录；POST /_admin/provider 选厂商+套餐+Key →
//! 追加/更新 config.yaml provider 条目 + Key 写 `<home>/env`（同名替换）+ 热重载。

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
    let d = std::env::temp_dir().join(format!("buffet-vend-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const CFG: &str = r#"
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
  - { id: hermes, default_group: g }
"#;

/// home 指向临时目录，避免测试写真实 `~/.agentbuffet/env`。
fn state_for(tag: &str) -> (Arc<AppState>, PathBuf) {
    let dir = temp_dir(tag);
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, CFG).unwrap();
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
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let app = agentbuffet::build_admin_router(st);
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

#[tokio::test]
async fn catalog_lists_vendors() {
    let (st, _d) = state_for("cat");
    let (code, j) = call(st, LB, "GET", "/api/vendor-catalog", None).await;
    assert_eq!(code, StatusCode::OK);
    let arr = j.as_array().unwrap();
    assert!(arr.iter().any(|v| v["id"] == "deepseek"));
    assert!(arr.iter().any(|v| v["id"] == "opencode"));
    // opencode 有两套餐（zen/go）
    let oc = arr.iter().find(|v| v["id"] == "opencode").unwrap();
    assert_eq!(oc["plans"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn add_writes_config_env_and_reloads() {
    let (st, dir) = state_for("add");
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/provider",
        Some(serde_json::json!({
            "vendor": "deepseek", "plan": "api", "api_key": "sk-test-1"
        })),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert_eq!(j["status"], "reloaded");
    assert_eq!(j["provider"], "deepseek");
    assert_eq!(j["api_key_env"], "BUFFET_DEEPSEEK_API_KEY");
    // config.yaml 追加了 provider 条目（含 base_url，不含 Key）
    let on_disk = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(on_disk.contains("api.deepseek.com/v1"));
    assert!(on_disk.contains("BUFFET_DEEPSEEK_API_KEY"));
    assert!(!on_disk.contains("sk-test-1"));
    // Key 只进 env 文件
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(env.contains("BUFFET_DEEPSEEK_API_KEY=sk-test-1"));
    // 原配置备份
    assert_eq!(
        std::fs::read_to_string(dir.join("config.yaml.bak")).unwrap(),
        CFG
    );
    // 快照生效：providers 视图含新 provider
    let (code, rows) = call(st, LB, "GET", "/api/providers", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "deepseek"));
}

#[tokio::test]
async fn readd_updates_key_without_duplicating() {
    let (st, dir) = state_for("upd");
    let body = |key: &str| {
        Some(serde_json::json!({
            "vendor": "opencode", "plan": "zen", "api_key": key
        }))
    };
    let (code, _) = call(st.clone(), LB, "POST", "/_admin/provider", body("k1")).await;
    assert_eq!(code, StatusCode::OK);
    let (code, j) = call(st, LB, "POST", "/_admin/provider", body("k2")).await;
    assert_eq!(code, StatusCode::OK, "{j}");
    let on_disk = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    // 同名 provider 只出现一次（多套餐厂商 id 带套餐名）
    assert_eq!(on_disk.matches("opencode-zen").count(), 1);
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(env.contains("BUFFET_OPENCODE_ZEN_API_KEY=k2"));
    assert!(!env.contains("=k1"));
}

#[tokio::test]
async fn invalid_inputs_422_keep_file() {
    let (st, dir) = state_for("bad");
    // 未知厂商
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/provider",
        Some(serde_json::json!({"vendor": "nope", "plan": "api", "api_key": "k"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    // 空 Key
    let (code, _) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/provider",
        Some(serde_json::json!({"vendor": "deepseek", "plan": "api", "api_key": "  "})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    // 均未写盘、无备份、无 env
    assert_eq!(
        std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
        CFG
    );
    assert!(!dir.join("config.yaml.bak").exists());
    assert!(!dir.join("env").exists());
}

#[tokio::test]
async fn non_loopback_forbidden() {
    let (st, _d) = state_for("guard");
    let (code, _) = call(
        st,
        [10, 0, 0, 1],
        "POST",
        "/_admin/provider",
        Some(serde_json::json!({"vendor": "deepseek", "plan": "api", "api_key": "k"})),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}
