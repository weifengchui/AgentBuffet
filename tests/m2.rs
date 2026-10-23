//! M2 验收：mock server 模拟 429/超时/流中断，验证自动切换、熔断隔离与恢复、
//! degraded、429 被动校准、额度耗尽跳过、绝不半截写出。

use agentbuffet::{build_router, config, proxy::AppState};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

#[derive(Clone)]
struct MockState {
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl MockState {
    fn hit(&self, model: &str) {
        *self
            .hits
            .lock()
            .unwrap()
            .entry(model.to_string())
            .or_default() += 1;
    }
    fn count(&self, model: &str) -> usize {
        *self.hits.lock().unwrap().get(model).unwrap_or(&0)
    }
}

/// mock provider：按 model 名分流故障行为，并统计命中次数供"跳过未触达"断言。
async fn mock_chat(State(ms): State<MockState>, req: Request) -> Response {
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
    ms.hit(&model);

    let bad = |code: u16| {
        Response::builder()
            .status(code)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"error": {"message": format!("mock {code}")}}).to_string(),
            ))
            .unwrap()
    };

    if model.contains("sse-error") {
        // 200 + SSE error 载荷（首块即错误）
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"error\":{\"message\":\"upstream boom\"}}\n\n",
            ))
            .unwrap();
    }
    if model.contains("always-429-ra") {
        return Response::builder()
            .status(429)
            .header("retry-after", "2")
            .header("content-type", "application/json")
            .body(Body::from("{\"error\":{\"message\":\"rate limited\"}}"))
            .unwrap();
    }
    if model.contains("always-429") {
        return bad(429);
    }
    if model.contains("always-500") {
        return bad(500);
    }
    if model.contains("always-401") {
        return bad(401);
    }
    if model.contains("hang") {
        // 挂死超过 first_byte_seconds(1s)：触发超时切换
        tokio::time::sleep(Duration::from_secs(5)).await;
        return bad(500);
    }
    if model.contains("midstream") {
        // 首块正常写出并被对端读取后，再让流中断（立即报错会被 RST 吞掉首块）
        let s = futures_util::stream::iter(vec![Ok(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n" as &[u8],
        )])
        .chain(futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(std::io::Error::other("mock midstream break"))
        }));
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(s))
            .unwrap();
    }
    if model.contains("quota-usage") {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "object": "chat.completion",
                "echo_model": model,
                "usage": {"prompt_tokens": 100, "completion_tokens": 500, "total_tokens": 600},
            })),
        )
            .into_response();
    }
    if stream {
        let chunks = vec![
            format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":{"content":"He"}}],"echo_model":model})
            ),
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let s = futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(s))
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

fn prov(id: &str, mock: &str, models: &[&str], quota_tokens: Option<u64>) -> String {
    let q = match quota_tokens {
        Some(t) => format!("    quota: {{ daily_tokens: {t} }}\n"),
        None => String::new(),
    };
    format!(
        "  - id: {id}\n    vendor: mock\n    base_url: \"{mock}\"\n    api_key_env: \"BUFFET_TEST_KEY\"\n    models: {models:?}\n{q}    allow_private: true"
    )
}

fn group(members: &[(u32, &str, &str)]) -> String {
    let ms: Vec<String> = members
        .iter()
        .map(|(p, prov_id, model)| {
            format!("      - {{ provider: {prov_id}, model: {model}, priority: {p} }}")
        })
        .collect();
    format!(
        "  - name: g\n    strategy: order\n    members:\n{}",
        ms.join("\n")
    )
}

/// 预检超时收紧：first_byte 1s / stream_idle 2s；熔断阈值 2、隔离 1s。
fn gw_config(providers: &[String], members: &[(u32, &str, &str)]) -> String {
    format!(
        r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
providers:
{}
routing_groups:
{}
agents:
  - {{ id: hermes, display_name: Hermes, default_group: g }}
failover:
  retry_on_status: [429, 500, 502, 503, 504]
  retry_on_timeout: true
  max_retries: 3
  timeouts: {{ connect_seconds: 2, first_byte_seconds: 1, stream_idle_seconds: 2 }}
  circuit_breaker: {{ window_seconds: 60, threshold: 2, open_seconds: 1 }}
"#,
        providers.join("\n"),
        group(members)
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

async fn post_group(client: &reqwest::Client, gw: &str, stream: bool) -> reqwest::Response {
    client
        .post(format!("{gw}/hermes/v1/chat/completions"))
        .json(&serde_json::json!({"model":"group/g","stream":stream,"messages":[]}))
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

#[tokio::test]
async fn nonstream_fails_over_on_500() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("bad-1", &mock, &["always-500-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "bad-1", "always-500-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "bad-1:500,good-1:ok");
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["echo_model"], "ok-chat");
    assert_eq!(ms.count("always-500-chat"), 1);
    assert_eq!(ms.count("ok-chat"), 1);
}

#[tokio::test]
async fn fails_over_on_first_byte_timeout() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("hang-1", &mock, &["hang-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "hang-1", "hang-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "hang-1:timeout,good-1:ok");
    assert_eq!(ms.count("ok-chat"), 1);
}

#[tokio::test]
async fn all_candidates_fail_returns_last_error() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("a-1", &mock, &["always-429-chat"], None),
            prov("b-1", &mock, &["always-429-chat"], None),
        ],
        &[(1, "a-1", "always-429-chat"), (2, "b-1", "always-429-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();
    let r = post_group(&client, &gw, false).await;
    // 返回最后一次错误（429），链上记录全部尝试
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(chain_of(&r), "a-1:429,b-1:429");
}

#[tokio::test]
async fn circuit_breaker_isolates_and_recovers() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("bad-1", &mock, &["always-500-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "bad-1", "always-500-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    // 前两次请求让 bad-1 连续失败 2 次（threshold=2）→ 熔断打开
    for _ in 0..2 {
        let r = post_group(&client, &gw, false).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(chain_of(&r), "bad-1:500,good-1:ok");
    }
    assert_eq!(ms.count("always-500-chat"), 2);

    // 第三次：熔断 open，bad-1 被过滤且完全未触达上游
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "bad-1:open,good-1:ok");
    assert_eq!(ms.count("always-500-chat"), 2, "熔断期间不得再打 bad-1");

    // 隔离 1s 到期后恢复试探
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "bad-1:500,good-1:ok");
    assert_eq!(ms.count("always-500-chat"), 3);
}

#[tokio::test]
async fn auth_401_marks_degraded_and_skips_afterwards() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("auth-1", &mock, &["always-401-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "auth-1", "always-401-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "auth-1:401,good-1:ok");

    // degraded 持续生效：后续请求直接跳过，不再触达
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "auth-1:degraded,good-1:ok");
    assert_eq!(ms.count("always-401-chat"), 1, "degraded 后不得再打 auth-1");
}

#[tokio::test]
async fn retry_after_429_isolates_until_reset() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("ra-1", &mock, &["always-429-ra-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "ra-1", "always-429-ra-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "ra-1:429,good-1:ok");

    // retry-after=2s：隔离期内跳过且不触达
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "ra-1:open,good-1:ok");
    assert_eq!(ms.count("always-429-ra-chat"), 1);

    // 重置点后恢复
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(ms.count("always-429-ra-chat"), 2);
}

#[tokio::test]
async fn quota_exhausted_skips_candidate() {
    let (mock, ms) = spawn_mock().await;
    // q-1 软上限 1000×0.8=800，每次消耗 600 → 第二次后耗尽
    let yaml = gw_config(
        &[
            prov("q-1", &mock, &["quota-usage-chat"], Some(1000)),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "q-1", "quota-usage-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["echo_model"], "quota-usage-chat");

    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(ms.count("quota-usage-chat"), 2); // 累计 1200 ≥ 800

    // 第三次：额度耗尽被过滤，切 good-1 且不再触达 q-1
    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "q-1:quota,good-1:ok");
    assert_eq!(ms.count("quota-usage-chat"), 2);
    assert_eq!(ms.count("ok-chat"), 1);
}

#[tokio::test]
async fn stream_switches_before_first_byte_written() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("peek-1", &mock, &["sse-error-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "peek-1", "sse-error-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    // 首候选首 chunk 即 SSE error：未写出任何字节 → 干净切换
    let r = post_group(&client, &gw, true).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "peek-1:sse-error,good-1:ok");
    let text = r.text().await.unwrap();
    assert!(
        text.contains("He") && text.contains("llo"),
        "完整流: {text}"
    );
    assert!(!text.contains("boom"), "错误载荷不得透传: {text}");
    assert_eq!(ms.count("ok-chat"), 1);
}

#[tokio::test]
async fn midstream_break_never_switches_provider() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("mid-1", &mock, &["midstream-chat"], None),
            prov("good-1", &mock, &["ok-chat"], None),
        ],
        &[(1, "mid-1", "midstream-chat"), (2, "good-1", "ok-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    // 首块已写出后流中断：绝不换供应商，客户端收到干净截断 + chain 头
    let r = post_group(&client, &gw, true).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(chain_of(&r), "mid-1:ok");
    let mut got = String::new();
    let mut s = r.bytes_stream();
    while let Some(item) = s.next().await {
        match item {
            Ok(b) => got.push_str(&String::from_utf8_lossy(&b)),
            Err(_) => break,
        }
    }
    assert!(got.contains("He"), "首块应已透传: {got}");
    assert!(!got.contains("llo"), "写到一半不得切换到 good-1: {got}");
    assert_eq!(ms.count("ok-chat"), 0, "绝不能在流中断后换供应商");
}

#[tokio::test]
async fn all_filtered_returns_503() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("auth-1", &mock, &["always-401-chat"], None)],
        &[(1, "auth-1", "always-401-chat")],
    );
    let gw = spawn_gateway(&yaml).await;
    let client = reqwest::Client::new();

    let r = post_group(&client, &gw, false).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED); // 唯一候选 401 → 最后错误
    let r = post_group(&client, &gw, false).await;
    // auth-1 已 degraded，全部过滤 → 503
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(chain_of(&r), "auth-1:degraded");
}
