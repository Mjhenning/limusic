//! HTTP handlers and bearer-token middleware for the local control API.

use std::sync::Arc;

use crate::integrations::parse::{parse_input, ParseError};
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
    QueueRequest, QueueRequestResponse, QueueResponse, RemoveRequest, ResolveError, SeekRequest,
    SeekResponse, SimpleQueueRequest, StatusResponse, VolumeRequest, VolumeResponse,
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
        .route("/api/v1/queue/request", post(queue_request))
        .route("/api/v1/queue/add", post(queue_add))
        .route("/api/v1/queue/play-next", post(queue_play_next))
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

/// One lock for every endpoint that changes the current track or the queue. Callers hold the guard
/// across their state read and mutation so two simultaneous requests cannot both act on the same
/// pre-change snapshot.
async fn mutation_lock(shared: &ApiShared) -> tokio::sync::MutexGuard<'_, ()> {
    shared.mutation.lock().await
}

/// `POST /api/v1/playback/play`: idempotent. Starts a restored queue when mpv is idle.
async fn play(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().play().await;
    Json(PlaybackStateResponse { playing }).into_response()
}

/// `POST /api/v1/playback/pause`: idempotent. Safe on an idle or already-paused queue.
async fn pause(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().pause().await;
    Json(PlaybackStateResponse { playing }).into_response()
}

/// `POST /api/v1/playback/toggle`: flips transport state and reports the final state.
async fn toggle(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().toggle().await;
    Json(PlaybackStateResponse { playing }).into_response()
}

/// `POST /api/v1/playback/next`: waits for the new current row, including a synchronous autoplay
/// fetch, and returns that queue item. A `next_timeout` includes the track that was current when
/// the wait expired; it does not guarantee the skip did not complete, so re-read `/status` before
/// retrying.
async fn next(State(shared): State<Arc<ApiShared>>) -> Response {
    let _lock = mutation_lock(&shared).await;
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    match shared.context.clone().next().await {
        Some(item) => Json(item).into_response(),
        None => {
            let current = shared.context.playback_snapshot().await.get("now").cloned();
            ErrorBody::next_timeout(current).into_response()
        }
    }
}

/// `POST /api/v1/playback/previous`: uses the UI's rewind-then-step rule. `restarted` reports which
/// branch ran; the read-only queue `previous` field is always current-1.
async fn previous(State(shared): State<Arc<ApiShared>>) -> Response {
    let _lock = mutation_lock(&shared).await;
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
/// rejected instead of silently ignored. An optional `video_id` makes the removal conditional: a
/// stale id gets `queue_changed` without touching the queue.
async fn remove(State(shared): State<Arc<ApiShared>>, Json(body): Json<RemoveRequest>) -> Response {
    let _lock = mutation_lock(&shared).await;
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let snap = shared.context.queue_snapshot().await;
    let current =
        snap.get("currentIndex").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    if body.index >= shared.context.clone().queue_len().await || body.index == current {
        return bad_request("invalid_index", "index is not an upcoming queue row").into_response();
    }
    if let Some(expected) = &body.video_id {
        let actual = snap
            .get("items")
            .and_then(serde_json::Value::as_array)
            .and_then(|rows| rows.get(body.index))
            .and_then(|row| row.get("videoId"))
            .and_then(serde_json::Value::as_str);
        if actual != Some(expected.as_str()) {
            return ErrorBody::queue_changed().into_response();
        }
    }
    shared.context.clone().remove_at(body.index).await;
    Json(OkResponse { ok: true }).into_response()
}

/// `POST /api/v1/queue/clear-queued`: clears manually queued rows via the existing app path.
async fn clear_queued(State(shared): State<Arc<ApiShared>>) -> Response {
    let _lock = mutation_lock(&shared).await;
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    shared.context.clone().clear_queued().await;
    Json(OkResponse { ok: true }).into_response()
}

fn parse_error(error: ParseError) -> ErrorBody {
    match error {
        ParseError::InvalidInput => bad_request("invalid_input", "input is empty or not a song"),
        ParseError::UnsupportedUrl => {
            bad_request("unsupported_url", "only YouTube URLs are accepted")
        }
        ParseError::PlaylistNotSupported => bad_request(
            "playlist_not_supported",
            "playlists are not supported; use one video or a query",
        ),
    }
}

fn resolve_error(error: ResolveError) -> ErrorBody {
    match error {
        ResolveError::NotFound => ErrorBody::not_found(),
        ResolveError::ResolveFailed(message) => ErrorBody::resolve_failed(message),
        ResolveError::LiveStream => ErrorBody::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "duration_unknown",
            "live streams have no normal duration and cannot be queued",
        ),
    }
}

/// Parse a `SongItem.duration` string (`m:ss` / `h:m:ss`) into whole seconds. `None` means the
/// metadata did not give us a usable length, which is the `duration_unknown` case.
fn duration_secs(text: Option<&str>) -> Option<u64> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut parts = 0;
    for part in text.split(':') {
        parts += 1;
        if parts > 3 {
            return None;
        }
        total = total.checked_mul(60)?.checked_add(part.parse::<u64>().ok()?)?;
    }
    Some(total)
}

/// `POST /api/v1/queue/request`: resolve one song and add it to the queue. The response is
/// `{ok, position, item}`; `position` is the final zero-based index and `item` includes `duration`
/// and `queued_by`.
///
/// `max_duration_sec` fails closed with 422 `duration_unknown` unless `allow_unknown_duration` is
/// true; a known but longer track gets 422 `too_long` with `duration_sec` and `max_duration_sec`.
/// Duplicate rejection (409 with the existing `position`) is skipped by `allow_duplicates`, but the
/// app's global `prevent_duplicates` setting still applies inside the insertion. A timeout from a
/// previous skip never leaves this endpoint guessing: it holds the same mutation lock as other
/// current-track/queue changes.
async fn handle_queue_request(shared: Arc<ApiShared>, body: QueueRequest) -> Response {
    let _lock = mutation_lock(&shared).await;
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let parsed = match parse_input(&body.input) {
        Ok(parsed) => parsed,
        Err(error) => return parse_error(error).into_response(),
    };
    let mut item = match shared.context.resolve_input(&parsed).await {
        Ok(item) => item,
        Err(error) => return resolve_error(error).into_response(),
    };
    if let Some(max) = body.max_duration_sec {
        match duration_secs(item.duration.as_deref()) {
            None if !body.allow_unknown_duration => {
                return ErrorBody::duration_unknown().into_response()
            }
            Some(seconds) if seconds > max => {
                return ErrorBody::too_long(seconds, max).into_response()
            }
            _ => {}
        }
    }
    if let Some(by) = body.requested_by {
        let by = by.trim();
        if !by.is_empty() {
            item.queued_by = Some(by.to_string());
        }
    }
    match shared
        .context
        .clone()
        .insert_request(item.clone(), body.play_next, body.allow_duplicates)
        .await
    {
        Ok(position) => Json(QueueRequestResponse {
            ok: true,
            position,
            item: serde_json::to_value(item).unwrap_or(serde_json::Value::Null),
        })
        .into_response(),
        Err(position) => ErrorBody::duplicate(position).into_response(),
    }
}

async fn queue_request(
    State(shared): State<Arc<ApiShared>>,
    Json(body): Json<QueueRequest>,
) -> Response {
    handle_queue_request(shared, body).await
}

/// `POST /api/v1/queue/add`: thin append variant of `/queue/request` (same defaults as an empty
/// options object). `play_next` is false and `allow_duplicates` stays at the app default.
async fn queue_add(
    State(shared): State<Arc<ApiShared>>,
    Json(body): Json<SimpleQueueRequest>,
) -> Response {
    handle_queue_request(
        shared,
        QueueRequest {
            input: body.input,
            requested_by: body.requested_by,
            max_duration_sec: None,
            allow_duplicates: false,
            play_next: false,
            allow_unknown_duration: false,
        },
    )
    .await
}

/// `POST /api/v1/queue/play-next`: thin Play-next variant of `/queue/request`.
async fn queue_play_next(
    State(shared): State<Arc<ApiShared>>,
    Json(body): Json<SimpleQueueRequest>,
) -> Response {
    handle_queue_request(
        shared,
        QueueRequest {
            input: body.input,
            requested_by: body.requested_by,
            max_duration_sec: None,
            allow_duplicates: false,
            play_next: true,
            allow_unknown_duration: false,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::super::types::ControlApiContext;
    use super::*;
    use crate::integrations::parse::ParsedInput;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use innertube::SongItem;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::RwLock;
    use tokio::time::sleep;
    use tower::ServiceExt;

    use crate::state::QueueNeighbors;

    #[derive(Debug, Clone)]
    struct MockRow {
        id: String,
        autoplay: bool,
        queued: bool,
        queued_by: Option<String>,
    }

    #[derive(Debug)]
    struct MockContext {
        guest: bool,
        linked: bool,
        restarted: bool,
        paused: AtomicBool,
        /// The mirror read by `playback_snapshot`. Deliberately lags `paused` in the pause test,
        /// reproducing the async mpv event that made the old handler answer stale state.
        event_paused: AtomicBool,
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
        delay: Duration,
        active: AtomicUsize,
        max_active: AtomicUsize,
        resolve_item: SongItem,
        resolve_error: Option<ResolveError>,
        rows: StdMutex<Vec<MockRow>>,
        current: AtomicUsize,
        inserted: StdMutex<Vec<(SongItem, bool, bool)>>,
    }

    impl Default for MockContext {
        fn default() -> Self {
            Self {
                guest: false,
                linked: false,
                restarted: false,
                paused: AtomicBool::new(false),
                event_paused: AtomicBool::new(false),
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
                delay: Duration::ZERO,
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                resolve_item: SongItem {
                    video_id: "resolved123".to_string(),
                    title: "Resolved Song".to_string(),
                    artists: "Tester".to_string(),
                    duration: Some("3:00".to_string()),
                    ..Default::default()
                },
                resolve_error: None,
                rows: StdMutex::new(vec![
                    MockRow { id: "a".into(), autoplay: false, queued: false, queued_by: None },
                    MockRow {
                        id: "b".into(),
                        autoplay: false,
                        queued: false,
                        queued_by: Some("friend".into()),
                    },
                    MockRow { id: "c".into(), autoplay: true, queued: false, queued_by: None },
                ]),
                current: AtomicUsize::new(0),
                inserted: StdMutex::new(Vec::new()),
            }
        }
    }

    impl MockContext {
        async fn enter(&self) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            if !self.delay.is_zero() {
                // Sleep while still counted, so overlapping calls are visible to the test.
                sleep(self.delay).await;
            }
        }

        fn exit(&self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ControlApiContext for MockContext {
        async fn playback_snapshot(&self) -> Value {
            json!({
                "now": { "videoId": "abc", "queued_by": "friend" },
                "paused": self.event_paused.load(Ordering::SeqCst),
                "position": self.position,
                "duration": self.duration,
                "volume": self.volume.load(Ordering::SeqCst),
                "speed": 1.0,
                "semitones": 0,
                "reverb": {},
            })
        }

        async fn queue_snapshot(&self) -> Value {
            let rows = self.rows.lock().unwrap();
            let items: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let mut item = json!({
                        "videoId": row.id,
                        "autoplay": row.autoplay,
                        "queued": row.queued,
                    });
                    if let Some(by) = &row.queued_by {
                        item.as_object_mut().unwrap().insert("queued_by".into(), json!(by));
                    }
                    item
                })
                .collect();
            json!({
                "items": items,
                "currentIndex": self.current.load(Ordering::SeqCst),
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

        async fn play(self: Arc<Self>) -> bool {
            self.play_calls.fetch_add(1, Ordering::SeqCst);
            self.paused.store(false, Ordering::SeqCst);
            self.event_paused.store(false, Ordering::SeqCst);
            true
        }

        async fn pause(self: Arc<Self>) -> bool {
            self.pause_calls.fetch_add(1, Ordering::SeqCst);
            self.paused.store(true, Ordering::SeqCst);
            // The real mpv `Playing(false)` event arrives on the pump later; leave the mirror
            // stale on purpose so the endpoint must use the command's returned state.
            false
        }

        async fn toggle(self: Arc<Self>) -> bool {
            self.toggle_calls.fetch_add(1, Ordering::SeqCst);
            let paused = !self.paused.load(Ordering::SeqCst);
            self.paused.store(paused, Ordering::SeqCst);
            !paused
        }

        async fn next(self: Arc<Self>) -> Option<Value> {
            self.enter().await;
            let item = self.next_item.lock().unwrap().clone();
            self.exit();
            item
        }

        async fn previous(self: Arc<Self>) -> bool {
            self.enter().await;
            let restarted = self.restarted;
            self.exit();
            restarted
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
            self.rows.lock().unwrap().len()
        }

        async fn remove_at(self: Arc<Self>, index: usize) {
            self.enter().await;
            self.removals.lock().unwrap().push(index);
            let mut rows = self.rows.lock().unwrap();
            if index < rows.len() {
                rows.remove(index);
                if index < self.current.load(Ordering::SeqCst) {
                    self.current.fetch_sub(1, Ordering::SeqCst);
                }
            }
            self.exit();
        }

        async fn clear_queued(self: Arc<Self>) {
            self.enter().await;
            self.clears.fetch_add(1, Ordering::SeqCst);
            self.rows.lock().unwrap().retain(|row| !row.queued);
            self.exit();
        }

        async fn resolve_input(&self, parsed: &ParsedInput) -> Result<SongItem, ResolveError> {
            if let Some(error) = &self.resolve_error {
                return Err(error.clone());
            }
            if matches!(parsed, ParsedInput::Query(query) if query.contains("next")) {
                return Ok(resolved("next1234567", Some("3:00")));
            }
            Ok(self.resolve_item.clone())
        }

        async fn insert_request(
            self: Arc<Self>,
            item: SongItem,
            next: bool,
            allow_duplicates: bool,
        ) -> Result<usize, usize> {
            self.enter().await;
            let result = {
                let mut rows = self.rows.lock().unwrap();
                if !allow_duplicates {
                    if let Some(existing) = rows.iter().position(|row| row.id == item.video_id) {
                        Err(existing)
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
                .and_then(|()| {
                    let at = if next {
                        let mut at = self.current.load(Ordering::SeqCst) + 1;
                        while at < rows.len() && rows[at].queued {
                            at += 1;
                        }
                        at
                    } else {
                        rows.iter().position(|row| row.autoplay).unwrap_or(rows.len())
                    };
                    rows.insert(
                        at,
                        MockRow {
                            id: item.video_id.clone(),
                            autoplay: false,
                            queued: next,
                            queued_by: item.queued_by.clone(),
                        },
                    );
                    Ok(at)
                })
            };
            if result.is_ok() {
                self.inserted.lock().unwrap().push((item, next, allow_duplicates));
            }
            self.exit();
            result
        }
    }

    fn resolved(id: &str, duration: Option<&str>) -> SongItem {
        SongItem {
            video_id: id.to_string(),
            title: "Resolved Song".to_string(),
            artists: "Tester".to_string(),
            duration: duration.map(str::to_string),
            ..Default::default()
        }
    }

    fn test_router_with(context: MockContext) -> (Router, Arc<MockContext>) {
        let context = Arc::new(context);
        let shared = Arc::new(ApiShared {
            token: Arc::new(RwLock::new("abc123".to_string())),
            context: context.clone(),
            mutation: Arc::new(tokio::sync::Mutex::new(())),
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
    async fn pause_is_idempotent_even_with_a_stale_state_mirror() {
        let (app, ctx) = test_router_with(MockContext {
            event_paused: AtomicBool::new(true), // mpv paused, but its event has not been pumped
            ..Default::default()
        });
        for _ in 0..2 {
            let response =
                request(app.clone(), "/api/v1/playback/pause", json!({}), Some("Bearer abc123"))
                    .await;
            assert_eq!(response.status(), StatusCode::OK);
            let json = json_body(response).await;
            assert_eq!(json["playing"], false);
        }
        assert_eq!(ctx.pause_calls.load(Ordering::SeqCst), 2);
        assert!(ctx.event_paused.load(Ordering::SeqCst)); // response did not trust the stale mirror

        let response =
            request(app, "/api/v1/playback/toggle", json!({}), Some("Bearer abc123")).await;
        assert_eq!(json_body(response).await["playing"], true);
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
            ("/api/v1/queue/request", json!({ "input": "a song" })),
            ("/api/v1/queue/add", json!({ "input": "a song" })),
            ("/api/v1/queue/play-next", json!({ "input": "a song" })),
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
        assert!(ctx.inserted.lock().unwrap().is_empty());
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
        assert!(ctx.inserted.lock().unwrap().is_empty());
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
        let json = json_body(response).await;
        assert_eq!(json["error"], "next_timeout");
        assert_eq!(json["current"]["videoId"], "abc");
        assert!(json["message"].as_str().unwrap().contains("may still have completed"));
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

    #[tokio::test]
    async fn remove_refuses_stale_video_id() {
        let (app, ctx) = test_router();
        let response = request(
            app.clone(),
            "/api/v1/queue/remove",
            json!({ "index": 1, "video_id": "stale" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let json = json_body(response).await;
        assert_eq!(json["error"], "queue_changed");
        assert!(ctx.removals.lock().unwrap().is_empty());

        let response = request(
            app,
            "/api/v1/queue/remove",
            json!({ "index": 1, "video_id": "b" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*ctx.removals.lock().unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn current_track_and_queue_mutations_are_serialized() {
        let (app, ctx) = test_router_with(MockContext {
            delay: Duration::from_millis(80),
            ..Default::default()
        });
        let next = request(app.clone(), "/api/v1/playback/next", json!({}), Some("Bearer abc123"));
        let clear = request(app, "/api/v1/queue/clear-queued", json!({}), Some("Bearer abc123"));
        let (next, clear) = tokio::join!(next, clear);
        assert_eq!(next.status(), StatusCode::OK);
        assert_eq!(clear.status(), StatusCode::OK);
        assert_eq!(ctx.max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn queue_request_maps_parser_errors() {
        let cases = vec![
            ("", "invalid_input"),
            ("https://vimeo.com/123456", "unsupported_url"),
            ("https://music.youtube.com/playlist?list=PLxxx", "playlist_not_supported"),
        ];
        for (input, code) in cases {
            let response = post("/api/v1/queue/request", json!({ "input": input })).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{input}");
            let json = json_body(response).await;
            assert_eq!(json["error"], code, "{input}");
        }
    }

    #[tokio::test]
    async fn queue_request_maps_resolve_errors() {
        let (app, _) = test_router_with(MockContext {
            resolve_error: Some(ResolveError::NotFound),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(response).await["error"], "not_found");

        let (app, _) = test_router_with(MockContext {
            resolve_error: Some(ResolveError::ResolveFailed("boom".into())),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(json_body(response).await["error"], "resolve_failed");

        let (app, _) = test_router_with(MockContext {
            resolve_error: Some(ResolveError::LiveStream),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json_body(response).await["error"], "duration_unknown");
    }

    #[tokio::test]
    async fn queue_request_enforces_duration() {
        let (app, _) = test_router_with(MockContext {
            resolve_item: resolved("resolved123", None),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song", "max_duration_sec": 60 }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json_body(response).await["error"], "duration_unknown");

        let response =
            post("/api/v1/queue/request", json!({ "input": "song", "max_duration_sec": 60 })).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let json = json_body(response).await;
        assert_eq!(json["error"], "too_long");
        assert_eq!(json["duration_sec"], 180);
        assert_eq!(json["max_duration_sec"], 60);

        let (app, _) = test_router_with(MockContext {
            resolve_item: resolved("resolved123", None),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song", "max_duration_sec": 60, "allow_unknown_duration": true }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn queue_request_rejects_duplicates_and_honours_override() {
        for (id, expected) in [("a", 0), ("b", 1)] {
            let (app, _) = test_router_with(MockContext {
                resolve_item: resolved(id, Some("3:00")),
                ..Default::default()
            });
            let response = request(
                app,
                "/api/v1/queue/request",
                json!({ "input": "song" }),
                Some("Bearer abc123"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let json = json_body(response).await;
            assert_eq!(json["error"], "duplicate");
            assert_eq!(json["position"], expected);
        }

        let (app, ctx) = test_router_with(MockContext {
            resolve_item: resolved("b", Some("3:00")),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song", "allow_duplicates": true }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(ctx.inserted.lock().unwrap()[0].2, true);
    }

    #[tokio::test]
    async fn play_next_and_append_use_different_positions() {
        let (app, ctx) = test_router();
        let response = request(
            app.clone(),
            "/api/v1/queue/add",
            json!({ "input": "song", "requested_by": "alice" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["position"], 2); // before the autoplay row at index 2
        assert_eq!(json["item"]["queued_by"], "alice");
        assert_eq!(json["item"]["duration"], "3:00");

        let response = request(
            app,
            "/api/v1/queue/play-next",
            json!({ "input": "next song" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["position"], 1); // after the current row

        let inserted = ctx.inserted.lock().unwrap();
        assert_eq!(inserted.len(), 2);
        assert_eq!((inserted[0].1, inserted[1].1), (false, true));
    }

    #[tokio::test]
    async fn empty_queue_reports_position_zero() {
        let (app, _) = test_router_with(MockContext {
            rows: StdMutex::new(Vec::new()),
            current: AtomicUsize::new(0),
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song", "requested_by": "alice" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["position"], 0);
        assert_eq!(json["item"]["queued_by"], "alice");
    }

    #[tokio::test]
    async fn concurrent_duplicate_requests_have_one_winner() {
        let (app, ctx) = test_router_with(MockContext {
            delay: Duration::from_millis(100),
            ..Default::default()
        });
        let first = request(
            app.clone(),
            "/api/v1/queue/request",
            json!({ "input": "song" }),
            Some("Bearer abc123"),
        );
        let second = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song" }),
            Some("Bearer abc123"),
        );
        let (first, second) = tokio::join!(first, second);
        let mut codes = [first.status().as_u16(), second.status().as_u16()];
        codes.sort_unstable();
        assert_eq!(codes, [200, 409]);
        assert_eq!(ctx.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.inserted.lock().unwrap().len(), 1);
    }
}
