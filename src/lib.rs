//! AgentBuffet（Agent食堂）网关内核：M1 转发链路 + M2 故障转移。

pub mod config;
pub mod failover;
pub mod proxy;
pub mod router;

use axum::body::Body;
use axum::extract::{Path as AxPath, Request, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use std::collections::HashSet;
use std::sync::Arc;

pub fn build_router(state: Arc<proxy::AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models_direct))
        .route("/{agent}/v1/models", get(models_agent))
        .fallback(fallback)
        .with_state(state)
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
