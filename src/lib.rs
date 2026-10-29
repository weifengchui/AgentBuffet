//! AgentBuffet（Agent食堂）网关内核：M1 转发链路 + M2 故障转移 + M3 热重载与用量记账
//! + M4 Web UI 与接入器管理面（设计文档 §8/§9）。

pub mod autostart;
pub mod config;
pub mod failover;
pub mod onboard;
pub mod proxy;
pub mod router;
pub mod scope;
pub mod securebox;
pub mod usage;
pub mod vendors;

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
        .route("/api/agents/detect", get(agents_detect))
        .route("/api/config", get(config_get))
        .route("/api/vendor-catalog", get(vendor_catalog))
        .route("/_admin/config", post(config_import))
        .route("/_admin/provider", post(provider_add))
        .route("/_admin/provider/edit", post(provider_edit))
        .route("/_admin/provider/delete", post(provider_delete))
        .route("/_admin/provider/keys", post(provider_keys))
        .route("/_admin/provider/models", post(provider_fetch_models))
        .route("/_admin/group", post(group_upsert))
        .route("/_admin/group/delete", post(group_delete))
        .route("/_admin/agent", post(agent_upsert))
        .route("/_admin/agent/delete", post(agent_delete))
        .route("/_admin/gateway", post(gateway_edit))
        .route("/_admin/export-enc", post(export_enc))
        .route("/_admin/import-enc", post(import_enc))
        .route("/api/agents/attach", post(agent_attach))
        .route("/api/agents/detach", post(agent_detach))
        .route("/api/autostart", get(autostart_get))
        .route("/_admin/autostart", post(autostart_set))
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
            models: routable_models(snap, &snap.agents[id].default_group),
            gateway_addr: snap.gateway_addr.to_string(),
            adapter: snap.agents[id].adapter.clone(),
        })
        .collect()
}

/// 组内可路由的真实模型（已排序去重），供注入器枚举可直接选择的模型。
pub fn routable_models(snap: &config::Snapshot, group: &str) -> Vec<String> {
    let mut v: Vec<String> = snap
        .active_members(group)
        .into_iter()
        .map(|m| m.model.clone())
        .collect();
    v.sort();
    v.dedup();
    v
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
    let html = UI_HTML.replace("__BUFFET_VERSION__", env!("CARGO_PKG_VERSION"));
    let mut r = Response::new(Body::from(html));
    *r.status_mut() = StatusCode::OK;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
            let keys: Vec<String> = std::env::var(&p.api_key_env)
                .map(|v| crate::proxy::split_keys(&v))
                .unwrap_or_default();
            let key_hints: Vec<String> = keys
                .iter()
                .map(|k| {
                    let tail: String = k
                        .chars()
                        .skip(k.chars().count().saturating_sub(4))
                        .collect();
                    format!("****{tail}")
                })
                .collect();
            serde_json::json!({
                "id": p.id,
                "vendor": p.vendor,
                "api_key_env": p.api_key_env,
                "key_count": keys.len(),
                "key_hints": key_hints,
                "models": p.models,
                "free_models": p.free_models,
                "base_url": p.base_url,
                "allow_private": p.allow_private,
                "disabled": p.disabled,
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
    let groups: Vec<serde_json::Value> = snap
        .raw
        .routing_groups
        .iter()
        .map(|g| {
            let name = g.name.as_str();
            let members: Vec<serde_json::Value> = snap
                .group_members(name)
                .into_iter()
                .map(|m| {
                    let disabled = m.disabled
                        || snap
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
    let mut out: Vec<serde_json::Value> = Vec::new();
    for s in list {
        let mut v = serde_json::to_value(&s).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(a) = snap.agents.get(&s.agent_id) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("default_group".into(), serde_json::json!(a.default_group));
                obj.insert("display_name".into(), serde_json::json!(a.display_name));
            }
        }
        out.push(v);
    }
    json_response(StatusCode::OK, serde_json::Value::Array(out))
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
    let existing = snap.agents.get(&req.agent_id);
    let mut gnames: Vec<&String> = snap.groups.keys().collect();
    gnames.sort();
    let groups: Vec<String> = gnames.iter().map(|s| (*s).clone()).collect();

    // 组：显式指定 > 已配置实例的默认组 > 第一个组。
    //
    // 多环境实例（hermes@wsl:Ubuntu / hermes@win:yxf）默认不在配置里，
    // 以前这里直接 400，导致 UI 的"快速接入"按钮点不动——现在允许直接接入，
    // 写完工具配置后自动补一条 agent 记录，让网关侧的 /<agent-id>/v1 对得上。
    let group = req
        .group
        .clone()
        .or_else(|| existing.map(|a| a.default_group.clone()))
        .or_else(|| groups.first().cloned())
        .unwrap_or_default();
    if group.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "配置中无可用路由组，请先创建路由组或指定 group"}),
        );
    }
    if !snap.groups.contains_key(&group) {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": format!("路由组不存在: {group}")}),
        );
    }
    let adapter = existing.map(|a| a.adapter.clone()).unwrap_or_default();
    let target = onboard::Target {
        agent_id: req.agent_id.clone(),
        default_group: group.clone(),
        groups,
        models: routable_models(&snap, &group),
        gateway_addr: snap.gateway_addr.to_string(),
        adapter: adapter.clone(),
    };
    let res = if req.dry_run {
        onboard::plan(&st.home, &target)
    } else {
        onboard::apply(&st.home, &target)
    };
    match res {
        Ok(r) => {
            let mut payload = serde_json::to_value(&r).unwrap_or_else(|_| serde_json::json!({}));
            // 未在配置里的实例：补一条 agent 记录并热重载，网关侧即刻认得这个前缀。
            if existing.is_none() && !req.dry_run {
                if let Some(path) = st.config_path.clone() {
                    match config::upsert_agent(&path, &req.agent_id, &group, &adapter) {
                        Ok(()) => {
                            let _ = do_reload(&st).await;
                        }
                        Err(e) => {
                            if let Some(obj) = payload.as_object_mut() {
                                obj.insert(
                                    "warning".into(),
                                    serde_json::json!(format!(
                                        "已写入工具配置，但补写 agent 配置失败: {e}"
                                    )),
                                );
                            }
                        }
                    }
                }
            }
            json_response(StatusCode::OK, payload)
        }
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
    let hint = {
        let snap = st.snap.read().await;
        snap.agents
            .get(&req.agent_id)
            .map(|a| a.adapter.clone())
            .unwrap_or_default()
    };
    let mut payload = match onboard::revert(&st.home, &req.agent_id, &hint) {
        Ok(r) => serde_json::to_value(&r).unwrap_or_else(|_| serde_json::json!({})),
        Err(e) => {
            // 配置文件本就不存在 / 环境暂时不可达：文件无从还原，但登记必须照删——
            // 否则 UI 永远显示"已接入"（"删了还提示已接入"的根因之一）。
            if !(e.contains("不存在") || e.contains("未找到运行环境")) {
                return json_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({"error": e}),
                );
            }
            serde_json::json!({
                "agent_id": req.agent_id,
                "files": [],
                "note": format!("无需还原工具文件（{e}），已删除网关登记"),
            })
        }
    };
    // 还原工具文件之外，还要删掉 config.yaml 里的 agent 登记并热重载；
    // 只 revert 不删登记的话，这条实例会一直留在列表里显示"已接入"，
    // 且 30s 漂移巡检还会继续按它写配置。
    if let Some(path) = st.config_path.clone() {
        match config::remove_agent(&path, &req.agent_id) {
            Ok(true) => {
                let _ = do_reload(&st).await;
            }
            Ok(false) => {}
            Err(e) => {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert(
                        "warning".into(),
                        serde_json::json!(format!("已还原工具配置，但删除 agent 登记失败: {e}")),
                    );
                }
            }
        }
    }
    json_response(StatusCode::OK, payload)
}

// ---------------------------------------------------------------- 开机自启

#[derive(Debug, Deserialize)]
struct AutostartReq {
    enable: bool,
}

async fn autostart_get(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    json_response(
        StatusCode::OK,
        serde_json::to_value(autostart::status(&st.home)).unwrap_or_else(|_| serde_json::json!({})),
    )
}

async fn autostart_set(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(req): Json<AutostartReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let res = if req.enable {
        autostart::enable(&st.home)
    } else {
        autostart::disable(&st.home)
    };
    match res {
        Ok(s) => json_response(
            StatusCode::OK,
            serde_json::to_value(s).unwrap_or_else(|_| serde_json::json!({})),
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
/// 自动修复已接入但配置漂移的 Agent（仅会写文件的适配器），返回被修复的 agent id。
pub async fn repair_drifted(st: &proxy::AppState) -> Vec<String> {
    let snap = st.snap.read().await.clone();
    let mut gnames: Vec<&String> = snap.groups.keys().collect();
    gnames.sort();
    let groups: Vec<String> = gnames.iter().map(|s| (*s).clone()).collect();

    // 巡检范围 = 网关配置里登记的 agent。
    //
    // 只覆盖"已登记"的：接入（attach）会把实例自动补登记进配置，因此用户接入过
    // 的实例（含 WSL 里的 hermes@wsl:Ubuntu）都在范围内；没接入过的不碰——
    // 自动改写用户没要求接入的环境，属于越权。
    //
    // 这样也带来一个硬保证：巡检只写"网关配置里点名的 agent 所在环境的配置
    // 文件"，不会因为扫到别的环境就把用户真实配置改掉。
    let ids: Vec<String> = {
        let mut v: Vec<String> = snap.agents.keys().cloned().collect();
        v.sort();
        v
    };

    let mut refreshed = Vec::new();
    for id in ids {
        // 已登记的实例用它自己的默认组与适配器。
        let Some(agent) = snap.agents.get(&id) else {
            continue;
        };
        let adapter = agent.adapter.clone();
        let default_group = agent.default_group.clone();
        if default_group.is_empty() {
            continue; // 没有任何路由组时构造不出注入目标
        }
        let a = onboard::Adapter::resolve(&id, &adapter);
        if !matches!(
            a,
            onboard::Adapter::OpenCode | onboard::Adapter::Codex | onboard::Adapter::Hermes
        ) {
            continue;
        }
        let target = onboard::Target {
            agent_id: id.clone(),
            default_group: default_group.clone(),
            groups: groups.clone(),
            models: routable_models(&snap, &default_group),
            gateway_addr: snap.gateway_addr.to_string(),
            adapter,
        };
        let d = onboard::detect(&st.home, &target);
        if d.attached && d.drifted && onboard::apply(&st.home, &target).is_ok() {
            refreshed.push(id);
        }
    }
    refreshed
}

async fn do_reload(st: &proxy::AppState) -> (StatusCode, serde_json::Value) {
    load_env_file(&st.env_file);
    let Some(path) = &st.config_path else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "无配置文件路径，不支持热重载"}),
        );
    };
    let filename = path.display().to_string();
    // 旧版（v0.2.16）遗留的 dash id 先迁移成 <kind>@<scope> 编码，再读取校验，
    // 否则 WSL/Windows 侧实例会被当成"本机实例"，跨环境读写串台。
    let renames = onboard::migrate_legacy_at(path, &st.home);
    if !renames.is_empty() {
        eprintln!(
            "AgentBuffet: 已迁移旧版 agent 登记：{}",
            renames
                .iter()
                .map(|(o, n)| if o == n {
                    format!("{o}（清除 home）")
                } else {
                    format!("{o} → {n}")
                })
                .collect::<Vec<_>>()
                .join("，")
        );
    }
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
    // 热重载后自动重写已接入但漂移的 Agent 配置（如路由组增删模型后需重新枚举）
    let refreshed = repair_drifted(st).await;
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
            "agents_refreshed": refreshed,
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
    #[serde(default)]
    disabled: bool,
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
    #[serde(default)]
    order: Option<Vec<String>>,
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
                if m.disabled {
                    map.insert(
                        serde_yaml::Value::String("disabled".into()),
                        serde_yaml::Value::Bool(true),
                    );
                }
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
    } else if let Some(order) = &body.order {
        let seq = match doc
            .get_mut("routing_groups")
            .and_then(|v| v.as_sequence_mut())
        {
            Some(s) => s,
            None => {
                return err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "配置中无 routing_groups 段".into(),
                )
            }
        };
        let mut items: Vec<serde_yaml::Value> = std::mem::take(seq);
        let mut out: Vec<serde_yaml::Value> = Vec::with_capacity(items.len());
        for name in order {
            match items
                .iter()
                .position(|it| yaml_str(it, "name").as_deref() == Some(name.as_str()))
            {
                Some(i) => out.push(items.remove(i)),
                None => {
                    return err(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        format!("路由组不存在: {name}"),
                    )
                }
            }
        }
        out.extend(items);
        *seq = out;
    } else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "body 需为 {provider, disabled} 或 {group, members:[{provider, model, priority}]} 或 {order:[组名,…]}"
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

// ---------------------------------------------------------------- 配置导入导出

/// 导出：返回当前 config.yaml 原文（Key 本就不在配置中，导出安全）。
async fn config_get(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    let Some(path) = &st.config_path else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "无配置文件路径，不支持导出".into(),
        );
    };
    let snap = st.snap.read().await.clone();
    let gw = serde_json::json!({
        "addr": snap.gateway_addr.to_string(),
        "ui_addr": snap.ui_addr.to_string(),
        "log_retain_days": snap.raw.gateway.log_retain_days,
        "expose_groups_as_models": snap.raw.gateway.expose_groups_as_models,
    });
    match std::fs::read_to_string(path) {
        Ok(text) => json_response(
            StatusCode::OK,
            serde_json::json!({
                "path": path.display().to_string(),
                "text": text,
                "gateway": gw,
            }),
        ),
        Err(e) => err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("读取配置失败: {e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct ConfigImport {
    text: String,
}

/// 导入：整份替换 config.yaml。校验失败 422 且不写盘；写前备份 `.bak`（已存在不覆盖）；
/// 成功后复用热重载，旧快照继续服务直到新配置生效。
async fn config_import(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<ConfigImport>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    let Some(path) = st.config_path.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "无配置文件路径，不支持导入".into(),
        );
    };
    let filename = path.display().to_string();
    if let Err(errs) = config::load_str(&body.text, &filename) {
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
    if let Err(e) = std::fs::write(&path, &body.text) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("写入配置失败: {e}"),
        );
    }
    let (code, payload) = do_reload(&st).await;
    json_response(code, payload)
}

// ---------------------------------------------------------------- 厂商快捷添加

/// 厂商目录（UI 下拉数据源）：静态数据，见 `vendors` 模块。
async fn vendor_catalog(ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    json_response(
        StatusCode::OK,
        serde_json::to_value(vendors::catalog()).unwrap_or_else(|_| serde_json::json!([])),
    )
}

#[derive(Debug, Deserialize)]
struct ProviderAdd {
    vendor: String,
    plan: String,
    api_key: String,
}

/// 把 `NAME=VALUE` 写入 env 文件（systemd EnvironmentFile 格式），同名行替换。
fn write_env_key(path: &std::path::Path, name: &str, value: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("{name}={value}");
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for l in text.lines() {
        match l.split_once('=') {
            Some((k, _)) if k.trim() == name => {
                out.push(line.clone());
                replaced = true;
            }
            _ => out.push(l.to_string()),
        }
    }
    if !replaced {
        out.push(line);
    }
    std::fs::write(path, out.join("\n") + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn seq_of_strings(v: &[String]) -> serde_yaml::Value {
    serde_yaml::Value::Sequence(
        v.iter()
            .map(|s| serde_yaml::Value::String(s.clone()))
            .collect(),
    )
}

/// 快捷添加 provider（UI Config 视图）：厂商 + 套餐 + Key → 写 config.yaml（追加/更新
/// provider 条目）+ Key 进 `<home>/env`（0600，绝不进配置）+ 进程环境变量即时生效 → 热重载。
/// 校验失败 422 不落盘；备份 `.bak` 已存在不覆盖（与 routing_edit 同一范式）。
async fn provider_add(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<ProviderAdd>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    let Some(path) = st.config_path.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "无配置文件路径，不支持添加 provider".into(),
        );
    };
    let api_keys = proxy::split_keys(body.api_key.trim());
    if api_keys.is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "API Key 不能为空".into());
    }
    let Some((pid, plan)) = vendors::lookup(&body.vendor, &body.plan) else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("厂商/套餐不存在: {}/{}", body.vendor, body.plan),
        );
    };
    let env_name = vendors::env_name(&pid);
    // 目录未给默认模型（如 OpenRouter）：先用 Key 试拉 /models，失败不阻断添加。
    let mut models = plan.models.clone();
    let mut free: Vec<String> = Vec::new();
    let mut extra_warnings: Vec<String> = Vec::new();
    if models.is_empty() {
        match fetch_models_list(&st, &plan.base_url, &env_name, Some(&api_keys[0])).await {
            Ok(pairs) => {
                models = pairs.iter().map(|(m, _)| m.clone()).collect();
                free = pairs
                    .iter()
                    .filter(|(_, f)| *f)
                    .map(|(m, _)| m.clone())
                    .collect();
            }
            Err(_) => {
                extra_warnings.push("模型列表拉取失败：可在服务商卡片点「拉取模型」重试".into())
            }
        }
    } else {
        free = models
            .iter()
            .filter(|m| free_rank(m) < 2)
            .cloned()
            .collect();
    }

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
            return json_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": "配置 YAML 解析失败", "details": [e.to_string()]}),
            )
        }
    };
    match doc.get("providers") {
        None => doc["providers"] = serde_yaml::Value::Sequence(Vec::new()),
        Some(v) if v.is_sequence() => {}
        Some(_) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "providers 字段不是列表".into(),
            )
        }
    }
    let list = doc
        .get_mut("providers")
        .and_then(|v| v.as_sequence_mut())
        .expect("providers 已确认为列表");
    // 已存在则只更新 base_url/api_key_env/models，保留 quota 等其余字段；否则追加。
    let mut updated = false;
    for item in list.iter_mut() {
        if yaml_str(item, "id").as_deref() == Some(pid.as_str()) {
            item["base_url"] = serde_yaml::Value::String(plan.base_url.clone());
            item["api_key_env"] = serde_yaml::Value::String(env_name.clone());
            item["models"] = seq_of_strings(&models);
            if free.is_empty() {
                item.as_mapping_mut()
                    .map(|m| m.remove(serde_yaml::Value::String("free_models".into())));
            } else {
                item["free_models"] = seq_of_strings(&free);
            }
            updated = true;
        }
    }
    if !updated {
        let mut map = serde_yaml::Mapping::new();
        map.insert(
            serde_yaml::Value::String("id".into()),
            serde_yaml::Value::String(pid.clone()),
        );
        map.insert(
            serde_yaml::Value::String("vendor".into()),
            serde_yaml::Value::String(body.vendor.clone()),
        );
        map.insert(
            serde_yaml::Value::String("base_url".into()),
            serde_yaml::Value::String(plan.base_url.clone()),
        );
        map.insert(
            serde_yaml::Value::String("api_key_env".into()),
            serde_yaml::Value::String(env_name.clone()),
        );
        map.insert(
            serde_yaml::Value::String("models".into()),
            seq_of_strings(&models),
        );
        if !free.is_empty() {
            map.insert(
                serde_yaml::Value::String("free_models".into()),
                seq_of_strings(&free),
            );
        }
        list.push(serde_yaml::Value::Mapping(map));
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
    if let Err(e) = write_env_key(&st.env_file, &env_name, &api_keys.join(",")) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("写入 {} 失败: {e}", st.env_file.display()),
        );
    }
    std::env::set_var(&env_name, api_keys.join(","));
    let (code, mut payload) = do_reload(&st).await;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("provider".into(), serde_json::json!(pid));
        obj.insert("api_key_env".into(), serde_json::json!(env_name));
        obj.insert("models".into(), serde_json::json!(models.len()));
        if !extra_warnings.is_empty() {
            let w = obj
                .entry("warnings")
                .or_insert_with(|| serde_json::json!([]));
            if let Some(arr) = w.as_array_mut() {
                arr.extend(extra_warnings.into_iter().map(serde_json::Value::String));
            }
        }
    }
    json_response(code, payload)
}

// ---------------------------------------------------------------- 表单化配置 CRUD

/// 通用「读-改-校验-备份-写-热重载」管道：闭包修改 YAML 文档并返回警告列表。
/// 任一步失败均 422 且不落盘；备份 `.bak` 已存在不覆盖（与 routing_edit 同范式）。
async fn apply_config_edit<F>(st: &Arc<proxy::AppState>, f: F) -> (StatusCode, serde_json::Value)
where
    F: FnOnce(&mut serde_yaml::Value) -> Result<Vec<String>, String>,
{
    let Some(path) = st.config_path.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "无配置文件路径，不支持编辑"}),
        );
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": format!("读取配置失败: {e}")}),
            )
        }
    };
    let mut doc: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": "配置 YAML 解析失败", "details": [e.to_string()]}),
            )
        }
    };
    let warnings = match f(&mut doc) {
        Ok(w) => w,
        Err(msg) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": msg}),
            )
        }
    };
    let new_text = match serde_yaml::to_string(&doc) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": format!("序列化失败: {e}")}),
            )
        }
    };
    let filename = path.display().to_string();
    if let Err(errs) = config::load_str(&new_text, &filename) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": "配置校验失败", "details": errs}),
        );
    }
    let bak = PathBuf::from(format!("{}.bak", path.display()));
    if !bak.exists() {
        if let Err(e) = std::fs::copy(&path, &bak) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": format!("备份失败: {e}")}),
            );
        }
    }
    if let Err(e) = std::fs::write(&path, &new_text) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": format!("写入配置失败: {e}")}),
        );
    }
    let (code, mut payload) = do_reload(st).await;
    if let Some(obj) = payload.as_object_mut() {
        if !warnings.is_empty() {
            obj.insert("warnings".into(), serde_json::json!(warnings));
        }
    }
    (code, payload)
}

fn ystr(v: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(v.to_string())
}

fn find_in_seq(doc: &serde_yaml::Value, key: &str, id_key: &str, id: &str) -> bool {
    doc.get(key)
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .any(|i| yaml_str(i, id_key).as_deref() == Some(id))
        })
        .unwrap_or(false)
}

fn ensure_seq(doc: &mut serde_yaml::Value, key: &str) {
    match doc.get(key) {
        None => doc[key] = serde_yaml::Value::Sequence(Vec::new()),
        Some(v) if v.is_sequence() => {}
        Some(_) => doc[key] = serde_yaml::Value::Sequence(Vec::new()),
    }
}

#[derive(Debug, Deserialize)]
struct ProviderEdit {
    id: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    quota_daily: Option<u64>,
    #[serde(default)]
    allow_private: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
}

async fn provider_edit(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<ProviderEdit>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let id = body.id.clone();
    let id2 = id.clone();
    let (code, payload) = apply_config_edit(&st, move |doc| {
        let list = doc
            .get_mut("providers")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "providers 字段缺失或不是列表".to_string())?;
        let item = list
            .iter_mut()
            .find(|i| yaml_str(i, "id").as_deref() == Some(id.as_str()))
            .ok_or_else(|| format!("provider 不存在: {id}"))?;
        if let Some(u) = body.base_url.filter(|x| !x.trim().is_empty()) {
            item["base_url"] = ystr(u.trim());
        }
        if let Some(ms) = body.models {
            item["models"] = seq_of_strings(
                &ms.iter()
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect::<Vec<_>>(),
            );
        }
        match body.quota_daily {
            Some(0) | None => {}
            Some(n) => {
                let mut q = serde_yaml::Mapping::new();
                q.insert(ystr("daily_tokens"), serde_yaml::Value::Number(n.into()));
                item["quota"] = serde_yaml::Value::Mapping(q);
            }
        }
        if let Some(b) = body.allow_private {
            if b {
                item["allow_private"] = serde_yaml::Value::Bool(true);
            } else {
                if let Some(m) = item.as_mapping_mut() {
                    m.remove(ystr("allow_private"));
                }
            }
        }
        if let Some(b) = body.disabled {
            if b {
                item["disabled"] = serde_yaml::Value::Bool(true);
            } else {
                if let Some(m) = item.as_mapping_mut() {
                    m.remove(ystr("disabled"));
                }
            }
        }
        Ok(vec![])
    })
    .await;
    // Key 单独走 env（0600），不进配置。
    if code == StatusCode::OK {
        if let Some(key) = body.api_key.filter(|k| !k.trim().is_empty()) {
            let env_name = {
                let snap = st.snap.read().await;
                snap.providers
                    .get(&id2)
                    .map(|p| p.api_key_env.clone())
                    .unwrap_or_else(|| vendors::env_name(&id2))
            };
            let val = proxy::split_keys(key.trim()).join(",");
            if let Err(e) = write_env_key(&st.env_file, &env_name, &val) {
                return json_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({"error": format!("写入 env 失败: {e}")}),
                );
            }
            std::env::set_var(&env_name, &val);
        }
    }
    json_response(code, payload)
}

/// 免费模型排序权重：`:free` 后缀最前，其次名称含 free，其余按字母序。
fn free_rank(id: &str) -> u8 {
    let l = id.to_lowercase();
    if l.ends_with(":free") {
        0
    } else if l.contains("free") {
        1
    } else {
        2
    }
}

/// 读取定价字段中的价格，缺失/不可解析返回 None。
fn price_zero(entry: &serde_json::Value, k: &str) -> Option<bool> {
    let v = entry.get("pricing")?.get(k)?;
    let n = match v {
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }?;
    Some(n == 0.0)
}

/// 免费模型判定：优先服务商定价（prompt 与 completion 均为 0，如 OpenRouter），
/// 无定价字段时退回名称启发式（`:free` 后缀或含 free，如 OpenCode 的 beam-free）。
fn model_is_free(entry: &serde_json::Value, id: &str) -> bool {
    match (price_zero(entry, "prompt"), price_zero(entry, "completion")) {
        (Some(p), Some(c)) => p && c,
        _ => free_rank(id) < 2,
    }
}

/// 从服务商 `/models` 拉取模型目录：免费模型优先，上限 200 个。
/// 返回 (模型 ID, 是否免费)。优先用显式 Key，否则读 env（多数服务商 /models 公开，可匿名访问）。
async fn fetch_models_list(
    st: &proxy::AppState,
    base_url: &str,
    api_key_env: &str,
    key_override: Option<&str>,
) -> Result<Vec<(String, bool)>, String> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let key = match key_override {
        Some(k) => Some(k.to_string()),
        None => std::env::var(api_key_env)
            .ok()
            .and_then(|v| proxy::split_keys(&v).into_iter().next()),
    };
    let mut req = st.client.get(&url);
    if let Some(k) = &key {
        req = req.bearer_auth(k);
    }
    let resp = tokio::time::timeout(Duration::from_secs(15), req.send())
        .await
        .map_err(|_| "模型列表拉取超时".to_string())?
        .map_err(|e| format!("模型列表拉取失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("模型列表拉取失败: HTTP {}", resp.status()));
    }
    let j: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("模型列表解析失败: {e}"))?;
    let arr = j
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| j.get("models").and_then(|d| d.as_array()))
        .ok_or_else(|| "模型列表响应缺少 data 字段".to_string())?;
    let mut ids: Vec<(String, bool)> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|i| i.as_str())?;
            Some((id.to_string(), model_is_free(m, id)))
        })
        .collect();
    ids.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ids.truncate(200);
    if ids.is_empty() {
        return Err("模型列表为空".to_string());
    }
    Ok(ids)
}

/// 将拉到的模型列表写入 config.yaml 的 provider 条目并热重载。
async fn set_provider_models(
    st: &Arc<proxy::AppState>,
    id: String,
    fetched: Vec<(String, bool)>,
) -> (StatusCode, serde_json::Value) {
    apply_config_edit(st, move |doc| {
        let list = doc
            .get_mut("providers")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "providers 字段缺失或不是列表".to_string())?;
        let item = list
            .iter_mut()
            .find(|i| yaml_str(i, "id").as_deref() == Some(id.as_str()))
            .ok_or_else(|| format!("provider 不存在: {id}"))?;
        let mut models: Vec<String> = fetched.iter().map(|(m, _)| m.clone()).collect();
        let mut free: Vec<String> = fetched
            .iter()
            .filter(|(_, f)| *f)
            .map(|(m, _)| m.clone())
            .collect();
        // 合并保留原声明模型（可能被路由组引用），避免替换后校验失败
        if let Some(cur) = item.get("models").and_then(|v| v.as_sequence()) {
            for c in cur.iter().filter_map(|v| v.as_str()) {
                if !models.iter().any(|m| m == c) {
                    models.push(c.to_string());
                    if !free.iter().any(|f| f == c) && free_rank(c) < 2 {
                        free.push(c.to_string());
                    }
                }
            }
        }
        let isf = |m: &str| free.iter().any(|f| f == m);
        models.sort_by(|a, b| isf(b).cmp(&isf(a)).then_with(|| a.cmp(b)));
        free.sort();
        item["models"] = seq_of_strings(&models);
        if free.is_empty() {
            item.as_mapping_mut()
                .map(|m| m.remove(serde_yaml::Value::String("free_models".into())));
        } else {
            item["free_models"] = seq_of_strings(&free);
        }
        Ok(vec![])
    })
    .await
}

/// POST /_admin/provider/models {id}：手动触发拉取服务商模型目录。
async fn provider_fetch_models(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<IdReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let id = body.id.clone();
    let Some(p) = st.snap.read().await.providers.get(&id).cloned() else {
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": format!("provider 不存在: {id}")}),
        );
    };
    let models = match fetch_models_list(&st, &p.base_url, &p.api_key_env, None).await {
        Ok(m) => m,
        Err(e) => {
            return json_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": e}),
            )
        }
    };
    let count = models.len();
    let (code, mut payload) = set_provider_models(&st, id, models).await;
    if code == StatusCode::OK {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("models_fetched".into(), serde_json::json!(count));
        }
    }
    json_response(code, payload)
}

/// 启动时自动为 models 为空的服务商拉取模型目录（如 OpenRouter 的免费模型目录），
/// 让 UI 下拉框可直接选模型；失败仅记日志，不阻断启动。
pub async fn auto_fetch_models(st: Arc<proxy::AppState>) {
    // 等 env 文件载入与服务就绪
    tokio::time::sleep(Duration::from_secs(2)).await;
    let targets: Vec<(String, String, String)> = {
        let snap = st.snap.read().await;
        snap.providers
            .values()
            .filter(|p| p.models.is_empty() && !p.disabled)
            .map(|p| (p.id.clone(), p.base_url.clone(), p.api_key_env.clone()))
            .collect()
    };
    for (id, base_url, env) in targets {
        match fetch_models_list(&st, &base_url, &env, None).await {
            Ok(models) => {
                let n = models.len();
                let (code, _) = set_provider_models(&st, id.clone(), models).await;
                if code == StatusCode::OK {
                    println!("已为服务商 {id} 自动拉取 {n} 个模型");
                }
            }
            Err(e) => eprintln!("服务商 {id} 模型目录自动拉取失败: {e}"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct IdReq {
    id: String,
}

async fn provider_delete(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<IdReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let id = body.id.clone();
    let (code, payload) = apply_config_edit(&st, move |doc| {
        let mut warnings = Vec::new();
        let list = doc
            .get_mut("providers")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "providers 字段缺失或不是列表".to_string())?;
        let before = list.len();
        list.retain(|i| yaml_str(i, "id").as_deref() != Some(id.as_str()));
        if list.len() == before {
            return Err(format!("provider 不存在: {id}"));
        }
        // 同步清理路由组里引用它的成员，避免悬空引用校验失败。
        if let Some(gs) = doc
            .get_mut("routing_groups")
            .and_then(|v| v.as_sequence_mut())
        {
            for g in gs.iter_mut() {
                let name = yaml_str(g, "name");
                if let Some(ms) = g.get_mut("members").and_then(|v| v.as_sequence_mut()) {
                    let n0 = ms.len();
                    ms.retain(|m| yaml_str(m, "provider").as_deref() != Some(id.as_str()));
                    if ms.len() != n0 {
                        if let Some(name) = name {
                            warnings.push(format!(
                                "已从路由组 {name} 移除 {}/{} 个成员",
                                name,
                                n0 - ms.len()
                            ));
                        }
                    }
                }
            }
        }
        // 成员清空的路由组一并删除（校验规则不允许空组）。
        if let Some(gs) = doc
            .get_mut("routing_groups")
            .and_then(|v| v.as_sequence_mut())
        {
            let empties: Vec<String> = gs
                .iter()
                .filter(|g| {
                    g.get("members")
                        .and_then(|m| m.as_sequence())
                        .map(|ms| ms.is_empty())
                        .unwrap_or(true)
                })
                .filter_map(|g| yaml_str(g, "name"))
                .collect();
            if !empties.is_empty() {
                gs.retain(|g| !empties.contains(&yaml_str(g, "name").unwrap_or_default()));
                warnings.push(format!("空路由组已一并删除: {}", empties.join(", ")));
            }
        }
        warnings.push(format!("env 中的 API Key 未删除，可在 {id} 复用"));
        Ok(warnings)
    })
    .await;
    json_response(code, payload)
}

#[derive(Debug, Clone, Deserialize)]
struct GroupMemberIn {
    provider: String,
    model: String,
    #[serde(default)]
    priority: u32,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct GroupUpsert {
    name: String,
    #[serde(default)]
    strategy: Option<String>,
    members: Vec<GroupMemberIn>,
}

async fn group_upsert(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<GroupUpsert>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    if body.name.trim().is_empty() {
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": "组名不能为空"}),
        );
    }
    if body.members.is_empty() {
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": "路由组至少需要一个成员"}),
        );
    }
    let name = body.name.trim().to_string();
    let strategy = body
        .strategy
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "order".into());
    let members = body.members.clone();
    let (code, payload) = apply_config_edit(&st, move |doc| {
        ensure_seq(doc, "routing_groups");
        let gs = doc
            .get_mut("routing_groups")
            .and_then(|v| v.as_sequence_mut())
            .expect("ensure_seq");
        let mut map = serde_yaml::Mapping::new();
        map.insert(ystr("name"), ystr(&name));
        if strategy != "order" {
            map.insert(ystr("strategy"), ystr(&strategy));
        }
        let ms: Vec<serde_yaml::Value> = members
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let mut e = serde_yaml::Mapping::new();
                e.insert(ystr("provider"), ystr(m.provider.trim()));
                e.insert(ystr("model"), ystr(m.model.trim()));
                e.insert(
                    ystr("priority"),
                    serde_yaml::Value::Number((m.priority.max(1) + i as u32).into()),
                );
                if m.disabled {
                    e.insert(ystr("disabled"), serde_yaml::Value::Bool(true));
                }
                serde_yaml::Value::Mapping(e)
            })
            .collect();
        map.insert(ystr("members"), serde_yaml::Value::Sequence(ms));
        match gs
            .iter_mut()
            .find(|g| yaml_str(g, "name").as_deref() == Some(name.as_str()))
        {
            Some(g) => *g = serde_yaml::Value::Mapping(map),
            None => gs.push(serde_yaml::Value::Mapping(map)),
        }
        Ok(vec![])
    })
    .await;
    json_response(code, payload)
}

#[derive(Debug, Deserialize)]
struct NameReq {
    name: String,
}

async fn group_delete(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<NameReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let name = body.name.clone();
    let (code, payload) = apply_config_edit(&st, move |doc| {
        let gs = doc
            .get_mut("routing_groups")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "routing_groups 字段缺失".to_string())?;
        let before = gs.len();
        gs.retain(|g| yaml_str(g, "name").as_deref() != Some(name.as_str()));
        if gs.len() == before {
            return Err(format!("路由组不存在: {name}"));
        }
        Ok(vec![])
    })
    .await;
    json_response(code, payload)
}

#[derive(Debug, Deserialize)]
struct AgentUpsert {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    default_group: String,
    #[serde(default)]
    adapter: Option<String>,
}

async fn agent_upsert(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<AgentUpsert>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let id = body.id.trim().to_string();
    if id.is_empty() || body.default_group.trim().is_empty() {
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": "agent id 与默认组不能为空"}),
        );
    }
    let dg = body.default_group.trim().to_string();
    let dn = body.display_name.unwrap_or_default();
    let (code, payload) = apply_config_edit(&st, move |doc| {
        if !find_in_seq(doc, "routing_groups", "name", &dg) {
            return Err(format!("路由组不存在: {dg}"));
        }
        ensure_seq(doc, "agents");
        let as_ = doc
            .get_mut("agents")
            .and_then(|v| v.as_sequence_mut())
            .expect("ensure_seq");
        let mut map = serde_yaml::Mapping::new();
        map.insert(ystr("id"), ystr(&id));
        if !dn.trim().is_empty() {
            map.insert(ystr("display_name"), ystr(dn.trim()));
        }
        if let Some(ad) = body
            .adapter
            .as_deref()
            .map(str::trim)
            .filter(|x| !x.is_empty())
        {
            map.insert(ystr("adapter"), ystr(ad));
        }
        map.insert(ystr("default_group"), ystr(&dg));
        match as_
            .iter_mut()
            .find(|a| yaml_str(a, "id").as_deref() == Some(id.as_str()))
        {
            Some(a) => {
                // 已在配置里的实例（如漂移后重接）：保留原有 display_name / adapter，
                // 只更新本次明确给出的字段，避免"快捷接入"把用户的显示名抹掉。
                if dn.trim().is_empty() {
                    if let Some(old) = yaml_str(a, "display_name") {
                        map.insert(ystr("display_name"), ystr(&old));
                    }
                }
                if body
                    .adapter
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    if let Some(old) = yaml_str(a, "adapter") {
                        map.insert(ystr("adapter"), ystr(&old));
                    }
                }
                *a = serde_yaml::Value::Mapping(map);
            }
            None => as_.push(serde_yaml::Value::Mapping(map)),
        }
        Ok(vec![])
    })
    .await;
    json_response(code, payload)
}

async fn agent_delete(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<IdReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let id = body.id.clone();
    let (code, payload) = apply_config_edit(&st, move |doc| {
        let as_ = doc
            .get_mut("agents")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "agents 字段缺失".to_string())?;
        let before = as_.len();
        as_.retain(|a| yaml_str(a, "id").as_deref() != Some(id.as_str()));
        if as_.len() == before {
            return Err(format!("agent 不存在: {id}"));
        }
        Ok(vec![format!(
            "已删除 agent {id} 的配置；若此前已接入工具，其本地配置文件不受影响，可在接入器视图还原"
        )])
    })
    .await;
    json_response(code, payload)
}

#[derive(Debug, Deserialize)]
struct GatewayEdit {
    #[serde(default)]
    addr: Option<String>,
    #[serde(default)]
    ui_addr: Option<String>,
    #[serde(default)]
    log_retain_days: Option<u32>,
    #[serde(default)]
    expose_groups_as_models: Option<bool>,
}

async fn gateway_edit(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<GatewayEdit>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let (code, payload) = apply_config_edit(&st, move |doc| {
        if doc.get("gateway").map(|v| !v.is_mapping()).unwrap_or(true) {
            doc["gateway"] = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        let g = doc
            .get_mut("gateway")
            .and_then(|v| v.as_mapping_mut())
            .expect("gateway");
        let mut set = |k: &str, v: Option<serde_yaml::Value>| {
            if let Some(v) = v {
                g.insert(ystr(k), v);
            }
        };
        set(
            "addr",
            body.addr
                .filter(|a| !a.trim().is_empty())
                .map(|a| ystr(a.trim())),
        );
        set(
            "ui_addr",
            body.ui_addr
                .filter(|a| !a.trim().is_empty())
                .map(|a| ystr(a.trim())),
        );
        if let Some(n) = body.log_retain_days {
            g.insert(ystr("log_retain_days"), serde_yaml::Value::Number(n.into()));
        }
        if let Some(b) = body.expose_groups_as_models {
            g.insert(ystr("expose_groups_as_models"), serde_yaml::Value::Bool(b));
        }
        Ok(vec![])
    })
    .await;
    json_response(code, payload)
}

#[derive(Debug, Deserialize)]
struct ProviderKeysReq {
    id: String,
    /// add | remove
    action: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    index: Option<usize>,
}

/// 多 Key 管理：直接操作 env 值（逗号分隔），即时生效，不改 config.yaml。
async fn provider_keys(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<ProviderKeysReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    let env_name = {
        let snap = st.snap.read().await;
        match snap.providers.get(&body.id) {
            Some(p) => p.api_key_env.clone(),
            None => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"error": format!("provider 不存在: {}", body.id)}),
                )
            }
        }
    };
    let raw = std::env::var(&env_name).unwrap_or_default();
    let mut keys = proxy::split_keys(&raw);
    match body.action.as_str() {
        "add" => {
            let k = body.key.unwrap_or_default();
            let k = k.trim().to_string();
            if k.is_empty() {
                return err(StatusCode::UNPROCESSABLE_ENTITY, "Key 不能为空".into());
            }
            if k.contains(',') {
                return err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "单个 Key 不得含逗号".into(),
                );
            }
            if keys.iter().any(|x| x == &k) {
                return err(StatusCode::UNPROCESSABLE_ENTITY, "该 Key 已存在".into());
            }
            keys.push(k);
        }
        "remove" => {
            let Some(i) = body.index else {
                return err(StatusCode::UNPROCESSABLE_ENTITY, "缺少 index".into());
            };
            if i >= keys.len() {
                return err(StatusCode::UNPROCESSABLE_ENTITY, "index 越界".into());
            }
            keys.remove(i);
        }
        _ => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "action 需为 add 或 remove".into(),
            )
        }
    }
    let val = keys.join(",");
    if let Err(e) = write_env_key(&st.env_file, &env_name, &val) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("写入 env 失败: {e}"),
        );
    }
    std::env::set_var(&env_name, &val);
    json_response(StatusCode::OK, serde_json::json!({"key_count": keys.len()}))
}

/// 本机 Agent 检测（接入器快速接入）：扫描**本机 + 每个 WSL 发行版**，
/// 按环境分组返回已安装的 Agent 实例。
///
/// 本机的 `hermes` 与 WSL 里的 `hermes@wsl:Ubuntu` 是两个独立条目，
/// 各自的配置文件、接入状态、漂移状态互不相干。
async fn agents_detect(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let snap = st.snap.read().await.clone();
    let detected = onboard::detect_all(&st.home, &snap);
    let scopes = scope::enumerate_cached(&st.home);

    // 按环境分组（只列装了 Agent 的环境）
    let mut groups: Vec<serde_json::Value> = Vec::new();
    for sc in &scopes {
        let items: Vec<&onboard::DetectedAgent> =
            detected.iter().filter(|a| a.scope_id == sc.id).collect();
        if items.is_empty() {
            continue;
        }
        groups.push(serde_json::json!({
            "id": sc.id,
            "kind": sc.kind.as_str(),
            "label": sc.label(),
            "distro": sc.distro,
            "home": sc.home.display().to_string(),
            "note": sc.note,
            "agents": items,
        }));
    }

    // 扁平列表：附上"是否已在网关配置里"与默认组，供前端一键接入
    let flat: Vec<serde_json::Value> = detected
        .iter()
        .map(|d| {
            let mut v = serde_json::to_value(d).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = v.as_object_mut() {
                let in_config = snap.agents.get(&d.agent_id);
                obj.insert("in_config".into(), serde_json::json!(in_config.is_some()));
                obj.insert(
                    "default_group".into(),
                    serde_json::json!(in_config.map(|a| a.default_group.clone())),
                );
                obj.insert(
                    "display_name".into(),
                    serde_json::json!(in_config
                        .map(|a| a.display_name.clone())
                        .unwrap_or_default()),
                );
            }
            v
        })
        .collect();

    json_response(
        StatusCode::OK,
        serde_json::json!({ "scopes": groups, "agents": flat }),
    )
}

// ---------------------------------------------------------------- 加密备份

/// 把 env 文件载入进程环境：仅补充当前未设置的变量（启动方显式导出的值优先）。
/// 启动与热重载各调用一次，Key 填进文件后 reload 即生效，degraded 随重载复位。
pub fn load_env_file(env_file: &std::path::Path) -> usize {
    let mut n = 0;
    for (k, v) in read_env_pairs(env_file) {
        if k.is_empty() {
            continue;
        }
        if std::env::var_os(&k).is_none() {
            std::env::set_var(&k, v);
            n += 1;
        }
    }
    n
}

fn read_env_pairs(path: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct EncReq {
    password: String,
}

/// 导出加密备份：config.yaml + env 全部键值 → JSON → Argon2id+AES-256-GCM → base64。
async fn export_enc(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<EncReq>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    if body.password.is_empty() {
        return err(StatusCode::UNPROCESSABLE_ENTITY, "密码不能为空".into());
    }
    let Some(path) = st.config_path.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "无配置文件路径".into());
    };
    let config_text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("读取配置失败: {e}"),
            )
        }
    };
    let env: serde_json::Map<String, serde_json::Value> = read_env_pairs(&st.env_file)
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    let plain = serde_json::to_vec(&serde_json::json!({"config": config_text, "env": env}))
        .unwrap_or_default();
    let blob = match securebox::seal(&body.password, &plain) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    use base64::Engine;
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json_response(
        StatusCode::OK,
        serde_json::json!({"filename": format!("agentbuffet-backup-{stamp}.aback"), "data_b64": data_b64}),
    )
}

#[derive(Debug, Deserialize)]
struct EncImport {
    password: String,
    data_b64: String,
}

/// 导入加密备份：解密 → 校验 config → 写 config.yaml（.bak）+ 合并 env 键 + 热重载。
async fn import_enc(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
    Json(body): Json<EncImport>,
) -> Response<Body> {
    if let Some(r) = guard(peer) {
        return r;
    }
    let err =
        |code: StatusCode, msg: String| json_response(code, serde_json::json!({"error": msg}));
    let Some(path) = st.config_path.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "无配置文件路径".into());
    };
    use base64::Engine;
    let blob = match base64::engine::general_purpose::STANDARD.decode(body.data_b64.trim()) {
        Ok(b) => b,
        Err(_) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "备份数据不是有效的 base64".into(),
            )
        }
    };
    let plain = match securebox::unseal(&body.password, &blob) {
        Ok(p) => p,
        Err(_) => {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "解密失败：密码错误，或备份文件不完整/已损坏".into(),
            )
        }
    };
    let val: serde_json::Value = match serde_json::from_slice(&plain) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::UNPROCESSABLE_ENTITY, "备份内容损坏".into()),
    };
    let Some(config_text) = val.get("config").and_then(|c| c.as_str()) else {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "备份缺少 config 字段".into(),
        );
    };
    let filename = path.display().to_string();
    if let Err(errs) = config::load_str(config_text, &filename) {
        return json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": "配置校验失败", "details": errs}),
        );
    }
    let bak = PathBuf::from(format!("{}.bak", path.display()));
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("创建目录 {} 失败: {e}", dir.display()),
            );
        }
    }
    // 首次运行（尚无 config.yaml）时没有旧文件可备份，直接写入即可
    if !bak.exists() && path.exists() {
        if let Err(e) = std::fs::copy(&path, &bak) {
            return err(StatusCode::UNPROCESSABLE_ENTITY, format!("备份失败: {e}"));
        }
    }
    if let Err(e) = std::fs::write(&path, config_text) {
        return err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("写入配置失败: {e}"),
        );
    }
    let mut env_count = 0usize;
    if let Some(map) = val.get("env").and_then(|e| e.as_object()) {
        for (k, v) in map {
            let Some(v) = v.as_str() else { continue };
            if let Err(e) = write_env_key(&st.env_file, k, v) {
                return err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("写入 env 失败: {e}"),
                );
            }
            std::env::set_var(k, v);
            env_count += 1;
        }
    }
    let (code, mut payload) = do_reload(&st).await;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("env_keys".into(), serde_json::json!(env_count));
    }
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
    let _key = std::env::var(&p.api_key_env)
        .map(|v| proxy::split_keys(&v).into_iter().next().unwrap_or_default())
        .unwrap_or_default();
    if _key.is_empty() {
        return json_response(
            StatusCode::OK,
            serde_json::json!({
                "ok": false, "status": null, "latency_ms": null,
                "error": format!("环境变量 {} 未设置", p.api_key_env),
            }),
        );
    }
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
        for m in snap.active_members(&a.default_group) {
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
