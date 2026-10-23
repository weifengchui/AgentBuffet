//! AgentBuffet（Agent食堂）网关内核：M1 转发链路 + M2 故障转移 + M3 热重载与用量记账。

pub mod config;
pub mod failover;
pub mod proxy;
pub mod router;
pub mod usage;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxPath, Request, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

pub fn build_router(state: Arc<proxy::AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models_direct))
        .route("/{agent}/v1/models", get(models_agent))
        .fallback(fallback)
        .with_state(state)
}

/// `_admin` 管理面（挂 ui_addr，§5/§9）：仅 reload；serve 时需
/// `into_make_service_with_connect_info::<SocketAddr>()` 以校验来源。
pub fn build_admin_router(state: Arc<proxy::AppState>) -> Router {
    Router::new()
        .route("/_admin/reload", post(reload))
        .with_state(state)
}

/// 热重载（§5）：仅 loopback；校验失败 422（带文件名），旧快照继续服务；
/// 成功则 swap 快照、复位 degraded/熔断（保留额度计数）、重读单价表（counters 保留）。
async fn reload(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(st): State<Arc<proxy::AppState>>,
) -> Response<Body> {
    if !peer.ip().is_loopback() {
        return json_response(
            StatusCode::FORBIDDEN,
            serde_json::json!({"error": "仅允许 loopback 访问 _admin"}),
        );
    }
    let Some(path) = &st.config_path else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "无配置文件路径，不支持热重载"}),
        );
    };
    let filename = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return json_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({"error": format!("读取配置 {filename} 失败: {e}")}),
            );
        }
    };
    let snap = match config::load_str(&text, &filename) {
        Ok(s) => s,
        Err(errs) => {
            return json_response(
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
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "status": "reloaded",
            "addr_changed": addr_changed,
            "warnings": w,
        }),
    )
}

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
