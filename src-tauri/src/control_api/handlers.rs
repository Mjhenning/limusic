//! HTTP handlers and bearer-token middleware for the local control API.

use std::sync::Arc;

use crate::integrations::token::tokens_equal;
use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use super::types::{
    ApiShared, ErrorBody, NowPlayingResponse, OkResponse, PlaybackStateResponse, PreviousResponse,
    QueueResponse, RemoveRequest, SeekRequest, SeekResponse, StatusResponse, VolumeRequest,
    VolumeResponse,
};

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
        .route("/api/v1/playback/play", post(play))
        .route("/api/v1/playback/pause", post(pause))
        .route("/api/v1/playback/toggle", post(toggle))
        .route("/api/v1/playback/next", post(next))
        .route("/api/v1/playback/previous", post(previous))
        .route("/api/v1/playback/seek", post(seek))
        .route("/api/v1/playback/volume", post(volume))
        .route("/api/v1/queue/remove", post(remove))
        .route("/api/v1/queue/clear-queued", post(clear_queued))
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

/// Guest first, then remote link, before any handler touches playback or queue state. The order is
/// part of the contract: a linked guest gets the more specific Listen Together error.
async fn guard(shared: &ApiShared) -> Result<(), ErrorBody> {
    if shared.context.is_guest().await {
        return Err(ErrorBody::listen_together_guest());
    }
    if shared.context.remote_linked() {
        return Err(ErrorBody::remote_linked());
    }
    Ok(())
}

fn bad_request(error: &'static str, message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(StatusCode::BAD_REQUEST, error, message)
}

async fn final_playback_state(shared: &ApiShared) -> PlaybackStateResponse {
    let snap = shared.context.playback_snapshot().await;
    PlaybackStateResponse {
        playing: !snap.get("paused").and_then(serde_json::Value::as_bool).unwrap_or(true),
    }
}

/// `POST /api/v1/playback/play`: idempotent. Starts a restored queue when mpv is idle.
async fn play(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    shared.context.clone().play().await;
    Json(final_playback_state(&shared).await).into_response()
}

/// `POST /api/v1/playback/pause`: idempotent. Safe on an idle or already-paused queue.
async fn pause(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    shared.context.clone().pause().await;
    Json(final_playback_state(&shared).await).into_response()
}

/// `POST /api/v1/playback/toggle`: flips transport state and reports the final state.
async fn toggle(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    shared.context.clone().toggle().await;
    Json(final_playback_state(&shared).await).into_response()
}

/// `POST /api/v1/playback/next`: waits for the new current row, including a synchronous autoplay
/// fetch, and returns that queue item. A miss is a `next_timeout`, not a silent success.
async fn next(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    match shared.context.clone().next().await {
        Some(item) => Json(item).into_response(),
        None => ErrorBody::next_timeout().into_response(),
    }
}

/// `POST /api/v1/playback/previous`: uses the UI's rewind-then-step rule. `restarted` reports which
/// branch ran; the read-only queue `previous` field is always current-1.
async fn previous(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let restarted = shared.context.clone().previous().await;
    Json(PreviousResponse { ok: true, restarted }).into_response()
}

/// `POST /api/v1/playback/seek`: validates a finite position in seconds from 0 to the reported
/// duration, then echoes the effective position.
async fn seek(State(shared): State<Arc<ApiShared>>, Json(body): Json<SeekRequest>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    if !body.position.is_finite() || body.position < 0.0 {
        return bad_request("invalid_position", "position must be finite and non-negative")
            .into_response();
    }
    let snap = shared.context.playback_snapshot().await;
    let duration = snap.get("duration").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    if !duration.is_finite() || body.position > duration {
        return bad_request("invalid_position", "position exceeds the current duration")
            .into_response();
    }
    match shared.context.clone().seek(body.position).await {
        Ok(position) => Json(SeekResponse { position }).into_response(),
        Err(message) => ErrorBody::playback_error(message).into_response(),
    }
}

/// `POST /api/v1/playback/volume`: validates an integer 0–100 before touching the player.
async fn volume(State(shared): State<Arc<ApiShared>>, Json(body): Json<VolumeRequest>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    if !(0..=100).contains(&body.volume) {
        return bad_request("invalid_volume", "volume must be an integer from 0 to 100")
            .into_response();
    }
    match shared.context.clone().set_volume(body.volume).await {
        Ok(()) => Json(VolumeResponse { volume: body.volume }).into_response(),
        Err(message) => ErrorBody::playback_error(message).into_response(),
    }
}

/// `POST /api/v1/queue/remove`: removes an upcoming row only; out-of-range and current indices are
/// rejected instead of silently ignored.
async fn remove(State(shared): State<Arc<ApiShared>>, Json(body): Json<RemoveRequest>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let snap = shared.context.queue_snapshot().await;
    let current =
        snap.get("currentIndex").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    if body.index >= shared.context.clone().queue_len().await || body.index == current {
        return bad_request("invalid_index", "index is not an upcoming queue row").into_response();
    }
    shared.context.clone().remove_at(body.index).await;
    Json(OkResponse { ok: true }).into_response()
}

/// `POST /api/v1/queue/clear-queued`: clears manually queued rows via the existing app path.
async fn clear_queued(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    shared.context.clone().clear_queued().await;
    Json(OkResponse { ok: true }).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::types::ControlApiContext;
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::state::QueueNeighbors;

    #[derive(Debug)]
    struct MockContext {
        guest: bool,
        linked: bool,
        restarted: bool,
        paused: AtomicBool,
        position: f64,
        duration: f64,
        next_item: StdMutex<Option<Value>>,
        volume: AtomicI64,
        play_calls: AtomicUsize,
        pause_calls: AtomicUsize,
        toggle_calls: AtomicUsize,
        seek_calls: AtomicUsize,
        volume_calls: AtomicUsize,
        removals: StdMutex<Vec<usize>>,
        clears: AtomicUsize,
    }

    impl Default for MockContext {
        fn default() -> Self {
            Self {
                guest: false,
                linked: false,
                restarted: false,
                paused: AtomicBool::new(false),
                position: 1.0,
                duration: 10.0,
                next_item: StdMutex::new(Some(json!({ "videoId": "next" }))),
                volume: AtomicI64::new(100),
                play_calls: AtomicUsize::new(0),
                pause_calls: AtomicUsize::new(0),
                toggle_calls: AtomicUsize::new(0),
                seek_calls: AtomicUsize::new(0),
                volume_calls: AtomicUsize::new(0),
                removals: StdMutex::new(Vec::new()),
                clears: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ControlApiContext for MockContext {
        async fn playback_snapshot(&self) -> Value {
            json!({
                "now": { "videoId": "abc", "queued_by": "friend" },
                "paused": self.paused.load(Ordering::SeqCst),
                "position": self.position,
                "duration": self.duration,
                "volume": self.volume.load(Ordering::SeqCst),
                "speed": 1.0,
                "semitones": 0,
                "reverb": {},
            })
        }

        async fn queue_snapshot(&self) -> Value {
            json!({
                "items": [
                    { "videoId": "a" },
                    { "videoId": "b", "queued_by": "friend" },
                    { "videoId": "c" },
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
            self.linked
        }

        async fn listen_together_role(&self) -> String {
            if self.guest { "guest" } else { "none" }.to_string()
        }

        async fn is_guest(&self) -> bool {
            self.guest
        }

        async fn play(self: Arc<Self>) {
            self.play_calls.fetch_add(1, Ordering::SeqCst);
            self.paused.store(false, Ordering::SeqCst);
        }

        async fn pause(self: Arc<Self>) {
            self.pause_calls.fetch_add(1, Ordering::SeqCst);
            self.paused.store(true, Ordering::SeqCst);
        }

        async fn toggle(self: Arc<Self>) {
            self.toggle_calls.fetch_add(1, Ordering::SeqCst);
            let paused = !self.paused.load(Ordering::SeqCst);
            self.paused.store(paused, Ordering::SeqCst);
        }

        async fn next(self: Arc<Self>) -> Option<Value> {
            self.next_item.lock().unwrap().clone()
        }

        async fn previous(self: Arc<Self>) -> bool {
            self.restarted
        }

        async fn seek(self: Arc<Self>, position: f64) -> Result<f64, String> {
            self.seek_calls.fetch_add(1, Ordering::SeqCst);
            Ok(position)
        }

        async fn set_volume(self: Arc<Self>, volume: i64) -> Result<(), String> {
            self.volume_calls.fetch_add(1, Ordering::SeqCst);
            self.volume.store(volume, Ordering::SeqCst);
            Ok(())
        }

        async fn queue_len(self: Arc<Self>) -> usize {
            3
        }

        async fn remove_at(self: Arc<Self>, index: usize) {
            self.removals.lock().unwrap().push(index);
        }

        async fn clear_queued(self: Arc<Self>) {
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_router_with(context: MockContext) -> (Router, Arc<MockContext>) {
        let context = Arc::new(context);
        let shared = Arc::new(ApiShared {
            token: Arc::new(RwLock::new("abc123".to_string())),
            context: context.clone(),
        });
        (router(shared), context)
    }

    fn test_router() -> (Router, Arc<MockContext>) {
        test_router_with(MockContext::default())
    }

    async fn request(router: Router, path: &str, body: Value, auth: Option<&str>) -> Response {
        let mut req =
            Request::builder().uri(path).method(if body.is_null() { "GET" } else { "POST" });
        if let Some(value) = auth {
            req = req.header(AUTHORIZATION, value);
        }
        if body.is_null() {
            router.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
        } else {
            router
                .oneshot(
                    req.header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    }

    async fn get(path: &str, auth: Option<&str>) -> Response {
        request(test_router().0, path, Value::Null, auth).await
    }

    async fn post(path: &str, body: Value) -> Response {
        request(test_router().0, path, body, Some("Bearer abc123")).await
    }

    async fn json_body(response: Response) -> Value {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let response = get("/api/v1/status", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let error = json_body(response).await;
        assert_eq!(error["error"], "unauthorized");
        assert!(error["message"].as_str().is_some());
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let response = get("/api/v1/status", Some("Bearer wrong")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mutation_requires_bearer_too() {
        let response = request(test_router().0, "/api/v1/playback/play", json!({}), None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_correct_token_and_adds_context() {
        let response = get("/api/v1/status", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["listen_together_role"], "none");
        assert_eq!(json["remote_linked"], false);
        assert_eq!(json["now"]["queued_by"], "friend");
        assert_eq!(json["position"], 1.0);
    }

    #[tokio::test]
    async fn now_playing_carries_queued_by_and_seconds() {
        let response = get("/api/v1/now-playing", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["now"]["queued_by"], "friend");
        assert_eq!(json["position"], 1.0);
        assert_eq!(json["duration"], 10.0);
        assert_eq!(json["live"], false);
    }

    #[tokio::test]
    async fn queue_returns_next_and_queued_by() {
        let response = get("/api/v1/queue", Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
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

    #[tokio::test]
    async fn play_is_idempotent() {
        let (app, ctx) = test_router();
        for _ in 0..2 {
            let response =
                request(app.clone(), "/api/v1/playback/play", json!({}), Some("Bearer abc123"))
                    .await;
            assert_eq!(response.status(), StatusCode::OK);
            let json = json_body(response).await;
            assert_eq!(json["playing"], true);
        }
        assert_eq!(ctx.play_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn pause_is_idempotent() {
        let (app, ctx) = test_router();
        for _ in 0..2 {
            let response =
                request(app.clone(), "/api/v1/playback/pause", json!({}), Some("Bearer abc123"))
                    .await;
            assert_eq!(response.status(), StatusCode::OK);
            let json = json_body(response).await;
            assert_eq!(json["playing"], false);
        }
        assert_eq!(ctx.pause_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn toggle_reports_final_state() {
        let response = post("/api/v1/playback/toggle", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["playing"], false);
    }

    fn mutation_cases() -> Vec<(&'static str, Value)> {
        vec![
            ("/api/v1/playback/play", json!({})),
            ("/api/v1/playback/pause", json!({})),
            ("/api/v1/playback/toggle", json!({})),
            ("/api/v1/playback/next", json!({})),
            ("/api/v1/playback/previous", json!({})),
            ("/api/v1/playback/seek", json!({ "position": 1.0 })),
            ("/api/v1/playback/volume", json!({ "volume": 50 })),
            ("/api/v1/queue/remove", json!({ "index": 1 })),
            ("/api/v1/queue/clear-queued", json!({})),
        ]
    }

    #[tokio::test]
    async fn guest_rejection_precedes_remote_linked_for_every_mutation() {
        let (app, ctx) =
            test_router_with(MockContext { guest: true, linked: true, ..Default::default() });
        for (path, body) in mutation_cases() {
            let response = request(app.clone(), path, body, Some("Bearer abc123")).await;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
            let json = json_body(response).await;
            assert_eq!(json["error"], "listen_together_guest", "{path}");
        }
        assert_eq!(ctx.play_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.pause_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.seek_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.volume_calls.load(Ordering::SeqCst), 0);
        assert!(ctx.removals.lock().unwrap().is_empty());
        assert_eq!(ctx.clears.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn remote_linked_rejects_every_mutation() {
        let (app, ctx) = test_router_with(MockContext { linked: true, ..Default::default() });
        for (path, body) in mutation_cases() {
            let response = request(app.clone(), path, body, Some("Bearer abc123")).await;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
            let json = json_body(response).await;
            assert_eq!(json["error"], "remote_linked", "{path}");
        }
        assert_eq!(ctx.play_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.pause_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.seek_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.volume_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn validation_errors_do_not_reach_the_player() {
        let (app, ctx) = test_router();
        let cases = vec![
            ("/api/v1/playback/seek", json!({ "position": -1.0 }), "invalid_position"),
            ("/api/v1/playback/seek", json!({ "position": 11.0 }), "invalid_position"),
            ("/api/v1/playback/volume", json!({ "volume": -1 }), "invalid_volume"),
            ("/api/v1/playback/volume", json!({ "volume": 101 }), "invalid_volume"),
            ("/api/v1/queue/remove", json!({ "index": 3 }), "invalid_index"),
            ("/api/v1/queue/remove", json!({ "index": 0 }), "invalid_index"),
        ];
        for (path, body, code) in cases {
            let response = request(app.clone(), path, body, Some("Bearer abc123")).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            let json = json_body(response).await;
            assert_eq!(json["error"], code, "{path}");
        }
        assert_eq!(ctx.seek_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.volume_calls.load(Ordering::SeqCst), 0);
        assert!(ctx.removals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn seek_and_volume_echo_applied_values() {
        let response = post("/api/v1/playback/seek", json!({ "position": 5.0 })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["position"], 5.0);

        let response = post("/api/v1/playback/volume", json!({ "volume": 42 })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["volume"], 42);
    }

    #[tokio::test]
    async fn next_returns_new_current_item() {
        let response = post("/api/v1/playback/next", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["videoId"], "next");
    }

    #[tokio::test]
    async fn next_timeout_is_not_a_silent_success() {
        let (app, _) =
            test_router_with(MockContext { next_item: StdMutex::new(None), ..Default::default() });
        let response =
            request(app, "/api/v1/playback/next", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(json_body(response).await["error"], "next_timeout");
    }

    #[tokio::test]
    async fn previous_reports_rewind() {
        let (app, _) = test_router_with(MockContext { restarted: true, ..Default::default() });
        let response =
            request(app, "/api/v1/playback/previous", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["ok"], true);
        assert_eq!(json["restarted"], true);
    }

    #[tokio::test]
    async fn queue_edits_succeed() {
        let (app, ctx) = test_router();
        let response = request(
            app.clone(),
            "/api/v1/queue/remove",
            json!({ "index": 1 }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["ok"], true);

        let response =
            request(app, "/api/v1/queue/clear-queued", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["ok"], true);
        assert_eq!(*ctx.removals.lock().unwrap(), vec![1]);
        assert_eq!(ctx.clears.load(Ordering::SeqCst), 1);
    }
}
