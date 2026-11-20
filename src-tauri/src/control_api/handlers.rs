//! HTTP handlers and bearer-token middleware for the local control API.

use std::sync::Arc;

use crate::integrations::token::tokens_equal;
use axum::{
    extract::{Request, State},
    http::header::AUTHORIZATION,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};

use super::types::{ApiShared, ErrorBody, NowPlayingResponse, QueueResponse, StatusResponse};

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
    let snap = shared.context.playback_snapshot().await;
    let linked = shared.context.remote_linked();
    let role = shared.context.listen_together_role().await;
    Json(StatusResponse::from_snapshot(snap, linked, role)).into_response()
}

/// `GET /api/v1/now-playing`: the currently heard track and its transport facts. `now` carries
/// `queued_by` when present; `position` and `duration` are seconds. See [`NowPlayingResponse`].
async fn now_playing(State(shared): State<Arc<ApiShared>>) -> Response {
    let snap = shared.context.playback_snapshot().await;
    Json(NowPlayingResponse::from_snapshot(&snap)).into_response()
}

/// `GET /api/v1/queue`: a defined projection of the displayed queue. UI-only fields such as
/// `currentIndex` and `prevTrack` are deliberately absent. See [`QueueResponse`] for the contract.
async fn queue(State(shared): State<Arc<ApiShared>>) -> Response {
    let snap = shared.context.queue_snapshot().await;
    let neighbors = shared.context.queue_neighbors().await;
    Json(QueueResponse::from_snapshot(snap, neighbors)).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::types::ControlApiContext;
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::state::QueueNeighbors;

    struct MockContext;

    #[async_trait::async_trait]
    impl ControlApiContext for MockContext {
        async fn playback_snapshot(&self) -> Value {
            json!({
                "now": { "videoId": "abc", "queued_by": "friend" },
                "paused": false,
                "position": 1.0,
                "duration": 2.0,
                "volume": 100,
                "speed": 1.0,
                "semitones": 0,
                "reverb": {},
            })
        }

        async fn queue_snapshot(&self) -> Value {
            json!({
                "items": [
                    { "videoId": "a" },
                    { "videoId": "b", "queued_by": "friend" }
                ],
                "currentIndex": 0,
                "prevTrack": { "videoId": "old" },
                "sourceName": "ui-only",
                "sourceId": "ui-only",
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
        assert_eq!(json["now"]["queued_by"], "friend");
        assert_eq!(json["position"], 1.0);
    }

    #[tokio::test]
    async fn now_playing_carries_queued_by_and_seconds() {
        let response = get("/api/v1/now-playing", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["now"]["queued_by"], "friend");
        assert_eq!(json["position"], 1.0);
        assert_eq!(json["duration"], 2.0);
        assert_eq!(json["live"], false);
    }

    #[tokio::test]
    async fn queue_returns_next_and_queued_by() {
        let response = get("/api/v1/queue", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["current_index"], 0);
        assert_eq!(json["next"]["videoId"], "b");
        assert_eq!(json["next"]["queued_by"], "friend");
        assert_eq!(json["next_is_autoplay_fetch"], false);
        assert!(json["previous"].is_null());
        assert!(json.get("currentIndex").is_none());
        assert!(json.get("prevTrack").is_none());
        assert!(json.get("sourceName").is_none());
        assert_eq!(json["shuffle"], false);
        assert_eq!(json["repeat"], "off");
        assert_eq!(json["autoplay"], true);
    }

    #[tokio::test]
    async fn query_string_token_is_not_accepted() {
        let response = get("/api/v1/status?token=abc123", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
