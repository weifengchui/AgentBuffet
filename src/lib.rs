//! AgentBuffet（Agent食堂）网关内核：M1 转发链路 + M2 故障转移 + M3 热重载与用量记账
//! + M4 Web UI 与接入器管理面（设计文档 §8/§9）。

pub mod config;
pub mod failover;
pub mod onboard;
pub mod proxy;
pub mod router;
pub mod usage;

use axum::body::Body;
use axum::extract::{ConnectInfo, Json, Path as AxPath, Request, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 内嵌单文件 UI（零运行时依赖：无框架、无外部资源）。
const UI_HTML: &str = include_str!("ui.html");

pub fn build_router(state: Arc<proxy::AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models_direct))
        .route("/{agent}/v1/models", get(models_agent))
        .fallback(fallback)
        .with_state(state)
}

/// `_admin` 管理面 + UI（挂 ui_addr，§5/§8/§9）：全部端点仅 loopback；serve 时需
/// `into_make_service_with_connect_info::<SocketAddr>()` 以校验来源。
pub fn build_admin_router(state: Arc<proxy::AppState>) -> Router {
    Router::new()
        .route("/", get(ui_index))
        .route("/api/overview", get(overview))
        .route("/api/providers", get(providers_view))
        .route("/api/routing", get(routing_view))
        .route("/api/agents", get(agents_view))
        .route("/api/agents/attach", post(agent_attach))
        .route("/api/agents/detach", post(agent_detach))
        .route("/_admin/reload", post(reload))
        .route("/_admin/routing", post(routing_edit))
        .route("/_admin/routing/test", post(routing_test))
        .with_state(state)
}

/// 由配置快照构造全部 agent 的注入目标（CLI status / UI Agents 视图共用）。
pub fn agent_targets(snap: &config::Snapshot) -> Vec<onboard::Target> {
    let mut gnames: Vec<&String> = snap.groups.keys().collect();
    gnames.sort();
    let groups: Vec<String> = gnames.iter().map(|s| (*s).clone()).collect();
    let mut ids: Vec<&String> = snap.agents.keys().collect();
    ids.sort();
    ids.into_iter()
        .map(|id| onboard::Target {
            agent_id: id.clone(),
            default_group: snap.agents[id].default_group.clone(),
            groups: groups.clone(),
            gateway_addr: snap.gateway_addr.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------- 守卫与响应

fn guard(peer: SocketAddr) -> Option<Response<Body>> {
    if peer.ip().is_loopback() {
        None
    } else {
        Some(json_response(
            StatusCode::FORBIDDEN,
            serde_json::json!({"error": "仅允许 loopback 访问 _admin"}),
        ))
    }
}

// ---------------------------------------------------------------- UI

async fn ui_index(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let mut r = Response::new(Body::from(UI_HTML));
    *r.status_mut() = StatusCode::OK;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    r
}

// ---------------------------------------------------------------- /api/*（只读）

async fn overview(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let now = failover::now_epoch_secs();
    let (totals, recent) = match &st.usage {
        Some(u) => {
            let mut t = usage::Counters::default();
            for c in u.counters().values() {
                t.requests += c.requests;
                t.prompt_tokens += c.prompt_tokens;
                t.completion_tokens += c.completion_tokens;
                t.cost_est += c.cost_est;
            }
            (t, u.recent(20))
        }
        None => (usage::Counters::default(), Vec::new()),
    };
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "date": usage::utc_date(now),
            "totals": totals,
            "recent": recent,
        }),
    )
}

async fn providers_view(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let snap = st.snap.read().await.clone();
    let now = failover::now_epoch_secs();
    let snaps = st.states.snapshot(now);
    let ratio = snap.raw.gateway.conservative_quota_ratio;
    let mut ids: Vec<&String> = snap.providers.keys().collect();
    ids.sort();
    let out: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| {
            let p = &snap.providers[id];
            let s = snaps.get(id);
            let daily = p.quota.as_ref().map(|q| q.daily_tokens);
            let tighten = s.map(|x| x.tighten).unwrap_or(1.0);
            let used = s.map(|x| x.tokens_used).unwrap_or(0);
            let cap = daily.map(|d| (d as f64 * ratio * tighten) as u64);
            let open_until = s.and_then(|x| x.circuit_open_until);
            let status = if p.disabled {
                "disabled"
            } else if s.map(|x| x.degraded).unwrap_or(false) {
                "degraded"
            } else if open_until.is_some() {
                "circuit_open"
            } else if cap.is_some() && cap.is_some_and(|c| used >= c) {
                "quota_exhausted"
            } else {
                "ok"
            };
            serde_json::json!({
                "id": p.id,
                "vendor": p.vendor,
                "status": status,
                "tokens_used": used,
                "quota_daily": daily,
                "effective_cap": cap,
                "last_fail_ts": s.and_then(|x| x.last_fail_ts),
                "last_fail_reason": s.and_then(|x| x.last_fail_reason.clone()),
                "circuit_open_until": open_until,
            })
        })
        .collect();
    json_response(StatusCode::OK, serde_json::Value::Array(out))
}

async fn routing_view(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let snap = st.snap.read().await.clone();
    let mut names: Vec<&String> = snap.groups.keys().collect();
    names.sort();
    let groups: Vec<serde_json::Value> = names
        .into_iter()
        .map(|name| {
            let g = &snap.groups[name];
            let members: Vec<serde_json::Value> = snap
                .group_members(name)
                .into_iter()
                .map(|m| {
                    let disabled = snap
                        .providers
                        .get(&m.provider)
                        .map(|p| p.disabled)
                        .unwrap_or(false);
                    serde_json::json!({
                        "provider": m.provider,
                        "model": m.model,
                        "priority": m.priority,
                        "disabled": disabled,
                    })
                })
                .collect();
            serde_json::json!({
                "name": g.name,
                "strategy": g.strategy,
                "members": members,
            })
        })
        .collect();
    json_response(StatusCode::OK, serde_json::json!({ "groups": groups }))
}

async fn agents_view(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let snap = st.snap.read().await.clone();
    let targets = agent_targets(&snap);
    let list = onboard::status_all(&st.home, &targets);
    json_response(
        StatusCode::OK,
        serde_json::to_value(&list).unwrap_or_else(|_| serde_json::json!([])),
    )
}

// ---------------------------------------------------------------- /api/agents 写操作

#[derive(Debug, Deserialize)]
struct AttachReq {
    agent_id: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct DetachReq {
    agent_id: String,
}

async fn agent_attach(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(req): Json<AttachReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let snap = st.snap.read().await.clone();
    let Some(a) = snap.agents.get(&req.agent_id) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": format!("配置中不存在 agent: {}", req.agent_id)}),
        );
    };
    let group = req.group.unwrap_or_else(|| a.default_group.clone());
    if !snap.groups.contains_key(&group) {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": format!("路由组不存在: {group}")}),
        );
    }
    let mut gnames: Vec<&String> = snap.groups.keys().collect();
    gnames.sort();
    let target = onboard::Target {
        agent_id: req.agent_id.clone(),
        default_group: group,
        groups: gnames.into_iter().cloned().collect(),
        gateway_addr: snap.gateway_addr.to_string(),
    };
    let res = if req.dry_run {
        onboard::plan(&st.home, &target)
    } else {
        onboard::apply(&st.home, &target)
    };
    match res {
        Ok(r) => json_response(
            StatusCode::OK,
            serde_json::to_value(&r).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Err(e) => json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": e}),
        ),
    }
}

async fn agent_detach(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(req): Json<DetachReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    match onboard::revert(&st.home, &req.agent_id) {
        Ok(r) => json_response(
            StatusCode::OK,
            serde_json::to_value(&r).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Err(e) => json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": e}),
        ),
    }
}

// ---------------------------------------------------------------- _admin/reload

/// 热重载核心（§5）：校验失败 422（带文件名），旧快照继续服务；
/// 成功则 swap 快照、复位 degraded/熔断（保留额度计数）、重读单价表（counters 保留）。
async fn do_reload(st: &proxy::AppState) -> (StatusCode, serde_json::Value) {
    let Some(path) = &st.config_path else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "无配置文件路径，不支持热重载"}),
        );
    };
    let filename = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": format!("读取配置 {filename} 失败: {e}")}),
            );
        }
    };
    let snap = match config::load_str(&text, &filename) {
        Ok(s) => s,
        Err(errs) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": "配置校验失败", "details": errs}),
            );
        }
    };
    let warnings = config::startup_warnings(&snap);
    let addr_changed = {
        let old = st.snap.read().await;
        old.gateway_addr != snap.gateway_addr || old.ui_addr != snap.ui_addr
    };
    *st.snap.write().await = Arc::new(snap.clone());
    st.states.reset();
    let mut w = warnings;
    if let Some(u) = &st.usage {
        let (prices, warn) = usage::load_prices(&snap.raw.prices.file);
        u.set_prices(prices);
        if let Some(x) = warn {
            w.push(x);
        }
    }
    (
        StatusCode::OK,
        serde_json::json!({
            "status": "reloaded",
            "addr_changed": addr_changed,
            "warnings": w,
        }),
    )
}

async fn reload(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let (code, payload) = do_reload(&st).await;
    json_response(code, payload)
}

// ---------------------------------------------------------------- _admin/routing

#[derive(Debug, Deserialize)]
struct MemberReq {
    provider: String,
    model: String,
    priority: u32,
}

#[derive(Debug, Deserialize)]
struct RoutingEdit {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    members: Option<Vec<MemberReq>>,
}

fn yaml_str(v: &serde_yaml::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// 改路由（§8 Routing 视图）：YAML 文本级定位编辑 → 校验 → 备份 → 写盘 → 复用 reload。
/// 校验失败 422 且不写盘；备份 `config.yaml.bak` 已存在不覆盖。
async fn routing_edit(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<RoutingEdit>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    let Some(path) = st.config_path.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "无配置文件路径，不支持改路由".into(),
        );
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("读取配置失败: {e}"),
            )
        }
    };
    let mut doc: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(d) => d,
        Err(e) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("配置 YAML 解析失败: {e}"),
            )
        }
    };

    if let (Some(provider), Some(disabled)) = (&body.provider, body.disabled) {
        let mut found = false;
        if let Some(seq) = doc.get_mut("providers").and_then(|v| v.as_sequence_mut()) {
            for item in seq {
                if yaml_str(item, "id").as_deref() == Some(provider.as_str()) {
                    item["disabled"] = serde_yaml::Value::Bool(disabled);
                    found = true;
                }
            }
        }
        if !found {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("provider 不存在: {provider}"),
            );
        }
    } else if let (Some(group), Some(members)) = (&body.group, &body.members) {
        let mut found = false;
        let seq: Vec<serde_yaml::Value> = members
            .iter()
            .map(|m| {
                let mut map = serde_yaml::Mapping::new();
                map.insert(
                    serde_yaml::Value::String("provider".into()),
                    serde_yaml::Value::String(m.provider.clone()),
                );
                map.insert(
                    serde_yaml::Value::String("model".into()),
                    serde_yaml::Value::String(m.model.clone()),
                );
                map.insert(
                    serde_yaml::Value::String("priority".into()),
                    serde_yaml::Value::Number(m.priority.into()),
                );
                serde_yaml::Value::Mapping(map)
            })
            .collect();
        if let Some(groups) = doc
            .get_mut("routing_groups")
            .and_then(|v| v.as_sequence_mut())
        {
            for item in groups {
                if yaml_str(item, "name").as_deref() == Some(group.as_str()) {
                    item["members"] = serde_yaml::Value::Sequence(seq.clone());
                    found = true;
                }
            }
        }
        if !found {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("路由组不存在: {group}"),
            );
        }
    } else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "body 需为 {provider, disabled} 或 {group, members:[{provider, model, priority}]}"
                .into(),
        );
    }

    let new_text = match serde_yaml::to_string(&doc) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, format!("序列化失败: {e}")),
    };
    let filename = path.display().to_string();
    if let Err(errs) = config::load_str(&new_text, &filename) {
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": "配置校验失败", "details": errs}),
        );
    }
    let bak = PathBuf::from(format!("{}.bak", path.display()));
    if !bak.exists() {
        if let Err(e) = std::fs::copy(&path, &bak) {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("备份 {} 失败: {e}", bak.display()),
            );
        }
    }
    if let Err(e) = std::fs::write(&path, &new_text) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("写入配置失败: {e}"),
        );
    }
    let (code, payload) = do_reload(&st).await;
    json_response(code, payload)
}

#[derive(Debug, Deserialize)]
struct TestReq {
    provider: String,
}

/// 连通性测试：GET {base_url}/models（Bearer 取自环境变量，绝不回显 Key），5s 超时。
async fn routing_test(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<TestReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let snap = st.snap.read().await.clone();
    let Some(p) = snap.providers.get(&body.provider) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": format!("provider 不存在: {}", body.provider)}),
        );
    };
    let Ok(_key) = std::env::var(&p.api_key_env) else {
        return json_response(
            StatusCode::OK,
            serde_json::json!({
                "ok": false, "status": null, "latency_ms": null,
                "error": format!("环境变量 {} 未设置", p.api_key_env),
            }),
        );
    };
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
    let t0 = Instant::now();
    let sent = tokio::time::timeout(
        Duration::from_secs(5),
        st.client.get(&url).bearer_auth(_key).send(),
    )
    .await;
    let latency = t0.elapsed().as_millis() as u64;
    let payload = match sent {
        Err(_) => serde_json::json!({
            "ok": false, "status": null, "latency_ms": latency, "error": "超时（5s）",
        }),
        Ok(Err(e)) => serde_json::json!({
            "ok": false, "status": null, "latency_ms": latency,
            "error": format!("请求失败: {e}"),
        }),
        Ok(Ok(resp)) => {
            let status = resp.status().as_u16();
            serde_json::json!({
                "ok": (200..300).contains(&status),
                "status": status,
                "latency_ms": latency,
                "error": if (200..300).contains(&status) { serde_json::Value::Null } else { serde_json::json!("上游返回非 2xx") },
            })
        }
    };
    json_response(StatusCode::OK, payload)
}

// ---------------------------------------------------------------- 网关面（M1 起）

async fn fallback(State(st): State<Arc<proxy::AppState>>, req: Request) -> Response<Body> {
    proxy::handle(st, req).await
}

async fn health() -> Response<Body> {
    let payload = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    });
    json_response(StatusCode::OK, payload)
}

/// 直连 /v1/models（无 agent 前缀）：仅暴露 group/ 虚拟模型。
async fn models_direct(State(st): State<Arc<proxy::AppState>>) -> Response<Body> {
    let snap = st.snap.read().await.clone();
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "object": "list",
            "data": group_models(&snap),
            "note": "unidentified agent; use /<agent>/v1/models for default-group models"
        }),
    )
}

async fn models_agent(
    AxPath(agent): AxPath<String>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    let snap = st.snap.read().await.clone();
    let mut data: Vec<serde_json::Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if let Some(a) = snap.agents.get(&agent) {
        for m in snap.group_members(&a.default_group) {
            if let Some(p) = snap.providers.get(&m.provider) {
                if seen.insert(m.model.clone()) {
                    data.push(serde_json::json!({
                        "id": m.model, "object": "model", "owned_by": p.vendor
                    }));
                }
            }
        }
    }
    data.extend(group_models(&snap));
    json_response(
        StatusCode::OK,
        serde_json::json!({ "object": "list", "data": data }),
    )
}

fn group_models(snap: &config::Snapshot) -> Vec<serde_json::Value> {
    if !snap.raw.gateway.expose_groups_as_models {
        return Vec::new();
    }
    let mut names: Vec<&String> = snap.groups.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|g| {
            serde_json::json!({
                "id": format!("group/{g}"), "object": "model", "owned_by": "agentbuffet"
            })
        })
        .collect()
}

fn json_response(status: StatusCode, payload: serde_json::Value) -> Response<Body> {
    let mut r = Response::new(Body::from(payload.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}
