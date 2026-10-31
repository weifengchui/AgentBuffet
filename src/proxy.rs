//! OpenAI 兼容转发 + 故障转移：候选循环、熔断、额度软上限、429 被动校准。
//! 硬约束：流式零缓冲（仅首 chunk peek）；绝不写到一半换供应商。
//! M3：请求终局按 §8 记 usage 事件（非流式在 handle、流式在流完成分支）。

use crate::config::{Provider, Snapshot};
use crate::failover::{now_epoch_secs, StateTable};
use crate::router;
use crate::usage::{Event, Usage};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode};
use bytes::Bytes;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 流式 usage 跨 poll 累计器（unfold 闭包多次调用间读写）。
struct UsageAcc {
    pt: u64,
    ct: u64,
    total: u64,
}

impl UsageAcc {
    fn add(&mut self, p: u64, c: u64, t: u64) {
        self.pt += p;
        self.ct += c;
        self.total += t;
    }
}

pub struct AppState {
    pub snap: RwLock<Arc<Snapshot>>,
    pub client: reqwest::Client,
    pub states: Arc<StateTable>,
    /// M3 用量记录器；None 时完全不记账（m1/m2 测试路径）。
    pub usage: Option<Arc<Usage>>,
    /// 热重载读取的配置文件路径。
    pub config_path: Option<PathBuf>,
    /// 接入器读写 Agent 配置文件所用的用户主目录。
    pub home: PathBuf,
    /// API Key 存放的 env 文件（与 systemd EnvironmentFile 同一约定）。
    pub env_file: PathBuf,
}

fn mk_client(snap: &Snapshot) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(
            snap.raw.failover.timeouts.connect_seconds,
        ))
        .tcp_nodelay(true)
        .build()
        .expect("build reqwest client")
}

/// env 文件默认路径：`$AGENTBUFFET_HOME/env`，否则 `~/.agentbuffet/env`。
pub fn default_env_file() -> PathBuf {
    if let Ok(h) = std::env::var("AGENTBUFFET_HOME") {
        PathBuf::from(h).join("env")
    } else {
        crate::onboard::home_dir().join(".agentbuffet").join("env")
    }
}

impl AppState {
    pub fn new(snap: Snapshot) -> Self {
        Self {
            snap: RwLock::new(Arc::new(snap.clone())),
            client: mk_client(&snap),
            states: Arc::new(StateTable::new()),
            usage: None,
            config_path: None,
            home: crate::onboard::home_dir(),
            env_file: default_env_file(),
        }
    }

    /// 带用量记账与热重载配置路径的构造（main.rs 与 m3 测试用）。
    pub fn with_paths(snap: Snapshot, config_path: PathBuf, logs_dir: PathBuf) -> Self {
        let prices_path = snap.raw.prices.file.clone();
        let usage = Arc::new(Usage::new(logs_dir, &prices_path));
        Self {
            snap: RwLock::new(Arc::new(snap.clone())),
            client: mk_client(&snap),
            states: Arc::new(StateTable::new()),
            usage: Some(usage),
            config_path: Some(config_path),
            home: crate::onboard::home_dir(),
            env_file: default_env_file(),
        }
    }
}

/// 单次尝试的记账上下文（handle 构造，流式路径克隆进 StreamCtx）。
struct RecordCtx {
    usage: Arc<Usage>,
    agent: String,
    group: String,
    provider: String,
    model: String,
    retries: u32,
    t0: Instant,
}

/// attempt_once 终局响应附带的记账元数据。
struct RespMeta {
    status: u16,
    prompt_tokens: u64,
    completion_tokens: u64,
    /// true 时由流完成分支记账，handle 不再重复记。
    streamed: bool,
    /// fail chain 末段标签，如 "ok"、"400"、"timeout"。
    chain_entry: String,
}

/// 流完成时记账（tokens + usage 事件）。
struct StreamCtx {
    usage: Arc<Usage>,
    agent: String,
    group: String,
    provider: String,
    model: String,
    retries: u32,
    t0: Instant,
    /// 流开始时的 chain 快照（不含本次 ":ok" 末段）。
    chain: Vec<String>,
}

fn finish_stream(st_tbl: &StateTable, pid: &str, acc: &UsageAcc, ctx: Option<&StreamCtx>) {
    let now = now_epoch_secs();
    st_tbl.add_tokens(pid, acc.total, now);
    if let Some(c) = ctx {
        let mut fail_chain = c.chain.clone();
        fail_chain.push(format!("{}:ok", c.provider));
        c.usage.record(&Event {
            ts: now,
            agent: c.agent.clone(),
            group: c.group.clone(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            status: 200,
            prompt_tokens: acc.pt,
            completion_tokens: acc.ct,
            cost_est: c.usage.cost(&c.model, acc.pt, acc.ct),
            duration_ms: c.t0.elapsed().as_millis() as u64,
            retries: c.retries,
            fail_chain,
        });
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
    /// 400/404/410/422：该 model 对此供应商无效（如 OpenRouter 无此 ID、410 已下架）。
    /// 切换下一候选，但不计入熔断/degraded/额度——不是供应商健康失败。
    ModelMismatch,
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

static KEY_RR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// env 值 → Key 列表：逗号分隔、去空白、过滤空项（单 Key 与历史行为一致）。
pub fn split_keys(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 主转发入口：候选循环故障转移。
pub async fn handle(state: Arc<AppState>, req: Request) -> Response<Body> {
    let t0 = Instant::now();
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
    let (group, candidates) = match router::resolve(&snap, &agent_id, &model) {
        Ok(v) => v,
        Err(e) => return json_error(e.status(), &e.to_string()),
    };

    // 运行时过滤：degraded / 熔断 open / 额度耗尽。
    // 候选已按 priority 排序；被隔离的候选先单独记下，稍后按原始优先级顺序
    // 并回路由链——若像以前那样边过滤边写链，跳过的候选会全部挤在链首，
    // 看起来像被优先尝试过（例如 priority=1 的 Qiniu 成功，链却显示
    // opencode-zen:open → Qiniu:ok）。
    let ratio = snap.raw.gateway.conservative_quota_ratio;
    let now = now_epoch_secs();
    let mut avail: Vec<(&router::Candidate, &Provider, usize)> = Vec::new();
    let mut skips: Vec<(usize, String)> = Vec::new();
    for (idx, c) in candidates.iter().enumerate() {
        let Some(p) = snap.providers.get(&c.provider_id) else {
            continue;
        };
        match state.states.availability(&c.provider_id, p, ratio, now) {
            Ok(()) => avail.push((c, p, idx)),
            Err(u) => skips.push((idx, format!("{}:{}", p.id, u.tag()))),
        }
    }
    if avail.is_empty() {
        let chain: Vec<String> = skips.iter().map(|(_, s)| s.clone()).collect();
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
    // OpenCode Zen/Go 套餐要求 x-opencode-session 头（上游按其做路由亲和）；
    // 客户端未携带时注入每进程稳定的会话 ID，否则上游以 400 MissingSessionID 拒绝。
    if avail.iter().any(|(_, p, _)| p.vendor == "opencode")
        && !base_headers.contains_key(header::HeaderName::from_static("x-opencode-session"))
    {
        if let Ok(v) = HeaderValue::from_str(opencode_session_id()) {
            base_headers.insert(header::HeaderName::from_static("x-opencode-session"), v);
        }
    }

    let fo = &snap.raw.failover;
    let max_attempts = 1 + fo.max_retries as usize;
    let mut last_err: Option<(u16, String)> = None;
    let mut attempts = 0u32;
    let mut last_prov: Option<(String, String)> = None;
    let mut chain: Vec<String> = Vec::new();
    let mut skip_i = 0usize;

    for (c, p, idx) in avail.into_iter().take(max_attempts) {
        // 先把排在当前候选之前、但被运行时隔离的候选按优先级补进链。
        while skip_i < skips.len() && skips[skip_i].0 < idx {
            chain.push(skips[skip_i].1.clone());
            skip_i += 1;
        }
        attempts += 1;
        last_prov = Some((p.id.clone(), c.model.clone()));
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
        let rec = state.usage.as_ref().map(|u| RecordCtx {
            usage: u.clone(),
            agent: agent_id.clone(),
            group: group.clone(),
            provider: p.id.clone(),
            model: c.model.clone(),
            retries: attempts - 1,
            t0,
        });
        match attempt_once(&state, &snap, p, ob, &chain, rec.as_ref()).await {
            Ok((resp, meta)) => {
                // 非流式终局在此记账；流式由流完成分支记账（streamed=true）
                if let (Some(rec), false) = (&rec, meta.streamed) {
                    let mut fail_chain = chain.clone();
                    fail_chain.push(format!("{}:{}", rec.provider, meta.chain_entry));
                    rec.usage.record(&Event {
                        ts: now_epoch_secs(),
                        agent: rec.agent.clone(),
                        group: rec.group.clone(),
                        provider: rec.provider.clone(),
                        model: rec.model.clone(),
                        status: meta.status,
                        prompt_tokens: meta.prompt_tokens,
                        completion_tokens: meta.completion_tokens,
                        cost_est: rec.usage.cost(
                            &rec.model,
                            meta.prompt_tokens,
                            meta.completion_tokens,
                        ),
                        duration_ms: t0.elapsed().as_millis() as u64,
                        retries: rec.retries,
                        fail_chain,
                    });
                }
                return resp;
            }
            Err(fail) => {
                chain.push(format!("{}:{}", p.id, fail.tag));
                let now2 = now_epoch_secs();
                state.states.note_failure(&p.id, &fail.tag, now2);
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
                    FailKind::ModelMismatch => {
                        // 仅记最近失败原因（note_failure 已做），不改任何隔离状态
                    }
                }
                last_err = Some(fail.last);
            }
        }
    }

    // 尝试次数用尽：把尚未记录的隔离候选补到链尾，保持优先级顺序。
    for (_, s) in skips.iter().skip(skip_i) {
        chain.push(s.clone());
    }

    // 候选耗尽：返回最后一次错误，并记一条失败事件（tokens 0）
    let (status, msg) = last_err.unwrap_or((502, "故障转移候选全部失败".to_string()));
    if let Some(u) = &state.usage {
        let (provider, mdl) = last_prov.unwrap_or_else(|| (String::new(), String::new()));
        u.record(&Event {
            ts: now_epoch_secs(),
            agent: agent_id,
            group,
            provider,
            model: mdl,
            status,
            prompt_tokens: 0,
            completion_tokens: 0,
            cost_est: None,
            duration_ms: t0.elapsed().as_millis() as u64,
            retries: attempts.saturating_sub(1),
            fail_chain: chain.clone(),
        });
    }
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
    rec: Option<&RecordCtx>,
) -> Result<(Response<Body>, RespMeta), AttemptFail> {
    let fo = &snap.raw.failover;

    // 鉴权：env 值支持逗号分隔多 Key。轮转起点全局自增分摊负载；401/403 时换下一个
    // Key 重试，全部 Key 都鉴权失败才按 degraded 切换下一候选；缺失 Key 仍按 degraded。
    let keys: Vec<String> = std::env::var(&p.api_key_env)
        .map(|v| split_keys(&v))
        .unwrap_or_default();
    if keys.is_empty() {
        return Err(AttemptFail {
            tag: "nokey".into(),
            kind: FailKind::Degraded,
            last: (
                502,
                format!("provider {} 的 {} 未设置（degraded）", p.id, p.api_key_env),
            ),
        });
    }
    let n = keys.len();
    let start = if n > 1 {
        (KEY_RR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize) % n
    } else {
        0
    };
    let fb = Duration::from_secs(fo.timeouts.first_byte_seconds);
    let mut k = 0usize;
    let resp = loop {
        let idx = (start + k) % n;
        let mut hdrs = ob.headers.clone();
        match HeaderValue::from_maybe_shared(format!("Bearer {}", keys[idx]).into_bytes()) {
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
            .request(ob.method.clone(), &ob.url)
            .headers(hdrs)
            .body(ob.body.clone());
        let r = match tokio::time::timeout(fb, rb.send()).await {
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
                return Ok((
                    r,
                    RespMeta {
                        status: 504,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        streamed: false,
                        chain_entry: "timeout".into(),
                    },
                ));
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
                return Ok((
                    r,
                    RespMeta {
                        status: 502,
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        streamed: false,
                        chain_entry: "conn".into(),
                    },
                ));
            }
            Ok(Ok(r)) => r,
        };
        let sc = r.status().as_u16();
        if (sc == 401 || sc == 403) && k + 1 < n {
            k += 1;
            continue; // 该 Key 鉴权失败且还有备用 Key：换下一个 Key 重试
        }
        break r;
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
                chain,
                Duration::from_secs(fo.timeouts.stream_idle_seconds),
                rec,
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
        let (pt, ct, total) = usage_tokens_json(&bytes);
        if total > 0 {
            state.states.add_tokens(&p.id, total, now);
        }
        let mut r = Response::new(Body::from(bytes));
        *r.status_mut() = status;
        if let Ok(v) = HeaderValue::from_str(&ctype) {
            r.headers_mut().insert(header::CONTENT_TYPE, v);
        }
        attach_chain(&mut r, &chain_line(chain, &p.id, "ok"));
        return Ok((
            r,
            RespMeta {
                status: code,
                prompt_tokens: pt,
                completion_tokens: ct,
                streamed: false,
                chain_entry: "ok".into(),
            },
        ));
    }

    if code == 401 || code == 403 {
        return Err(AttemptFail {
            tag: code.to_string(),
            kind: FailKind::Degraded,
            last: (
                code,
                format!(
                    "上游 {} 鉴权失败（{code}，共 {n} 个 Key），已标记 degraded",
                    p.id
                ),
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
    if matches!(code, 400 | 404 | 410 | 422) {
        // 模型 ID 对本供应商无效（410=模型已下架）：带上游原文切换下一候选，全部失败时返回最后一个错误
        let body = resp.text().await.unwrap_or_default();
        let brief: String = body.chars().take(512).collect();
        return Err(AttemptFail {
            tag: code.to_string(),
            kind: FailKind::ModelMismatch,
            last: (code, format!("上游 {} {code}: {brief}", p.id)),
        });
    }
    if fo.retry_on_status.contains(&code) {
        return Err(AttemptFail {
            tag: code.to_string(),
            kind: FailKind::Circuit,
            last: (code, format!("上游 {} 返回 {code}", p.id)),
        });
    }
    // 其他不可重试的上游错误（如 405/415）：原样透传，换供应商也无意义
    let mut r = passthrough_buffered(resp, status, &ctype).await;
    attach_chain(&mut r, &chain_line(chain, &p.id, &code.to_string()));
    Ok((
        r,
        RespMeta {
            status: code,
            prompt_tokens: 0,
            completion_tokens: 0,
            streamed: false,
            chain_entry: code.to_string(),
        },
    ))
}

/// 流式尝试：peek 首 chunk 判定，成功则零缓冲透传并在完成时记账 usage。
/// 判定失败发生在任何字节写出之前，可安全切换下一候选。
async fn stream_attempt(
    state: &Arc<AppState>,
    p: &Provider,
    resp: reqwest::Response,
    chain: &[String],
    idle: Duration,
    rec: Option<&RecordCtx>,
) -> Result<(Response<Body>, RespMeta), AttemptFail> {
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
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
    let ctx = rec.map(|r| {
        Arc::new(StreamCtx {
            usage: r.usage.clone(),
            agent: r.agent.clone(),
            group: r.group.clone(),
            provider: r.provider.clone(),
            model: r.model.clone(),
            retries: r.retries,
            t0: r.t0,
            chain: chain.to_vec(),
        })
    });
    let (p0, c0, t0) = usage_tokens_sse(&first);
    let init = UsageAcc {
        pt: p0,
        ct: c0,
        total: t0,
    };
    let rest =
        futures_util::stream::unfold((Some(stream), init, ctx), move |(opt, mut acc, ctx)| {
            let st_tbl = st_tbl.clone();
            let pid = pid.clone();
            let ctx = ctx.clone();
            async move {
                let mut s = opt?;
                match tokio::time::timeout(idle, s.next()).await {
                    Ok(Some(Ok(b))) => {
                        let (p, c, t) = usage_tokens_sse(&b);
                        acc.add(p, c, t);
                        Some((Ok::<Bytes, BoxError>(b), (Some(s), acc, ctx)))
                    }
                    Ok(Some(Err(e))) => {
                        finish_stream(&st_tbl, &pid, &acc, ctx.as_deref());
                        Some((Err::<Bytes, BoxError>(Box::new(e)), (None, acc, ctx)))
                    }
                    Ok(None) => {
                        finish_stream(&st_tbl, &pid, &acc, ctx.as_deref());
                        None
                    }
                    Err(_) => {
                        finish_stream(&st_tbl, &pid, &acc, ctx.as_deref());
                        Some((
                            Err::<Bytes, BoxError>(Box::from("buffet: 上游流静默超时")),
                            (None, acc, ctx),
                        ))
                    }
                }
            }
        });

    let combined =
        futures_util::stream::once(async move { Ok::<Bytes, BoxError>(first) }).chain(rest);
    let mut r = Response::new(Body::from_stream(combined));
    *r.status_mut() = status;
    if let Ok(v) = HeaderValue::from_str(&ctype) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    attach_chain(&mut r, &chain_line(chain, &p.id, "ok"));
    Ok((
        r,
        RespMeta {
            status: status.as_u16(),
            prompt_tokens: 0,
            completion_tokens: 0,
            streamed: true,
            chain_entry: "ok".into(),
        },
    ))
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

/// 非流式响应 usage 记账：返回 (prompt, completion, total)，total 优先取 total_tokens。
fn usage_tokens_json(bytes: &[u8]) -> (u64, u64, u64) {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .map(|v| usage_from_value(&v))
        .unwrap_or((0, 0, 0))
}

fn usage_from_value(v: &serde_json::Value) -> (u64, u64, u64) {
    let Some(u) = v.get("usage") else {
        return (0, 0, 0);
    };
    let p = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let c = u
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let t = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(p + c);
    (p, c, t)
}

/// 流式末块 usage 记账：仅当 chunk 含 "usage" 字样才逐行解析。
fn usage_tokens_sse(chunk: &Bytes) -> (u64, u64, u64) {
    if chunk.len() < 5 || !chunk.windows(5).any(|w| w == b"usage") {
        return (0, 0, 0);
    }
    let text = String::from_utf8_lossy(chunk);
    let mut sp = 0u64;
    let mut sc = 0u64;
    let mut st = 0u64;
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                let (p, c, t) = usage_from_value(&v);
                sp += p;
                sc += c;
                st += t;
            }
        }
    }
    (sp, sc, st)
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

/// 每进程稳定的 OpenCode 会话 ID（伪 UUID 格式），仅用于补齐 x-opencode-session 头。
fn opencode_session_id() -> &'static str {
    static SID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SID.get_or_init(|| {
        let mut b = [0u8; 16];
        let _ = getrandom::getrandom(&mut b);
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    })
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
        // 网关按自身能力（reqwest gzip）声明编码并重写响应体；
        // 透传客户端 Accept-Encoding 会让上游回 br/zstd，压缩字节会原样漏给客户端
        || name == header::ACCEPT_ENCODING
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
