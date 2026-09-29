use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::metrics::{History, HistoryPoint, Snapshot};

pub struct AppState {
    pub latest: RwLock<Arc<Snapshot>>,
    pub history: RwLock<History>,
    pub token: Option<String>,
    pub interval_secs: u64,
}

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/metrics", get(all_metrics))
        .route("/cpu", get(cpu))
        .route("/memory", get(memory))
        .route("/disks", get(disks))
        .route("/history", get(history))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .route("/health", get(health))
        .nest("/api/v1", protected)
        .fallback(not_found)
        .with_state(state)
}

fn latest(state: &AppState) -> Arc<Snapshot> {
    state.latest.read().expect("latest lock poisoned").clone()
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn all_metrics(State(state): State<Arc<AppState>>) -> Response {
    Json(&*latest(&state)).into_response()
}

async fn cpu(State(state): State<Arc<AppState>>) -> Response {
    let s = latest(&state);
    Json(json!({ "timestamp": s.timestamp, "cpu": s.cpu })).into_response()
}

async fn memory(State(state): State<Arc<AppState>>) -> Response {
    let s = latest(&state);
    Json(json!({ "timestamp": s.timestamp, "memory": s.memory })).into_response()
}

async fn disks(State(state): State<Arc<AppState>>) -> Response {
    let s = latest(&state);
    Json(json!({ "timestamp": s.timestamp, "disks": s.disks })).into_response()
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// Only return points from the last N seconds.
    seconds: Option<u64>,
}

#[derive(Serialize)]
struct HistoryResponse {
    interval_secs: u64,
    count: usize,
    points: Vec<HistoryPoint>,
}

async fn history(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<HistoryResponse> {
    let since = q.seconds.map(|secs| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(secs)
    });
    let points = state.history.read().expect("history lock poisoned").since(since);
    Json(HistoryResponse {
        interval_secs: state.interval_secs,
        count: points.len(),
        points,
    })
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

async fn require_token(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let Some(expected) = state.token.as_deref() else {
        return next.run(req).await;
    };
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
