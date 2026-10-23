//! M1 验收：curl 通、SSE 不断流、group/ 路由、400 提示、Key 替换、SSRF 校验。

use agentbuffet::{build_router, config, proxy::AppState};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use std::sync::Arc;
use tokio::net::TcpListener;

/// mock provider：按 model 名分流行为，并回显收到的 auth/model 供断言。
async fn mock_chat(req: Request) -> Response {
    let headers = req.headers().clone();
    let auth = headers
        .get("authorization")
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
    let stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // 特殊 model 触发流内 error（200 + SSE error 载荷）
    if model.contains("sse-error") {
        let sse = "data: {\"error\":{\"message\":\"upstream boom\"}}\n\n";
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from(sse))
            .unwrap();
    }

    if stream {
        // 分块 SSE，验证零缓冲透传拼接完整
        let chunks = vec![
            format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"content":"He"}}],"echo_model":model,"echo_auth":auth})
            ),
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let stream = futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }

    // 非流式：回显 auth 与 model
    let payload = serde_json::json!({
        "object": "chat.completion",
        "echo_model": model,
        "echo_auth": auth,
    });
    (StatusCode::OK, axum::Json(payload)).into_response()
}

async fn spawn_mock() -> String {
    let app = Router::new().route("/v1/chat/completions", post(mock_chat));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/v1")
}

fn base_config(mock_url: &str) -> String {
    format!(
        r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
providers:
  - id: ds-1
    vendor: deepseek
    base_url: "{mock_url}"
    api_key_env: "BUFFET_TEST_KEY"
    models: ["deepseek-chat", "sse-error-chat"]
    allow_private: true
  - id: qwen-1
    vendor: qwen
    base_url: "{mock_url}"
    api_key_env: "BUFFET_TEST_KEY"
    models: ["qwen-plus"]
    allow_private: true
routing_groups:
  - name: coding-flash
    strategy: order
    members:
      - {{ provider: ds-1, model: deepseek-chat, priority: 1 }}
      - {{ provider: qwen-1, model: qwen-plus, priority: 2 }}
      - {{ provider: ds-1, model: sse-error-chat, priority: 3 }}
agents:
  - {{ id: hermes, display_name: Hermes, default_group: coding-flash }}
"#
    )
}

async fn spawn_gateway(yaml: &str) -> String {
    std::env::set_var("BUFFET_TEST_KEY", "sk-secret-123");
    let snap = config::load_str(yaml, "test.yaml").expect("config ok");
    let state = Arc::new(AppState::new(snap));
    let app = build_router(state);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = client.post(url).json(&body).send().await.unwrap();
    let status = r.status();
    let json: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn non_stream_forward_and_key_replaced() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let (status, json) = post_json(
        &client,
        &format!("{gw}/hermes/v1/chat/completions"),
        serde_json::json!({"model":"deepseek-chat","messages":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["echo_model"], "deepseek-chat");
    // Key 从环境变量注入并替换客户端 Authorization
    assert_eq!(json["echo_auth"], "Bearer sk-secret-123");
}

#[tokio::test]
async fn group_virtual_model_routes_to_first_priority() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let (status, json) = post_json(
        &client,
        &format!("{gw}/hermes/v1/chat/completions"),
        serde_json::json!({"model":"group/coding-flash","messages":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // group/ 命中 priority=1 成员，上游 model 被改写为 deepseek-chat
    assert_eq!(json["echo_model"], "deepseek-chat");
}

#[tokio::test]
async fn model_resolves_to_matching_member() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let (status, json) = post_json(
        &client,
        &format!("{gw}/hermes/v1/chat/completions"),
        serde_json::json!({"model":"qwen-plus","messages":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["echo_model"], "qwen-plus");
}

#[tokio::test]
async fn unknown_model_returns_400_with_hint() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let (status, json) = post_json(
        &client,
        &format!("{gw}/hermes/v1/chat/completions"),
        serde_json::json!({"model":"gpt-9","messages":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = json["error"]["message"].as_str().unwrap();
    assert!(msg.contains("gpt-9"), "应含未命中的 model 名: {msg}");
    assert!(msg.contains("coding-flash"), "应列出可用组: {msg}");
}

#[tokio::test]
async fn direct_v1_unknown_agent_needs_group() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    // /v1 直连无 agent，用具体 model 应 400
    let (status, _) = post_json(
        &client,
        &format!("{gw}/v1/chat/completions"),
        serde_json::json!({"model":"deepseek-chat","messages":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // 用 group/ 应命中
    let (status, json) = post_json(
        &client,
        &format!("{gw}/v1/chat/completions"),
        serde_json::json!({"model":"group/coding-flash","messages":[]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["echo_model"], "deepseek-chat");
}

#[tokio::test]
async fn sse_stream_passthrough_not_broken() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let r = client
        .post(format!("{gw}/hermes/v1/chat/completions"))
        .json(&serde_json::json!({"model":"deepseek-chat","stream":true,"messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let text = r.text().await.unwrap();
    // 三块都在，且 [DONE] 收尾 → 不断流
    assert!(text.contains("He"), "首块内容应透传: {text}");
    assert!(text.contains("llo"), "后续块应透传: {text}");
    assert!(
        text.trim_end().ends_with("[DONE]"),
        "应以 DONE 收尾: {text}"
    );
    assert!(text.contains("Bearer sk-secret-123"));
}

#[tokio::test]
async fn sse_error_detected_on_first_chunk() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let (status, json) = post_json(
        &client,
        &format!("{gw}/hermes/v1/chat/completions"),
        serde_json::json!({"model":"sse-error-chat","stream":true,"messages":[]}),
    )
    .await;
    // 首 chunk 是 SSE error → 网关转成 502，且不污染上下文
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("upstream boom"));
}

#[tokio::test]
async fn health_ok() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let r = client.get(format!("{gw}/health")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn models_lists_group_virtual_models() {
    let mock = spawn_mock().await;
    let gw = spawn_gateway(&base_config(&mock)).await;
    let client = reqwest::Client::new();
    let r = client
        .get(format!("{gw}/hermes/v1/models"))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = r.json().await.unwrap();
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"group/coding-flash"),
        "应含 group 虚拟模型: {ids:?}"
    );
    assert!(ids.contains(&"deepseek-chat"));
}

#[test]
fn ssrf_rejects_private_without_allow_private() {
    let yaml = r#"
gateway: { addr: "127.0.0.1:0", ui_addr: "127.0.0.1:0" }
providers:
  - id: local
    vendor: x
    base_url: "http://127.0.0.1:9999/v1"
    api_key_env: "K"
    models: ["m"]
routing_groups:
  - name: g
    members: [ { provider: local, model: m, priority: 1 } ]
"#;
    let errs = config::load_str(yaml, "t.yaml").unwrap_err();
    assert!(errs.iter().any(|e| e.contains("allow_private")), "{errs:?}");
}

#[test]
fn reserved_agent_id_rejected() {
    let yaml = r#"
gateway: { addr: "127.0.0.1:0", ui_addr: "127.0.0.1:0" }
providers:
  - { id: p, vendor: x, base_url: "https://x.test/v1", api_key_env: "K", models: ["m"], allow_private: true }
routing_groups:
  - { name: g, members: [ { provider: p, model: m, priority: 1 } ] }
agents:
  - { id: health, display_name: H, default_group: g }
"#;
    let errs = config::load_str(yaml, "t.yaml").unwrap_err();
    assert!(errs.iter().any(|e| e.contains("保留字")), "{errs:?}");
}

#[test]
fn dangling_provider_reference_rejected() {
    let yaml = r#"
gateway: { addr: "127.0.0.1:0", ui_addr: "127.0.0.1:0" }
providers:
  - { id: p, vendor: x, base_url: "https://x.test/v1", api_key_env: "K", models: ["m"], allow_private: true }
routing_groups:
  - { name: g, members: [ { provider: ghost, model: m, priority: 1 } ] }
"#;
    let errs = config::load_str(yaml, "t.yaml").unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("不存在的 provider")),
        "{errs:?}"
    );
}

#[allow(dead_code)]
fn _unused(_: HeaderMap) {}
