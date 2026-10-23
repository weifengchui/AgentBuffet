//! 配置导入导出 API：GET /api/config 导出原文；POST /_admin/config 整份导入
//! （校验失败 422 不落盘；成功备份 .bak + 写盘 + 热重载）；仅 loopback。

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
    let d = std::env::temp_dir().join(format!("buffet-cfgio-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg_yaml(model: &str) -> String {
    format!(
        r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
providers:
  - id: p1
    vendor: mock
    base_url: "http://127.0.0.1:1/v1"
    api_key_env: K1
    allow_private: true
    models: [{model}]
routing_groups:
  - name: g
    strategy: order
    members:
      - {{ provider: p1, model: {model}, priority: 1 }}
agents:
  - {{ id: hermes, default_group: g }}
"#
    )
}

fn state_for(tag: &str, yaml: &str) -> (Arc<AppState>, PathBuf) {
    let dir = temp_dir(tag);
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, yaml).unwrap();
    let snap = config::load_str(yaml, "test.yaml").expect("config ok");
    let st = AppState::with_paths(snap, cfg, dir.join("logs"));
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
async fn export_returns_current_text() {
    let yaml = cfg_yaml("m-a");
    let (st, dir) = state_for("exp", &yaml);
    let (code, j) = call(st, LB, "GET", "/api/config", None).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(j["text"], yaml);
    assert!(j["path"]
        .as_str()
        .unwrap()
        .contains(&dir.file_name().unwrap().to_string_lossy().to_string()));
}

#[tokio::test]
async fn import_replaces_and_backups() {
    let (st, dir) = state_for("imp", &cfg_yaml("m-a"));
    let new_yaml = cfg_yaml("m-b");
    let (code, j) = call(
        st.clone(),
        LB,
        "POST",
        "/_admin/config",
        Some(serde_json::json!({"text": new_yaml})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{j}");
    assert_eq!(j["status"], "reloaded");
    // 文件已整份替换
    let on_disk = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert_eq!(on_disk, new_yaml);
    // 原内容备份到 .bak（已存在则不覆盖）
    let bak = std::fs::read_to_string(dir.join("config.yaml.bak")).unwrap();
    assert_eq!(bak, cfg_yaml("m-a"));
    // 新快照生效：模型换成 m-b
    let (code, j) = call(st, LB, "GET", "/api/config", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(j["text"].as_str().unwrap().contains("m-b"));
}

#[tokio::test]
async fn import_invalid_422_keeps_file() {
    let yaml = cfg_yaml("m-a");
    let (st, dir) = state_for("bad", &yaml);
    let (code, j) = call(
        st,
        LB,
        "POST",
        "/_admin/config",
        Some(serde_json::json!({"text": "gateway: not-an-object\n"})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{j}");
    assert!(j["details"].is_array());
    // 未写盘、无备份
    assert_eq!(
        std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
        yaml
    );
    assert!(!dir.join("config.yaml.bak").exists());
}

#[tokio::test]
async fn non_loopback_forbidden() {
    let (st, _) = state_for("guard", &cfg_yaml("m-a"));
    let (code, _) = call(st, [10, 0, 0, 1], "GET", "/api/config", None).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
}
