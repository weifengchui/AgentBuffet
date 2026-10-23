//! M3 验收：usage JSONL 记账（字段/cost 命中未命中/流式末块/retries/fail_chain 不含 Key）、
//! 热重载改路由不重启（坏配置 422 旧快照继续服务、复位 degraded）、重启后 counters 不丢。

use agentbuffet::{build_admin_router, build_router, config, proxy::AppState, usage::Usage};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("buffet-m3-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Full {
    gw: String,
    admin: String,
    state: Arc<AppState>,
    dir: PathBuf,
}

impl Full {
    fn cfg_path(&self) -> PathBuf {
        self.dir.join("config.yaml")
    }
    fn logs(&self) -> PathBuf {
        self.dir.join("logs")
    }
    async fn reload(&self) -> (StatusCode, serde_json::Value) {
        let r = reqwest::Client::new()
            .post(format!("{}/_admin/reload", self.admin))
            .send()
            .await
            .unwrap();
        let code = r.status();
        (code, r.json().await.unwrap_or(serde_json::Value::Null))
    }
}

/// 收集 logs 目录下全部 JSONL 行。
fn jsonl_lines(logs: &Path) -> Vec<serde_json::Value> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(logs) {
        for e in rd.flatten() {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                for line in text.lines() {
                    if let Ok(j) = serde_json::from_str::<serde_json::Value>(line) {
                        v.push(j);
                    }
                }
            }
        }
    }
    v
}

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

    if model.contains("always-500") {
        return Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(Body::from("{\"error\":{\"message\":\"mock 500\"}}"))
            .unwrap();
    }
    if model.contains("always-401") {
        return Response::builder()
            .status(401)
            .header("content-type", "application/json")
            .body(Body::from("{\"error\":{\"message\":\"mock 401\"}}"))
            .unwrap();
    }
    if model.contains("stream-usage") {
        // 末块携带 usage（OpenAI 流式惯例）
        let chunks = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n".to_string(),
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5,\"total_tokens\":16}}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let s = futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(s))
            .unwrap();
    }
    if model.contains("usage") {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "object": "chat.completion",
                "echo_model": model,
                "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19},
            })),
        )
            .into_response();
    }
    if stream {
        let chunks = vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n".to_string(),
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

fn prov(id: &str, mock: &str, models: &[&str]) -> String {
    format!(
        "  - id: {id}\n    vendor: mock\n    base_url: \"{mock}\"\n    api_key_env: \"BUFFET_TEST_KEY\"\n    models: {models:?}\n    allow_private: true"
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

fn gw_config(providers: &[String], members: &[(u32, &str, &str)], prices: Option<&Path>) -> String {
    let pr = match prices {
        Some(p) => format!("prices:\n  file: \"{}\"\n", p.display()),
        None => String::new(),
    };
    format!(
        r#"
gateway:
  addr: "127.0.0.1:0"
  ui_addr: "127.0.0.1:0"
{pr}providers:
{}
routing_groups:
{}
agents:
  - {{ id: hermes, display_name: Hermes, default_group: g }}
failover:
  retry_on_status: [429, 500, 502, 503, 504]
  max_retries: 3
  timeouts: {{ connect_seconds: 2, first_byte_seconds: 1, stream_idle_seconds: 2 }}
  circuit_breaker: {{ window_seconds: 60, threshold: 2, open_seconds: 1 }}
"#,
        providers.join("\n"),
        group(members)
    )
}

async fn spawn_full(yaml: &str, tag: &str) -> Full {
    std::env::set_var("BUFFET_TEST_KEY", "sk-secret-123");
    let dir = temp_dir(tag);
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, yaml).unwrap();
    let snap = config::load_str(yaml, "test.yaml").expect("config ok");
    let state = Arc::new(AppState::with_paths(snap, cfg, dir.join("logs")));

    let l1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw = format!("http://{}", l1.local_addr().unwrap());
    let app = build_router(state.clone());
    tokio::spawn(async move { axum::serve(l1, app).await.unwrap() });

    let l2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin = format!("http://{}", l2.local_addr().unwrap());
    let adm = build_admin_router(state.clone());
    tokio::spawn(async move {
        axum::serve(l2, adm.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap()
    });
    Full {
        gw,
        admin,
        state,
        dir,
    }
}

async fn post_chat(gw: &str, model: &str, stream: bool) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{gw}/hermes/v1/chat/completions"))
        .json(&serde_json::json!({"model": model, "stream": stream, "messages": []}))
        .send()
        .await
        .unwrap()
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn nonstream_records_jsonl_fields_and_counters() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("good-1", &mock, &["usage-chat"])],
        &[(1, "good-1", "usage-chat")],
        None,
    );
    let f = spawn_full(&yaml, "fields").await;
    let r = post_chat(&f.gw, "usage-chat", false).await;
    assert_eq!(r.status(), StatusCode::OK);
    // 等流式响应写回后记账（非流式在 handle 内同步 record，此处仅需读文件）
    let lines = jsonl_lines(&f.logs());
    assert_eq!(lines.len(), 1);
    let ev = &lines[0];
    assert_eq!(ev["agent"], "hermes");
    assert_eq!(ev["group"], "g");
    assert_eq!(ev["provider"], "good-1");
    assert_eq!(ev["model"], "usage-chat");
    assert_eq!(ev["status"], 200);
    assert_eq!(ev["prompt_tokens"], 12);
    assert_eq!(ev["completion_tokens"], 7);
    assert_eq!(ev["cost_est"], serde_json::Value::Null); // 无 prices 必须 null 不得 0
    assert!(ev["duration_ms"].as_u64().unwrap() < 10_000);
    assert_eq!(ev["retries"], 0);
    assert_eq!(ev["fail_chain"], serde_json::json!(["good-1:ok"]));
    assert!(ev["ts"].as_u64().unwrap() > 1_700_000_000);

    // counters 内存聚合
    let c = f.state.usage.as_ref().unwrap().counters();
    let one = &c[&("hermes".into(), "g".into(), "good-1".into())];
    assert_eq!(one.requests, 1);
    assert_eq!(one.prompt_tokens, 12);
    assert_eq!(one.completion_tokens, 7);
    assert_eq!(one.failures, 0);

    // Key 不得出现在任何日志行
    let raw = std::fs::read_dir(f.logs())
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        !raw.contains("sk-secret"),
        "fail_chain 不得含 API Key: {raw}"
    );
    cleanup(&f.dir);
}

#[tokio::test]
async fn cost_est_hit_and_miss() {
    let (mock, _ms) = spawn_mock().await;
    let dir0 = temp_dir("cost");
    let prices = dir0.join("prices.yaml");
    std::fs::write(&prices, "usage-chat:\n  input: 2.0\n  output: 8.0\n").unwrap();
    let yaml = gw_config(
        &[prov("good-1", &mock, &["usage-chat", "ok-chat"])],
        &[(1, "good-1", "usage-chat"), (2, "good-1", "ok-chat")],
        Some(&prices),
    );
    let f = spawn_full(&yaml, "cost").await;
    let r = post_chat(&f.gw, "usage-chat", false).await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = post_chat(&f.gw, "ok-chat", false).await;
    assert_eq!(r.status(), StatusCode::OK);

    let lines = jsonl_lines(&f.logs());
    assert_eq!(lines.len(), 2);
    let hit = lines.iter().find(|l| l["model"] == "usage-chat").unwrap();
    // 12/1M*2.0 + 7/1M*8.0 = 8e-5
    let c = hit["cost_est"].as_f64().unwrap();
    assert!((c - 8e-5).abs() < 1e-12, "cost_est={c}");
    let miss = lines.iter().find(|l| l["model"] == "ok-chat").unwrap();
    assert_eq!(miss["cost_est"], serde_json::Value::Null);
    cleanup(&f.dir);
    let _ = std::fs::remove_dir_all(dir0);
}

#[tokio::test]
async fn stream_records_usage_from_final_chunk() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("s-1", &mock, &["stream-usage-chat"])],
        &[(1, "s-1", "stream-usage-chat")],
        None,
    );
    let f = spawn_full(&yaml, "stream").await;
    let r = post_chat(&f.gw, "stream-usage-chat", true).await;
    assert_eq!(r.status(), StatusCode::OK);
    let text = r.text().await.unwrap();
    assert!(text.contains("He") && text.contains("llo"));

    // 流完成记账在响应体内异步收尾，轮询等待落盘
    let mut lines = Vec::new();
    for _ in 0..50 {
        lines = jsonl_lines(&f.logs());
        if !lines.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(lines.len(), 1);
    let ev = &lines[0];
    assert_eq!(ev["status"], 200);
    assert_eq!(ev["prompt_tokens"], 11);
    assert_eq!(ev["completion_tokens"], 5);
    assert_eq!(ev["fail_chain"], serde_json::json!(["s-1:ok"]));
    cleanup(&f.dir);
}

#[tokio::test]
async fn failover_records_retries_and_chain() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("bad-1", &mock, &["always-500-chat"]),
            prov("good-1", &mock, &["usage-chat"]),
        ],
        &[(1, "bad-1", "always-500-chat"), (2, "good-1", "usage-chat")],
        None,
    );
    let f = spawn_full(&yaml, "retry").await;
    let r = post_chat(&f.gw, "group/g", false).await;
    assert_eq!(r.status(), StatusCode::OK);
    let lines = jsonl_lines(&f.logs());
    assert_eq!(lines.len(), 1);
    let ev = &lines[0];
    assert_eq!(ev["status"], 200);
    assert_eq!(ev["provider"], "good-1");
    assert_eq!(ev["retries"], 1);
    assert_eq!(
        ev["fail_chain"],
        serde_json::json!(["bad-1:500", "good-1:ok"])
    );
    cleanup(&f.dir);
}

#[tokio::test]
async fn all_fail_records_error_event() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("a-1", &mock, &["always-500-chat"]),
            prov("b-1", &mock, &["always-500-chat"]),
        ],
        &[(1, "a-1", "always-500-chat"), (2, "b-1", "always-500-chat")],
        None,
    );
    let f = spawn_full(&yaml, "allfail").await;
    let r = post_chat(&f.gw, "group/g", false).await;
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let lines = jsonl_lines(&f.logs());
    assert_eq!(lines.len(), 1);
    let ev = &lines[0];
    assert_eq!(ev["status"], 500);
    assert_eq!(ev["prompt_tokens"], 0);
    assert_eq!(ev["retries"], 1);
    assert_eq!(ev["fail_chain"], serde_json::json!(["a-1:500", "b-1:500"]));
    let c = f.state.usage.as_ref().unwrap().counters();
    assert_eq!(c[&("hermes".into(), "g".into(), "b-1".into())].failures, 1);
    cleanup(&f.dir);
}

#[tokio::test]
async fn all_filtered_503_not_recorded() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("auth-1", &mock, &["always-401-chat"])],
        &[(1, "auth-1", "always-401-chat")],
        None,
    );
    let f = spawn_full(&yaml, "filtered").await;
    let r = post_chat(&f.gw, "always-401-chat", false).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(jsonl_lines(&f.logs()).len(), 1); // 401 记一条

    let r = post_chat(&f.gw, "always-401-chat", false).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE); // degraded 全过滤
    assert_eq!(jsonl_lines(&f.logs()).len(), 1, "503 全过滤不得记账");
    cleanup(&f.dir);
}

#[tokio::test]
async fn reload_swaps_routing_without_restart() {
    let (mock, ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("p-1", &mock, &["ok-chat"])],
        &[(1, "p-1", "ok-chat")],
        None,
    );
    let f = spawn_full(&yaml, "reload").await;
    let r = post_chat(&f.gw, "ok-chat", false).await;
    assert_eq!(r.status(), StatusCode::OK);

    // 改配置文件：成员换成 ok2-chat，不重启
    let yaml2 = gw_config(
        &[prov("p-1", &mock, &["ok2-chat"])],
        &[(1, "p-1", "ok2-chat")],
        None,
    );
    std::fs::write(f.cfg_path(), &yaml2).unwrap();
    let (code, body) = f.reload().await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "reloaded");

    let r = post_chat(&f.gw, "ok2-chat", false).await;
    assert_eq!(r.status(), StatusCode::OK);
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["echo_model"], "ok2-chat");
    assert_eq!(ms.count("ok-chat"), 1);
    assert_eq!(ms.count("ok2-chat"), 1);

    // 旧 model 已不在新快照路由组内 → 400
    let r = post_chat(&f.gw, "ok-chat", false).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    cleanup(&f.dir);
}

#[tokio::test]
async fn reload_invalid_422_keeps_old_snapshot() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("p-1", &mock, &["ok-chat"])],
        &[(1, "p-1", "ok-chat")],
        None,
    );
    let f = spawn_full(&yaml, "bad422").await;

    std::fs::write(f.cfg_path(), "gateway: { addr: bad, ui_addr: also bad }\n").unwrap();
    let (code, body) = f.reload().await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
    let text = body.to_string();
    assert!(text.contains("config.yaml"), "422 需带文件名: {text}");

    // 旧快照继续服务
    let r = post_chat(&f.gw, "ok-chat", false).await;
    assert_eq!(r.status(), StatusCode::OK);
    cleanup(&f.dir);
}

#[tokio::test]
async fn reload_resets_degraded_runtime_state() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[
            prov("auth-1", &mock, &["always-401-chat"]),
            prov("good-1", &mock, &["ok-chat"]),
        ],
        &[(1, "auth-1", "always-401-chat"), (2, "good-1", "ok-chat")],
        None,
    );
    let f = spawn_full(&yaml, "reset").await;

    let r = post_chat(&f.gw, "group/g", false).await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = post_chat(&f.gw, "group/g", false).await;
    let chain = r
        .headers()
        .get("x-buffet-failover-chain")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(chain.contains("auth-1:degraded"), "{chain}");

    // 同配置重载 → degraded 复位，auth-1 重新被尝试
    std::fs::write(f.cfg_path(), &yaml).unwrap();
    let (code, _body) = f.reload().await;
    assert_eq!(code, StatusCode::OK);
    let r = post_chat(&f.gw, "group/g", false).await;
    let chain = r
        .headers()
        .get("x-buffet-failover-chain")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(chain.contains("auth-1:401"), "重载后应重新尝试: {chain}");
    cleanup(&f.dir);
}

#[tokio::test]
async fn restart_rebuilds_today_counters() {
    let (mock, _ms) = spawn_mock().await;
    let yaml = gw_config(
        &[prov("good-1", &mock, &["usage-chat"])],
        &[(1, "good-1", "usage-chat")],
        None,
    );
    let f = spawn_full(&yaml, "restart").await;
    for _ in 0..2 {
        let r = post_chat(&f.gw, "usage-chat", false).await;
        assert_eq!(r.status(), StatusCode::OK);
    }
    let before = f.state.usage.as_ref().unwrap().counters();
    assert_eq!(
        before[&("hermes".into(), "g".into(), "good-1".into())].requests,
        2
    );

    // 模拟重启：同 logs 目录新建 Usage
    let u2 = Usage::new(f.logs(), Path::new("/nonexistent-prices.yaml"));
    let after = u2.counters();
    assert_eq!(before, after, "重启后当日 counters 必须从 JSONL 重建一致");
    cleanup(&f.dir);
}
