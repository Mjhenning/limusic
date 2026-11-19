//! HTTP handlers and bearer-token middleware for the local control API.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::integrations::token::tokens_equal;
use crate::state::QueueNeighbors;

use super::types::{ApiShared, ErrorBody};

/// Accept only `Authorization: Bearer <token>`. No query-string fallback, no CORS headers. The
/// header and token are never logged, including on failure.
async fn require_bearer(
    State(shared): State<Arc<ApiShared>>,
    req: Request,
    next: Next,
) -> Response {
    let supplied = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    let authorized = match supplied {
        Some(value) => tokens_equal(&value, &shared.token.read().await),
        None => false,
    };
    if authorized {
        next.run(req).await
    } else {
        ErrorBody::unauthorized().into_response()
    }
}

/// Build the router used by both the live server and the auth tests.
pub fn router(shared: Arc<ApiShared>) -> Router {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/now-playing", get(now_playing))
        .route("/api/v1/queue", get(queue))
        .layer(middleware::from_fn_with_state(shared.clone(), require_bearer))
        .with_state(shared)
}

/// `GET /api/v1/status`: the full playback snapshot plus device and Listen Together context.
/// Never includes the bearer token.
async fn status(State(shared): State<Arc<ApiShared>>) -> Response {
    let mut snap = shared.context.playback_snapshot().await;
    if let Some(object) = snap.as_object_mut() {
        object.insert("linked".into(), json!(shared.context.remote_linked()));
        object.insert("remote_linked".into(), json!(shared.context.remote_linked()));
        object.insert(
            "listen_together_role".into(),
            json!(shared.context.listen_together_role().await),
        );
    }
    Json(snap).into_response()
}

/// `GET /api/v1/now-playing`: the currently heard track and its transport facts, using the same
/// keys as the UI's `now-playing` and `playback-snapshot` events.
async fn now_playing(State(shared): State<Arc<ApiShared>>) -> Response {
    let snap = shared.context.playback_snapshot().await;
    Json(json!({
        "now": snap.get("now").cloned().unwrap_or(Value::Null),
        "paused": snap.get("paused").cloned().unwrap_or(json!(true)),
        "position": snap.get("position").cloned().unwrap_or(json!(0.0)),
        "duration": snap.get("duration").cloned().unwrap_or(json!(0.0)),
        "live": snap.get("live").cloned().unwrap_or(json!(false)),
    }))
    .into_response()
}

/// `GET /api/v1/queue`: the UI queue snapshot plus read-only `next`/`previous` track objects.
/// Item objects are unchanged. `current_index` mirrors the existing `currentIndex`; new top-level
/// fields are snake_case. `next` and `previous` are `null` when there is no such row in the current
/// (possibly shuffled) snapshot. `next_is_autoplay_fetch` says that a repeat-off tail would need
/// to fetch autoplay; the read itself never fetches or mutates state.
async fn queue(State(shared): State<Arc<ApiShared>>) -> Response {
    let mut snap = shared.context.queue_snapshot().await;
    let neighbors: QueueNeighbors = shared.context.queue_neighbors().await;
    if let Some(object) = snap.as_object_mut() {
        let current = object.get("currentIndex").cloned().unwrap_or(json!(0));
        object.insert("current_index".into(), current);
        let items = object.get("items").and_then(Value::as_array).cloned();
        let track = |index: Option<usize>| -> Value {
            index
                .and_then(|i| items.as_ref().and_then(|rows| rows.get(i)))
                .cloned()
                .unwrap_or(Value::Null)
        };
        object.insert("next".into(), track(neighbors.next));
        object.insert("previous".into(), track(neighbors.previous));
        object.insert("next_index".into(), json!(neighbors.next));
        object.insert("previous_index".into(), json!(neighbors.previous));
        object.insert("next_is_autoplay_fetch".into(), json!(neighbors.next_is_autoplay_fetch));
    }
    Json(snap).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::types::ControlApiContext;
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    struct MockContext;

    #[async_trait::async_trait]
    impl ControlApiContext for MockContext {
        async fn playback_snapshot(&self) -> Value {
            json!({ "now": { "videoId": "abc" }, "paused": false, "position": 1.0, "duration": 2.0 })
        }

        async fn queue_snapshot(&self) -> Value {
            json!({
                "items": [
                    { "videoId": "a", "queued_by": null },
                    { "videoId": "b", "queued_by": "friend" }
                ],
                "currentIndex": 0,
                "autoplay": true,
                "shuffle": false,
                "repeat": "off"
            })
        }

        async fn queue_neighbors(&self) -> QueueNeighbors {
            QueueNeighbors { next: Some(1), previous: None, next_is_autoplay_fetch: false }
        }

        fn remote_linked(&self) -> bool {
            false
        }

        async fn listen_together_role(&self) -> String {
            "none".to_string()
        }
    }

    fn test_router() -> Router {
        let shared = Arc::new(ApiShared {
            token: Arc::new(RwLock::new("abc123".to_string())),
            context: Arc::new(MockContext),
        });
        router(shared)
    }

    async fn get(path: &str, auth: Option<&str>) -> Response {
        let mut req = Request::builder().uri(path);
        if let Some(value) = auth {
            req = req.header(AUTHORIZATION, value);
        }
        test_router().oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let response = get("/api/v1/status", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"], "unauthorized");
        assert!(error["message"].as_str().is_some());
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let response = get("/api/v1/status", Some("Bearer wrong")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_correct_token_and_adds_context() {
        let response = get("/api/v1/status", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["listen_together_role"], "none");
        assert_eq!(json["remote_linked"], false);
    }

    #[tokio::test]
    async fn queue_returns_next_and_queued_by() {
        let response = get("/api/v1/queue", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["current_index"], 0);
        assert_eq!(json["currentIndex"], 0);
        assert_eq!(json["next"]["videoId"], "b");
        assert_eq!(json["next"]["queued_by"], "friend");
        assert_eq!(json["next_is_autoplay_fetch"], false);
        assert!(json["previous"].is_null());
    }

    #[tokio::test]
    async fn query_string_token_is_not_accepted() {
        let response = get("/api/v1/status?token=abc123", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
