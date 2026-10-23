//! OpenAI 兼容转发 + 故障转移：候选循环、熔断、额度软上限、429 被动校准。
//! 硬约束：流式零缓冲（仅首 chunk peek）；绝不写到一半换供应商。

use crate::config::{Provider, Snapshot};
use crate::failover::{now_epoch_secs, StateTable};
use crate::router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 流式 usage 跨 poll 累计器（unfold 闭包多次调用间读写）。
struct UsageAcc {
    total: u64,
}

impl UsageAcc {
    fn add(&mut self, t: u64) {
        self.total += t;
    }
}

pub struct AppState {
    pub snap: RwLock<Arc<Snapshot>>,
    pub client: reqwest::Client,
    pub states: Arc<StateTable>,
}

impl AppState {
    pub fn new(snap: Snapshot) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(
                snap.raw.failover.timeouts.connect_seconds,
            ))
            .tcp_nodelay(true)
            .build()
            .expect("build reqwest client");
        Self {
            snap: RwLock::new(Arc::new(snap)),
            client,
            states: Arc::new(StateTable::new()),
        }
    }
}

/// 失败处置类别：决定 handle() 如何更新运行时状态。
enum FailKind {
    /// 计入熔断窗口（5xx/超时/连接错误/流首块失败）
    Circuit,
    /// 429 被动校准（有 retry-after 隔离至重置点，否则收紧软上限）
    Rate429 { retry_after: Option<u64> },
    /// 401/403/缺 key：标记 degraded 需人工修 key
    Degraded,
}

struct AttemptFail {
    /// fail chain 标签，如 "500"、"timeout"、"nokey"
    tag: String,
    kind: FailKind,
    /// (status, message)：全部候选耗尽时返回最后一次错误
    last: (u16, String),
}

/// 单次上游调用的请求上下文。
struct Outbound {
    url: String,
    method: Method,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// 主转发入口：候选循环故障转移。
pub async fn handle(state: Arc<AppState>, req: Request) -> Response<Body> {
    let snap = state.snap.read().await.clone();
    let (parts, body) = req.into_parts();
    let (method, headers, uri) = (parts.method, parts.headers, parts.uri);
    let path = uri.path().to_string();

    // 解析路径 → agent_id + 上游相对路径
    let (agent_id, upstream_rel) = match router::parse_path(&path) {
        Ok(v) => v,
        Err(e) => return json_error(e.status(), &e.to_string()),
    };

    // 读 body，取 model
    let body_bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_error(400, "读取请求体失败"),
    };
    let parsed: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => return json_error(400, "请求体不是合法 JSON（OpenAI 兼容）"),
    };
    let model = match parsed.get("model").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => return json_error(400, "请求缺少 model 字段（OpenAI 兼容）"),
    };
    // 解析路由组与候选（disabled 已在 resolve 过滤）
    let (_group, candidates) = match router::resolve(&snap, &agent_id, &model) {
        Ok(v) => v,
        Err(e) => return json_error(e.status(), &e.to_string()),
    };

    // 运行时过滤：degraded / 熔断 open / 额度耗尽
    let ratio = snap.raw.gateway.conservative_quota_ratio;
    let now = now_epoch_secs();
    let mut avail: Vec<(&router::Candidate, &Provider)> = Vec::new();
    let mut chain: Vec<String> = Vec::new();
    for c in &candidates {
        let Some(p) = snap.providers.get(&c.provider_id) else {
            continue;
        };
        match state.states.availability(&c.provider_id, p, ratio, now) {
            Ok(()) => avail.push((c, p)),
            Err(u) => chain.push(format!("{}:{}", p.id, u.tag())),
        }
    }
    if avail.is_empty() {
        let mut resp = json_error(
            503,
            &format!(
                "所有候选均被隔离（degraded/熔断/额度）: {}",
                chain.join(", ")
            ),
        );
        attach_chain(&mut resp, &chain.join(","));
        return resp;
    }

    // 转发 header（剔除逐跳与 host/auth/content-length），Authorization 每候选注入
    let mut base_headers = HeaderMap::new();
    for (k, v) in headers.iter() {
        if is_hop_by_hop_or_strip(k) {
            continue;
        }
        base_headers.insert(k, v.clone());
    }

    let fo = &snap.raw.failover;
    let max_attempts = 1 + fo.max_retries as usize;
    let mut last_err: Option<(u16, String)> = None;

    for (c, p) in avail.into_iter().take(max_attempts) {
        // 上游 model 可能被 group/ 虚拟模型改写为成员自己的 model
        let mut outbound_json = parsed.clone();
        outbound_json["model"] = serde_json::Value::String(c.model.clone());
        let Ok(out_body) = serde_json::to_vec(&outbound_json) else {
            return json_error(500, "序列化转发请求失败");
        };
        let ob = Outbound {
            url: build_url(&p.base_url, &upstream_rel, uri.query()),
            method: method.clone(),
            headers: base_headers.clone(),
            body: out_body,
        };
        match attempt_once(&state, &snap, p, ob, &chain).await {
            Ok(resp) => return resp,
            Err(fail) => {
                chain.push(format!("{}:{}", p.id, fail.tag));
                let now2 = now_epoch_secs();
                match fail.kind {
                    FailKind::Circuit => {
                        state
                            .states
                            .record_failure(&p.id, &fo.circuit_breaker, now2);
                    }
                    FailKind::Rate429 { retry_after } => {
                        state.states.calibrate_429(
                            &p.id,
                            retry_after,
                            &fo.circuit_breaker,
                            ratio,
                            now2,
                        );
                    }
                    FailKind::Degraded => {
                        state.states.mark_degraded(&p.id, now2);
                    }
                }
                last_err = Some(fail.last);
            }
        }
    }

    // 候选耗尽：返回最后一次错误
    let (status, msg) = last_err.unwrap_or((502, "故障转移候选全部失败".to_string()));
    let mut resp = json_error(status, &msg);
    attach_chain(&mut resp, &chain.join(","));
    resp
}

/// 单个候选的一次尝试。Ok = 终局响应（成功或不可重试透传），Err = 可切换下一候选。
async fn attempt_once(
    state: &Arc<AppState>,
    snap: &Arc<Snapshot>,
    p: &Provider,
    ob: Outbound,
    chain: &[String],
) -> Result<Response<Body>, AttemptFail> {
    let fo = &snap.raw.failover;

    // 鉴权：从环境变量取 key，替换 Authorization；缺失按 degraded 处理
    let api_key = match std::env::var(&p.api_key_env) {
        Ok(k) if !k.is_empty() => k,
        _ => {
            return Err(AttemptFail {
                tag: "nokey".into(),
                kind: FailKind::Degraded,
                last: (
                    502,
                    format!("provider {} 的 {} 未设置（degraded）", p.id, p.api_key_env),
                ),
            })
        }
    };
    let mut hdrs = ob.headers;
    match HeaderValue::from_maybe_shared(format!("Bearer {api_key}").into_bytes()) {
        Ok(v) => {
            hdrs.insert(header::AUTHORIZATION, v);
        }
        Err(_) => {
            return Err(AttemptFail {
                tag: "badkey".into(),
                kind: FailKind::Degraded,
                last: (502, format!("provider {} 的 key 含非法字符", p.id)),
            })
        }
    }

    let rb = state
        .client
        .request(ob.method, &ob.url)
        .headers(hdrs)
        .body(ob.body);

    let fb = Duration::from_secs(fo.timeouts.first_byte_seconds);
    let resp = match tokio::time::timeout(fb, rb.send()).await {
        Err(_) => {
            let msg = format!("上游 {} 首字节超时", p.id);
            if fo.retry_on_timeout {
                return Err(AttemptFail {
                    tag: "timeout".into(),
                    kind: FailKind::Circuit,
                    last: (504, msg),
                });
            }
            let mut r = json_error(504, &msg);
            attach_chain(&mut r, &chain_line(chain, &p.id, "timeout"));
            return Ok(r);
        }
        Ok(Err(e)) => {
            let msg = format!("上游请求失败: {e}");
            if fo.retry_on_timeout {
                return Err(AttemptFail {
                    tag: "conn".into(),
                    kind: FailKind::Circuit,
                    last: (502, msg),
                });
            }
            let mut r = json_error(502, &msg);
            attach_chain(&mut r, &chain_line(chain, &p.id, "conn"));
            return Ok(r);
        }
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    let code = status.as_u16();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let is_sse = ctype.contains("text/event-stream");

    if status.is_success() {
        if is_sse {
            return stream_attempt(
                state,
                p,
                resp,
                &ctype,
                status,
                chain,
                Duration::from_secs(fo.timeouts.stream_idle_seconds),
            )
            .await;
        }
        // 缓冲成功路径（非流式响应）：读全量、记账 usage
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return Err(AttemptFail {
                    tag: "read".into(),
                    kind: FailKind::Circuit,
                    last: (502, format!("读取上游响应失败: {e}")),
                })
            }
        };
        let now = now_epoch_secs();
        state.states.record_success(&p.id, now);
        let tokens = usage_tokens_json(&bytes);
        if tokens > 0 {
            state.states.add_tokens(&p.id, tokens, now);
        }
        let mut r = Response::new(Body::from(bytes));
        *r.status_mut() = status;
        if let Ok(v) = HeaderValue::from_str(&ctype) {
            r.headers_mut().insert(header::CONTENT_TYPE, v);
        }
        attach_chain(&mut r, &chain_line(chain, &p.id, "ok"));
        return Ok(r);
    }

    if code == 401 || code == 403 {
        return Err(AttemptFail {
            tag: code.to_string(),
            kind: FailKind::Degraded,
            last: (
                code,
                format!("上游 {} 鉴权失败（{code}），已标记 degraded", p.id),
            ),
        });
    }
    if code == 429 {
        let ra = parse_retry_after(&resp);
        return Err(AttemptFail {
            tag: "429".into(),
            kind: FailKind::Rate429 { retry_after: ra },
            last: (429, format!("上游 {} 限流（429）", p.id)),
        });
    }
    if fo.retry_on_status.contains(&code) {
        return Err(AttemptFail {
            tag: code.to_string(),
            kind: FailKind::Circuit,
            last: (code, format!("上游 {} 返回 {code}", p.id)),
        });
    }
    // 不可重试的上游错误（如 400）：原样透传，换供应商也无意义
    let mut r = passthrough_buffered(resp, status, &ctype).await;
    attach_chain(&mut r, &chain_line(chain, &p.id, &code.to_string()));
    Ok(r)
}

/// 流式尝试：peek 首 chunk 判定，成功则零缓冲透传并在完成时记账 usage。
/// 判定失败发生在任何字节写出之前，可安全切换下一候选。
async fn stream_attempt(
    state: &Arc<AppState>,
    p: &Provider,
    resp: reqwest::Response,
    ctype: &str,
    status: StatusCode,
    chain: &[String],
    idle: Duration,
) -> Result<Response<Body>, AttemptFail> {
    let mut stream = resp.bytes_stream();

    let first = match tokio::time::timeout(idle, stream.next()).await {
        Err(_) => {
            return Err(AttemptFail {
                tag: "stream-timeout".into(),
                kind: FailKind::Circuit,
                last: (504, format!("上游 {} 流首 chunk 超时", p.id)),
            })
        }
        Ok(None) => {
            return Err(AttemptFail {
                tag: "stream-empty".into(),
                kind: FailKind::Circuit,
                last: (502, format!("上游 {} 流为空", p.id)),
            })
        }
        Ok(Some(Err(e))) => {
            return Err(AttemptFail {
                tag: "stream-err".into(),
                kind: FailKind::Circuit,
                last: (502, format!("上游流错误: {e}")),
            })
        }
        Ok(Some(Ok(b))) => b,
    };

    if let Some(err_body) = detect_sse_error(&first) {
        return Err(AttemptFail {
            tag: "sse-error".into(),
            kind: FailKind::Circuit,
            last: (502, format!("上游流返回错误: {err_body}")),
        });
    }

    // 首 chunk 判定成功：此刻起绝不换供应商，只透传
    let now = now_epoch_secs();
    state.states.record_success(&p.id, now);
    let st_tbl = state.states.clone();
    let pid = p.id.clone();
    let init = UsageAcc {
        total: usage_tokens_sse(&first),
    };
    let rest = futures_util::stream::unfold((Some(stream), init), move |(opt, mut acc)| {
        let st_tbl = st_tbl.clone();
        let pid = pid.clone();
        async move {
            let mut s = opt?;
            match tokio::time::timeout(idle, s.next()).await {
                Ok(Some(Ok(b))) => {
                    acc.add(usage_tokens_sse(&b));
                    Some((Ok::<Bytes, BoxError>(b), (Some(s), acc)))
                }
                Ok(Some(Err(e))) => {
                    st_tbl.add_tokens(&pid, acc.total, now_epoch_secs());
                    Some((Err::<Bytes, BoxError>(Box::new(e)), (None, acc)))
                }
                Ok(None) => {
                    st_tbl.add_tokens(&pid, acc.total, now_epoch_secs());
                    None
                }
                Err(_) => {
                    st_tbl.add_tokens(&pid, acc.total, now_epoch_secs());
                    Some((
                        Err::<Bytes, BoxError>(Box::from("buffet: 上游流静默超时")),
                        (None, acc),
                    ))
                }
            }
        }
    });

    let combined =
        futures_util::stream::once(async move { Ok::<Bytes, BoxError>(first) }).chain(rest);
    let mut r = Response::new(Body::from_stream(combined));
    *r.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(ctype) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    attach_chain(&mut r, &chain_line(chain, &p.id, "ok"));
    Ok(r)
}

async fn passthrough_buffered(
    resp: reqwest::Response,
    status: StatusCode,
    ctype: &str,
) -> Response<Body> {
    match resp.bytes().await {
        Err(e) => json_error(502, &format!("读取上游响应失败: {e}")),
        Ok(bytes) => {
            let mut r = Response::new(Body::from(bytes));
            *r.status_mut() = status;
            if let Ok(v) = HeaderValue::from_str(ctype) {
                r.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            r
        }
    }
}

/// 从首 chunk 检测 SSE error 事件（部分供应商以 200 + SSE error 载荷返回错误）。
fn detect_sse_error(chunk: &Bytes) -> Option<String> {
    let text = String::from_utf8_lossy(chunk);
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                if let Some(err) = v.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown upstream error");
                    return Some(msg.to_string());
                }
            }
        }
    }
    None
}

fn chain_line(chain: &[String], id: &str, entry: &str) -> String {
    let entry = &format!("{id}:{entry}");
    if chain.is_empty() {
        entry.to_string()
    } else {
        format!("{},{}", chain.join(","), entry)
    }
}

fn attach_chain(resp: &mut Response<Body>, chain: &str) {
    if let Ok(v) = HeaderValue::from_str(chain) {
        resp.headers_mut()
            .insert(HeaderName::from_static("x-buffet-failover-chain"), v);
    }
}

fn parse_retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// 非流式响应 usage 记账：total_tokens 优先，否则 prompt+completion。
fn usage_tokens_json(bytes: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .map(|v| usage_from_value(&v))
        .unwrap_or(0)
}

fn usage_from_value(v: &serde_json::Value) -> u64 {
    let Some(u) = v.get("usage") else {
        return 0;
    };
    if let Some(t) = u.get("total_tokens").and_then(|x| x.as_u64()) {
        return t;
    }
    let p = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let c = u
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    p + c
}

/// 流式末块 usage 记账：仅当 chunk 含 "usage" 字样才逐行解析。
fn usage_tokens_sse(chunk: &Bytes) -> u64 {
    if chunk.len() < 5 || !chunk.windows(5).any(|w| w == b"usage") {
        return 0;
    }
    let text = String::from_utf8_lossy(chunk);
    let mut sum = 0u64;
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                sum += usage_from_value(&v);
            }
        }
    }
    sum
}

/// base_url 已含 /v1；upstream_rel 为去掉前导 v1/ 后的路径。
fn build_url(base: &str, rel: &str, query: Option<&str>) -> String {
    let base = base.trim_end_matches('/');
    let mut url = if rel.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rel}")
    };
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    url
}

fn is_hop_by_hop_or_strip(name: &HeaderName) -> bool {
    name == header::HOST
        || name == header::AUTHORIZATION
        || name == header::CONTENT_LENGTH
        || name == header::CONNECTION
        || name == header::TRANSFER_ENCODING
        || name == HeaderName::from_static("keep-alive")
        || name == header::UPGRADE
        || name == HeaderName::from_static("proxy-authorization")
        || name == HeaderName::from_static("te")
        || name == HeaderName::from_static("trailer")
}

fn json_error(status: u16, message: &str) -> Response<Body> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    let payload = serde_json::json!({
        "error": { "message": message, "type": "buffet_error", "code": status }
    });
    let mut r = Response::new(Body::from(payload.to_string()));
    *r.status_mut() = code;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}
