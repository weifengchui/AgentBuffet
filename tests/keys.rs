//! 多 Key 路由：env 逗号分隔多 Key；401/403 换 Key 重试；全部 Key 鉴权失败才转移下一成员。

use agentbuffet::{build_router, config, proxy, proxy::AppState};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

#[derive(Clone)]
struct MockState {
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl MockState {
    fn hit(&self, key: &str) {
        *self
            .hits
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_default() += 1;
    }
    fn count(&self, key: &str) -> usize {
        *self.hits.lock().unwrap().get(key).unwrap_or(&0)
    }
}

/// mock provider：k-bad / k-bad2 一律 401，其余 Key 返回 200；按 (model, auth) 记命中。
async fn mock_chat(State(ms): State<MockState>, req: Request) -> Response {
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    ms.hit(&format!("{model}|{auth}"));
    if auth == "Bearer k-bad" || auth == "Bearer k-bad2" {
        return Response::builder()
            .status(401)
            .header("content-type", "application/json")
            .body(Body::from("{\"error\":{\"message\":\"bad key\"}}"))
            .unwrap();
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"object": "chat.completion", "echo_model": model})),
    )
        .into_response()
}

async fn spawn_mock() -> (String, MockState) {
    let ms = MockState {
        hits: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat))
        .with_state(ms.clone());
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), ms)
}

fn gw_config(providers: &str, members: &str) -> String {
    format!(
        r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
providers:
{providers}
routing_groups:
  - name: g
    strategy: order
    members:
{members}
agents:
  - {{ id: hermes, display_name: Hermes, default_group: g }}
failover:
  retry_on_status: [429, 500, 502, 503, 504]
  retry_on_timeout: true
  max_retries: 3
  timeouts: {{ connect_seconds: 2, first_byte_seconds: 2, stream_idle_seconds: 2 }}
  circuit_breaker: {{ window_seconds: 60, threshold: 5, open_seconds: 1 }}
"#
    )
}

async fn spawn_gateway(yaml: &str) -> String {
    let snap = config::load_str(yaml, "test.yaml").expect("config ok");
    let state = Arc::new(AppState::new(snap));
    let app = build_router(state);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

async fn chat(gw: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/hermes/v1/chat/completions"))
        .json(&serde_json::json!({"model": model, "messages": []}))
        .send()
        .await
        .unwrap()
}

fn chain_of(r: &reqwest::Response) -> String {
    r.headers()
        .get("x-buffet-failover-chain")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[test]
fn split_keys_basic() {
    assert_eq!(proxy::split_keys("  a , ,b "), vec!["a", "b"]);
    assert_eq!(proxy::split_keys("single"), vec!["single"]);
    assert!(proxy::split_keys("").is_empty());
    assert!(proxy::split_keys(" , ").is_empty());
}

#[tokio::test]
async fn bad_key_rotates_to_good_key_same_provider() {
    std::env::set_var("BUFFET_KM_ROT", "k-bad,k-good");
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &format!(
            "  - id: p1\n    vendor: mock\n    base_url: \"{mock}\"\n    api_key_env: BUFFET_KM_ROT\n    allow_private: true\n    models: [chat]"
        ),
        "      - { provider: p1, model: chat, priority: 1 }",
    );
    let gw = spawn_gateway(&yaml).await;
    let r = chat(&gw, "group/g").await;
    assert_eq!(r.status(), 200, "chain={}", chain_of(&r));
    // 无论轮转起点如何，k-good 必然被使用；若起点为坏 Key 则发生同服务商换 Key
    assert!(ms.count("chat|Bearer k-good") >= 1);
    let total = ms.count("chat|Bearer k-good") + ms.count("chat|Bearer k-bad");
    assert!(total >= 1);
}

#[tokio::test]
async fn all_keys_bad_fails_over_to_next_provider() {
    std::env::set_var("BUFFET_KM_ALLBAD", "k-bad,k-bad2");
    std::env::set_var("BUFFET_KM_OK", "k-good");
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &format!(
            "  - id: p1\n    vendor: mock\n    base_url: \"{mock}\"\n    api_key_env: BUFFET_KM_ALLBAD\n    allow_private: true\n    models: [chat]\n  - id: p2\n    vendor: mock\n    base_url: \"{mock}\"\n    api_key_env: BUFFET_KM_OK\n    allow_private: true\n    models: [chat]"
        ),
        "      - { provider: p1, model: chat, priority: 1 }\n      - { provider: p2, model: chat, priority: 2 }",
    );
    let gw = spawn_gateway(&yaml).await;
    let r = chat(&gw, "group/g").await;
    assert_eq!(r.status(), 200, "chain={}", chain_of(&r));
    // p1 的两个 Key 都试过才转移
    assert_eq!(ms.count("chat|Bearer k-bad"), 1);
    assert_eq!(ms.count("chat|Bearer k-bad2"), 1);
    assert!(ms.count("chat|Bearer k-good") >= 1);
    let chain = chain_of(&r);
    assert!(chain.contains("p1") && chain.contains("p2"), "{chain}");
}
