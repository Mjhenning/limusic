//! HTTP handlers and bearer-token middleware for the local control API.

use std::sync::Arc;
use std::time::Duration;

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
    duration_secs, ApiJson, ApiQuery, ApiShared, ApiTrack, ErrorBody, NextResponse,
    NowPlayingResponse, OkResponse, PlaybackStateResponse, PreviousResponse, QueueQuery,
    QueueRequest, QueueRequestResponse, QueueResponse, RemoveRequest, ResolveError, SeekRequest,
    SeekResponse, SimpleQueueRequest, StatusResponse, VolumeRequest, VolumeResponse,
};

/// A mutation waits at most this long for the shared lock; longer waits answer `503 busy` with
/// `Retry-After: 10` instead of tying up an HTTP worker behind a stuck skip.
const CONTROL_API_MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// The HTTP-side deadline for `POST /playback/next`. The underlying skip is detached, not aborted.
const CONTROL_API_NEXT_TIMEOUT: Duration = Duration::from_secs(15);

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
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(shared.clone(), require_bearer))
        .with_state(shared)
}

async fn route_not_found() -> Response {
    ErrorBody::route_not_found().into_response()
}

async fn method_not_allowed() -> Response {
    ErrorBody::method_not_allowed().into_response()
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
/// `currentIndex` and `prevTrack` are deliberately absent. `limit` and `offset` page `items` only;
/// `total` and the navigation indices still describe the full queue. `from=current` starts the
/// window at the current row so a client can fetch it plus upcoming tracks in one call; the
/// effective full-queue start is echoed as `offset`. See [`QueueResponse`].
async fn queue(
    State(shared): State<Arc<ApiShared>>,
    ApiQuery(query): ApiQuery<QueueQuery>,
) -> Response {
    let snap = shared.context.queue_snapshot().await;
    let neighbors = shared.context.queue_neighbors().await;
    let response = QueueResponse::from_snapshot(snap, neighbors);
    let offset = match query.from.as_deref() {
        Some("current") => query.offset.or(Some(response.current_index)),
        Some(_) => return bad_request("invalid_input", "from must be 'current'").into_response(),
        None => query.offset,
    };
    Json(response.page(query.limit, offset)).into_response()
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
/// pre-change snapshot. A wait longer than 10 s is rejected as `busy`; the caller must not proceed
/// without the guard, otherwise ordering would be lost.
async fn mutation_lock(shared: &ApiShared) -> Result<tokio::sync::OwnedMutexGuard<()>, ErrorBody> {
    // `lock_owned` ties the guard to the Arc, not to this borrow, so the next handler can hand it
    // to the detached skip task and still be sure the lock is held until that task finishes.
    tokio::time::timeout(
        CONTROL_API_MUTATION_LOCK_TIMEOUT,
        Arc::clone(&shared.mutation).lock_owned(),
    )
    .await
    .map_err(|_| ErrorBody::busy())
}

/// `POST /api/v1/playback/play`: idempotent. Starts a restored queue when mpv is idle.
async fn play(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().play().await;
    Json(PlaybackStateResponse { ok: true, playing }).into_response()
}

/// `POST /api/v1/playback/pause`: idempotent. Safe on an idle or already-paused queue.
async fn pause(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().pause().await;
    Json(PlaybackStateResponse { ok: true, playing }).into_response()
}

/// `POST /api/v1/playback/toggle`: flips transport state and reports the final state.
async fn toggle(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let playing = shared.context.clone().toggle().await;
    Json(PlaybackStateResponse { ok: true, playing }).into_response()
}

/// `POST /api/v1/playback/next`: waits up to 15 s for the new current row, including a synchronous
/// autoplay fetch, and returns `{ok, current}` with that queue item. The wait ends when the queue's
/// current index or video id changes, not when audio starts. The skip itself is never cancelled: it
/// runs in a spawned task that carries the mutation lock to completion, so a following mutation
/// cannot observe a half-applied skip (new track armed but autoplay fetch unfinished). On expiry the
/// response is `next_timeout` with the track that was current when the request began. That response
/// does **not** guarantee the skip failed; clients should use an HTTP timeout of at least 20 s and
/// re-read `/status` before retrying.
async fn next(State(shared): State<Arc<ApiShared>>) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
    // Read before handing the guard to the task: after the deadline the handler must answer with
    // the track that was current at request time, not with a later snapshot.
    let current = shared.context.playback_snapshot().await.get("now").cloned();
    let task_shared = Arc::clone(&shared);
    let task = tokio::spawn(async move {
        let _lock = lock;
        task_shared.context.clone().next().await
    });
    match tokio::time::timeout(CONTROL_API_NEXT_TIMEOUT, task).await {
        Ok(Ok(Some(item))) => {
            Json(NextResponse { ok: true, current: ApiTrack::from_value(&item) }).into_response()
        }
        Ok(Ok(None)) => ErrorBody::next_timeout(current).into_response(),
        Ok(Err(join_error)) => {
            tracing::warn!("control API skip task failed: {join_error}");
            ErrorBody::playback_error("the skip task failed").into_response()
        }
        Err(_) => ErrorBody::next_timeout(current).into_response(),
    }
}

/// `POST /api/v1/playback/previous`: uses the UI's rewind-then-step rule. The body has `ok`,
/// `restarted` (which branch ran) and `current` (the row now playing). The read-only queue
/// `previous` field is always current-1.
async fn previous(State(shared): State<Arc<ApiShared>>) -> Response {
    let _lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    let restarted = shared.context.clone().previous().await;
    Json(PreviousResponse { ok: true, restarted, current: current_api_track(&shared).await })
        .into_response()
}

/// The current row after a transport action, read from the queue projection rather than the raw UI
/// snapshot. A stale or empty queue is represented by a default (empty) track instead of an error.
async fn current_api_track(shared: &ApiShared) -> ApiTrack {
    let snap = shared.context.queue_snapshot().await;
    let index = snap.get("currentIndex").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    snap.get("items")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.get(index))
        .map(ApiTrack::from_value)
        .unwrap_or_default()
}

/// `POST /api/v1/playback/seek`: validates a finite `position_sec` (alias `position`) from 0 to the
/// reported duration, then echoes the effective value as `position_sec`.
async fn seek(
    State(shared): State<Arc<ApiShared>>,
    ApiJson(body): ApiJson<SeekRequest>,
) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    if !body.position_sec.is_finite() || body.position_sec < 0.0 {
        return bad_request("invalid_position", "position must be finite and non-negative")
            .into_response();
    }
    let snap = shared.context.playback_snapshot().await;
    let duration = snap.get("duration").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    if !duration.is_finite() || body.position_sec > duration {
        return bad_request("invalid_position", "position exceeds the current duration")
            .into_response();
    }
    match shared.context.clone().seek(body.position_sec).await {
        Ok(position) => Json(SeekResponse { ok: true, position_sec: position }).into_response(),
        Err(message) => ErrorBody::playback_error(message).into_response(),
    }
}

/// `POST /api/v1/playback/volume`: validates an integer 0–100 before touching the player.
async fn volume(
    State(shared): State<Arc<ApiShared>>,
    ApiJson(body): ApiJson<VolumeRequest>,
) -> Response {
    if let Err(error) = guard(&shared).await {
        return error.into_response();
    }
    if !(0..=100).contains(&body.volume) {
        return bad_request("invalid_volume", "volume must be an integer from 0 to 100")
            .into_response();
    }
    match shared.context.clone().set_volume(body.volume).await {
        Ok(()) => Json(VolumeResponse { ok: true, volume: body.volume }).into_response(),
        Err(message) => ErrorBody::playback_error(message).into_response(),
    }
}

fn row_video_id(row: &serde_json::Value) -> Option<&serde_json::Value> {
    row.get("video_id").or_else(|| row.get("videoId"))
}

/// `POST /api/v1/queue/remove`: removes an upcoming row only; out-of-range and current indices are
/// rejected instead of silently ignored. An optional `video_id` makes the removal conditional: a
/// stale id gets `queue_changed` without touching the queue.
async fn remove(
    State(shared): State<Arc<ApiShared>>,
    ApiJson(body): ApiJson<RemoveRequest>,
) -> Response {
    let _lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
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
            .and_then(row_video_id)
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
    let _lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
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

/// `POST /api/v1/queue/request`: resolve one song and add it to the queue. The response is
/// `{ok, position, item}`; `position` is the final zero-based index and `item` is the queued
/// [`ApiTrack`], including `is_video`, `duration_sec` and `queued_by`.
///
/// `max_duration_sec` measures the version that will actually play: the queued `video_id`'s own
/// duration, never its counterpart's. No extra `/player` lookup is made. It fails closed with 422
/// `duration_unknown` unless `allow_unknown_duration` is true; a known but longer track gets 422
/// `too_long` with `duration_sec` and `max_duration_sec`.
/// Duplicate rejection (409 with the existing `position`) is skipped by `allow_duplicates`, but the
/// app's global `prevent_duplicates` setting still applies inside the insertion. A timeout from a
/// previous skip never leaves this endpoint guessing: it holds the same mutation lock as other
/// current-track/queue changes.
async fn handle_queue_request(shared: Arc<ApiShared>, body: QueueRequest) -> Response {
    let _lock = match mutation_lock(&shared).await {
        Ok(lock) => lock,
        Err(error) => return error.into_response(),
    };
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
    if body.reject_explicit && item.explicit {
        return ErrorBody::explicit_content().into_response();
    }
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
        .insert_request(item.clone(), body.play_next.unwrap_or(true), body.allow_duplicates)
        .await
    {
        Ok((position, inserted)) => Json(QueueRequestResponse {
            ok: true,
            position,
            item: ApiTrack::from_song_item(&inserted, false),
        })
        .into_response(),
        Err(matched) => ErrorBody::duplicate(matched).into_response(),
    }
}

async fn queue_request(
    State(shared): State<Arc<ApiShared>>,
    ApiJson(body): ApiJson<QueueRequest>,
) -> Response {
    handle_queue_request(shared, body).await
}

/// `POST /api/v1/queue/add`: thin append variant of `/queue/request` (same defaults as an empty
/// options object). `play_next` is false and `allow_duplicates` stays at the app default.
async fn queue_add(
    State(shared): State<Arc<ApiShared>>,
    ApiJson(body): ApiJson<SimpleQueueRequest>,
) -> Response {
    handle_queue_request(
        shared,
        QueueRequest {
            input: body.input,
            requested_by: body.requested_by,
            max_duration_sec: None,
            allow_duplicates: false,
            play_next: Some(false),
            allow_unknown_duration: false,
            reject_explicit: false,
        },
    )
    .await
}

/// `POST /api/v1/queue/play-next`: thin Play-next variant of `/queue/request`.
async fn queue_play_next(
    State(shared): State<Arc<ApiShared>>,
    ApiJson(body): ApiJson<SimpleQueueRequest>,
) -> Response {
    handle_queue_request(
        shared,
        QueueRequest {
            input: body.input,
            requested_by: body.requested_by,
            max_duration_sec: None,
            allow_duplicates: false,
            play_next: Some(true),
            allow_unknown_duration: false,
            reject_explicit: false,
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
        counterpart: Option<String>,
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
                    MockRow {
                        id: "a".into(),
                        counterpart: None,
                        autoplay: false,
                        queued: false,
                        queued_by: None,
                    },
                    MockRow {
                        id: "b".into(),
                        counterpart: None,
                        autoplay: false,
                        queued: false,
                        queued_by: Some("friend".into()),
                    },
                    MockRow {
                        id: "c".into(),
                        counterpart: None,
                        autoplay: true,
                        queued: false,
                        queued_by: None,
                    },
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
                        "counterpart": row.counterpart,
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
        ) -> Result<(usize, SongItem), super::super::types::DuplicateMatch> {
            self.enter().await;
            let result = {
                let mut rows = self.rows.lock().unwrap();
                if !allow_duplicates {
                    let queue: Vec<SongItem> = rows
                        .iter()
                        .map(|row| SongItem {
                            video_id: row.id.clone(),
                            counterpart: row.counterpart.clone(),
                            ..Default::default()
                        })
                        .collect();
                    super::super::types::find_request_duplicate(&item, queue.iter())
                        .map_or(Ok(()), Err)
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
                    }
                    .min(rows.len());
                    rows.insert(
                        at,
                        MockRow {
                            id: item.video_id.clone(),
                            counterpart: item.counterpart.clone(),
                            autoplay: false,
                            queued: next,
                            queued_by: item.queued_by.clone(),
                        },
                    );
                    let mut inserted = item.clone();
                    inserted.queued = next;
                    inserted.queued_end = at + 1 == rows.len();
                    Ok((at, inserted))
                })
            };
            if let Ok((_, ref inserted)) = result {
                self.inserted.lock().unwrap().push((inserted.clone(), next, allow_duplicates));
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

    fn test_router_shared(context: MockContext) -> (Router, Arc<MockContext>, Arc<ApiShared>) {
        let context = Arc::new(context);
        let shared = Arc::new(ApiShared {
            token: Arc::new(RwLock::new("abc123".to_string())),
            context: context.clone(),
            mutation: Arc::new(tokio::sync::Mutex::new(())),
        });
        (router(Arc::clone(&shared)), context, shared)
    }

    fn test_router_with(context: MockContext) -> (Router, Arc<MockContext>) {
        let (app, context, _) = test_router_shared(context);
        (app, context)
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
        assert_eq!(json["offset"], 0);
        assert_eq!(json["next"]["video_id"], "b");
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
        assert_eq!(json_body(response).await["position_sec"], 5.0);

        let response = post("/api/v1/playback/volume", json!({ "volume": 42 })).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["volume"], 42);
    }

    #[tokio::test]
    async fn next_returns_new_current_item() {
        let response = post("/api/v1/playback/next", json!({})).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["current"]["video_id"], "next");
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

    #[tokio::test(start_paused = true)]
    async fn lock_wait_timeout_returns_busy_with_retry_after() {
        let (app, _, shared) = test_router_shared(MockContext::default());
        let held = shared.mutation.lock().await;
        let response =
            request(app, "/api/v1/queue/clear-queued", json!({}), Some("Bearer abc123")).await;
        drop(held);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(axum::http::header::RETRY_AFTER).unwrap(), "10");
        let json = json_body(response).await;
        assert_eq!(json["error"], "busy");
    }

    #[tokio::test(start_paused = true)]
    async fn next_deadline_reports_current_track_and_keeps_the_skip_running() {
        let (app, ctx) = test_router_with(MockContext {
            delay: Duration::from_secs(20),
            next_item: StdMutex::new(Some(json!({ "videoId": "late" }))),
            ..Default::default()
        });
        let response =
            request(app, "/api/v1/playback/next", json!({}), Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let json = json_body(response).await;
        assert_eq!(json["error"], "next_timeout");
        assert_eq!(json["current"]["videoId"], "abc");
        assert!(json["message"].as_str().unwrap().contains("at least 20 s"));
        // The detached task still owns the lock; it has not been cancelled at the deadline.
        assert_eq!(ctx.active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn second_mutation_waits_for_the_first_skip() {
        let (app, ctx) =
            test_router_with(MockContext { delay: Duration::from_secs(5), ..Default::default() });
        let first_app = app.clone();
        let first = tokio::spawn(async move {
            request(first_app, "/api/v1/playback/next", json!({}), Some("Bearer abc123")).await
        });
        tokio::time::advance(Duration::from_millis(100)).await;
        assert_eq!(ctx.active.load(Ordering::SeqCst), 1);

        let second = tokio::spawn(async move {
            request(app, "/api/v1/queue/clear-queued", json!({}), Some("Bearer abc123")).await
        });
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!second.is_finished());
        assert_eq!(ctx.clears.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_secs(4)).await;
        let second = second.await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(ctx.clears.load(Ordering::SeqCst), 1);
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
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
            assert_eq!(json["matched"]["video_id"], id);
            assert_eq!(json["matched"]["matched_by"], "video_id");
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

    #[tokio::test]
    async fn malformed_json_bodies_use_the_error_envelope() {
        let (app, _) = test_router();
        for (body, field) in [(json!({}), "volume"), (json!({ "volume": "loud" }), "volume")] {
            let response =
                request(app.clone(), "/api/v1/playback/volume", body, Some("Bearer abc123")).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")));
            let json = json_body(response).await;
            assert_eq!(json["error"], "invalid_input");
            assert!(json["message"].as_str().unwrap().contains(field));
        }
    }

    #[tokio::test]
    async fn wrong_content_type_uses_the_error_envelope() {
        let req = Request::builder()
            .uri("/api/v1/playback/volume")
            .method("POST")
            .header(AUTHORIZATION, "Bearer abc123")
            .header("content-type", "text/plain")
            .body(Body::from("{}"))
            .unwrap();
        let response = test_router().0.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let json = json_body(response).await;
        assert_eq!(json["error"], "unsupported_media_type");
        assert_eq!(json["message"], "request bodies must use application/json");
    }

    #[tokio::test]
    async fn routing_failures_use_the_error_envelope() {
        let (app, _) = test_router();
        let response =
            request(app.clone(), "/api/v1/nope", Value::Null, Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(response).await["error"], "not_found");

        let response =
            request(app, "/api/v1/playback/volume", Value::Null, Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(json_body(response).await["error"], "method_not_allowed");
    }

    #[tokio::test]
    async fn queue_paging_keeps_full_queue_context() {
        let (app, _) = test_router();
        let response =
            request(app, "/api/v1/queue?limit=1&offset=1", Value::Null, Some("Bearer abc123"))
                .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        assert_eq!(json["items"][0]["video_id"], "b");
        assert_eq!(json["total"], 3);
        assert_eq!(json["current_index"], 0);
        assert_eq!(json["offset"], 1);
        assert_eq!(json["next_index"], 1);
    }

    #[tokio::test]
    async fn queue_request_defaults_to_play_next_and_returns_final_row() {
        let (app, ctx) = test_router();
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song", "requested_by": "alice" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["position"], 1);
        assert_eq!(json["item"]["video_id"], "resolved123");
        assert_eq!(json["item"]["queued"], true);
        assert_eq!(json["item"]["queued_end"], false);
        assert_eq!(json["item"]["duration_sec"], 180);
        assert_eq!(json["item"]["is_video"], false);
        assert_eq!(json["item"]["artist_list"][0], "Tester");
        assert_eq!(ctx.inserted.lock().unwrap()[0].1, true);
    }

    #[tokio::test]
    async fn reject_explicit_refuses_before_insertion() {
        let explicit = SongItem { explicit: true, ..resolved("explicit123", Some("3:00")) };
        let (app, ctx) =
            test_router_with(MockContext { resolve_item: explicit, ..Default::default() });
        let response = request(
            app.clone(),
            "/api/v1/queue/request",
            json!({ "input": "song", "reject_explicit": true }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(json_body(response).await["error"], "explicit_content");
        assert!(ctx.inserted.lock().unwrap().is_empty());

        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song" }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn seek_accepts_canonical_seconds_field() {
        let response = post("/api/v1/playback/seek", json!({ "position_sec": 5.0 })).await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["ok"], true);
        assert_eq!(json["position_sec"], 5.0);
    }

    #[tokio::test]
    async fn counterpart_matches_are_duplicates_in_both_directions() {
        let audio = |id: &str, twin: Option<&str>| SongItem {
            counterpart: twin.map(str::to_owned),
            ..resolved(id, Some("3:00"))
        };
        let row = |id: &str, twin: Option<&str>| MockRow {
            id: id.into(),
            counterpart: twin.map(str::to_owned),
            autoplay: false,
            queued: false,
            queued_by: None,
        };
        let cases = vec![
            // requested video_id == queue counterpart
            (audio("audio1", None), row("video1", Some("audio1")), "counterpart"),
            // requested video_id == queue counterpart, roles reversed
            (
                SongItem { is_video: true, ..audio("video2", Some("audio2")) },
                row("audio2", Some("video2")),
                "counterpart",
            ),
            // requested counterpart == queue video_id
            (
                SongItem { is_video: true, ..audio("audio3", Some("video3")) },
                row("video3", None),
                "counterpart",
            ),
            // requested counterpart == queue counterpart
            (
                SongItem { is_video: true, ..audio("video4", Some("audio4")) },
                row("other4", Some("audio4")),
                "counterpart",
            ),
        ];
        for (resolve_item, existing, matched_by) in cases {
            let (app, ctx) = test_router_with(MockContext {
                resolve_item,
                rows: StdMutex::new(vec![existing]),
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
            assert_eq!(json["position"], 0);
            assert_eq!(json["matched"]["matched_by"], matched_by);
            assert!(json["matched"]["video_id"].as_str().is_some());
            assert!(ctx.inserted.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn request_response_reports_the_queued_video_version() {
        let (app, _) = test_router_with(MockContext {
            resolve_item: SongItem {
                is_video: true,
                counterpart: Some("audio123".into()),
                ..resolved("video123", Some("5:00"))
            },
            ..Default::default()
        });
        let response = request(
            app,
            "/api/v1/queue/request",
            json!({ "input": "song", "max_duration_sec": 360 }),
            Some("Bearer abc123"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["item"]["video_id"], "video123");
        assert_eq!(json["item"]["is_video"], true);
        assert_eq!(json["item"]["duration_sec"], 300);
    }

    #[tokio::test]
    async fn audio_request_hitting_music_video_counterpart_reports_counterpart() {
        let (app, _) = test_router_with(MockContext {
            resolve_item: resolved("audio123", Some("3:00")),
            rows: StdMutex::new(vec![MockRow {
                id: "video123".into(),
                counterpart: Some("audio123".into()),
                autoplay: false,
                queued: false,
                queued_by: None,
            }]),
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
        assert_eq!(json["position"], 0);
        assert_eq!(json["matched"]["video_id"], "video123");
        assert_eq!(json["matched"]["counterpart"], "audio123");
        assert_eq!(json["matched"]["matched_by"], "counterpart");
    }

    #[tokio::test]
    async fn queue_from_current_starts_at_the_current_track() {
        let (app, _) =
            test_router_with(MockContext { current: AtomicUsize::new(1), ..Default::default() });
        let response =
            request(app, "/api/v1/queue?from=current&limit=2", Value::Null, Some("Bearer abc123"))
                .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["offset"], 1);
        assert_eq!(json["current_index"], 1);
        assert_eq!(json["total"], 3);
        let items = json["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["video_id"], "b");
        assert_eq!(items[1]["video_id"], "c");
    }

    #[tokio::test]
    async fn queue_from_current_yields_to_an_explicit_offset() {
        let (app, _) =
            test_router_with(MockContext { current: AtomicUsize::new(1), ..Default::default() });
        let response =
            request(app, "/api/v1/queue?from=current&offset=2", Value::Null, Some("Bearer abc123"))
                .await;
        assert_eq!(response.status(), StatusCode::OK);
        let json = json_body(response).await;
        assert_eq!(json["offset"], 2);
        assert_eq!(json["items"][0]["video_id"], "c");
    }

    #[tokio::test]
    async fn queue_rejects_an_unknown_from_value() {
        let (app, _) = test_router();
        let response =
            request(app, "/api/v1/queue?from=previous", Value::Null, Some("Bearer abc123")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json = json_body(response).await;
        assert_eq!(json["error"], "invalid_input");
        assert_eq!(json["message"], "from must be 'current'");
    }
}
